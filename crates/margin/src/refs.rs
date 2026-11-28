//! What a code span in a cover note or comment names: a document, a line or definition in one, or
//! a file of the source repository. A span that names none of them stays plain.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::num::NonZero;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;

use crate::highlight::{Definition, Highlighter, Parsed, is_identifier};
use crate::store::{Collection, Document, Source};

struct DocRef {
    /// The `N` of its section, `#doc-N`.
    index: usize,
    path: String,
    lines: u32,
    definitions: Vec<Definition>,
}

pub struct Refs {
    docs: Vec<DocRef>,
    source: Option<Source>,
    /// Every definition across the documents, by name: `(document index, line)`.
    defined: HashMap<String, Vec<(usize, u32)>>,
    /// Each type's definitions, and the `impl`s of it that no document's definition of it
    /// accompanies, by name.
    types: HashMap<String, Places>,
    /// Each name defined directly in an `impl` body, by `(type, name)`.
    members: HashMap<(String, String), Vec<(usize, u32)>>,
    /// What [`Refs::new`] had to parse for: `(version row id, definitions)`.
    recorded: Vec<(i64, Vec<Definition>)>,
}

#[derive(Default)]
struct Places {
    defined: Vec<(usize, u32)>,
    impls: Vec<(usize, u32)>,
}

/// A line, or a first and last line, after a document's path.
enum Place {
    Line(u32),
    Range(u32, u32),
}

impl Place {
    fn parse(s: &str) -> Option<Place> {
        let number = |s: &str| {
            (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
                .then(|| s.parse::<u32>().ok())
                .flatten()
        };
        match s.split_once('-') {
            Some((a, b)) => Some(Place::Range(number(a)?, number(b)?)),
            None => Some(Place::Line(number(s)?)),
        }
    }

    fn first(&self) -> u32 {
        match self {
            Place::Line(n) | Place::Range(n, _) => *n,
        }
    }

    fn last(&self) -> u32 {
        match self {
            Place::Line(n) | Place::Range(_, n) => *n,
        }
    }
}

/// A parse of each document whose definitions are not recorded, `None` for the others, in
/// document order. Those not cached are parsed on one thread per core, the longest first so
/// none of them starts last; when there are none to parse, no thread is started.
fn parse_unrecorded(documents: &[Document], highlighter: &Highlighter) -> Vec<Option<Arc<Parsed>>> {
    let parsed: Vec<OnceLock<Arc<Parsed>>> = documents
        .iter()
        .map(|d| {
            let hit = d
                .definitions
                .is_none()
                .then(|| highlighter.cached(&d.id, d.version_id, &d.text));
            hit.flatten().map(OnceLock::from).unwrap_or_default()
        })
        .collect();
    parse_missing(documents, &parsed, highlighter);
    parsed.into_iter().map(OnceLock::into_inner).collect()
}

fn parse_missing(
    documents: &[Document],
    parsed: &[OnceLock<Arc<Parsed>>],
    highlighter: &Highlighter,
) {
    let mut missing: Vec<usize> = (0..documents.len())
        .filter(|&i| documents[i].definitions.is_none() && parsed[i].get().is_none())
        .collect();
    if missing.is_empty() {
        return;
    }
    missing.sort_by_cached_key(|&i| Reverse(documents[i].text.lines().count()));
    let next = AtomicUsize::new(0);
    let work = || {
        while let Some(&i) = missing.get(next.fetch_add(1, Ordering::Relaxed)) {
            let d = &documents[i];
            parsed[i].get_or_init(|| {
                highlighter.parsed_cached(&d.id, d.version_id, &d.path, d.kind, &d.text)
            });
        }
    };
    let cores = thread::available_parallelism().map_or(1, NonZero::get);
    thread::scope(|scope| {
        for _ in 0..cores.min(missing.len()) {
            scope.spawn(work);
        }
    });
}

impl Refs {
    /// Takes each document's definitions from the store, and parses (cached) only the ones
    /// not yet recorded there, in parallel.
    pub fn new(c: &Collection, highlighter: &Highlighter) -> Refs {
        let mut defined: HashMap<String, Vec<(usize, u32)>> = HashMap::new();
        let mut types: HashMap<String, Places> = HashMap::new();
        let mut members: HashMap<(String, String), Vec<(usize, u32)>> = HashMap::new();
        let mut recorded = Vec::new();
        // In document order, so each name's places, and `recorded`, are too.
        let docs = c
            .documents
            .iter()
            .zip(parse_unrecorded(&c.documents, highlighter))
            .enumerate()
            .map(|(i, (d, parsed))| {
                let (lines, definitions) = match &d.definitions {
                    Some(stored) => (margin_core::lines::split(&d.text).len(), stored.clone()),
                    None => {
                        let parsed = parsed.expect("every unrecorded document is parsed");
                        recorded.push((d.version_id, parsed.definitions.clone()));
                        (parsed.lines.len(), parsed.definitions.clone())
                    }
                };
                for d in &definitions {
                    let at = (i + 1, d.line);
                    defined.entry(d.name.clone()).or_default().push(at);
                    let places = types.entry(d.name.clone()).or_default();
                    if d.imp {
                        places.impls.push(at);
                    } else {
                        places.defined.push(at);
                    }
                    if let Some(owner) = &d.owner {
                        members
                            .entry((owner.clone(), d.name.clone()))
                            .or_default()
                            .push(at);
                    }
                }
                DocRef {
                    index: i + 1,
                    path: d.path.clone(),
                    lines: lines as u32,
                    definitions,
                }
            })
            .collect();
        Refs {
            docs,
            source: c.source.clone(),
            defined,
            types,
            members,
            recorded,
        }
    }

    /// The definitions this had to parse for, which the store does not hold yet.
    pub fn recorded(&self) -> &[(i64, Vec<Definition>)] {
        &self.recorded
    }

    /// No documents, no source: nothing resolves.
    pub fn none() -> Refs {
        Refs {
            docs: Vec::new(),
            source: None,
            defined: HashMap::new(),
            types: HashMap::new(),
            members: HashMap::new(),
            recorded: Vec::new(),
        }
    }

    /// The `href` a code span links to, if it names anything. A trailing `()` is ignored.
    pub fn resolve(&self, span: &str) -> Option<String> {
        let span = span.trim();
        let span = span.strip_suffix("()").unwrap_or(span);
        if span.is_empty() {
            return None;
        }
        self.in_document(span)
            .or_else(|| self.definition(span))
            .or_else(|| self.member(span))
            .or_else(|| self.in_source(span))
            .or_else(|| self.commit(span))
    }

    fn section(doc: &DocRef) -> String {
        format!("#doc-{}", doc.index)
    }

    fn row(doc_index: usize, line: u32) -> String {
        format!("#doc-{doc_index}-{line}")
    }

    /// The document `name` is the path of, or the basename of when only one has it.
    fn document(&self, name: &str) -> Option<&DocRef> {
        if let Some(d) = self.docs.iter().find(|d| d.path == name) {
            return Some(d);
        }
        if name.contains('/') {
            return None;
        }
        let mut same = self
            .docs
            .iter()
            .filter(|d| d.path.rsplit('/').next() == Some(name));
        match (same.next(), same.next()) {
            (Some(d), None) => Some(d),
            _ => None,
        }
    }

    fn in_document(&self, span: &str) -> Option<String> {
        if let Some((head, name)) = span.split_once("::") {
            let doc = self.document(head)?;
            if !is_identifier(name) {
                return None;
            }
            let mut found = doc.definitions.iter().filter(|d| d.name == name);
            return Some(match (found.next(), found.next()) {
                (Some(d), None) => Self::row(doc.index, d.line),
                _ => Self::section(doc),
            });
        }
        if let Some((head, place)) = span.split_once(':') {
            let doc = self.document(head)?;
            let place = Place::parse(place)?;
            let (first, last) = (place.first(), place.last());
            return Some(if 1 <= first && first <= last && last <= doc.lines {
                Self::row(doc.index, first)
            } else {
                Self::section(doc)
            });
        }
        self.document(span).map(Self::section)
    }

    /// A name defined once across the documents.
    fn definition(&self, span: &str) -> Option<String> {
        match self.defined.get(span)?.as_slice() {
            [(doc, line)] => Some(Self::row(*doc, *line)),
            _ => None,
        }
    }

    /// `Type::member`: the member when the type's `impl`s define it once, else the type, when
    /// the type is defined once (or, with no definition, has one `impl`).
    fn member(&self, span: &str) -> Option<String> {
        let (ty, name) = span.split_once("::")?;
        if !is_identifier(ty) || !is_identifier(name) {
            return None;
        }
        let places = self.types.get(ty)?;
        let list = if places.defined.is_empty() {
            &places.impls
        } else {
            &places.defined
        };
        let [(doc, line)] = list.as_slice() else {
            return None;
        };
        let at = match self
            .members
            .get(&(ty.to_string(), name.to_string()))
            .map(Vec::as_slice)
        {
            Some([member]) => *member,
            _ => (*doc, *line),
        };
        Some(Self::row(at.0, at.1))
    }

    /// A commit of the source repository: 7 to 40 hexadecimal digits.
    fn commit(&self, span: &str) -> Option<String> {
        let source = self.source.as_ref()?;
        let hex = (7..=40).contains(&span.len())
            && span.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        hex.then(|| format!("{}/commit/{span}", source.repository))
    }

    /// A file of the source repository, at the source commit.
    fn in_source(&self, span: &str) -> Option<String> {
        let source = self.source.as_ref()?;
        let (path, place) = match span.split_once("::") {
            Some((path, _name)) => (path, None),
            None => match span.split_once(':') {
                Some((path, place)) => (path, Some(Place::parse(place)?)),
                None => (span, None),
            },
        };
        let file = path.rsplit('/').next()?;
        let plain = path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._/+-".contains(&b));
        let (stem, extension) = file.rsplit_once('.')?;
        let relative = !path.starts_with('/')
            && path.contains('/')
            && path
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != "..");
        if !(plain && relative && !stem.is_empty() && !extension.is_empty()) {
            return None;
        }
        let at = match place {
            None => String::new(),
            Some(Place::Line(n)) => format!("#L{n}"),
            Some(Place::Range(a, b)) => format!("#L{a}-L{b}"),
        };
        Some(format!(
            "{}/blob/{}/{path}{at}",
            source.repository, source.commit
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Document, Kind, State};
    use margin_test_id::test_id;

    /// Documents `src/{name}.rs`, in this order.
    fn collection(documents: &[(&str, &str)]) -> Collection {
        Collection {
            id: "review".to_string(),
            number: 1,
            title: String::new(),
            cover_note: String::new(),
            state: State::Open,
            created_by: String::new(),
            created_at: 0,
            updated_at: 0,
            source: None,
            documents: documents
                .iter()
                .zip(1..)
                .map(|((name, text), version_id)| Document {
                    id: name.to_string(),
                    version_id,
                    path: format!("src/{name}.rs"),
                    kind: Kind::Code,
                    version: 1,
                    text: text.to_string(),
                    previous_text: None,
                    definitions: None,
                })
                .collect(),
        }
    }

    #[test_id(ttw5q3v)]
    #[test]
    fn names_resolve_in_document_order_whether_parsed_cached_or_recorded() {
        // Each document is longer than the one before, so size order is not document order.
        let mut c = collection(&[
            ("a", "pub struct Widget;\nfn shared() {}\n"),
            ("b", "fn only_here() {}\n\nfn shared() {}\n"),
            ("c", "impl Widget {\n    pub fn spin(&self) {}\n}\n\n"),
        ]);
        let check = |c: &Collection, highlighter: &Highlighter, to_record: &[i64], pass: &str| {
            let refs = Refs::new(c, highlighter);
            assert_eq!(refs.defined["shared"], [(1, 2), (2, 3)], "{pass}");
            for (span, href) in [
                ("shared", None),
                ("b.rs::shared", Some("#doc-2-3")),
                ("only_here", Some("#doc-2-1")),
                // The `impl` apart from its type is a second place for the bare name only.
                ("Widget", None),
                ("Widget::spin", Some("#doc-3-2")),
                ("Widget::absent", Some("#doc-1-1")),
                ("c.rs:4", Some("#doc-3-4")),
                ("a.rs:4", Some("#doc-1")),
            ] {
                assert_eq!(refs.resolve(span).as_deref(), href, "{span}, {pass}");
            }
            let ids: Vec<i64> = refs.recorded().iter().map(|(id, _)| *id).collect();
            assert_eq!(ids, to_record, "{pass}");
            refs
        };
        let highlighter = Highlighter::new();
        let b = &c.documents[1];
        highlighter.parsed_cached(&b.id, b.version_id, &b.path, b.kind, &b.text);
        check(&c, &highlighter, &[1, 2, 3], "partly cached");
        let refs = check(&c, &highlighter, &[1, 2, 3], "all cached");
        assert_eq!(highlighter.parses(), 3);
        // The first document's definitions come from the store, the others from a parse.
        c.documents[0].definitions = Some(refs.recorded()[0].1.clone());
        let restarted = Highlighter::new();
        check(&c, &restarted, &[2, 3], "partly recorded");
        assert_eq!(restarted.parses(), 2);
    }
}
