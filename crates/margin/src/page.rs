//! The reader page: server-rendered HTML, one stylesheet, one small script.

use askama::Template;

use crate::highlight::Highlighter;
use crate::store::Collection;

pub struct LineView {
    pub n: usize,
    pub html: String,
}

pub struct DocView {
    pub path: String,
    pub digits: usize,
    pub lines: Vec<LineView>,
}

#[derive(Template)]
#[template(path = "page.html")]
pub struct Page {
    pub title: String,
    pub state: &'static str,
    pub cover_note: String,
    pub docs: Vec<DocView>,
}

pub const CSS: &str = include_str!("../static/margin.css");
pub const JS: &str = include_str!("../static/margin.js");

/// CPU-bound (highlighting a few thousand lines); callers run it off the event loop.
pub fn render(c: &Collection, highlighter: &Highlighter) -> Result<String, askama::Error> {
    let docs = c
        .documents
        .iter()
        .map(|d| {
            let lines: Vec<LineView> = highlighter
                .lines_cached(&d.id, d.version, &d.path, d.kind, &d.text)
                .iter()
                .enumerate()
                .map(|(i, html)| LineView {
                    n: i + 1,
                    html: html.clone(),
                })
                .collect();
            DocView {
                path: d.path.clone(),
                digits: lines.len().max(1).to_string().len(),
                lines,
            }
        })
        .collect();
    Page {
        title: c.title.clone(),
        state: c.state.as_str(),
        cover_note: c.cover_note.clone(),
        docs,
    }
    .render()
}
