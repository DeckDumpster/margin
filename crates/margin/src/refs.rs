//! What a code span in a cover note or comment names: a document, a line or definition in one, or
//! a file of the source repository. A span that names none of them stays plain.

use std::collections::HashMap;

use crate::highlight::{Definition, Highlighter, is_identifier};
use crate::store::{Collection, Source};

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

impl Refs {
    /// Parses each document (cached) for its definitions.
    pub fn new(c: &Collection, highlighter: &Highlighter) -> Refs {
        let mut defined: HashMap<String, Vec<(usize, u32)>> = HashMap::new();
        let mut types: HashMap<String, Places> = HashMap::new();
        let mut members: HashMap<(String, String), Vec<(usize, u32)>> = HashMap::new();
        let docs = c
            .documents
            .iter()
            .enumerate()
            .map(|(i, d)| {
                let parsed =
                    highlighter.parsed_cached(&d.id, d.version_id, &d.path, d.kind, &d.text);
                for d in &parsed.definitions {
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
                    lines: parsed.lines.len() as u32,
                    definitions: parsed.definitions.clone(),
                }
            })
            .collect();
        Refs {
            docs,
            source: c.source.clone(),
            defined,
            types,
            members,
        }
    }

    /// No documents, no source: nothing resolves.
    pub fn none() -> Refs {
        Refs {
            docs: Vec::new(),
            source: None,
            defined: HashMap::new(),
            types: HashMap::new(),
            members: HashMap::new(),
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
