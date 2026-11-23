//! New versions of a document: the text, and every thread carried onto it by matching its
//! lines' text (see `margin_core::carry`). Plain functions over a connection, run on a
//! [`crate::db::Db`] worker thread.

use margin_core::anchor::Anchor;
use margin_core::carry::{self, Carried};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::changes::{self, Kind, Logged};
use crate::threads::CarryState;

#[derive(Debug, Clone, Serialize)]
pub struct Version {
    pub id: i64,
    pub number: i64,
    pub created_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

pub struct Added {
    pub version: Version,
    pub collection_id: String,
}

struct Carrying {
    thread: String,
    anchor_id: i64,
    anchor: Anchor,
    state: CarryState,
}

/// Where a thread lands: the carried anchor and how it got there. A thread already shown as
/// removed or text-changed stays so: its stored anchor is only a place to show it.
fn place(c: &Carrying, map: &[Option<u32>], old: &[&str], new: &[&str]) -> (Anchor, CarryState) {
    let (anchor, state) = match carry::carry(&c.anchor, map, old, new) {
        Carried::Placed(a) => (a, CarryState::Placed),
        Carried::TextChanged(a) => (a, CarryState::TextChanged),
        Carried::Removed { at } => (Anchor::lines(at, at), CarryState::Removed),
    };
    let state = match (c.state, state) {
        (CarryState::Placed, s) => s,
        (kept, CarryState::Placed) => kept,
        (_, s) => s,
    };
    (anchor, state)
}

/// Insert the next version of `document_id`; `None` when there is no such document.
pub fn add(
    conn: &mut Connection,
    document_id: &str,
    text: &str,
    now: i64,
) -> rusqlite::Result<Option<Added>> {
    let tx = conn.transaction()?;
    let head = tx
        .query_row(
            "SELECT d.collection_id, v.id, v.number, v.text
             FROM documents d JOIN versions v ON v.document_id = d.id
             WHERE d.id = ?1 ORDER BY v.number DESC LIMIT 1",
            [document_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?;
    let Some((collection_id, old_id, old_number, old_text)) = head else {
        return Ok(None);
    };
    let number = old_number + 1;
    tx.execute(
        "INSERT INTO versions (document_id, number, text, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![document_id, number, text, now],
    )?;
    let version_id = tx.last_insert_rowid();

    let (old, new) = (
        margin_core::lines::split(&old_text),
        margin_core::lines::split(text),
    );
    let map = carry::line_map(&old, &new);
    let carrying = {
        let mut stmt = tx.prepare(
            "SELECT t.id, a.id, a.first_line, a.last_line, a.start_col, a.end_col, a.carry_state
             FROM threads t JOIN anchors a ON a.id = t.anchor_id
             WHERE a.version_id = ?1 ORDER BY t.created_at, t.id",
        )?;
        stmt.query_map([old_id], |r| {
            Ok(Carrying {
                thread: r.get(0)?,
                anchor_id: r.get(1)?,
                anchor: Anchor {
                    first: r.get(2)?,
                    last: r.get(3)?,
                    cols: r.get::<_, Option<u32>>(4)?.zip(r.get::<_, Option<u32>>(5)?),
                },
                state: CarryState::parse(&r.get::<_, String>(6)?)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for c in &carrying {
        let (anchor, state) = place(c, &map, &old, &new);
        let (start, end) = anchor.cols.unzip();
        tx.execute(
            "INSERT INTO anchors (version_id, first_line, last_line, start_col, end_col, carry_state, carried_from)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![version_id, anchor.first, anchor.last, start, end, state.as_str(), c.anchor_id],
        )?;
        tx.execute(
            "UPDATE threads SET anchor_id = ?1 WHERE id = ?2",
            params![tx.last_insert_rowid(), c.thread],
        )?;
    }
    changes::record(
        &tx,
        &Logged {
            collection_id: &collection_id,
            kind: Kind::VersionCreated,
            thread_id: None,
            comment_id: None,
            comment: None,
            at: now,
        },
    )?;
    tx.commit()?;
    Ok(Some(Added {
        version: Version {
            id: version_id,
            number,
            created_at: now,
            text: None,
        },
        collection_id,
    }))
}

/// Versions of a document by number, oldest first, those after number `after`.
pub fn list(
    conn: &Connection,
    document_id: &str,
    limit: usize,
    after: i64,
) -> rusqlite::Result<Option<Vec<Version>>> {
    if !exists(conn, document_id)? {
        return Ok(None);
    }
    let mut stmt = conn.prepare(
        "SELECT id, number, created_at FROM versions
         WHERE document_id = ?1 AND number > ?2 ORDER BY number LIMIT ?3",
    )?;
    let rows = stmt
        .query_map(params![document_id, after, limit as i64], |r| {
            Ok(Version {
                id: r.get(0)?,
                number: r.get(1)?,
                created_at: r.get(2)?,
                text: None,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Some(rows))
}

pub fn get(conn: &Connection, document_id: &str, number: i64) -> rusqlite::Result<Option<Version>> {
    conn.query_row(
        "SELECT id, number, created_at, text FROM versions WHERE document_id = ?1 AND number = ?2",
        params![document_id, number],
        |r| {
            Ok(Version {
                id: r.get(0)?,
                number: r.get(1)?,
                created_at: r.get(2)?,
                text: Some(r.get(3)?),
            })
        },
    )
    .optional()
}

fn exists(conn: &Connection, document_id: &str) -> rusqlite::Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM documents WHERE id = ?1",
            [document_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

#[derive(Debug, PartialEq, Eq)]
pub enum Deleted {
    Gone,
    NoSuchVersion,
    /// Not the latest version, or version 1.
    NotAllowed,
    /// A thread was started on this version and has no earlier anchor to go back to.
    HasNewThreads,
}

/// Delete the latest version (never version 1), putting its threads back on their previous
/// anchors.
pub fn delete_latest(
    conn: &mut Connection,
    document_id: &str,
    number: i64,
    now: i64,
) -> rusqlite::Result<(Deleted, Option<String>)> {
    let tx = conn.transaction()?;
    let row = tx
        .query_row(
            "SELECT d.collection_id, v.id, v.number,
                    (SELECT MAX(number) FROM versions WHERE document_id = d.id)
             FROM documents d JOIN versions v ON v.document_id = d.id
             WHERE d.id = ?1 AND v.number = ?2",
            params![document_id, number],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            },
        )
        .optional()?;
    let Some((collection_id, version_id, number, newest)) = row else {
        return Ok((Deleted::NoSuchVersion, None));
    };
    if number != newest || number == 1 {
        return Ok((Deleted::NotAllowed, None));
    }
    let orphans: i64 = tx.query_row(
        "SELECT COUNT(*) FROM threads t JOIN anchors a ON a.id = t.anchor_id
         WHERE a.version_id = ?1 AND a.carried_from IS NULL",
        [version_id],
        |r| r.get(0),
    )?;
    if orphans > 0 {
        return Ok((Deleted::HasNewThreads, None));
    }
    tx.execute(
        "UPDATE threads SET anchor_id = (SELECT carried_from FROM anchors WHERE id = threads.anchor_id)
         WHERE anchor_id IN (SELECT id FROM anchors WHERE version_id = ?1)",
        [version_id],
    )?;
    tx.execute("DELETE FROM versions WHERE id = ?1", [version_id])?;
    changes::record(
        &tx,
        &Logged {
            collection_id: &collection_id,
            kind: Kind::CollectionUpdated,
            thread_id: None,
            comment_id: None,
            comment: None,
            at: now,
        },
    )?;
    tx.commit()?;
    Ok((Deleted::Gone, Some(collection_id)))
}
