//! Per-line syntax highlighting with classed spans, so colours live in the stylesheet.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use margin_core::highlight::Segment;
use syntect::html::{ClassStyle, line_tokens_to_classed_spans};
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxSet};
use syntect::util::LinesWithEndings;

use crate::store::Kind;

/// Highlighted documents kept in memory, by (document id, version); both are immutable.
const CACHE_ENTRIES: usize = 64;

type Cache = Mutex<HashMap<(String, i64), Arc<Vec<String>>>>;

pub struct Highlighter {
    syntaxes: SyntaxSet,
    cache: Cache,
}

pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

fn open_tag(scope: Scope) -> String {
    let classes = scope.build_string().replace('.', " ");
    format!("<span class=\"{classes}\">")
}

impl Highlighter {
    pub fn new() -> Highlighter {
        Highlighter {
            syntaxes: SyntaxSet::load_defaults_newlines(),
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// One HTML fragment per line of `text`, each balanced (spans open at a line's end are
    /// closed there and reopened on the next line), so a line is a self-contained element.
    pub fn lines(&self, path: &str, kind: Kind, text: &str) -> Vec<String> {
        let split = margin_core::lines::split(text);
        let syntax = match kind {
            Kind::Prose => None,
            Kind::Code => Path::new(path)
                .extension()
                .and_then(|e| e.to_str())
                .and_then(|ext| self.syntaxes.find_syntax_by_extension(ext)),
        };
        let Some(syntax) = syntax else {
            return split.iter().map(|l| escape(l)).collect();
        };
        let mut parse = ParseState::new(syntax);
        let mut stack = ScopeStack::new();
        let mut out = Vec::with_capacity(split.len());
        for line in
            LinesWithEndings::from(&crate::highlight::normalise(text, &split)).take(split.len())
        {
            let rendered = (|| {
                let ops = parse.parse_line(line, &self.syntaxes).ok()?;
                let reopen: String = stack.as_slice().iter().map(|s| open_tag(*s)).collect();
                let (mut html, _delta) =
                    line_tokens_to_classed_spans(line, &ops, ClassStyle::Spaced, &mut stack)
                        .ok()?;
                if let Some(i) = html.rfind('\n') {
                    html.remove(i);
                }
                let mut s = reopen;
                s.push_str(&html);
                for _ in 0..stack.len() {
                    s.push_str("</span>");
                }
                Some(s)
            })();
            out.push(rendered.unwrap_or_else(|| escape(line.trim_end_matches('\n'))));
        }
        out
    }
}

impl Default for Highlighter {
    fn default() -> Self {
        Self::new()
    }
}

/// The lines joined with `\n` and a final `\n`, which is what the parser expects.
pub fn normalise(_text: &str, lines: &[&str]) -> String {
    let mut s = String::new();
    for l in lines {
        s.push_str(l);
        s.push('\n');
    }
    s
}

impl Highlighter {
    /// [`Highlighter::lines`], remembered per version row (its id, never reused) so a reload costs nothing.
    pub fn lines_cached(
        &self,
        doc_id: &str,
        version: i64,
        path: &str,
        kind: Kind,
        text: &str,
    ) -> Arc<Vec<String>> {
        let key = (doc_id.to_string(), version);
        if let Some(hit) = self.cache.lock().ok().and_then(|c| c.get(&key).cloned()) {
            return hit;
        }
        let lines = Arc::new(self.lines(path, kind, text));
        if let Ok(mut c) = self.cache.lock() {
            if c.len() >= CACHE_ENTRIES {
                c.clear();
            }
            c.insert(key, lines.clone());
        }
        lines
    }
}

/// The class a lane's colour is set by; lanes past 3 share the last colour, as in the gutter.
pub fn lane_class(lane: u16) -> String {
    format!("hl-l{}", lane.min(3))
}

/// One line's classed-span HTML with each of `segments` wrapped in a `hl` run carrying a class
/// per lane and the threads covering it in `data-threads` (`threads` maps lane to thread id).
/// The run is outermost: the syntax spans are closed at a boundary and reopened inside the
/// next run, so every element stays balanced and the syntax classes survive within it.
pub fn tint(html: &str, segments: &[Segment], threads: &[(u16, String)]) -> String {
    let open_run = |seg: &Segment| {
        let mut classes: Vec<String> = seg.lanes.iter().map(|l| lane_class(*l)).collect();
        classes.dedup();
        let ids: Vec<&str> = seg
            .lanes
            .iter()
            .filter_map(|l| {
                threads
                    .iter()
                    .find(|(t, _)| t == l)
                    .map(|(_, id)| id.as_str())
            })
            .collect();
        format!(
            "<span class=\"hl {}\" data-threads=\"{}\">",
            classes.join(" "),
            ids.join(" ")
        )
    };
    let mut out = String::with_capacity(html.len() + segments.len() * 64);
    let mut open: Vec<&str> = Vec::new();
    let (mut col, mut inside, mut at) = (0u32, None::<usize>, 0usize);
    while at < html.len() {
        let rest = &html[at..];
        if rest.starts_with("</span>") {
            open.pop();
            out.push_str("</span>");
            at += "</span>".len();
        } else if rest.starts_with('<') {
            let end = rest.find('>').map_or(rest.len(), |p| p + 1);
            open.push(&rest[..end]);
            out.push_str(&rest[..end]);
            at += end;
        } else {
            let width = match rest.starts_with('&').then(|| rest.find(';')).flatten() {
                Some(p) => p + 1,
                None => rest.chars().next().map_or(1, char::len_utf8),
            };
            let want = segments.iter().position(|s| s.range.contains(&col));
            if want != inside {
                for _ in &open {
                    out.push_str("</span>");
                }
                if inside.is_some() {
                    out.push_str("</span>");
                }
                if let Some(k) = want {
                    out.push_str(&open_run(&segments[k]));
                }
                for tag in &open {
                    out.push_str(tag);
                }
                inside = want;
            }
            out.push_str(&rest[..width]);
            col += 1;
            at += width;
        }
    }
    if inside.is_some() {
        out.push_str("</span>");
    }
    out
}
