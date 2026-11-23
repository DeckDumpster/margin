//! The reader page: server-rendered HTML, one stylesheet, one small script.

use askama::Template;

use crate::highlight::Highlighter;
use crate::store::Collection;
use crate::threads::{self, READER};
use margin_core::anchor::Anchor;
use margin_core::highlight;

pub struct MarkView {
    pub thread: String,
    pub lane: u16,
}

pub struct CommentView {
    pub id: String,
    pub author: String,
    pub body: String,
    /// The reader may edit and delete it.
    pub mine: bool,
}

pub struct ThreadView {
    pub id: String,
    pub first: u32,
    pub last: u32,
    pub open: bool,
    pub comments: Vec<CommentView>,
}

impl ThreadView {
    pub fn new(t: &threads::Thread, open: bool) -> ThreadView {
        ThreadView {
            id: t.id.clone(),
            first: t.first_line,
            last: t.last_line,
            open,
            comments: t
                .comments
                .iter()
                .map(|c| CommentView {
                    id: c.id.clone(),
                    author: c.author.clone(),
                    body: c.body.clone(),
                    mine: c.author == READER,
                })
                .collect(),
        }
    }
}

/// The partial every write answers with; the page includes the same template.
#[derive(Template)]
#[template(path = "thread.html")]
pub struct ThreadFragment {
    pub thread: ThreadView,
}

pub struct LineView {
    pub n: usize,
    pub html: String,
    /// One per thread covering this line, in that thread's lane.
    pub marks: Vec<MarkView>,
    /// Threads whose anchor ends on this line, shown under it.
    pub threads: Vec<ThreadView>,
}

pub struct DocView {
    pub path: String,
    pub version_id: i64,
    pub digits: usize,
    pub lines: Vec<LineView>,
}

#[derive(Template)]
#[template(path = "page.html")]
pub struct Page {
    pub id: String,
    pub title: String,
    pub state: &'static str,
    pub cover_note: String,
    pub docs: Vec<DocView>,
}

/// `html` with the highlights of `marks` that reach line `n` (`len` columns long) wrapped in runs.
fn tinted(html: &str, marks: &[threads::Mark], n: u32, len: u32) -> String {
    let mut spans = Vec::new();
    let mut threads = Vec::new();
    for m in marks {
        let anchor = Anchor {
            first: m.first,
            last: m.last,
            cols: m.start_col.zip(m.end_col),
        };
        if let Some((s, e)) = highlight::span_on_line(&anchor, n, len) {
            spans.push((m.lane, s, e));
            threads.push((m.lane, m.thread.clone()));
        }
    }
    if spans.is_empty() {
        return html.to_string();
    }
    crate::highlight::tint(html, &highlight::segments(len, &spans), &threads)
}

pub const CSS: &str = include_str!("../static/margin.css");
pub const JS: &str = include_str!("../static/margin.js");

/// CPU-bound (highlighting a few thousand lines); callers run it off the event loop.
pub fn render(
    c: &Collection,
    all_threads: &[threads::Thread],
    highlighter: &Highlighter,
) -> Result<String, askama::Error> {
    let docs = c
        .documents
        .iter()
        .map(|d| {
            let on: Vec<&threads::Thread> = all_threads
                .iter()
                .filter(|t| t.version_id == d.version_id)
                .collect();
            let marks = threads::marks(&on);
            let lens: Vec<u32> = margin_core::lines::split(&d.text)
                .iter()
                .map(|l| l.chars().count() as u32)
                .collect();
            let lines: Vec<LineView> = highlighter
                .lines_cached(&d.id, d.version, &d.path, d.kind, &d.text)
                .iter()
                .enumerate()
                .map(|(i, html)| {
                    let n = (i + 1) as u32;
                    LineView {
                        n: i + 1,
                        html: tinted(html, &marks, n, lens[i]),
                        marks: marks
                            .iter()
                            .filter(|m| m.first <= n && n <= m.last)
                            .map(|m| MarkView {
                                thread: m.thread.clone(),
                                lane: m.lane,
                            })
                            .collect(),
                        threads: on
                            .iter()
                            .filter(|t| t.last_line == n)
                            .map(|t| ThreadView::new(t, false))
                            .collect(),
                    }
                })
                .collect();
            DocView {
                path: d.path.clone(),
                version_id: d.version_id,
                digits: lines.len().max(1).to_string().len(),
                lines,
            }
        })
        .collect();
    Page {
        id: c.id.clone(),
        title: c.title.clone(),
        state: c.state.as_str(),
        cover_note: c.cover_note.clone(),
        docs,
    }
    .render()
}
