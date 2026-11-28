//! The reader page: server-rendered HTML, one stylesheet, one small script.

use askama::Template;

use crate::highlight::Highlighter;
use crate::refs::Refs;
use crate::store::{Collection, Document};
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
    /// The comment as the author wrote it, which an edit starts from.
    pub body: String,
    /// The comment rendered as markdown.
    pub html: String,
    /// The reader may edit and delete it.
    pub mine: bool,
}

pub struct ThreadView {
    pub id: String,
    pub first: u32,
    pub last: u32,
    pub open: bool,
    /// Why the thread is not where it was written, when it is not.
    pub label: Option<&'static str>,
    pub comments: Vec<CommentView>,
}

impl ThreadView {
    pub fn new(t: &threads::Thread, open: bool, refs: &Refs) -> ThreadView {
        ThreadView {
            id: t.id.clone(),
            first: t.first_line,
            last: t.last_line,
            open,
            label: match t.carry_state {
                threads::CarryState::Placed => None,
                threads::CarryState::Removed => Some("on a line since removed"),
                threads::CarryState::TextChanged => Some("text since changed"),
            },
            comments: t
                .comments
                .iter()
                .map(|c| CommentView {
                    id: c.id.clone(),
                    author: c.author.clone(),
                    body: c.body.clone(),
                    html: crate::markdown::html(&c.body, refs),
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
    /// Inserted or edited in this version.
    pub changed: bool,
    pub html: String,
    /// One per thread covering this line, in that thread's lane.
    pub marks: Vec<MarkView>,
    /// The row's lane count: its highest lane + 1 (0 without marks).
    pub lanes: u16,
    /// Threads whose anchor ends on this line, shown under it.
    pub threads: Vec<ThreadView>,
}

pub struct DocView {
    /// The `N` of the section, `#doc-N`.
    pub index: usize,
    pub path: String,
    pub version: i64,
    pub version_id: i64,
    pub digits: usize,
    pub lines: Vec<LineView>,
}

/// A row of the files list, which names every document of the review on every page.
pub struct FileEntry {
    /// The `N` of the document's section, `#doc-N`.
    pub index: usize,
    pub path: String,
    /// `#doc-N` for a document on this page, else the page that holds it, scrolled to it.
    pub href: String,
    pub summary: String,
}

fn count(n: usize, one: &str, many: &str) -> String {
    match n {
        1 => format!("1 {one}"),
        _ => format!("{n} {many}"),
    }
}

/// `235 lines · 2 comments`, as the files list shows it.
fn summary(lines: usize, comments: usize) -> String {
    let comments = if comments == 0 {
        "no comments".to_string()
    } else {
        count(comments, "comment", "comments")
    };
    format!("{} · {comments}", count(lines, "line", "lines"))
}

#[derive(Template)]
#[template(path = "page.html")]
pub struct Page {
    pub id: String,
    pub number: i64,
    pub title: String,
    pub state: &'static str,
    pub can_publish: bool,
    /// The newest change this page shows; its feed asks for what came after.
    pub seq: i64,
    pub cover_html: String,
    pub source: Option<SourceView>,
    /// Every document of the review.
    pub files: Vec<FileEntry>,
    /// The documents on this page.
    pub docs: Vec<DocView>,
    /// 1-based, of `pages`.
    pub page: usize,
    pub pages: usize,
    /// The previous page's last document, and the next page's first.
    pub prev: Option<String>,
    pub next: Option<String>,
}

/// The meta line's link to the commit the review's documents came from.
pub struct SourceView {
    pub href: String,
    pub label: String,
}

impl SourceView {
    fn new(s: &crate::store::Source) -> SourceView {
        SourceView {
            href: format!("{}/commit/{}", s.repository, s.commit),
            label: format!("{}@{}", s.name(), &s.commit[..7]),
        }
    }
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

/// The lines of the document's latest version that its previous one did not have.
fn changed_in_latest(d: &Document) -> std::collections::HashSet<u32> {
    let Some(previous) = &d.previous_text else {
        return Default::default();
    };
    let (old, new) = (
        margin_core::lines::split(previous),
        margin_core::lines::split(&d.text),
    );
    let map = margin_core::carry::line_map(&old, &new);
    margin_core::carry::changed_lines(&map, new.len())
        .into_iter()
        .collect()
}

/// Where document `index` is on page `page` of collection `id`.
fn page_href(id: &str, page: usize, index: usize) -> String {
    format!("/c/{id}?page={page}#doc-{index}")
}

/// Page `page`, 1-based, of the review cut into pages of up to `page_lines` lines of whole
/// documents; `None` when there is no such page. Only that page's documents are highlighted for
/// display, though code spans resolve against every document.
///
/// CPU-bound (highlighting a few thousand lines); callers run it off the event loop.
pub fn render(
    c: &Collection,
    all_threads: &[threads::Thread],
    seq: i64,
    highlighter: &Highlighter,
    page_lines: usize,
    page: usize,
) -> Result<Option<String>, askama::Error> {
    let line_counts: Vec<usize> = c
        .documents
        .iter()
        .map(|d| margin_core::lines::split(&d.text).len())
        .collect();
    let split = margin_core::pages::split(&line_counts, page_lines);
    let Some(shown) = page.checked_sub(1).and_then(|i| split.get(i)) else {
        return Ok(None);
    };
    let refs = Refs::new(c, highlighter);
    let threads_on = |d: &Document| -> Vec<&threads::Thread> {
        all_threads
            .iter()
            .filter(|t| t.version_id == d.version_id)
            .collect()
    };
    // Documents are numbered across the whole review, so an anchor is the same on every page.
    let files = split
        .iter()
        .enumerate()
        .flat_map(|(p, docs)| docs.clone().map(move |n| (p + 1, n)))
        .map(|(on_page, n)| {
            let d = &c.documents[n];
            FileEntry {
                index: n + 1,
                path: d.path.clone(),
                href: if on_page == page {
                    format!("#doc-{}", n + 1)
                } else {
                    page_href(&c.id, on_page, n + 1)
                },
                summary: summary(
                    line_counts[n],
                    threads_on(d).iter().map(|t| t.comments.len()).sum(),
                ),
            }
        })
        .collect();
    // A neighbouring page exists only for a review of several documents, so it is never empty.
    let prev = (page > 1).then(|| page_href(&c.id, page - 1, split[page - 2].end));
    let next = split
        .get(page)
        .map(|docs| page_href(&c.id, page + 1, docs.start + 1));
    let docs = shown
        .clone()
        .map(|n| {
            let d = &c.documents[n];
            let on = threads_on(d);
            let marks = threads::marks(&on);
            let lens: Vec<u32> = margin_core::lines::split(&d.text)
                .iter()
                .map(|l| l.chars().count() as u32)
                .collect();
            let changed = changed_in_latest(d);
            let lines: Vec<LineView> = highlighter
                .parsed_cached(&d.id, d.version_id, &d.path, d.kind, &d.text)
                .lines
                .iter()
                .enumerate()
                .map(|(i, html)| {
                    let n = (i + 1) as u32;
                    let row_marks: Vec<MarkView> = marks
                        .iter()
                        .filter(|m| m.first <= n && n <= m.last)
                        .map(|m| MarkView {
                            thread: m.thread.clone(),
                            lane: m.lane,
                        })
                        .collect();
                    LineView {
                        n: i + 1,
                        changed: changed.contains(&n),
                        html: tinted(html, &marks, n, lens[i]),
                        lanes: row_marks.iter().map(|m| m.lane + 1).max().unwrap_or(0),
                        marks: row_marks,
                        threads: on
                            .iter()
                            .filter(|t| t.last_line == n)
                            .map(|t| ThreadView::new(t, false, &refs))
                            .collect(),
                    }
                })
                .collect();
            DocView {
                index: n + 1,
                path: d.path.clone(),
                version: d.version,
                version_id: d.version_id,
                digits: lines.len().max(1).to_string().len(),
                lines,
            }
        })
        .collect();
    Page {
        id: c.id.clone(),
        number: c.number,
        title: c.title.clone(),
        state: c.state.as_str(),
        can_publish: c.state != crate::store::State::Closed,
        seq,
        cover_html: if page == 1 {
            crate::markdown::html(&c.cover_note, &refs)
        } else {
            String::new()
        },
        source: c.source.as_ref().map(SourceView::new),
        files,
        docs,
        page,
        pages: split.len(),
        prev,
        next,
    }
    .render()
    .map(Some)
}
