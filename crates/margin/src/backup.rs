//! Off-box backups: `margin backup` uploads a consistent snapshot of the database with a
//! credential that can only put objects, and `margin restore-drill` (run with the operator's own
//! identity, which can list and read) fetches the newest one back and reads all of it with
//! margin's own code. Both talk to the bucket through [`ObjectStore`].

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use margin_core::Clock;
use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;

use crate::db::{self, Db};
use crate::{metrics, store, threads};

/// Every object this module writes starts with this; the drill lists by it.
pub const KEY_PREFIX: &str = "margin-";
/// The drill refuses a newest snapshot older than this: backups run every 15 minutes.
pub const MAX_AGE_SECS: i64 = 60 * 60;
/// The environment variable naming the bucket.
pub const BUCKET_ENV: &str = "MARGIN_BACKUP_BUCKET";

const BUSY_TIMEOUT: Duration = Duration::from_secs(30);
const PUT_DEADLINE: Duration = Duration::from_secs(300);
const LIST_DEADLINE: Duration = Duration::from_secs(120);
const GET_DEADLINE: Duration = Duration::from_secs(300);
const PAGE: usize = 100;

#[derive(Debug, PartialEq, Eq)]
pub enum BackupError {
    MissingBucket,
    Snapshot(String),
    Store(String),
    Drill(String),
}

impl std::fmt::Display for BackupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackupError::MissingBucket => write!(f, "{BUCKET_ENV} is not set"),
            BackupError::Snapshot(e) => write!(f, "snapshot: {e}"),
            BackupError::Store(e) => write!(f, "object store: {e}"),
            BackupError::Drill(e) => write!(f, "restore drill: {e}"),
        }
    }
}

impl std::error::Error for BackupError {}

/// The bucket name from the value of [`BUCKET_ENV`]; a missing or empty one is refused by name.
pub fn bucket_from(value: Option<String>) -> Result<String, BackupError> {
    match value {
        Some(v) if !v.trim().is_empty() => Ok(v.trim().to_string()),
        _ => Err(BackupError::MissingBucket),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectInfo {
    pub key: String,
    /// Seconds since the Unix epoch.
    pub last_modified: i64,
}

/// The bucket, as far as backup and drill need it. The writer only ever calls `put`.
pub trait ObjectStore {
    fn put(&self, key: &str, body: &Path) -> Result<(), BackupError>;
    /// Every object whose key starts with `prefix`.
    fn list(&self, prefix: &str) -> Result<Vec<ObjectInfo>, BackupError>;
    /// Write the object to `dest`.
    fn get(&self, key: &str, dest: &Path) -> Result<(), BackupError>;
}

// --- UTC time without a calendar dependency (Howard Hinnant's civil-date algorithms) ---

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = y - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `yyyymmddThhmmssZ`, UTC.
pub fn stamp(unix: i64) -> String {
    let (y, m, d) = civil_from_days(unix.div_euclid(86_400));
    let s = unix.rem_euclid(86_400);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        s / 3600,
        s % 3600 / 60,
        s % 60
    )
}

/// Unix seconds of an ISO-8601 time such as `2026-11-22T10:00:00+00:00` or `...00.000Z`.
fn parse_iso(s: &str) -> Option<i64> {
    let num = |range: std::ops::Range<usize>| s.get(range)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, sec) = (
        num(0..4)?,
        num(5..7)?,
        num(8..10)?,
        num(11..13)?,
        num(14..16)?,
        num(17..19)?,
    );
    let mut rest = &s[19..];
    if let Some(frac) = rest.strip_prefix('.') {
        rest = frac.trim_start_matches(|c: char| c.is_ascii_digit());
    }
    let offset = match rest {
        "Z" | "" => 0,
        _ => {
            let sign = match rest.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let hh = rest.get(1..3)?.parse::<i64>().ok()?;
            let mm = rest.get(4..6)?.parse::<i64>().ok()?;
            sign * (hh * 3600 + mm * 60)
        }
    };
    Some(days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + sec - offset)
}

// --- the real bucket: the aws CLI ---

/// The bucket through `aws s3api`, JSON answers deserialised, a deadline on every call.
pub struct AwsCli {
    bucket: String,
}

impl AwsCli {
    pub fn new(bucket: String) -> AwsCli {
        AwsCli { bucket }
    }

    fn run(&self, args: &[&str], deadline: Duration) -> Result<Vec<u8>, BackupError> {
        let fail = |e: String| BackupError::Store(format!("aws s3api {}: {e}", args[0]));
        let mut child = Command::new("aws")
            .arg("s3api")
            .args(args)
            .args(["--bucket", &self.bucket, "--no-cli-pager"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| fail(format!("cannot start aws: {e}")))?;
        let drain = |pipe: Option<Box<dyn Read + Send>>| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                if let Some(mut p) = pipe {
                    let _ = p.read_to_end(&mut buf);
                }
                buf
            })
        };
        let out = drain(child.stdout.take().map(|s| Box::new(s) as _));
        let err = drain(child.stderr.take().map(|s| Box::new(s) as _));
        let started = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if started.elapsed() > deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(fail(format!("no answer within {}s", deadline.as_secs())));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(e) => return Err(fail(e.to_string())),
            }
        };
        let out = out.join().unwrap_or_default();
        let err = err.join().unwrap_or_default();
        if status.success() {
            Ok(out)
        } else {
            Err(fail(format!(
                "{status}: {}",
                String::from_utf8_lossy(&err).trim()
            )))
        }
    }
}

#[derive(Deserialize)]
struct Listing {
    #[serde(rename = "Contents", default)]
    contents: Option<Vec<Listed>>,
}

#[derive(Deserialize)]
struct Listed {
    #[serde(rename = "Key")]
    key: String,
    #[serde(rename = "LastModified")]
    last_modified: String,
}

impl ObjectStore for AwsCli {
    fn put(&self, key: &str, body: &Path) -> Result<(), BackupError> {
        let body = body.to_string_lossy();
        self.run(&["put-object", "--key", key, "--body", &body], PUT_DEADLINE)
            .map(|_| ())
    }

    fn list(&self, prefix: &str) -> Result<Vec<ObjectInfo>, BackupError> {
        let out = self.run(
            &["list-objects-v2", "--prefix", prefix, "--output", "json"],
            LIST_DEADLINE,
        )?;
        if out.iter().all(u8::is_ascii_whitespace) {
            return Ok(Vec::new());
        }
        let listing: Listing = serde_json::from_slice(&out)
            .map_err(|e| BackupError::Store(format!("list-objects-v2 answered badly: {e}")))?;
        listing
            .contents
            .unwrap_or_default()
            .into_iter()
            .map(|o| {
                let last_modified = parse_iso(&o.last_modified).ok_or_else(|| {
                    BackupError::Store(format!("unreadable LastModified {:?}", o.last_modified))
                })?;
                Ok(ObjectInfo {
                    key: o.key,
                    last_modified,
                })
            })
            .collect()
    }

    fn get(&self, key: &str, dest: &Path) -> Result<(), BackupError> {
        let dest = dest.to_string_lossy();
        self.run(&["get-object", "--key", key, &dest], GET_DEADLINE)
            .map(|_| ())
    }
}

// --- margin backup ---

#[derive(Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub key: String,
    /// Compressed size.
    pub bytes: u64,
}

fn snap<E: std::fmt::Display>(what: &'static str) -> impl FnOnce(E) -> BackupError {
    move |e| BackupError::Snapshot(format!("{what}: {e}"))
}

/// Snapshot `database` with `VACUUM INTO`, gzip it and put it under a key made from the clock.
/// The temporary files live in a fresh directory under `scratch` and are removed on every path.
pub fn snapshot(
    database: &Path,
    scratch: &Path,
    store: &dyn ObjectStore,
    clock: &dyn Clock,
) -> Result<Snapshot, BackupError> {
    let key = format!("{KEY_PREFIX}{}.db.gz", stamp(clock.now_unix()));
    let tmp = tempfile::tempdir_in(scratch).map_err(snap("temporary directory"))?;
    let plain = tmp.path().join("snapshot.db");
    let packed = tmp.path().join("snapshot.db.gz");

    // No CREATE: a wrong path is an error, not an empty database uploaded as a backup.
    let conn = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(snap("opening the database"))?;
    conn.busy_timeout(BUSY_TIMEOUT)
        .map_err(snap("busy_timeout"))?;
    conn.execute("VACUUM INTO ?1", [plain.to_string_lossy().as_ref()])
        .map_err(snap("VACUUM INTO"))?;
    drop(conn);

    let mut from = std::fs::File::open(&plain).map_err(snap("reading the snapshot"))?;
    let mut gz = GzEncoder::new(
        std::fs::File::create(&packed).map_err(snap("creating the archive"))?,
        Compression::default(),
    );
    std::io::copy(&mut from, &mut gz).map_err(snap("compressing"))?;
    gz.finish()
        .and_then(|mut f| f.flush())
        .map_err(snap("compressing"))?;
    let bytes = std::fs::metadata(&packed)
        .map_err(snap("sizing the archive"))?
        .len();

    store.put(&key, &packed)?;
    Ok(Snapshot { key, bytes })
}

/// One whole `margin backup` run: snapshot, upload, then hand the run's metrics to `send`
/// (best effort; its failure never changes the result).
pub fn execute(
    database: &Path,
    scratch: &Path,
    store: &dyn ObjectStore,
    clock: &dyn Clock,
    send: &dyn Fn(&opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest),
) -> Result<Snapshot, BackupError> {
    let started = clock.now_unix();
    let result = snapshot(database, scratch, store, clock);
    let finished = clock.now_unix();
    send(&metrics::backup_request(&metrics::BackupRun {
        ok: result.is_ok(),
        seconds: (finished - started).max(0) as f64,
        bytes: result.as_ref().map_or(0, |s| s.bytes),
        finished_unix: finished,
    }));
    result
}

// --- margin restore-drill ---

#[derive(Debug, PartialEq, Eq)]
pub struct DrillReport {
    pub key: String,
    pub age_secs: i64,
    pub collections: usize,
    pub documents: usize,
    pub threads: usize,
    pub comments: usize,
}

impl std::fmt::Display for DrillReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "object: {}\nage: {}s\ncollections: {}\ndocuments: {}\nthreads: {}\ncomments: {}",
            self.key, self.age_secs, self.collections, self.documents, self.threads, self.comments
        )
    }
}

fn drill_err<E: std::fmt::Display>(what: &'static str) -> impl FnOnce(E) -> BackupError {
    move |e| BackupError::Drill(format!("{what}: {e}"))
}

/// Fetch the newest snapshot, check it, open it with [`Db::open`] and read all of it.
pub async fn drill(store: &dyn ObjectStore, clock: &dyn Clock) -> Result<DrillReport, BackupError> {
    let newest = store
        .list(KEY_PREFIX)?
        .into_iter()
        .max_by(|a, b| (a.last_modified, &a.key).cmp(&(b.last_modified, &b.key)))
        .ok_or_else(|| BackupError::Drill("the bucket holds no snapshot".into()))?;
    let age_secs = clock.now_unix() - newest.last_modified;
    if age_secs > MAX_AGE_SECS {
        return Err(BackupError::Drill(format!(
            "the newest snapshot, {}, is {age_secs}s old; backups run every 15 minutes and the limit is {MAX_AGE_SECS}s",
            newest.key
        )));
    }

    let tmp = tempfile::tempdir().map_err(drill_err("temporary directory"))?;
    let packed = tmp.path().join("fetched.db.gz");
    let restored = tmp.path().join("margin.db");
    store.get(&newest.key, &packed)?;
    let mut unzipped =
        std::fs::File::create(&restored).map_err(drill_err("creating the restored file"))?;
    std::io::copy(
        &mut GzDecoder::new(std::fs::File::open(&packed).map_err(drill_err("reading the object"))?),
        &mut unzipped,
    )
    .map_err(drill_err("unzipping (is the object truncated?)"))?;
    drop(unzipped);

    check_file(&restored)?;

    let db = Db::open(&restored).map_err(drill_err("opening with Db::open"))?;
    let counts = db
        .call(read_everything)
        .await
        .map_err(drill_err("reading"))?
        .map_err(drill_err("reading"))?;
    Ok(DrillReport {
        key: newest.key,
        age_secs,
        ..counts
    })
}

/// `integrity_check`, `foreign_key_check`, and a `user_version` this binary understands.
fn check_file(path: &Path) -> Result<(), BackupError> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(drill_err("opening the restored file"))?;
    let problems: Vec<String> = conn
        .prepare("PRAGMA integrity_check")
        .and_then(|mut s| s.query_map([], |r| r.get(0))?.collect())
        .map_err(drill_err("integrity_check"))?;
    if problems != ["ok"] {
        return Err(BackupError::Drill(format!(
            "integrity_check failed: {}",
            problems.join("; ")
        )));
    }
    let violations = conn
        .prepare("PRAGMA foreign_key_check")
        .and_then(|mut s| {
            Ok(s.query_map([], |_| Ok(()))?
                .collect::<rusqlite::Result<Vec<()>>>()?
                .len())
        })
        .map_err(drill_err("foreign_key_check"))?;
    if violations > 0 {
        return Err(BackupError::Drill(format!(
            "foreign_key_check found {violations} violations"
        )));
    }
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .map_err(drill_err("user_version"))?;
    if version > db::schema_version() as i64 {
        return Err(BackupError::Drill(format!(
            "the snapshot has user_version {version}, newer than this binary's {}; restore with a newer margin",
            db::schema_version()
        )));
    }
    Ok(())
}

fn read_everything(conn: &mut Connection) -> rusqlite::Result<DrillReport> {
    let mut report = DrillReport {
        key: String::new(),
        age_secs: 0,
        collections: 0,
        documents: 0,
        threads: 0,
        comments: 0,
    };
    let mut after: Option<(i64, String)> = None;
    loop {
        let page = store::list(
            conn,
            PAGE,
            after.as_ref().map(|(at, id)| (*at, id.as_str())),
        )?;
        let Some(last) = page.last() else { break };
        after = Some((last.created_at, last.id.clone()));
        let full = page.len() == PAGE;
        for summary in page {
            let collection = store::get(conn, &summary.id)?
                .ok_or_else(|| rusqlite::Error::QueryReturnedNoRows)?;
            report.collections += 1;
            report.documents += collection.documents.len();
            let all = threads::all(conn, threads::Scope::Agent, &summary.id)?;
            report.threads += all.len();
            report.comments += all.iter().map(|t| t.comments.len()).sum::<usize>();
        }
        if !full {
            break;
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Kind, NewCollection, NewDocument};
    use crate::threads::{NewThread, Visibility};
    use margin_core::anchor::Anchor;
    use margin_test_id::test_id;
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicI64, Ordering};

    struct TestClock(AtomicI64);

    impl TestClock {
        fn at(unix: i64) -> TestClock {
            TestClock(AtomicI64::new(unix))
        }
        fn set(&self, unix: i64) {
            self.0.store(unix, Ordering::SeqCst);
        }
    }

    impl Clock for TestClock {
        fn now_unix(&self) -> i64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    const T0: i64 = 1_700_000_000; // 2023-11-14T22:13:20Z

    /// The bucket in memory; an object's last-modified is the clock's time when it was put.
    struct MemStore<'a> {
        clock: &'a TestClock,
        objects: Mutex<BTreeMap<String, (i64, Vec<u8>)>>,
        fail_put: bool,
    }

    impl<'a> MemStore<'a> {
        fn new(clock: &'a TestClock) -> Self {
            MemStore {
                clock,
                objects: Mutex::default(),
                fail_put: false,
            }
        }
        fn keys(&self) -> Vec<String> {
            self.objects.lock().unwrap().keys().cloned().collect()
        }
        fn insert(&self, key: &str, last_modified: i64, body: Vec<u8>) {
            self.objects
                .lock()
                .unwrap()
                .insert(key.into(), (last_modified, body));
        }
        fn body(&self, key: &str) -> Vec<u8> {
            self.objects.lock().unwrap()[key].1.clone()
        }
    }

    impl ObjectStore for MemStore<'_> {
        fn put(&self, key: &str, body: &Path) -> Result<(), BackupError> {
            if self.fail_put {
                return Err(BackupError::Store("put refused".into()));
            }
            let bytes = std::fs::read(body).unwrap();
            self.insert(key, self.clock.now_unix(), bytes);
            Ok(())
        }
        fn list(&self, prefix: &str) -> Result<Vec<ObjectInfo>, BackupError> {
            Ok(self
                .objects
                .lock()
                .unwrap()
                .iter()
                .filter(|(k, _)| k.starts_with(prefix))
                .map(|(k, (at, _))| ObjectInfo {
                    key: k.clone(),
                    last_modified: *at,
                })
                .collect())
        }
        fn get(&self, key: &str, dest: &Path) -> Result<(), BackupError> {
            std::fs::write(dest, self.body(key)).map_err(|e| BackupError::Store(e.to_string()))
        }
    }

    fn open(path: &Path) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        conn
    }

    fn new_collection(conn: &mut Connection, n: usize) {
        let id = format!("c{n}");
        let doc_id = format!("d{n}");
        let documents = [(
            doc_id,
            NewDocument {
                path: "a.rs".into(),
                kind: Kind::Code,
                text: "fn a() {}\nfn b() {}\n".into(),
            },
        )];
        store::create(
            conn,
            &NewCollection {
                id: &id,
                title: &format!("review {n}"),
                cover_note: "",
                created_by: "agent",
                source: None,
                now: T0 + n as i64,
                documents: &documents,
            },
        )
        .unwrap();
    }

    fn new_thread(conn: &mut Connection, n: usize, t: usize, replies: usize) {
        let cid = format!("c{n}");
        let version_id: i64 = conn
            .query_row(
                "SELECT v.id FROM versions v JOIN documents d ON d.id = v.document_id WHERE d.collection_id = ?1",
                [&cid],
                |r| r.get(0),
            )
            .unwrap();
        let tid = format!("t{n}-{t}");
        threads::create_thread(
            conn,
            &NewThread {
                thread_id: &tid,
                comment_id: &format!("m{n}-{t}-0"),
                collection_id: &cid,
                version_id,
                anchor: Anchor::lines(1, 2),
                author: threads::READER,
                visibility: Visibility::Now,
                body: "first",
                now: T0 + 1,
            },
        )
        .unwrap();
        for r in 0..replies {
            threads::add_comment(
                conn,
                &cid,
                &tid,
                &format!("m{n}-{t}-{}", r + 1),
                "agent",
                // Held replies are the reader's record too: the drill must count them.
                Visibility::Held,
                "reply",
                T0 + 2,
            )
            .unwrap();
        }
    }

    /// A database file: migrated, two collections, three threads, five comments.
    fn populated(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("margin.db");
        let mut conn = open(&path);
        db::migrate(&mut conn).unwrap();
        new_collection(&mut conn, 1);
        new_collection(&mut conn, 2);
        new_thread(&mut conn, 1, 1, 1);
        new_thread(&mut conn, 1, 2, 0);
        new_thread(&mut conn, 2, 1, 1);
        path
    }

    fn gz(bytes: &[u8]) -> Vec<u8> {
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(bytes).unwrap();
        enc.finish().unwrap()
    }

    fn drill_now(store: &MemStore, clock: &TestClock) -> Result<DrillReport, BackupError> {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(drill(store, clock))
    }

    fn snapshot_of(path: &Path, store: &MemStore, clock: &TestClock) -> Snapshot {
        snapshot(path, &std::env::temp_dir(), store, clock).unwrap()
    }

    #[test_id(tbe2ufu)]
    #[test]
    fn a_snapshot_taken_over_uncheckpointed_wal_writes_restores_with_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("margin.db");
        let mut writer = open(&path);
        db::migrate(&mut writer).unwrap();
        writer.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        new_collection(&mut writer, 1);
        new_collection(&mut writer, 2);
        // The writer stays open: its rows are in the -wal file only.
        assert!(
            std::fs::metadata(dir.path().join("margin.db-wal"))
                .unwrap()
                .len()
                > 0
        );

        let clock = TestClock::at(T0);
        let bucket = MemStore::new(&clock);
        let taken = snapshot_of(&path, &bucket, &clock);

        assert_eq!(bucket.keys(), std::slice::from_ref(&taken.key));
        assert_eq!(taken.key, "margin-20231114T221320Z.db.gz");
        let report = drill_now(&bucket, &clock).unwrap();
        assert_eq!((report.collections, report.documents), (2, 2));
        drop(writer);
    }

    #[test_id(tiqevlc)]
    #[test]
    fn a_missing_database_is_an_error_and_uploads_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let clock = TestClock::at(T0);
        let bucket = MemStore::new(&clock);
        let err = snapshot(&dir.path().join("absent.db"), dir.path(), &bucket, &clock);
        assert!(matches!(err, Err(BackupError::Snapshot(_))));
        assert!(bucket.keys().is_empty());
        assert!(!dir.path().join("absent.db").exists());
    }

    #[test_id(t19vbff)]
    #[test]
    fn two_runs_at_different_clock_times_make_two_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = populated(dir.path());
        let clock = TestClock::at(T0);
        let bucket = MemStore::new(&clock);
        snapshot_of(&path, &bucket, &clock);
        clock.set(T0 + 900);
        snapshot_of(&path, &bucket, &clock);
        assert_eq!(
            bucket.keys(),
            [
                "margin-20231114T221320Z.db.gz",
                "margin-20231114T222820Z.db.gz"
            ]
        );
    }

    #[test_id(twbxcfa)]
    #[test]
    fn a_failed_put_exits_with_an_error_records_it_and_leaves_no_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = populated(dir.path());
        let clock = TestClock::at(T0);
        let mut bucket = MemStore::new(&clock);
        bucket.fail_put = true;
        let sent = Mutex::new(Vec::new());
        let scratch = tempfile::tempdir().unwrap();
        let result = execute(&path, scratch.path(), &bucket, &clock, &|req| {
            sent.lock().unwrap().push(req.clone())
        });

        assert!(result.is_err());
        assert_eq!(std::fs::read_dir(scratch.path()).unwrap().count(), 0);
        let sent = sent.into_inner().unwrap();
        assert_eq!(sent.len(), 1);
        let names = metric_names(&sent[0]);
        assert!(names.contains(&metrics::BACKUP_RUNS.to_string()));
        assert!(!names.contains(&metrics::BACKUP_LAST_SUCCESS.to_string()));
        assert!(has_result(&sent[0], "error"));
    }

    fn metric_names(
        req: &opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest,
    ) -> Vec<String> {
        req.resource_metrics[0].scope_metrics[0]
            .metrics
            .iter()
            .map(|m| m.name.clone())
            .collect()
    }

    fn has_result(
        req: &opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest,
        result: &str,
    ) -> bool {
        use opentelemetry_proto::tonic::common::v1::any_value::Value;
        use opentelemetry_proto::tonic::metrics::v1::metric::Data;
        req.resource_metrics[0].scope_metrics[0]
            .metrics
            .iter()
            .filter(|m| m.name == metrics::BACKUP_RUNS)
            .filter_map(|m| match &m.data {
                Some(Data::Sum(s)) => Some(s),
                _ => None,
            })
            .flat_map(|s| &s.data_points)
            .flat_map(|p| &p.attributes)
            .any(|a| {
                a.key == "result"
                    && matches!(a.value.as_ref().and_then(|v| v.value.as_ref()), Some(Value::StringValue(v)) if v == result)
            })
    }

    #[test_id(tljpa05)]
    #[test]
    fn a_successful_run_records_ok_and_moves_last_success_to_the_finish_time() {
        let dir = tempfile::tempdir().unwrap();
        let path = populated(dir.path());
        let clock = TestClock::at(T0);
        let bucket = MemStore::new(&clock);
        let sent = Mutex::new(Vec::new());
        execute(&path, &std::env::temp_dir(), &bucket, &clock, &|req| {
            sent.lock().unwrap().push(req.clone())
        })
        .unwrap();
        let sent = sent.into_inner().unwrap();
        let names = metric_names(&sent[0]);
        for want in [
            metrics::BACKUP_RUNS,
            metrics::BACKUP_SECONDS,
            metrics::BACKUP_BYTES,
            metrics::BACKUP_LAST_SUCCESS,
        ] {
            assert!(
                names.contains(&want.to_string()),
                "{want} missing from {names:?}"
            );
        }
        assert!(has_result(&sent[0], "ok"));
    }

    #[test_id(tdse50r)]
    #[test]
    fn the_drill_reports_the_counts_of_a_two_collection_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = populated(dir.path());
        let clock = TestClock::at(T0);
        let bucket = MemStore::new(&clock);
        let taken = snapshot_of(&path, &bucket, &clock);
        clock.set(T0 + 120);
        let report = drill_now(&bucket, &clock).unwrap();
        assert_eq!(
            report,
            DrillReport {
                key: taken.key,
                age_secs: 120,
                collections: 2,
                documents: 2,
                threads: 3,
                comments: 5,
            }
        );
    }

    #[test_id(tlp9kuk)]
    #[test]
    fn the_drill_fails_on_an_empty_bucket() {
        let clock = TestClock::at(T0);
        let err = drill_now(&MemStore::new(&clock), &clock).unwrap_err();
        assert!(err.to_string().contains("no snapshot"), "{err}");
    }

    #[test_id(tomkq0d)]
    #[test]
    fn the_drill_fails_on_a_truncated_object() {
        let dir = tempfile::tempdir().unwrap();
        let path = populated(dir.path());
        let clock = TestClock::at(T0);
        let bucket = MemStore::new(&clock);
        let taken = snapshot_of(&path, &bucket, &clock);
        let whole = bucket.body(&taken.key);
        bucket.insert(&taken.key, T0, whole[..whole.len() / 2].to_vec());
        assert!(drill_now(&bucket, &clock).is_err());
    }

    #[test_id(tzcucrc)]
    #[test]
    fn the_drill_fails_on_a_file_that_fails_integrity_check() {
        let dir = tempfile::tempdir().unwrap();
        let path = populated(dir.path());
        let clock = TestClock::at(T0);
        let bucket = MemStore::new(&clock);
        let taken = snapshot_of(&path, &bucket, &clock);
        let mut plain = Vec::new();
        GzDecoder::new(&bucket.body(&taken.key)[..])
            .read_to_end(&mut plain)
            .unwrap();
        // Empty the index `comments_by_thread` while its table keeps its rows: the file still
        // opens and every table still reads, but integrity_check reports the index as short.
        let copy = dir.path().join("snapshot-copy.db");
        std::fs::write(&copy, &plain).unwrap();
        let conn = Connection::open(&copy).unwrap();
        let root: usize = conn
            .query_row(
                "SELECT rootpage FROM sqlite_master WHERE name = 'comments_by_thread'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let page_size: usize = conn
            .pragma_query_value(None, "page_size", |r| r.get(0))
            .unwrap();
        let at = (root - 1) * page_size;
        plain[at..at + page_size].fill(0);
        plain[at] = 0x0a; // an index leaf page,
        plain[at + 5..at + 7].copy_from_slice(&(page_size as u16).to_be_bytes()); // with no cells
        bucket.insert(&taken.key, T0, gz(&plain));
        let err = drill_now(&bucket, &clock).unwrap_err();
        assert!(matches!(err, BackupError::Drill(_)), "{err}");
    }

    #[test_id(tlv1i4r)]
    #[test]
    fn the_drill_refuses_a_user_version_newer_than_this_binary() {
        let dir = tempfile::tempdir().unwrap();
        let path = populated(dir.path());
        open(&path)
            .pragma_update(None, "user_version", db::schema_version() + 1)
            .unwrap();
        let clock = TestClock::at(T0);
        let bucket = MemStore::new(&clock);
        snapshot_of(&path, &bucket, &clock);
        let err = drill_now(&bucket, &clock).unwrap_err();
        assert!(err.to_string().contains("user_version"), "{err}");
    }

    #[test_id(tnxghlt)]
    #[test]
    fn the_drill_fails_on_a_snapshot_61_minutes_old_and_passes_at_60() {
        let dir = tempfile::tempdir().unwrap();
        let path = populated(dir.path());
        let clock = TestClock::at(T0);
        let bucket = MemStore::new(&clock);
        snapshot_of(&path, &bucket, &clock);
        clock.set(T0 + 60 * 60);
        assert!(drill_now(&bucket, &clock).is_ok());
        clock.set(T0 + 61 * 60);
        let err = drill_now(&bucket, &clock).unwrap_err();
        assert!(err.to_string().contains("old"), "{err}");
    }

    #[test_id(tdelyy6)]
    #[test]
    fn with_two_objects_the_drill_reads_the_newer() {
        let dir = tempfile::tempdir().unwrap();
        let path = populated(dir.path());
        let clock = TestClock::at(T0);
        let bucket = MemStore::new(&clock);
        snapshot_of(&path, &bucket, &clock); // the older: two collections
        let mut conn = open(&path);
        new_collection(&mut conn, 3);
        clock.set(T0 + 900);
        let newer = snapshot_of(&path, &bucket, &clock);
        let report = drill_now(&bucket, &clock).unwrap();
        assert_eq!((report.key, report.collections), (newer.key, 3));
    }

    #[test_id(tyjpeve)]
    #[test]
    fn a_missing_or_empty_bucket_variable_is_refused_by_name() {
        for value in [None, Some(String::new()), Some("  ".into())] {
            let err = bucket_from(value).unwrap_err();
            assert_eq!(err, BackupError::MissingBucket);
            assert!(err.to_string().contains("MARGIN_BACKUP_BUCKET"));
        }
        assert_eq!(
            bucket_from(Some("margin-backups-1".into())).unwrap(),
            "margin-backups-1"
        );
    }

    #[test_id(t7v1w3o)]
    #[test]
    fn stamps_and_aws_times_round_trip() {
        assert_eq!(stamp(T0), "20231114T221320Z");
        assert_eq!(parse_iso("2023-11-14T22:13:20+00:00"), Some(T0));
        assert_eq!(parse_iso("2023-11-14T22:13:20.000Z"), Some(T0));
        assert_eq!(parse_iso("2023-11-14T23:13:20+01:00"), Some(T0));
        assert_eq!(stamp(0), "19700101T000000Z");
        assert_eq!(stamp(951_782_400), "20000229T000000Z");
    }

    // law-periodic-tool-needs-a-tested-timer: the registration itself is under test.
    const DEPLOY_SH: &str = include_str!("../../../deploy/deploy.sh");
    const TIMER: &str = include_str!("../../../deploy/margin-backup.timer");
    const SERVICE: &str = include_str!("../../../deploy/margin-backup.service");

    #[test_id(txlc3bv)]
    #[test]
    fn deploy_installs_and_enables_the_backup_timer() {
        assert!(DEPLOY_SH.contains(r#"margin-backup.timer" "$unit_dir/margin-backup.timer""#));
        assert!(DEPLOY_SH.contains(r#"margin-backup.service" "$unit_dir/margin-backup.service""#));
        assert!(DEPLOY_SH.contains("systemctl --user enable --now margin-backup.timer"));
        assert!(DEPLOY_SH.contains("backups are NOT running"));
    }

    #[test_id(tlksd0h)]
    #[test]
    fn the_timer_fires_every_15_minutes_and_catches_up() {
        assert!(TIMER.contains("OnCalendar=*:0/15"));
        assert!(TIMER.contains("Persistent=true"));
    }

    #[test_id(tvh3rs9)]
    #[test]
    fn the_service_reads_the_backup_credentials_and_runs_the_installed_binary() {
        assert!(SERVICE.contains("EnvironmentFile=%h/.config/margin/backup.env"));
        assert!(SERVICE.contains("ExecStart=%h/.local/lib/margin/margin backup --config"));
        assert!(SERVICE.contains("Type=oneshot"));
        assert!(SERVICE.contains("CPUQuota=25%"));
    }
}
