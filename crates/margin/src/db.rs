//! Every SQLite call goes through [`Db`]: a bounded queue feeding a small fixed
//! set of threads that each own a connection, so request handlers only await.

use std::path::Path;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};

use rusqlite::Connection;

use crate::feed::Notifier;

/// Jobs that may wait for a connection; a full queue answers 503.
pub const QUEUE_CAPACITY: usize = 64;
/// Connections, one per thread.
pub const WORKERS: usize = 4;

const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/0001_collections.sql"),
    include_str!("../migrations/0002_threads.sql"),
    include_str!("../migrations/0003_changes.sql"),
    include_str!("../migrations/0004_versions.sql"),
    include_str!("../migrations/0005_source.sql"),
    include_str!("../migrations/0006_numbers.sql"),
];

/// The `user_version` of a database this binary has fully migrated.
pub fn schema_version() -> usize {
    MIGRATIONS.len()
}

type Job = Box<dyn FnOnce(&mut Connection) + Send>;

#[derive(Debug)]
pub enum DbError {
    QueueFull,
    Closed,
    Open(String),
}

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DbError::QueueFull => write!(f, "database queue is full"),
            DbError::Closed => write!(f, "database workers are gone"),
            DbError::Open(e) => write!(f, "opening database: {e}"),
        }
    }
}

impl std::error::Error for DbError {}

#[derive(Clone)]
pub struct Db {
    tx: SyncSender<Job>,
    notifier: Notifier,
}

fn connect(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(2))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(conn)
}

/// Apply every migration past `PRAGMA user_version`, in order, each in its own transaction.
pub fn migrate(conn: &mut Connection) -> rusqlite::Result<()> {
    let current: usize = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(current) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", i + 1)?;
        tx.commit()?;
    }
    Ok(())
}

impl Db {
    pub fn open(path: &Path) -> Result<Db, DbError> {
        Self::open_with(path, WORKERS, QUEUE_CAPACITY)
    }

    pub fn open_with(path: &Path, workers: usize, capacity: usize) -> Result<Db, DbError> {
        let mut first = connect(path).map_err(|e| DbError::Open(e.to_string()))?;
        migrate(&mut first).map_err(|e| DbError::Open(e.to_string()))?;
        let notifier = Notifier::new(
            crate::changes::newest(&first).map_err(|e| DbError::Open(e.to_string()))?,
        );
        let (tx, rx) = sync_channel::<Job>(capacity);
        let rx: Arc<Mutex<Receiver<Job>>> = Arc::new(Mutex::new(rx));
        let mut conns = vec![first];
        for _ in 1..workers {
            conns.push(connect(path).map_err(|e| DbError::Open(e.to_string()))?);
        }
        for (n, mut conn) in conns.into_iter().enumerate() {
            let rx = rx.clone();
            let notifier = notifier.clone();
            std::thread::Builder::new()
                .name(format!("margin-db-{n}"))
                .spawn(move || {
                    loop {
                        let job = match rx.lock() {
                            Ok(guard) => guard.recv(),
                            Err(_) => return,
                        };
                        match job {
                            Ok(job) => {
                                // A job that wrote has committed by the time it returns; tell
                                // the long-polls what the newest change is.
                                let before = conn.total_changes();
                                job(&mut conn);
                                if conn.total_changes() != before
                                    && let Ok(seq) = crate::changes::newest(&conn)
                                {
                                    notifier.publish(seq);
                                }
                            }
                            Err(_) => return,
                        }
                    }
                })
                .map_err(|e| DbError::Open(e.to_string()))?;
        }
        Ok(Db { tx, notifier })
    }

    /// Where the long-polls wait for the next change.
    pub fn notifier(&self) -> &Notifier {
        &self.notifier
    }

    /// Queue `f` for a connection thread and await its result. Never blocks the caller's thread.
    pub async fn call<T, F>(&self, f: F) -> Result<T, DbError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> T + Send + 'static,
    {
        let (done, wait) = tokio::sync::oneshot::channel();
        let job: Job = Box::new(move |conn| {
            let _ = done.send(f(conn));
        });
        self.tx.try_send(job).map_err(|e| match e {
            TrySendError::Full(_) => DbError::QueueFull,
            TrySendError::Disconnected(_) => DbError::Closed,
        })?;
        wait.await.map_err(|_| DbError::Closed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use margin_test_id::test_id;

    #[test_id(toe7ny9)]
    #[test]
    fn migrations_set_user_version_and_are_idempotent() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        migrate(&mut conn).unwrap();
        let v: usize = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, MIGRATIONS.len());
    }

    #[test_id(tv435h8)]
    #[test]
    fn a_full_queue_is_refused_not_waited_on() {
        let dir = tempfile::tempdir().unwrap();
        // One worker, one slot: park the worker, fill the slot, then the next call is refused.
        let db = Db::open_with(&dir.path().join("q.db"), 1, 1).unwrap();
        let (park_tx, park_rx) = std::sync::mpsc::channel::<()>();
        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        db.tx
            .try_send(Box::new(move |_| {
                started_tx.send(()).unwrap();
                park_rx.recv().unwrap();
            }))
            .unwrap();
        started_rx.recv().unwrap(); // the worker holds the first job; the queue is empty
        db.tx.try_send(Box::new(|_| {})).unwrap(); // fills the one slot
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let refused = rt.block_on(db.call(|_| 1));
        assert!(matches!(refused, Err(DbError::QueueFull)));
        park_tx.send(()).unwrap();
    }

    #[test_id(tevyg7f)]
    #[test]
    fn migration_four_keeps_the_change_log_and_admits_version_created() {
        let mut conn = Connection::open_in_memory().unwrap();
        for (i, sql) in MIGRATIONS.iter().take(3).enumerate() {
            conn.execute_batch(sql).unwrap();
            conn.pragma_update(None, "user_version", i + 1).unwrap();
        }
        conn.execute(
            "INSERT INTO changes (collection_id, kind, at) VALUES ('c', 'published', 5)",
            [],
        )
        .unwrap();
        migrate(&mut conn).unwrap();
        let kept: i64 = conn
            .query_row(
                "SELECT seq FROM changes WHERE kind = 'published'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept, 1);
        conn.execute(
            "INSERT INTO changes (collection_id, kind, at) VALUES ('c', 'version_created', 6)",
            [],
        )
        .unwrap();
    }

    #[test_id(ts5bx15)]
    #[test]
    fn a_version_four_database_keeps_its_collection_through_the_source_migration() {
        let mut conn = Connection::open_in_memory().unwrap();
        for (i, sql) in MIGRATIONS.iter().take(4).enumerate() {
            conn.execute_batch(sql).unwrap();
            conn.pragma_update(None, "user_version", i + 1).unwrap();
        }
        conn.execute(
            "INSERT INTO collections (id, title, cover_note, state, created_by, created_at, updated_at)
             VALUES ('c', 'kept', '', 'open', 'a', 1, 1)",
            [],
        )
        .unwrap();
        migrate(&mut conn).unwrap();
        let (title, source): (String, Option<String>) = conn
            .query_row(
                "SELECT title, source_repository FROM collections WHERE id = 'c'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((title.as_str(), source), ("kept", None));
        assert!(
            conn.execute("UPDATE collections SET source_repository = 'x'", [])
                .is_err(),
            "a repository without a commit is refused"
        );
    }

    #[test_id(tslffgt)]
    #[test]
    fn a_version_five_database_numbers_its_collections_by_age() {
        let mut conn = Connection::open_in_memory().unwrap();
        for (i, sql) in MIGRATIONS.iter().take(5).enumerate() {
            conn.execute_batch(sql).unwrap();
            conn.pragma_update(None, "user_version", i + 1).unwrap();
        }
        for (id, at) in [("newer", 20), ("older", 10)] {
            conn.execute(
                "INSERT INTO collections (id, title, cover_note, state, created_by, created_at, updated_at)
                 VALUES (?1, 't', '', 'open', 'a', ?2, ?2)",
                rusqlite::params![id, at],
            )
            .unwrap();
        }
        migrate(&mut conn).unwrap();
        let numbered: Vec<(i64, String)> = conn
            .prepare("SELECT number, collection_id FROM collection_numbers ORDER BY number")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            numbered,
            [(1, "older".to_string()), (2, "newer".to_string())]
        );
    }
}
