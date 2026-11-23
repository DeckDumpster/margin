//! Threads, their anchors and their comments, as plain functions over a connection.
//! These run on a [`crate::db::Db`] worker thread.

use margin_core::anchor::{self, Anchor, AnchorError};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

/// The author value of every comment written from the reader page.
pub const READER: &str = "reader";
/// Longest comment body accepted, in bytes.
pub const MAX_BODY: usize = 20_000;

#[derive(Debug, Clone, Serialize)]
pub struct Comment {
    pub id: String,
    pub author: String,
    pub body: String,
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
    pub body: &'a str,
    pub now: i64,
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
        "INSERT INTO comments (id, thread_id, author, body, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
        params![t.comment_id, t.thread_id, READER, t.body, t.now],
    )?;
    tx.commit()?;
    Ok(get(conn, t.collection_id, t.thread_id)?.expect("the thread was just inserted"))
}

const THREAD_COLUMNS: &str =
    "t.id, a.version_id, a.first_line, a.last_line, a.start_col, a.end_col, t.created_at";

fn thread_row(r: &rusqlite::Row) -> rusqlite::Result<Thread> {
    Ok(Thread {
        id: r.get(0)?,
        version_id: r.get(1)?,
        first_line: r.get(2)?,
        last_line: r.get(3)?,
        start_col: r.get(4)?,
        end_col: r.get(5)?,
        created_at: r.get(6)?,
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
            created_at: r.get(4)?,
            updated_at: r.get(5)?,
        },
    ))
}

/// Attach each thread's comments, oldest first, with one query for all of them.
fn fill_comments(conn: &Connection, threads: &mut [Thread]) -> rusqlite::Result<()> {
    if threads.is_empty() {
        return Ok(());
    }
    let marks = vec!["?"; threads.len()].join(",");
    let mut stmt = conn.prepare(&format!(
        "SELECT thread_id, id, author, body, created_at, updated_at FROM comments
         WHERE thread_id IN ({marks}) ORDER BY created_at, id"
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
    collection_id: &str,
    thread_id: &str,
) -> rusqlite::Result<Option<Thread>> {
    let found = conn
        .query_row(
            &format!(
                "SELECT {THREAD_COLUMNS} FROM threads t JOIN anchors a ON a.id = t.anchor_id
                 WHERE t.id = ?1 AND t.collection_id = ?2"
            ),
            params![thread_id, collection_id],
            thread_row,
        )
        .optional()?;
    let Some(t) = found else { return Ok(None) };
    let mut one = [t];
    fill_comments(conn, &mut one)?;
    let [t] = one;
    Ok(Some(t))
}

/// Keyset page, oldest first. `after` is the (created_at, id) of the last row already seen.
pub fn page(
    conn: &Connection,
    collection_id: &str,
    limit: usize,
    after: Option<(i64, &str)>,
) -> rusqlite::Result<Vec<Thread>> {
    let (after_at, after_id) = after.unwrap_or((i64::MIN, ""));
    let mut stmt = conn.prepare(&format!(
        "SELECT {THREAD_COLUMNS} FROM threads t JOIN anchors a ON a.id = t.anchor_id
         WHERE t.collection_id = ?1 AND (t.created_at, t.id) > (?2, ?3)
         ORDER BY t.created_at, t.id LIMIT ?4"
    ))?;
    let mut threads = stmt
        .query_map(
            params![collection_id, after_at, after_id, limit as i64],
            thread_row,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    fill_comments(conn, &mut threads)?;
    Ok(threads)
}

/// Every thread of a collection, oldest first.
pub fn all(conn: &Connection, collection_id: &str) -> rusqlite::Result<Vec<Thread>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {THREAD_COLUMNS} FROM threads t JOIN anchors a ON a.id = t.anchor_id
         WHERE t.collection_id = ?1 ORDER BY t.created_at, t.id"
    ))?;
    let mut threads = stmt
        .query_map([collection_id], thread_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    fill_comments(conn, &mut threads)?;
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
        })
        .collect();
    // Thread reads order by second and random id; the DOM and the header order by lane.
    marks.sort_by_key(|m| (m.lane, m.first));
    marks
}

/// The marks of every thread on `version_id`, for the script to redraw after a write.
pub fn version_marks(
    conn: &Connection,
    collection_id: &str,
    version_id: i64,
) -> rusqlite::Result<Vec<Mark>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {THREAD_COLUMNS} FROM threads t JOIN anchors a ON a.id = t.anchor_id
         WHERE t.collection_id = ?1 AND a.version_id = ?2 ORDER BY t.created_at, t.id"
    ))?;
    let on = stmt
        .query_map(params![collection_id, version_id], thread_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(marks(&on.iter().collect::<Vec<_>>()))
}

/// Add a comment to a thread of this collection; `None` when there is no such thread.
pub fn add_comment(
    conn: &Connection,
    collection_id: &str,
    thread_id: &str,
    comment_id: &str,
    author: &str,
    body: &str,
    now: i64,
) -> rusqlite::Result<Option<Thread>> {
    if get(conn, collection_id, thread_id)?.is_none() {
        return Ok(None);
    }
    conn.execute(
        "INSERT INTO comments (id, thread_id, author, body, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
        params![comment_id, thread_id, author, body, now],
    )?;
    get(conn, collection_id, thread_id)
}

pub enum Change<T> {
    NotFound,
    /// The comment exists and is not the reader's.
    NotYours,
    Done(T),
}

/// (thread id, author) of a comment of this collection.
fn owner(
    conn: &Connection,
    collection_id: &str,
    comment_id: &str,
) -> rusqlite::Result<Option<(String, String)>> {
    conn.query_row(
        "SELECT c.thread_id, c.author FROM comments c JOIN threads t ON t.id = c.thread_id
         WHERE c.id = ?1 AND t.collection_id = ?2",
        params![comment_id, collection_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
}

pub fn edit_comment(
    conn: &Connection,
    collection_id: &str,
    comment_id: &str,
    body: &str,
    now: i64,
) -> rusqlite::Result<Change<Thread>> {
    let Some((thread_id, author)) = owner(conn, collection_id, comment_id)? else {
        return Ok(Change::NotFound);
    };
    if author != READER {
        return Ok(Change::NotYours);
    }
    conn.execute(
        "UPDATE comments SET body = ?2, updated_at = ?3 WHERE id = ?1",
        params![comment_id, body, now],
    )?;
    Ok(Change::Done(
        get(conn, collection_id, &thread_id)?.expect("the comment's thread exists"),
    ))
}

pub enum Deleted {
    /// Comments remain: the thread as it is now.
    Left(Thread),
    /// The thread had no other comment and went with its anchor: the version's marks now.
    Gone(Vec<Mark>),
}

/// Delete the reader's comment. A thread left with no comments goes with its anchor.
pub fn delete_comment(
    conn: &mut Connection,
    collection_id: &str,
    comment_id: &str,
) -> rusqlite::Result<Change<Deleted>> {
    let Some((thread_id, author)) = owner(conn, collection_id, comment_id)? else {
        return Ok(Change::NotFound);
    };
    if author != READER {
        return Ok(Change::NotYours);
    }
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM comments WHERE id = ?1", [comment_id])?;
    let left: i64 = tx.query_row(
        "SELECT COUNT(*) FROM comments WHERE thread_id = ?1",
        [&thread_id],
        |r| r.get(0),
    )?;
    if left > 0 {
        let thread = get(&tx, collection_id, &thread_id)?.expect("the thread still has comments");
        tx.commit()?;
        return Ok(Change::Done(Deleted::Left(thread)));
    }
    let (anchor_id, version_id): (i64, i64) = tx.query_row(
        "SELECT t.anchor_id, a.version_id FROM threads t JOIN anchors a ON a.id = t.anchor_id
         WHERE t.id = ?1",
        [&thread_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    tx.execute("DELETE FROM threads WHERE id = ?1", [&thread_id])?;
    tx.execute("DELETE FROM anchors WHERE id = ?1", [anchor_id])?;
    let marks = version_marks(&tx, collection_id, version_id)?;
    tx.commit()?;
    Ok(Change::Done(Deleted::Gone(marks)))
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
                    body: "hello",
                    now: 200,
                },
            )
            .unwrap();
        }
        let collection = store::get(&conn, "col").unwrap().unwrap();
        let page = crate::page::render(
            &collection,
            &all(&conn, "col").unwrap(),
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
