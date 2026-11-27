//! Per-line syntax highlighting with classed spans, so colours live in the stylesheet.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use margin_core::highlight::Segment;
use syntect::html::{ClassStyle, line_tokens_to_classed_spans};
use syntect::parsing::{ParseState, Scope, ScopeStack, ScopeStackOp, SyntaxSet};
use syntect::util::LinesWithEndings;

use crate::store::Kind;

/// Highlighted lines kept in memory across documents; enough for the largest collection so far.
const CACHE_LINES: usize = 60_000;

/// (document id, version row id).
type Key = (String, i64);

/// Parsed documents, bounded by their total lines; the least recently used goes first. A
/// document longer than the bound is kept, alone.
#[derive(Default)]
struct Cache {
    entries: HashMap<Key, Entry>,
    lines: usize,
    /// Counts uses, to order them.
    uses: u64,
}

struct Entry {
    parsed: Arc<Parsed>,
    used: u64,
}

impl Entry {
    /// What the entry counts against the bound: at least 1, so empty documents are bounded too.
    fn cost(&self) -> usize {
        self.parsed.lines.len().max(1)
    }
}

impl Cache {
    fn get(&mut self, key: &Key) -> Option<Arc<Parsed>> {
        let entry = self.entries.get_mut(key)?;
        self.uses += 1;
        entry.used = self.uses;
        Some(entry.parsed.clone())
    }

    fn insert(&mut self, key: Key, parsed: Arc<Parsed>) {
        // Two requests can miss on one key and both insert it.
        self.remove(&key);
        self.uses += 1;
        let entry = Entry {
            parsed,
            used: self.uses,
        };
        while self.lines + entry.cost() > CACHE_LINES {
            let oldest = self.entries.iter().min_by_key(|(_, e)| e.used);
            let Some(oldest) = oldest.map(|(k, _)| k.clone()) else {
                break;
            };
            self.remove(&oldest);
        }
        self.lines += entry.cost();
        self.entries.insert(key, entry);
    }

    fn remove(&mut self, key: &Key) {
        if let Some(entry) = self.entries.remove(key) {
            self.lines -= entry.cost();
        }
    }
}

/// What one parse of a document yields: its lines as HTML, and what it defines.
pub struct Parsed {
    pub lines: Vec<String>,
    /// Each definition the syntax marks: functions, types, modules, macros, constants. An `impl`
    /// counts only where nothing else in the document defines its name.
    pub definitions: Vec<Definition>,
}

#[derive(Clone)]
pub struct Definition {
    pub name: String,
    pub line: u32,
    /// Whether the name is an `impl`'s type rather than a definition of it.
    pub imp: bool,
    /// For a name directly in a Rust `impl` body, the type that `impl` is for.
    pub owner: Option<String>,
}

/// A name found on a line, before the `impl`s that name already defined are dropped.
type Found = (String, u32, bool, Option<String>);

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
    cache: Mutex<Cache>,
    parses: AtomicU64,
    definition_scopes: Vec<(Scope, bool)>,
    storage_type: Scope,
    meta_impl: Scope,
    meta_block: Scope,
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
            cache: Mutex::default(),
            parses: AtomicU64::new(0),
            definition_scopes: DEFINITION_SCOPES
                .iter()
                .map(|(s, imp)| (Scope::new(s).expect("a valid scope"), *imp))
                .collect(),
            storage_type: Scope::new("storage.type").expect("a valid scope"),
            meta_impl: Scope::new("meta.impl").expect("a valid scope"),
            meta_block: Scope::new("meta.block").expect("a valid scope"),
        }
    }

    /// One HTML fragment per line of `text`, each balanced (spans open at a line's end are
    /// closed there and reopened on the next line), so a line is a self-contained element.
    pub fn lines(&self, path: &str, kind: Kind, text: &str) -> Vec<String> {
        self.parse(path, kind, text).lines
    }

    /// How many documents have been parsed, cache hits excluded.
    pub fn parses(&self) -> u64 {
        self.parses.load(Ordering::Relaxed)
    }

    /// [`Highlighter::lines`] and the document's definitions, from the one parse.
    pub fn parse(&self, path: &str, kind: Kind, text: &str) -> Parsed {
        self.parses.fetch_add(1, Ordering::Relaxed);
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
        let mut found: Vec<Found> = Vec::new();
        let mut owner = None;
        let mut out = Vec::with_capacity(split.len());
        for (i, line) in LinesWithEndings::from(&crate::highlight::normalise(text, &split))
            .take(split.len())
            .enumerate()
        {
            let rendered = (|| {
                let ops = parse.parse_line(line, &self.syntaxes).ok()?;
                self.scan_definitions(
                    line,
                    &ops,
                    &mut scan,
                    (i + 1) as u32,
                    &mut owner,
                    &mut found,
                );
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
            .filter(|(_, _, imp, _)| !imp)
            .map(|(n, ..)| n.as_str())
            .collect();
        let definitions = found
            .iter()
            .filter(|(n, _, imp, _)| !imp || !named.contains(n.as_str()))
            .map(|(name, line, imp, owner)| Definition {
                name: name.clone(),
                line: *line,
                imp: *imp,
                owner: owner.clone(),
            })
            .collect();
        Parsed {
            lines: out,
            definitions,
        }
    }

    /// Add to `found` the names defined on `line`, from the scopes of its tokens. `owner` is the
    /// type of the last `impl` header seen. A Rust constant is not marked as a definition: `const` or `static` is
    /// `storage.type`, and the name follows it, then `:`.
    fn scan_definitions(
        &self,
        line: &str,
        ops: &[(usize, ScopeStackOp)],
        scan: &mut ScopeStack,
        n: u32,
        owner: &mut Option<String>,
        found: &mut Vec<Found>,
    ) {
        let mut run: Option<(String, bool, bool)> = None;
        let mut constant_at = None;
        let mut flush = |run: &mut Option<(String, bool, bool)>, owner: &mut Option<String>| {
            if let Some((name, imp, direct)) = run.take() {
                let name = name.trim();
                if is_identifier(name) {
                    let of = if direct { owner.clone() } else { None };
                    if imp {
                        *owner = Some(name.to_string());
                    }
                    found.push((name.to_string(), n, imp, of));
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
                    (Some(imp), Some((name, was, _))) if *was == imp => name.push_str(text),
                    (Some(imp), _) => {
                        flush(&mut run, owner);
                        run = Some((text.to_string(), imp, self.directly_in_impl(scan)));
                    }
                    (None, _) => flush(&mut run, owner),
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
        flush(&mut run, owner);
        if let Some(from) = constant_at {
            let rest = line[from..].trim_start();
            let rest = rest.strip_prefix("mut ").map_or(rest, str::trim_start);
            let name_len = rest
                .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            let (name, after) = rest.split_at(name_len);
            if is_identifier(name) && after.trim_start().starts_with(':') {
                let of = self.directly_in_impl(scan).then(|| owner.clone()).flatten();
                found.push((name.to_string(), n, false, of));
            }
        }
    }

    /// Whether the scopes are inside an `impl` body and no deeper than its one block.
    fn directly_in_impl(&self, scan: &ScopeStack) -> bool {
        let scopes = scan.as_slice();
        let Some(from) = scopes.iter().position(|s| self.meta_impl.is_prefix_of(*s)) else {
            return false;
        };
        scopes[from..]
            .iter()
            .filter(|s| self.meta_block.is_prefix_of(**s))
            .count()
            == 1
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
    /// [`Highlighter::parse`], remembered per version row so a reload costs nothing.
    pub fn parsed_cached(
        &self,
        doc_id: &str,
        version: i64,
        path: &str,
        kind: Kind,
        text: &str,
    ) -> Arc<Parsed> {
        let key = (doc_id.to_string(), version);
        if let Some(hit) = self.cache.lock().ok().and_then(|mut c| c.get(&key)) {
            return hit;
        }
        let lines = Arc::new(self.parse(path, kind, text));
        if let Ok(mut c) = self.cache.lock() {
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
        Highlighter::new()
            .parse(path, Kind::Code, text)
            .definitions
            .into_iter()
            .map(|d| (d.name, d.line))
            .collect()
    }

    /// Asks the cache for prose document `id` of `lines` lines; whether that took a parse.
    fn parses_again(h: &Highlighter, id: &str, lines: usize) -> bool {
        let before = h.parses();
        h.parsed_cached(id, 1, "a.md", Kind::Prose, &"x\n".repeat(lines));
        h.parses() > before
    }

    #[test_id(tpvewpz)]
    #[test]
    fn a_collection_the_size_of_review_002_stays_whole_in_the_cache() {
        let h = Highlighter::new();
        // 65 documents, 38,247 lines.
        let sizes: Vec<usize> = (0..65).map(|d| 588 + usize::from(d < 27)).collect();
        assert_eq!(sizes.iter().sum::<usize>(), 38_247);
        for pass in 0..3 {
            for (d, lines) in sizes.iter().enumerate() {
                let parsed = parses_again(&h, &d.to_string(), *lines);
                assert_eq!(parsed, pass == 0, "document {d}, pass {pass}");
            }
        }
    }

    #[test_id(tmd5u4p)]
    #[test]
    fn a_full_cache_evicts_the_least_recently_used_document() {
        let h = Highlighter::new();
        let half = CACHE_LINES / 2;
        assert!(parses_again(&h, "a", half));
        assert!(parses_again(&h, "b", half));
        assert!(!parses_again(&h, "a", half), "both fit at the bound");
        assert!(parses_again(&h, "c", 1));
        assert!(!parses_again(&h, "a", half), "used since b was");
        assert!(!parses_again(&h, "c", 1));
        assert!(parses_again(&h, "b", half));
    }

    #[test_id(t265fku)]
    #[test]
    fn a_document_longer_than_the_bound_is_cached_alone() {
        let h = Highlighter::new();
        assert!(parses_again(&h, "small", 1));
        assert!(parses_again(&h, "big", CACHE_LINES + 1));
        assert!(!parses_again(&h, "big", CACHE_LINES + 1));
        assert!(parses_again(&h, "small", 1));
        assert!(parses_again(&h, "big", CACHE_LINES + 1));
    }

    #[test_id(tvp2pfx)]
    #[test]
    fn inserting_a_cached_key_again_counts_its_lines_once() {
        let mut cache = Cache::default();
        let parsed = Arc::new(Highlighter::new().parse("a.md", Kind::Prose, "x\ny\n"));
        cache.insert(("a".to_string(), 1), parsed.clone());
        cache.insert(("a".to_string(), 1), parsed);
        assert_eq!(cache.lines, 2);
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

    #[test_id(tfrv0me)]
    #[test]
    fn a_name_directly_in_an_impl_body_records_the_impls_type() {
        let text = "impl<T> Wrap<T> {\n    fn get(&self) {\n        fn inner() {}\n    }\n}\n\
                    impl fmt::Display for Wrap<u8> {\n    fn fmt(&self) {}\n}\nfn free() {}\n";
        let owners: Vec<_> = Highlighter::new()
            .parse("a.rs", Kind::Code, text)
            .definitions
            .into_iter()
            .map(|d| (d.name, d.owner))
            .collect();
        let some = |n: &str, o: Option<&str>| (n.to_string(), o.map(str::to_string));
        assert_eq!(
            owners,
            [
                some("Wrap", None),
                some("get", Some("Wrap")),
                some("inner", None),
                some("Wrap", None),
                some("fmt", Some("Wrap")),
                some("free", None),
            ]
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
