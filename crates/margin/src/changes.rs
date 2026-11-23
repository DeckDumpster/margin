//! The change log: one row per write, inserted in the write's own transaction, and the two
//! views of it: the agents' (everything) and the reader page's (nothing held back).

use rusqlite::{Connection, params};
use serde::Serialize;

use crate::feed::MAX_ROWS;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    ThreadCreated,
    CommentCreated,
    CommentUpdated,
    CommentDeleted,
    Published,
    CollectionUpdated,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::ThreadCreated => "thread_created",
            Kind::CommentCreated => "comment_created",
            Kind::CommentUpdated => "comment_updated",
            Kind::CommentDeleted => "comment_deleted",
            Kind::Published => "published",
            Kind::CollectionUpdated => "collection_updated",
        }
    }
}

/// What a write did, as it goes into the log.
pub struct Logged<'a> {
    pub collection_id: &'a str,
    pub kind: Kind,
    pub thread_id: Option<&'a str>,
    pub comment_id: Option<&'a str>,
    /// Author and visibility of the comment the write touched; `None` for writes to no comment.
    pub comment: Option<(&'a str, &'a str)>,
    pub at: i64,
}

pub fn record(conn: &Connection, c: &Logged) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO changes (collection_id, kind, thread_id, comment_id, author, visibility, at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            c.collection_id,
            c.kind.as_str(),
            c.thread_id,
            c.comment_id,
            c.comment.map(|(a, _)| a),
            c.comment.map(|(_, v)| v),
            c.at
        ],
    )?;
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct Row {
    pub seq: i64,
    pub kind: String,
    pub collection_id: String,
    pub thread_id: Option<String>,
    pub comment_id: Option<String>,
    pub author: Option<String>,
    pub visibility: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(skip)]
    pub at: i64,
}

pub struct Batch {
    pub rows: Vec<Row>,
    /// Pass it as `after` to read on from here.
    pub last_seq: i64,
}

/// Who is reading the log.
#[derive(Clone, Copy)]
pub enum View<'a> {
    /// An agent: every change of every collection.
    Agent,
    /// The reader page of one collection: its changes, less every comment held back.
    Page(&'a str),
}

/// Up to [`MAX_ROWS`] changes after `after`, oldest first, from one snapshot.
pub fn read(conn: &mut Connection, view: View, after: i64) -> rusqlite::Result<Batch> {
    let tx = conn.transaction()?;
    let (collection, hide_held) = match view {
        View::Agent => (None, false),
        View::Page(id) => (Some(id), true),
    };
    let rows = {
        let mut stmt = tx.prepare(
            "SELECT ch.seq, ch.kind, ch.collection_id, ch.thread_id, ch.comment_id,
                    ch.author, ch.visibility, c.body, ch.at
             FROM changes ch LEFT JOIN comments c ON c.id = ch.comment_id
             WHERE ch.seq > ?1
               AND (?2 IS NULL OR ch.collection_id = ?2)
               AND (?3 = 0 OR ch.visibility IS NULL OR ch.visibility = 'now')
             ORDER BY ch.seq LIMIT ?4",
        )?;
        stmt.query_map(params![after, collection, hide_held, MAX_ROWS as i64], |r| {
            Ok(Row {
                seq: r.get(0)?,
                kind: r.get(1)?,
                collection_id: r.get(2)?,
                thread_id: r.get(3)?,
                comment_id: r.get(4)?,
                author: r.get(5)?,
                visibility: r.get(6)?,
                body: r.get(7)?,
                at: r.get(8)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let last_seq = if rows.len() == MAX_ROWS {
        rows[MAX_ROWS - 1].seq
    } else {
        let newest: i64 = tx.query_row("SELECT COALESCE(MAX(seq), 0) FROM changes", [], |r| {
            r.get(0)
        })?;
        newest.max(after)
    };
    tx.commit()?;
    Ok(Batch { rows, last_seq })
}

/// The newest sequence number, for a page to start its feed from.
pub fn newest(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("SELECT COALESCE(MAX(seq), 0) FROM changes", [], |r| r.get(0))
}
