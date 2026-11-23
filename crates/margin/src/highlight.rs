//! Per-line syntax highlighting with classed spans, so colours live in the stylesheet.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

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
    /// [`Highlighter::lines`], remembered per document version so a reload costs nothing.
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
