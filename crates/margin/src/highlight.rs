//! Per-line syntax highlighting with classed spans, so colours live in the stylesheet.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use margin_core::highlight::Segment;
use syntect::html::{ClassStyle, line_tokens_to_classed_spans};
use syntect::parsing::{ParseState, Scope, ScopeStack, ScopeStackOp, SyntaxSet};
use syntect::util::LinesWithEndings;

use crate::store::Kind;

/// Highlighted documents kept in memory, by (document id, version); both are immutable.
const CACHE_ENTRIES: usize = 64;

type Cache = Mutex<HashMap<(String, i64), Arc<Parsed>>>;

/// What one parse of a document yields: its lines as HTML, and what it defines.
pub struct Parsed {
    pub lines: Vec<String>,
    /// `(name, line)` of each definition the syntax marks: functions, types, modules, macros,
    /// constants. An `impl` counts only where nothing else in the document defines its name.
    pub definitions: Vec<(String, u32)>,
}

/// The scopes that mark the name of a definition; the flag is whether it is an `impl`.
const DEFINITION_SCOPES: [(&str, bool); 11] = [
    ("entity.name.function", false),
    ("entity.name.struct", false),
    ("entity.name.enum", false),
    ("entity.name.union", false),
    ("entity.name.trait", false),
    ("entity.name.impl", true),
    ("entity.name.module", false),
    ("entity.name.type", false),
    ("entity.name.class", false),
    ("entity.name.interface", false),
    ("entity.name.macro", false),
];

pub struct Highlighter {
    syntaxes: SyntaxSet,
    cache: Cache,
    definition_scopes: Vec<(Scope, bool)>,
    storage_type: Scope,
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

/// Scope atoms are classes, and bare ones (`path`, `meta`) would match the page's own rules.
const CLASS_STYLE: ClassStyle = ClassStyle::SpacedPrefixed { prefix: "tok-" };
const CLASS_PREFIX: &str = "tok-";

fn open_tag(scope: Scope) -> String {
    let classes = scope
        .build_string()
        .split('.')
        .map(|atom| format!("{CLASS_PREFIX}{atom}"))
        .collect::<Vec<_>>()
        .join(" ");
    format!("<span class=\"{classes}\">")
}

impl Highlighter {
    pub fn new() -> Highlighter {
        Highlighter {
            syntaxes: SyntaxSet::load_defaults_newlines(),
            cache: Mutex::new(HashMap::new()),
            definition_scopes: DEFINITION_SCOPES
                .iter()
                .map(|(s, imp)| (Scope::new(s).expect("a valid scope"), *imp))
                .collect(),
            storage_type: Scope::new("storage.type").expect("a valid scope"),
        }
    }

    /// One HTML fragment per line of `text`, each balanced (spans open at a line's end are
    /// closed there and reopened on the next line), so a line is a self-contained element.
    pub fn lines(&self, path: &str, kind: Kind, text: &str) -> Vec<String> {
        self.parse(path, kind, text).lines
    }

    /// [`Highlighter::lines`] and the document's definitions, from the one parse.
    pub fn parse(&self, path: &str, kind: Kind, text: &str) -> Parsed {
        let split = margin_core::lines::split(text);
        let syntax = match kind {
            Kind::Prose => None,
            Kind::Code => Path::new(path)
                .extension()
                .and_then(|e| e.to_str())
                .and_then(|ext| self.syntaxes.find_syntax_by_extension(ext)),
        };
        let Some(syntax) = syntax else {
            return Parsed {
                lines: split.iter().map(|l| escape(l)).collect(),
                definitions: Vec::new(),
            };
        };
        let mut parse = ParseState::new(syntax);
        let mut stack = ScopeStack::new();
        let mut scan = ScopeStack::new();
        let mut found = Vec::new();
        let mut out = Vec::with_capacity(split.len());
        for (i, line) in LinesWithEndings::from(&crate::highlight::normalise(text, &split))
            .take(split.len())
            .enumerate()
        {
            let rendered = (|| {
                let ops = parse.parse_line(line, &self.syntaxes).ok()?;
                self.scan_definitions(line, &ops, &mut scan, (i + 1) as u32, &mut found);
                let reopen: String = stack.as_slice().iter().map(|s| open_tag(*s)).collect();
                let (mut html, _delta) =
                    line_tokens_to_classed_spans(line, &ops, CLASS_STYLE, &mut stack).ok()?;
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
        let named: std::collections::HashSet<&str> = found
            .iter()
            .filter(|(_, _, imp)| !imp)
            .map(|(n, _, _)| n.as_str())
            .collect();
        let definitions = found
            .iter()
            .filter(|(n, _, imp)| !imp || !named.contains(n.as_str()))
            .map(|(n, l, _)| (n.clone(), *l))
            .collect();
        Parsed {
            lines: out,
            definitions,
        }
    }

    /// Add to `found` the `(name, line, is_impl)` defined on `line`, from the scopes of its
    /// tokens. A Rust constant is not marked as a definition: `const` or `static` is
    /// `storage.type`, and the name follows it, then `:`.
    fn scan_definitions(
        &self,
        line: &str,
        ops: &[(usize, ScopeStackOp)],
        scan: &mut ScopeStack,
        n: u32,
        found: &mut Vec<(String, u32, bool)>,
    ) {
        let mut run: Option<(String, bool)> = None;
        let mut constant_at = None;
        let mut flush = |run: &mut Option<(String, bool)>| {
            if let Some((name, imp)) = run.take() {
                let name = name.trim();
                if is_identifier(name) {
                    found.push((name.to_string(), n, imp));
                }
            }
        };
        let mut at = 0;
        let end = [(line.len(), ScopeStackOp::Noop)];
        for (off, op) in ops.iter().chain(end.iter()) {
            let off = (*off).min(line.len());
            let text = &line[at.min(off)..off];
            if !text.is_empty() {
                let named = scan.as_slice().iter().find_map(|s| {
                    self.definition_scopes
                        .iter()
                        .find(|(d, _)| d.is_prefix_of(*s))
                        .map(|(_, imp)| *imp)
                });
                match (named, &mut run) {
                    (Some(imp), Some((name, was))) if *was == imp => name.push_str(text),
                    (Some(imp), _) => {
                        flush(&mut run);
                        run = Some((text.to_string(), imp));
                    }
                    (None, _) => flush(&mut run),
                }
                if matches!(text.trim(), "const" | "static")
                    && scan
                        .as_slice()
                        .iter()
                        .any(|s| self.storage_type.is_prefix_of(*s))
                {
                    constant_at = Some(off);
                }
            }
            scan.apply(op).ok();
            at = off;
        }
        flush(&mut run);
        if let Some(from) = constant_at {
            let rest = line[from..].trim_start();
            let rest = rest.strip_prefix("mut ").map_or(rest, str::trim_start);
            let name_len = rest
                .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            let (name, after) = rest.split_at(name_len);
            if is_identifier(name) && after.trim_start().starts_with(':') {
                found.push((name.to_string(), n, false));
            }
        }
    }
}

/// Whether `s` is a name: a letter or `_`, then letters, digits and `_`.
pub fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_alphabetic() || c == '_')
        && chars.all(|c| c.is_alphanumeric() || c == '_')
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
    /// [`Highlighter::parse`], remembered per version row (its id, never reused) so a reload costs nothing.
    pub fn parsed_cached(
        &self,
        doc_id: &str,
        version: i64,
        path: &str,
        kind: Kind,
        text: &str,
    ) -> Arc<Parsed> {
        let key = (doc_id.to_string(), version);
        if let Some(hit) = self.cache.lock().ok().and_then(|c| c.get(&key).cloned()) {
            return hit;
        }
        let lines = Arc::new(self.parse(path, kind, text));
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

#[cfg(test)]
mod tests {
    use super::*;
    use margin_test_id::test_id;

    #[test_id(t709k1x)]
    #[test]
    fn every_class_is_prefixed_including_a_comment_reopened_on_its_second_line() {
        let text = "use std::mem::size_of;\n/* one\ntwo */\n";
        let lines = Highlighter::new().lines("a.rs", Kind::Code, text);
        assert_eq!(lines.len(), 3, "{lines:?}");
        for line in &lines {
            let mut rest = line.as_str();
            while let Some(i) = rest.find("class=\"") {
                rest = &rest[i + 7..];
                let classes = &rest[..rest.find('"').unwrap()];
                for class in classes.split(' ') {
                    assert!(class.starts_with("tok-"), "{class:?} in {line}");
                }
            }
        }
        assert!(lines[0].contains("tok-path"), "{}", lines[0]);
        let reopened = lines[2].split("two").next().unwrap();
        assert!(reopened.contains("tok-comment"), "{}", lines[2]);
        assert!(lines[1].starts_with(reopened), "{reopened} vs {}", lines[1]);
    }

    fn defined(path: &str, text: &str) -> Vec<(String, u32)> {
        Highlighter::new().parse(path, Kind::Code, text).definitions
    }

    fn named(pairs: &[(&str, u32)]) -> Vec<(String, u32)> {
        pairs.iter().map(|(n, l)| (n.to_string(), *l)).collect()
    }

    #[test_id(tlf7rza)]
    #[test]
    fn rust_definitions_include_every_item_kind_and_constants() {
        let text = "mod m {}\nstruct S;\nenum E { A }\ntrait T {}\ntype Al = u8;\n\
                    macro_rules! mac { () => {} }\nfn f() {}\nconst MAX: u32 = 1;\n\
                    static mut COUNT: u32 = 0;\npub const fn g() {}\n";
        assert_eq!(
            defined("a.rs", text),
            named(&[
                ("m", 1),
                ("S", 2),
                ("E", 3),
                ("T", 4),
                ("Al", 5),
                ("mac", 6),
                ("f", 7),
                ("MAX", 8),
                ("COUNT", 9),
                ("g", 10),
            ])
        );
    }

    #[test_id(t5is4s6)]
    #[test]
    fn an_impl_counts_only_where_nothing_else_defines_the_name() {
        let with_type = "struct S;\nimpl S { fn a(&self) {} }\n";
        assert_eq!(defined("a.rs", with_type), named(&[("S", 1), ("a", 2)]));
        let alone = "impl S { fn a(&self) {} }\nimpl S {}\nimpl std::fmt::Display for Q {}\n";
        assert_eq!(
            defined("a.rs", alone),
            named(&[("S", 1), ("a", 1), ("S", 2), ("Q", 3)])
        );
    }

    #[test_id(tosigyx)]
    #[test]
    fn python_defs_and_classes_are_definitions() {
        let text = "class Box:\n    def open(self):\n        pass\n\ndef main():\n    x = 1\n";
        assert_eq!(
            defined("a.py", text),
            named(&[("Box", 1), ("open", 2), ("main", 5)])
        );
    }

    #[test_id(tgmxcud)]
    #[test]
    fn prose_and_unknown_languages_define_nothing() {
        assert!(
            Highlighter::new()
                .parse("a.md", Kind::Prose, "fn f() {}\n")
                .definitions
                .is_empty()
        );
    }
}
