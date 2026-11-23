//! The home page: every collection that is not closed, newest activity first, and what each
//! waits on whom.

use std::collections::BTreeMap;

use askama::Template;
use margin_core::waits::{Shown, Waiter, waits_on};
use rusqlite::{Connection, params, params_from_iter};

use crate::store::State;
use crate::threads::Visibility;

pub const PAGE: usize = 50;

pub struct Entry {
    pub id: String,
    pub title: String,
    pub first_line: String,
    pub published: bool,
    pub waits: String,
    pub activity: i64,
}

pub struct Listing {
    pub entries: Vec<Entry>,
    /// Cursor of the next, older page.
    pub older: Option<String>,
}

#[derive(Template)]
#[template(path = "home.html")]
pub struct Home {
    pub entries: Vec<Entry>,
    pub older: Option<String>,
}

struct Head {
    id: String,
    title: String,
    cover_note: String,
    state: State,
    created_by: String,
    activity: i64,
}

pub fn cursor(activity: i64, id: &str) -> String {
    format!("{activity}.{id}")
}

/// One keyset page of open and published collections, with the waits of the page's threads.
pub fn page(
    conn: &Connection,
    limit: usize,
    after: Option<(i64, &str)>,
) -> rusqlite::Result<Listing> {
    let (after_at, after_id) = after.unwrap_or((i64::MAX, "\u{10FFFF}"));
    let mut stmt = conn.prepare(
        "SELECT id, title, cover_note, state, created_by, activity FROM (
             SELECT c.id, c.title, c.cover_note, c.state, c.created_by,
                    COALESCE((SELECT at FROM changes WHERE collection_id = c.id
                              ORDER BY seq DESC LIMIT 1), c.updated_at) AS activity
             FROM collections c WHERE c.state != 'closed')
         WHERE (activity, id) < (?1, ?2)
         ORDER BY activity DESC, id DESC
         LIMIT ?3",
    )?;
    let mut heads = stmt
        .query_map(params![after_at, after_id, (limit + 1) as i64], |r| {
            Ok(Head {
                id: r.get(0)?,
                title: r.get(1)?,
                cover_note: r.get(2)?,
                state: State::parse(&r.get::<_, String>(3)?)?,
                created_by: r.get(4)?,
                activity: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let older = if heads.len() > limit {
        heads.truncate(limit);
        heads.last().map(|h| cursor(h.activity, &h.id))
    } else {
        None
    };

    // thread id -> (collection id, comments oldest first), for this page's collections only.
    let marks = vec!["?"; heads.len()].join(",");
    let mut stmt = conn.prepare(&format!(
        "SELECT t.collection_id, t.id, c.author, c.visibility
         FROM threads t JOIN comments c ON c.thread_id = t.id
         WHERE t.collection_id IN ({marks})
         ORDER BY t.id, c.created_at, c.rowid"
    ))?;
    let mut threads: BTreeMap<String, (String, Vec<(String, Shown)>)> = BTreeMap::new();
    let rows = stmt.query_map(params_from_iter(heads.iter().map(|h| &h.id)), |r| {
        let shown = match Visibility::parse(&r.get::<_, String>(3)?)? {
            Visibility::Now => Shown::Now,
            Visibility::Held => Shown::Held,
        };
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            shown,
        ))
    })?;
    for row in rows {
        let (collection, thread, author, shown) = row?;
        threads
            .entry(thread)
            .or_insert_with(|| (collection, Vec::new()))
            .1
            .push((author, shown));
    }

    let entries = heads
        .into_iter()
        .map(|h| {
            let mut tally = Tally::default();
            for (_, comments) in threads.values().filter(|(c, _)| *c == h.id) {
                let borrowed: Vec<(&str, Shown)> =
                    comments.iter().map(|(a, s)| (a.as_str(), *s)).collect();
                tally.held += borrowed.iter().filter(|(_, s)| *s == Shown::Held).count();
                match waits_on(&borrowed, &h.created_by) {
                    Waiter::Reader => tally.reader += 1,
                    Waiter::Agent(name) => *tally.agents.entry(name).or_default() += 1,
                    Waiter::Nobody => {}
                }
            }
            Entry {
                first_line: h
                    .cover_note
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .trim()
                    .to_string(),
                published: h.state == State::Published,
                waits: tally.line(),
                id: h.id,
                title: h.title,
                activity: h.activity,
            }
        })
        .collect();
    Ok(Listing { entries, older })
}

#[derive(Default)]
struct Tally {
    reader: usize,
    agents: BTreeMap<String, usize>,
    held: usize,
}

impl Tally {
    /// "2 wait on you · 1 waits on ada · 3 held for Publish"; zero counts are left out.
    fn line(&self) -> String {
        let verb = |n: usize| if n == 1 { "waits" } else { "wait" };
        let mut parts = Vec::new();
        if self.reader > 0 {
            parts.push(format!("{} {} on you", self.reader, verb(self.reader)));
        }
        for (name, n) in &self.agents {
            parts.push(format!("{n} {} on {name}", verb(*n)));
        }
        if self.held > 0 {
            parts.push(format!("{} held for Publish", self.held));
        }
        parts.join(" · ")
    }
}

pub fn render(l: Listing) -> Result<String, askama::Error> {
    Home {
        entries: l.entries,
        older: l.older,
    }
    .render()
}
