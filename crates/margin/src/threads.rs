//! Threads, their anchors and their comments, as plain functions over a connection.
//! These run on a [`crate::db::Db`] worker thread.

use margin_core::anchor::{self, Anchor, AnchorError};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::changes::{self, Kind, Logged};

/// The author value of every comment written from the reader page.
pub const READER: &str = margin_core::waits::READER;
/// Longest comment body accepted, in bytes.
pub const MAX_BODY: usize = 20_000;

/// Whether a comment is on the reader's page now or held back until he publishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    Now,
    Held,
}

impl Visibility {
    pub fn as_str(self) -> &'static str {
        match self {
            Visibility::Now => "now",
            Visibility::Held => "held",
        }
    }
    fn parse(s: &str) -> rusqlite::Result<Visibility> {
        match s {
            "now" => Ok(Visibility::Now),
            "held" => Ok(Visibility::Held),
            other => Err(rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                format!("unknown visibility {other:?} in the database").into(),
            )),
        }
    }
}

/// How a thread's anchor came to be where it is on its version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CarryState {
    Placed,
    /// Its lines are gone; it sits at the nearest surviving line.
    Removed,
    /// A highlight whose words changed; it fell back to its line.
    TextChanged,
}

impl CarryState {
    pub fn as_str(self) -> &'static str {
        match self {
            CarryState::Placed => "placed",
            CarryState::Removed => "removed",
            CarryState::TextChanged => "text_changed",
        }
    }
    pub fn parse(s: &str) -> rusqlite::Result<CarryState> {
        match s {
            "placed" => Ok(CarryState::Placed),
            "removed" => Ok(CarryState::Removed),
            "text_changed" => Ok(CarryState::TextChanged),
            other => Err(rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                format!("unknown carry state {other:?} in the database").into(),
            )),
        }
    }
}

/// Whose eyes a read is for. The reader never sees a held comment, nor a thread with no other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Reader,
    Agent,
}

impl Scope {
    /// The scope of whoever wrote as `author`.
    pub fn of(author: &str) -> Scope {
        if author == READER {
            Scope::Reader
        } else {
            Scope::Agent
        }
    }

    /// SQL over `t`, the threads table, true for the threads this scope can see.
    fn threads(self) -> &'static str {
        match self {
            Scope::Agent => "1",
            Scope::Reader => {
                "EXISTS (SELECT 1 FROM comments vc WHERE vc.thread_id = t.id AND vc.visibility = 'now')"
            }
        }
    }

    fn comments(self) -> &'static str {
        match self {
            Scope::Agent => "1",
            Scope::Reader => "visibility = 'now'",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Comment {
    pub id: String,
    pub author: String,
    pub body: String,
    pub visibility: Visibility,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Thread {
    pub id: String,
    pub version_id: i64,
    pub first_line: u32,
    pub last_line: u32,
    pub start_col: Option<u32>,
    pub end_col: Option<u32>,
    pub carry_state: CarryState,
    pub created_at: i64,
    pub comments: Vec<Comment>,
}

impl Thread {
    pub fn anchor(&self) -> Anchor {
        Anchor {
            first: self.first_line,
            last: self.last_line,
            cols: self.start_col.zip(self.end_col),
        }
    }
}

/// One margin mark's worth of a thread: which lines it covers and its lane.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Mark {
    pub thread: String,
    pub first: u32,
    pub last: u32,
    pub lane: u16,
    /// The highlighted columns when the thread is a highlight (see [`Anchor::cols`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_col: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_col: Option<u32>,
}

#[derive(Debug)]
pub enum CreateError {
    /// No such version in this collection.
    NoVersion,
    Anchor(AnchorError),
    Db(rusqlite::Error),
}

impl From<rusqlite::Error> for CreateError {
    fn from(e: rusqlite::Error) -> Self {
        CreateError::Db(e)
    }
}

pub struct NewThread<'a> {
    pub thread_id: &'a str,
    pub comment_id: &'a str,
    pub collection_id: &'a str,
    pub version_id: i64,
    pub anchor: Anchor,
    pub author: &'a str,
    pub visibility: Visibility,
    pub body: &'a str,
    pub now: i64,
}

/// The latest version of the document at `path` in the collection.
pub fn latest_version(
    conn: &Connection,
    collection_id: &str,
    path: &str,
) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT v.id FROM versions v JOIN documents d ON d.id = v.document_id
         WHERE d.collection_id = ?1 AND d.path = ?2 ORDER BY v.number DESC LIMIT 1",
        params![collection_id, path],
        |r| r.get(0),
    )
    .optional()
}

/// The collection a thread belongs to.
pub fn collection_of(conn: &Connection, thread_id: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT collection_id FROM threads WHERE id = ?1",
        [thread_id],
        |r| r.get(0),
    )
    .optional()
}

pub fn create_thread(conn: &mut Connection, t: &NewThread) -> Result<Thread, CreateError> {
    let text: Option<String> = conn
        .query_row(
            "SELECT v.text FROM versions v JOIN documents d ON d.id = v.document_id
             WHERE v.id = ?1 AND d.collection_id = ?2",
            params![t.version_id, t.collection_id],
            |r| r.get(0),
        )
        .optional()?;
    let text = text.ok_or(CreateError::NoVersion)?;
    let lens: Vec<u32> = margin_core::lines::split(&text)
        .iter()
        .map(|l| l.chars().count() as u32)
        .collect();
    t.anchor.validate(&lens).map_err(CreateError::Anchor)?;
    let tx = conn.transaction()?;
    let (start, end) = match t.anchor.cols {
        Some((s, e)) => (Some(s), Some(e)),
        None => (None, None),
    };
    tx.execute(
        "INSERT INTO anchors (version_id, first_line, last_line, start_col, end_col)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![t.version_id, t.anchor.first, t.anchor.last, start, end],
    )?;
    let anchor_id = tx.last_insert_rowid();
    tx.execute(
        "INSERT INTO threads (id, collection_id, anchor_id, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![t.thread_id, t.collection_id, anchor_id, t.now],
    )?;
    tx.execute(
        "INSERT INTO comments (id, thread_id, author, body, visibility, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
        params![
            t.comment_id,
            t.thread_id,
            t.author,
            t.body,
            t.visibility.as_str(),
            t.now
        ],
    )?;
    changes::record(
        &tx,
        &Logged {
            collection_id: t.collection_id,
            kind: Kind::ThreadCreated,
            thread_id: Some(t.thread_id),
            comment_id: Some(t.comment_id),
            comment: Some((t.author, t.visibility.as_str())),
            at: t.now,
        },
    )?;
    tx.commit()?;
    Ok(
        get(conn, Scope::of(t.author), t.collection_id, t.thread_id)?
            .expect("the thread was just inserted"),
    )
}

const THREAD_COLUMNS: &str = "t.id, a.version_id, a.first_line, a.last_line, a.start_col, a.end_col, t.created_at, a.carry_state";

fn thread_row(r: &rusqlite::Row) -> rusqlite::Result<Thread> {
    Ok(Thread {
        id: r.get(0)?,
        version_id: r.get(1)?,
        first_line: r.get(2)?,
        last_line: r.get(3)?,
        start_col: r.get(4)?,
        end_col: r.get(5)?,
        created_at: r.get(6)?,
        carry_state: CarryState::parse(&r.get::<_, String>(7)?)?,
        comments: Vec::new(),
    })
}

fn comment_row(r: &rusqlite::Row) -> rusqlite::Result<(String, Comment)> {
    Ok((
        r.get(0)?,
        Comment {
            id: r.get(1)?,
            author: r.get(2)?,
            body: r.get(3)?,
            visibility: Visibility::parse(&r.get::<_, String>(4)?)?,
            created_at: r.get(5)?,
            updated_at: r.get(6)?,
        },
    ))
}

/// Attach each thread's comments, oldest first, with one query for all of them.
fn fill_comments(conn: &Connection, scope: Scope, threads: &mut [Thread]) -> rusqlite::Result<()> {
    if threads.is_empty() {
        return Ok(());
    }
    let marks = vec!["?"; threads.len()].join(",");
    let mut stmt = conn.prepare(&format!(
        "SELECT thread_id, id, author, body, visibility, created_at, updated_at FROM comments
         WHERE thread_id IN ({marks}) AND {} ORDER BY created_at, id",
        scope.comments()
    ))?;
    let ids: Vec<&str> = threads.iter().map(|t| t.id.as_str()).collect();
    let rows = stmt
        .query_map(rusqlite::params_from_iter(ids), comment_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (tid, c) in rows {
        if let Some(t) = threads.iter_mut().find(|t| t.id == tid) {
            t.comments.push(c);
        }
    }
    Ok(())
}

pub fn get(
    conn: &Connection,
    scope: Scope,
    collection_id: &str,
    thread_id: &str,
) -> rusqlite::Result<Option<Thread>> {
    let found = conn
        .query_row(
            &format!(
                "SELECT {THREAD_COLUMNS} FROM threads t JOIN anchors a ON a.id = t.anchor_id
                 WHERE t.id = ?1 AND t.collection_id = ?2 AND {}",
                scope.threads()
            ),
            params![thread_id, collection_id],
            thread_row,
        )
        .optional()?;
    let Some(t) = found else { return Ok(None) };
    let mut one = [t];
    fill_comments(conn, scope, &mut one)?;
    let [t] = one;
    Ok(Some(t))
}

/// Keyset page, oldest first. `after` is the (created_at, id) of the last row already seen.
pub fn page(
    conn: &Connection,
    scope: Scope,
    collection_id: &str,
    limit: usize,
    after: Option<(i64, &str)>,
) -> rusqlite::Result<Vec<Thread>> {
    let (after_at, after_id) = after.unwrap_or((i64::MIN, ""));
    let mut stmt = conn.prepare(&format!(
        "SELECT {THREAD_COLUMNS} FROM threads t JOIN anchors a ON a.id = t.anchor_id
         WHERE t.collection_id = ?1 AND (t.created_at, t.id) > (?2, ?3) AND {}
         ORDER BY t.created_at, t.id LIMIT ?4",
        scope.threads()
    ))?;
    let mut threads = stmt
        .query_map(
            params![collection_id, after_at, after_id, limit as i64],
            thread_row,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    fill_comments(conn, scope, &mut threads)?;
    Ok(threads)
}

/// Every thread of a collection, oldest first.
pub fn all(conn: &Connection, scope: Scope, collection_id: &str) -> rusqlite::Result<Vec<Thread>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {THREAD_COLUMNS} FROM threads t JOIN anchors a ON a.id = t.anchor_id
         WHERE t.collection_id = ?1 AND {} ORDER BY t.created_at, t.id",
        scope.threads()
    ))?;
    let mut threads = stmt
        .query_map([collection_id], thread_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    fill_comments(conn, scope, &mut threads)?;
    Ok(threads)
}

/// The marks of the threads on one version, each in its lane.
pub fn marks(threads: &[&Thread]) -> Vec<Mark> {
    let anchors: Vec<Anchor> = threads.iter().map(|t| t.anchor()).collect();
    let lanes = anchor::lanes(&anchors);
    let mut marks: Vec<Mark> = threads
        .iter()
        .zip(lanes)
        .map(|(t, lane)| Mark {
            thread: t.id.clone(),
            first: t.first_line,
            last: t.last_line,
            lane,
            start_col: t.start_col,
            end_col: t.end_col,
        })
        .collect();
    // Thread reads order by second and random id; the DOM and the header order by lane.
    marks.sort_by_key(|m| (m.lane, m.first));
    marks
}

/// The marks of every thread the reader can see on `version_id`, for the script to redraw after
/// a write.
pub fn version_marks(
    conn: &Connection,
    collection_id: &str,
    version_id: i64,
) -> rusqlite::Result<Vec<Mark>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {THREAD_COLUMNS} FROM threads t JOIN anchors a ON a.id = t.anchor_id
         WHERE t.collection_id = ?1 AND a.version_id = ?2 AND {}
         ORDER BY t.created_at, t.id",
        Scope::Reader.threads()
    ))?;
    let on = stmt
        .query_map(params![collection_id, version_id], thread_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(marks(&on.iter().collect::<Vec<_>>()))
}

/// The marks the reader sees on each version of each document of the collection.
pub fn collection_marks(
    conn: &Connection,
    collection_id: &str,
) -> rusqlite::Result<Vec<(i64, Vec<Mark>)>> {
    let mut stmt = conn.prepare(
        "SELECT v.id FROM versions v JOIN documents d ON d.id = v.document_id
         WHERE d.collection_id = ?1 ORDER BY d.position, v.number",
    )?;
    let versions = stmt
        .query_map([collection_id], |r| r.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    versions
        .into_iter()
        .map(|v| Ok((v, version_marks(conn, collection_id, v)?)))
        .collect()
}

/// Add a comment to a thread of this collection; `None` when `author` can see no such thread.
#[allow(clippy::too_many_arguments)]
pub fn add_comment(
    conn: &mut Connection,
    collection_id: &str,
    thread_id: &str,
    comment_id: &str,
    author: &str,
    visibility: Visibility,
    body: &str,
    now: i64,
) -> rusqlite::Result<Option<Thread>> {
    let scope = Scope::of(author);
    if get(conn, scope, collection_id, thread_id)?.is_none() {
        return Ok(None);
    }
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO comments (id, thread_id, author, body, visibility, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
        params![
            comment_id,
            thread_id,
            author,
            body,
            visibility.as_str(),
            now
        ],
    )?;
    changes::record(
        &tx,
        &Logged {
            collection_id,
            kind: Kind::CommentCreated,
            thread_id: Some(thread_id),
            comment_id: Some(comment_id),
            comment: Some((author, visibility.as_str())),
            at: now,
        },
    )?;
    tx.commit()?;
    get(conn, scope, collection_id, thread_id)
}

pub enum Change<T> {
    NotFound,
    /// The comment exists and is not the caller's.
    NotYours,
    Done(T),
}

struct Owned {
    collection_id: String,
    thread_id: String,
    author: String,
    visibility: Visibility,
}

/// The comment, within `collection_id` when one is given.
fn owner(
    conn: &Connection,
    collection_id: Option<&str>,
    comment_id: &str,
) -> rusqlite::Result<Option<Owned>> {
    conn.query_row(
        "SELECT t.collection_id, c.thread_id, c.author, c.visibility
         FROM comments c JOIN threads t ON t.id = c.thread_id
         WHERE c.id = ?1 AND (?2 IS NULL OR t.collection_id = ?2)",
        params![comment_id, collection_id],
        |r| {
            Ok(Owned {
                collection_id: r.get(0)?,
                thread_id: r.get(1)?,
                author: r.get(2)?,
                visibility: Visibility::parse(&r.get::<_, String>(3)?)?,
            })
        },
    )
    .optional()
}

/// The comment as `who` may touch it: absent to the reader when held, refused unless `who`
/// wrote it.
fn own(
    conn: &Connection,
    collection_id: Option<&str>,
    who: &str,
    comment_id: &str,
) -> rusqlite::Result<Change<Owned>> {
    let Some(o) = owner(conn, collection_id, comment_id)? else {
        return Ok(Change::NotFound);
    };
    if who == READER && o.visibility == Visibility::Held {
        return Ok(Change::NotFound);
    }
    if o.author != who {
        return Ok(Change::NotYours);
    }
    Ok(Change::Done(o))
}

pub fn edit_comment(
    conn: &mut Connection,
    collection_id: Option<&str>,
    who: &str,
    comment_id: &str,
    body: &str,
    now: i64,
) -> rusqlite::Result<Change<Thread>> {
    let o = match own(conn, collection_id, who, comment_id)? {
        Change::Done(o) => o,
        Change::NotFound => return Ok(Change::NotFound),
        Change::NotYours => return Ok(Change::NotYours),
    };
    let tx = conn.transaction()?;
    tx.execute(
        "UPDATE comments SET body = ?2, updated_at = ?3 WHERE id = ?1",
        params![comment_id, body, now],
    )?;
    changes::record(
        &tx,
        &Logged {
            collection_id: &o.collection_id,
            kind: Kind::CommentUpdated,
            thread_id: Some(&o.thread_id),
            comment_id: Some(comment_id),
            comment: Some((&o.author, o.visibility.as_str())),
            at: now,
        },
    )?;
    tx.commit()?;
    Ok(Change::Done(
        get(conn, Scope::of(who), &o.collection_id, &o.thread_id)?
            .expect("the comment's thread exists"),
    ))
}

pub enum Deleted {
    /// Comments `who` can see remain: the thread as it is now.
    Left(Thread),
    /// No such comment remains: the reader's marks on the version now.
    Gone(Vec<Mark>),
}

/// Delete `who`'s own comment. A thread left with no comments goes with its anchor.
pub fn delete_comment(
    conn: &mut Connection,
    collection_id: Option<&str>,
    who: &str,
    comment_id: &str,
    now: i64,
) -> rusqlite::Result<Change<Deleted>> {
    let o = match own(conn, collection_id, who, comment_id)? {
        Change::Done(o) => o,
        Change::NotFound => return Ok(Change::NotFound),
        Change::NotYours => return Ok(Change::NotYours),
    };
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM comments WHERE id = ?1", [comment_id])?;
    changes::record(
        &tx,
        &Logged {
            collection_id: &o.collection_id,
            kind: Kind::CommentDeleted,
            thread_id: Some(&o.thread_id),
            comment_id: Some(comment_id),
            comment: Some((&o.author, o.visibility.as_str())),
            at: now,
        },
    )?;
    let left: i64 = tx.query_row(
        "SELECT COUNT(*) FROM comments WHERE thread_id = ?1",
        [&o.thread_id],
        |r| r.get(0),
    )?;
    let (anchor_id, version_id): (i64, i64) = tx.query_row(
        "SELECT t.anchor_id, a.version_id FROM threads t JOIN anchors a ON a.id = t.anchor_id
         WHERE t.id = ?1",
        [&o.thread_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if left == 0 {
        tx.execute("DELETE FROM threads WHERE id = ?1", [&o.thread_id])?;
        tx.execute("DELETE FROM anchors WHERE id = ?1", [anchor_id])?;
    }
    let thread = get(&tx, Scope::of(who), &o.collection_id, &o.thread_id)?;
    let done = match thread {
        Some(t) => Deleted::Left(t),
        None => Deleted::Gone(version_marks(&tx, &o.collection_id, version_id)?),
    };
    tx.commit()?;
    Ok(Change::Done(done))
}

pub enum Published {
    NotFound,
    /// A closed collection takes no more.
    Closed,
    /// How many held comments became visible.
    Done(usize),
}

/// Release every held comment of the collection, move `open` to `published`, and log it once.
pub fn publish(
    conn: &mut Connection,
    collection_id: &str,
    now: i64,
) -> rusqlite::Result<Published> {
    let tx = conn.transaction()?;
    let state: Option<String> = tx
        .query_row(
            "SELECT state FROM collections WHERE id = ?1",
            [collection_id],
            |r| r.get(0),
        )
        .optional()?;
    match state.as_deref() {
        None => return Ok(Published::NotFound),
        Some("closed") => return Ok(Published::Closed),
        Some(_) => {}
    }
    let released = tx.execute(
        "UPDATE comments SET visibility = 'now'
         WHERE visibility = 'held'
           AND thread_id IN (SELECT id FROM threads WHERE collection_id = ?1)",
        [collection_id],
    )?;
    tx.execute(
        "UPDATE collections SET state = 'published', updated_at = ?2 WHERE id = ?1",
        params![collection_id, now],
    )?;
    changes::record(
        &tx,
        &Logged {
            collection_id,
            kind: Kind::Published,
            thread_id: None,
            comment_id: None,
            comment: None,
            at: now,
        },
    )?;
    tx.commit()?;
    Ok(Published::Done(released))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::highlight::Highlighter;
    use crate::store::{self, Kind, NewCollection, NewDocument};
    use margin_test_id::test_id;

    fn lanes_on_line(html: &str, n: usize) -> Vec<u32> {
        let row = html.split(&format!("data-line=\"{n}\"")).nth(1).unwrap();
        let gut = row.split("<span class=\"gut\">").nth(1).unwrap();
        let gut = gut.split("<span class=\"code\">").next().unwrap();
        gut.split("--lane:")
            .skip(1)
            .map(|c| c[..c.find('"').unwrap()].parse().unwrap())
            .collect()
    }

    #[test_id(tps7j7x)]
    #[test]
    fn marks_on_a_line_come_in_lane_order_whatever_the_thread_ids() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::db::migrate(&mut conn).unwrap();
        let text = (1..=30).map(|n| format!("line {n}\n")).collect::<String>();
        store::create(
            &mut conn,
            &NewCollection {
                id: "col",
                title: "t",
                cover_note: "",
                created_by: "x",
                now: 100,
                documents: &[(
                    "doc".to_string(),
                    NewDocument {
                        path: "a.md".into(),
                        kind: Kind::Prose,
                        text,
                    },
                )],
            },
        )
        .unwrap();
        let version_id = store::get(&conn, "col").unwrap().unwrap().documents[0].version_id;
        for (id, first, last) in [("c", 10, 20), ("a", 15, 25), ("b", 17, 17)] {
            create_thread(
                &mut conn,
                &NewThread {
                    thread_id: id,
                    comment_id: &format!("{id}-comment"),
                    collection_id: "col",
                    version_id,
                    anchor: Anchor::lines(first, last),
                    author: READER,
                    visibility: Visibility::Now,
                    body: "hello",
                    now: 200,
                },
            )
            .unwrap();
        }
        let collection = store::get(&conn, "col").unwrap().unwrap();
        let page = crate::page::render(
            &collection,
            &all(&conn, Scope::Reader, "col").unwrap(),
            0,
            &Highlighter::new(),
        )
        .unwrap();
        let on_page = lanes_on_line(&page, 17);
        let in_header: Vec<u32> = version_marks(&conn, "col", version_id)
            .unwrap()
            .iter()
            .filter(|m| m.first <= 17 && 17 <= m.last)
            .map(|m| u32::from(m.lane))
            .collect();
        assert_eq!((on_page, in_header), (vec![0, 1, 2], vec![0, 1, 2]));
    }
}
