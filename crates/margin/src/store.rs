//! Collections, documents and versions, as plain functions over a connection.
//! These run on a [`crate::db::Db`] worker thread.

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::highlight::Definition;

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
    pub(crate) fn parse(s: &str) -> rusqlite::Result<State> {
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
    /// What this version defines, as [`record_definitions`] stored it; `None` when it has not
    /// been recorded. Not part of the API.
    #[serde(skip)]
    pub definitions: Option<Vec<Definition>>,
}

/// Where a review's documents came from: a repository's web URL and a commit in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    pub repository: String,
    pub commit: String,
}

impl Source {
    /// The last segment of the repository's URL.
    pub fn name(&self) -> &str {
        self.repository.rsplit('/').next().unwrap_or("")
    }

    /// Check the fields and normalise the repository: a trailing `/` and `.git` are dropped.
    /// The error names the field at fault.
    pub fn normalised(&self) -> Result<Source, String> {
        let r = self.repository.as_str();
        let bad =
            "source.repository must be http(s)://host/…/name with no whitespace, query or fragment";
        let rest = r
            .strip_prefix("https://")
            .or_else(|| r.strip_prefix("http://"))
            .ok_or(bad)?;
        if r.chars().any(|c| c.is_whitespace() || c.is_control())
            || r.contains('?')
            || r.contains('#')
        {
            return Err(bad.into());
        }
        let trimmed = r.trim_end_matches('/');
        let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);
        let trimmed = trimmed.trim_end_matches('/');
        let path = trimmed.get(r.len() - rest.len()..).ok_or(bad)?;
        match path.split_once('/') {
            Some((host, path)) if !host.is_empty() && !path.is_empty() => {}
            _ => return Err(bad.into()),
        }
        let ok = self.commit.len() == 40
            && self
                .commit
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !ok {
            return Err("source.commit must be 40 lowercase hex characters".into());
        }
        Ok(Source {
            repository: trimmed.to_string(),
            commit: self.commit.clone(),
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Collection {
    pub id: String,
    /// The short number the review is opened by, `/c/{number}`.
    pub number: i64,
    pub title: String,
    pub cover_note: String,
    pub state: State,
    pub created_by: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub source: Option<Source>,
    pub documents: Vec<Document>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub id: String,
    pub number: i64,
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
    pub source: Option<Source>,
}

pub struct NewCollection<'a> {
    pub id: &'a str,
    pub title: &'a str,
    pub cover_note: &'a str,
    pub created_by: &'a str,
    pub now: i64,
    pub source: Option<&'a Source>,
    /// (document id, document) in display order.
    pub documents: &'a [(String, NewDocument)],
}

/// Insert the collection and return its number.
pub fn create(conn: &mut Connection, c: &NewCollection) -> rusqlite::Result<i64> {
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO collections (id, title, cover_note, state, created_by, created_at, updated_at, source_repository, source_commit)
         VALUES (?1, ?2, ?3, 'open', ?4, ?5, ?5, ?6, ?7)",
        params![
            c.id,
            c.title,
            c.cover_note,
            c.created_by,
            c.now,
            c.source.map(|s| &s.repository),
            c.source.map(|s| &s.commit)
        ],
    )?;
    tx.execute(
        "INSERT INTO collection_numbers (collection_id) VALUES (?1)",
        [c.id],
    )?;
    let number = tx.last_insert_rowid();
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
    tx.commit()?;
    Ok(number)
}

/// The opaque id a URL key names: an exact id first, else an all-digit key as a number.
pub fn resolve(conn: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    let exact = conn
        .query_row("SELECT id FROM collections WHERE id = ?1", [key], |r| {
            r.get(0)
        })
        .optional()?;
    if exact.is_some() || key.is_empty() || !key.bytes().all(|b| b.is_ascii_digit()) {
        return Ok(exact);
    }
    let Ok(number) = key.parse::<i64>() else {
        return Ok(None);
    };
    conn.query_row(
        "SELECT collection_id FROM collection_numbers WHERE number = ?1",
        [number],
        |r| r.get(0),
    )
    .optional()
}

pub fn get(conn: &Connection, id: &str) -> rusqlite::Result<Option<Collection>> {
    let head = conn
        .query_row(
            "SELECT c.id, c.title, c.cover_note, c.state, c.created_by, c.created_at, c.updated_at,
                    c.source_repository, c.source_commit, n.number
             FROM collections c JOIN collection_numbers n ON n.collection_id = c.id
             WHERE c.id = ?1",
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
                    source: match (r.get(7)?, r.get(8)?) {
                        (Some(repository), Some(commit)) => Some(Source { repository, commit }),
                        _ => None,
                    },
                    number: r.get(9)?,
                    documents: Vec::new(),
                })
            },
        )
        .optional()?;
    let Some(mut c) = head else { return Ok(None) };
    let mut stmt = conn.prepare(
        "SELECT d.id, d.path, d.kind, v.number, v.text, v.id,
                (SELECT p.text FROM versions p WHERE p.document_id = d.id AND p.number = v.number - 1),
                v.definitions_recorded
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
                definitions: r.get::<_, bool>(7)?.then(Vec::new),
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    fill_definitions(conn, &mut c)?;
    Ok(Some(c))
}

/// The recorded definitions of each of the collection's documents that has them.
fn fill_definitions(conn: &Connection, c: &mut Collection) -> rusqlite::Result<()> {
    let mut wanted: HashMap<i64, &mut Vec<Definition>> = c
        .documents
        .iter_mut()
        .filter_map(|d| Some((d.version_id, d.definitions.as_mut()?)))
        .collect();
    let mut stmt = conn.prepare(
        "SELECT f.version_id, f.name, f.line, f.imp, f.owner
         FROM documents d
         JOIN versions v ON v.document_id = d.id
              AND v.number = (SELECT MAX(number) FROM versions WHERE document_id = d.id)
         JOIN definitions f ON f.version_id = v.id
         WHERE d.collection_id = ?1
         ORDER BY f.version_id, f.position",
    )?;
    let mut rows = stmt.query([&c.id])?;
    while let Some(r) = rows.next()? {
        if let Some(definitions) = wanted.get_mut(&r.get(0)?) {
            definitions.push(Definition {
                name: r.get(1)?,
                line: r.get(2)?,
                imp: r.get(3)?,
                owner: r.get(4)?,
            });
        }
    }
    Ok(())
}

/// Store what each version in `recorded`, by row id, defines, in one transaction. `read` holds
/// the documents as they were parsed. A version is skipped when it is already recorded, or when
/// its row no longer holds that document and text: it was deleted during the parse, and its id
/// may have been given to another version.
pub fn record_definitions(
    conn: &mut Connection,
    read: &[Document],
    recorded: &[(i64, Vec<Definition>)],
) -> rusqlite::Result<()> {
    let read: HashMap<i64, &Document> = read.iter().map(|d| (d.version_id, d)).collect();
    let tx = conn.transaction()?;
    {
        let mut claim = tx.prepare(
            "UPDATE versions SET definitions_recorded = 1
             WHERE id = ?1 AND definitions_recorded = 0 AND document_id = ?2 AND text = ?3",
        )?;
        let mut insert = tx.prepare(
            "INSERT INTO definitions (version_id, position, name, line, imp, owner)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for (version_id, definitions) in recorded {
            let Some(d) = read.get(version_id) else {
                continue;
            };
            if claim.execute(params![version_id, d.id, d.text])? == 0 {
                continue;
            }
            for (position, f) in definitions.iter().enumerate() {
                insert.execute(params![
                    version_id,
                    position as i64,
                    f.name,
                    f.line,
                    f.imp,
                    f.owner
                ])?;
            }
        }
    }
    tx.commit()
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
        "SELECT c.id, c.title, c.state, c.created_by, c.created_at, n.number
         FROM collections c JOIN collection_numbers n ON n.collection_id = c.id
         WHERE (c.created_at, c.id) < (?1, ?2)
         ORDER BY c.created_at DESC, c.id DESC
         LIMIT ?3",
    )?;
    stmt.query_map(params![after_at, after_id, limit as i64], |r| {
        Ok(Summary {
            id: r.get(0)?,
            title: r.get(1)?,
            state: State::parse(&r.get::<_, String>(2)?)?,
            created_by: r.get(3)?,
            created_at: r.get(4)?,
            number: r.get(5)?,
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
            source_repository = COALESCE(?6, source_repository),
            source_commit = COALESCE(?7, source_commit),
            updated_at = ?5
         WHERE id = ?1",
        params![
            id,
            p.title,
            p.cover_note,
            p.state.map(State::as_str),
            now,
            p.source.as_ref().map(|s| &s.repository),
            p.source.as_ref().map(|s| &s.commit)
        ],
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
