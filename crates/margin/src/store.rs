//! Collections, documents and versions, as plain functions over a connection.
//! These run on a [`crate::db::Db`] worker thread.

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

fn unknown(what: &str, value: &str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        format!("unknown {what} {value:?} in the database").into(),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Code,
    Prose,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Code => "code",
            Kind::Prose => "prose",
        }
    }
    fn parse(s: &str) -> rusqlite::Result<Kind> {
        match s {
            "code" => Ok(Kind::Code),
            "prose" => Ok(Kind::Prose),
            other => Err(unknown("kind", other)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Open,
    Published,
    Closed,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Open => "open",
            State::Published => "published",
            State::Closed => "closed",
        }
    }
    fn parse(s: &str) -> rusqlite::Result<State> {
        match s {
            "open" => Ok(State::Open),
            "published" => Ok(State::Published),
            "closed" => Ok(State::Closed),
            other => Err(unknown("state", other)),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct NewDocument {
    pub path: String,
    pub kind: Kind,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Document {
    pub id: String,
    pub version_id: i64,
    pub path: String,
    pub kind: Kind,
    pub version: i64,
    pub text: String,
    /// The text of version `version - 1`, for marking what changed; not part of the API.
    #[serde(skip)]
    pub previous_text: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Collection {
    pub id: String,
    pub title: String,
    pub cover_note: String,
    pub state: State,
    pub created_by: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub documents: Vec<Document>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub id: String,
    pub title: String,
    pub state: State,
    pub created_by: String,
    pub created_at: i64,
}

#[derive(Debug, Default, Deserialize)]
pub struct Patch {
    pub title: Option<String>,
    pub cover_note: Option<String>,
    pub state: Option<State>,
}

pub struct NewCollection<'a> {
    pub id: &'a str,
    pub title: &'a str,
    pub cover_note: &'a str,
    pub created_by: &'a str,
    pub now: i64,
    /// (document id, document) in display order.
    pub documents: &'a [(String, NewDocument)],
}

pub fn create(conn: &mut Connection, c: &NewCollection) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO collections (id, title, cover_note, state, created_by, created_at, updated_at)
         VALUES (?1, ?2, ?3, 'open', ?4, ?5, ?5)",
        params![c.id, c.title, c.cover_note, c.created_by, c.now],
    )?;
    for (position, (doc_id, d)) in c.documents.iter().enumerate() {
        tx.execute(
            "INSERT INTO documents (id, collection_id, position, path, kind) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![doc_id, c.id, position as i64, d.path, d.kind.as_str()],
        )?;
        tx.execute(
            "INSERT INTO versions (document_id, number, text, created_at) VALUES (?1, 1, ?2, ?3)",
            params![doc_id, d.text, c.now],
        )?;
    }
    tx.commit()
}

pub fn get(conn: &Connection, id: &str) -> rusqlite::Result<Option<Collection>> {
    let head = conn
        .query_row(
            "SELECT id, title, cover_note, state, created_by, created_at, updated_at FROM collections WHERE id = ?1",
            [id],
            |r| {
                Ok(Collection {
                    id: r.get(0)?,
                    title: r.get(1)?,
                    cover_note: r.get(2)?,
                    state: State::parse(&r.get::<_, String>(3)?)?,
                    created_by: r.get(4)?,
                    created_at: r.get(5)?,
                    updated_at: r.get(6)?,
                    documents: Vec::new(),
                })
            },
        )
        .optional()?;
    let Some(mut c) = head else { return Ok(None) };
    let mut stmt = conn.prepare(
        "SELECT d.id, d.path, d.kind, v.number, v.text, v.id,
                (SELECT p.text FROM versions p WHERE p.document_id = d.id AND p.number = v.number - 1)
         FROM documents d
         JOIN versions v ON v.document_id = d.id
              AND v.number = (SELECT MAX(number) FROM versions WHERE document_id = d.id)
         WHERE d.collection_id = ?1
         ORDER BY d.position",
    )?;
    c.documents = stmt
        .query_map([id], |r| {
            Ok(Document {
                id: r.get(0)?,
                path: r.get(1)?,
                kind: Kind::parse(&r.get::<_, String>(2)?)?,
                version: r.get(3)?,
                text: r.get(4)?,
                version_id: r.get(5)?,
                previous_text: r.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(Some(c))
}

/// Keyset page, newest first. `after` is the (created_at, id) of the last row already seen.
/// Returns up to `limit` rows.
pub fn list(
    conn: &Connection,
    limit: usize,
    after: Option<(i64, &str)>,
) -> rusqlite::Result<Vec<Summary>> {
    let (after_at, after_id) = after.unwrap_or((i64::MAX, "\u{10FFFF}"));
    let mut stmt = conn.prepare(
        "SELECT id, title, state, created_by, created_at FROM collections
         WHERE (created_at, id) < (?1, ?2)
         ORDER BY created_at DESC, id DESC
         LIMIT ?3",
    )?;
    stmt.query_map(params![after_at, after_id, limit as i64], |r| {
        Ok(Summary {
            id: r.get(0)?,
            title: r.get(1)?,
            state: State::parse(&r.get::<_, String>(2)?)?,
            created_by: r.get(3)?,
            created_at: r.get(4)?,
        })
    })?
    .collect()
}

pub fn patch(conn: &mut Connection, id: &str, p: &Patch, now: i64) -> rusqlite::Result<bool> {
    let tx = conn.transaction()?;
    let n = tx.execute(
        "UPDATE collections SET
            title = COALESCE(?2, title),
            cover_note = COALESCE(?3, cover_note),
            state = COALESCE(?4, state),
            updated_at = ?5
         WHERE id = ?1",
        params![id, p.title, p.cover_note, p.state.map(State::as_str), now],
    )?;
    if n > 0 {
        crate::changes::record(
            &tx,
            &crate::changes::Logged {
                collection_id: id,
                kind: crate::changes::Kind::CollectionUpdated,
                thread_id: None,
                comment_id: None,
                comment: None,
                at: now,
            },
        )?;
    }
    tx.commit()?;
    Ok(n > 0)
}

pub fn delete(conn: &Connection, id: &str) -> rusqlite::Result<bool> {
    Ok(conn.execute("DELETE FROM collections WHERE id = ?1", [id])? > 0)
}
