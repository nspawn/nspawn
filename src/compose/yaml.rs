//! YAML read as compose-go reads it with go.yaml.in/yaml/v4: the document as a graph of
//! nodes (aliases point at their anchor), the `!reset` and `!override` tags taken out
//! and remembered by path, then the values built with go-yaml's rules: plain scalars
//! resolved by the YAML 1.2 core schema with Go's integer syntax (`0755` is octal),
//! timestamps, merge keys (`<<`), and keys kept unique.

use std::collections::HashMap;
use std::fmt;

use super::construct::{Constructor, Go, GoMap, Stop};
use super::libyaml::{Event, EventKind, LoadError, Mark, Parser, ScalarStyle, Stage};

use super::path::Path;
use super::value::{Map, Value};

pub type NodeId = usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    Plain,
    SingleQuoted,
    DoubleQuoted,
    Literal,
    Folded,
}

#[derive(Debug, Clone)]
pub enum Kind {
    Scalar(String, Style),
    Sequence(Vec<NodeId>),
    Mapping(Vec<(NodeId, NodeId)>),
    Alias(NodeId),
}

#[derive(Debug, Clone)]
pub struct Node {
    pub kind: Kind,
    /// The tag as go-yaml shortens it (`!!str`, `!reset`), empty without one.
    pub tag: String,
    /// The anchor's name, for an alias.
    pub name: String,
    pub line: usize,
    pub column: usize,
    /// How deep its value goes, its aliases followed: what building it takes of the
    /// stack.
    pub height: usize,
}

#[derive(Debug, Clone)]
pub struct Document {
    pub nodes: Vec<Node>,
    pub root: NodeId,
}

/// An error with go-yaml's wording, `yaml: line 3: ...`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

fn error(text: impl Into<String>) -> Error {
    Error(text.into())
}

/// The documents of a YAML stream, each read when asked for as go-yaml's Decoder
/// does, so that an error in a later document comes after what the earlier ones did.
/// The nodes stay from one document to the next, as go-yaml's do: an anchor is known
/// in the documents after its own, with what the reset pass of its document left of it.
pub struct Documents<'a> {
    parser: Parser<'a>,
    doc: Document,
    anchors: HashMap<String, NodeId>,
    started: bool,
}

impl<'a> Documents<'a> {
    pub fn new(input: &'a [u8]) -> Self {
        Documents {
            parser: Parser::new(input),
            doc: Document {
                nodes: Vec::new(),
                root: 0,
            },
            anchors: HashMap::new(),
            started: false,
        }
    }

    fn next_event(&mut self) -> Result<Event, LoadError> {
        Ok(self.parser.next_event()?.unwrap_or_else(|| Event {
            kind: EventKind::StreamEnd,
            start: Mark::default(),
            end: Mark::default(),
        }))
    }

    /// The next document, None at the end of the stream.
    pub fn next_document(&mut self) -> Result<Option<&mut Document>, LoadError> {
        if !self.started {
            self.started = true;
            self.next_event()?;
        }
        let event = self.next_event()?;
        if !matches!(event.kind, EventKind::DocumentStart { .. }) {
            return Ok(None);
        }
        // Open containers, with the key of a mapping waiting for its value.
        let mut stack: Vec<(NodeId, Option<NodeId>)> = Vec::new();
        let mut root = None;
        loop {
            let event = self.next_event()?;
            let (line, column) = (event.start.line, event.start.column);
            let new = |kind: Kind, tag: &str, name: String| {
                let mut node = Node {
                    kind,
                    tag: short_tag(tag),
                    name,
                    line,
                    column,
                    height: 1,
                };
                // go-yaml's Resolver stage: every node but an alias gets its tag.
                node.tag = super::construct::resolver_tag(&node);
                node
            };
            let (node, anchor, container) = match event.kind {
                EventKind::DocumentEnd { .. } => break,
                EventKind::SequenceEnd | EventKind::MappingEnd => {
                    if let Some((id, _)) = stack.pop() {
                        let nodes = &self.doc.nodes;
                        let below = match &nodes[id].kind {
                            Kind::Sequence(items) => items.iter().map(|c| nodes[*c].height).max(),
                            Kind::Mapping(pairs) => pairs
                                .iter()
                                .map(|(k, v)| nodes[*k].height.max(nodes[*v].height))
                                .max(),
                            _ => None,
                        };
                        self.doc.nodes[id].height = 1 + below.unwrap_or(0);
                    }
                    continue;
                }
                EventKind::StreamStart | EventKind::StreamEnd | EventKind::DocumentStart { .. } => {
                    continue
                }
                EventKind::Alias { anchor } => {
                    let Some(&target) = self.anchors.get(&anchor) else {
                        return Err(LoadError::new(
                            Stage::Composer,
                            format!("unknown anchor '{anchor}' referenced"),
                            Mark {
                                index: 0,
                                line,
                                column,
                            },
                        ));
                    };
                    let mut alias = new(Kind::Alias(target), "", anchor);
                    alias.height = self.doc.nodes[target].height;
                    (alias, None, false)
                }
                EventKind::Scalar {
                    anchor,
                    tag,
                    value,
                    style,
                    ..
                } => {
                    let style = match style {
                        ScalarStyle::Plain => Style::Plain,
                        ScalarStyle::SingleQuoted => Style::SingleQuoted,
                        ScalarStyle::DoubleQuoted => Style::DoubleQuoted,
                        ScalarStyle::Literal => Style::Literal,
                        ScalarStyle::Folded => Style::Folded,
                    };
                    (
                        new(Kind::Scalar(value, style), &tag, String::new()),
                        anchor,
                        false,
                    )
                }
                EventKind::SequenceStart { anchor, tag, .. } => (
                    new(Kind::Sequence(Vec::new()), &tag, String::new()),
                    anchor,
                    true,
                ),
                EventKind::MappingStart { anchor, tag, .. } => (
                    new(Kind::Mapping(Vec::new()), &tag, String::new()),
                    anchor,
                    true,
                ),
            };
            let nodes = &mut self.doc.nodes;
            let id = nodes.len();
            nodes.push(node);
            if let Some(anchor) = anchor {
                self.anchors.insert(anchor, id);
            }
            match stack.last_mut() {
                None => root = Some(id),
                Some((parent, key)) => match &mut nodes[*parent].kind {
                    Kind::Sequence(items) => items.push(id),
                    Kind::Mapping(pairs) => match key.take() {
                        None => *key = Some(id),
                        Some(k) => pairs.push((k, id)),
                    },
                    _ => unreachable!("only containers are open"),
                },
            }
            if container {
                stack.push((id, None));
            }
        }
        self.doc.root = root.expect("a document holds a node");
        Ok(Some(&mut self.doc))
    }
}

/// Every document of a stream at once.
#[cfg(test)]
pub fn parse(text: &str) -> Result<Vec<Document>, Error> {
    let mut documents = Documents::new(text.as_bytes());
    let mut all = Vec::new();
    while let Some(doc) = documents
        .next_document()
        .map_err(|e| error(e.to_string()))?
    {
        all.push(Document {
            nodes: doc.nodes.clone(),
            root: doc.root,
        });
    }
    Ok(all)
}

/// go-yaml's shortTag, and the non-specific `!` kept.
fn short_tag(tag: &str) -> String {
    match tag.strip_prefix("tag:yaml.org,2002:") {
        Some(rest) => format!("!!{rest}"),
        None => tag.to_string(),
    }
}

/// The per-document cap on the nodes the reset pass visits, compose-go's.
pub const MAX_NODE_VISITS: usize = 100_000;
/// The cap on the values one document builds, aliases followed: far above what a
/// compose file holds, and what keeps an alias bomb from filling memory; go-yaml
/// itself sets no limit where compose-go decodes.
const MAX_VALUES: usize = 5_000_000;

/// The cap on the text of the `!reset` and `!override` paths one document records,
/// aliases followed. compose-go sets none and runs out of memory on a file whose nested
/// aliases multiply long paths; the count of its node visits does not see them.
pub const MAX_RESET_TEXT: usize = 256 << 20;
/// What a recorded path takes besides its text, counted against MAX_RESET_TEXT so that
/// many short paths are held to it as well.
const PATH_COST: usize = 64;

/// The root of a document whose root the reset pass took out.
const NO_ROOT: NodeId = NodeId::MAX;

/// What the reset pass found: the paths of `!reset` and `!override` nodes, which the
/// merge applies to the files before this one.
#[derive(Debug, Clone, Default)]
pub struct Resets {
    pub paths: Vec<Path>,
}

/// compose-go's ResetProcessor: takes `!reset` nodes out of the document and records
/// the path of every `!reset` and `!override` node, an alias counting at the path it
/// stands at.
pub fn take_resets(doc: &mut Document) -> Result<Resets, Error> {
    take_resets_within(doc, MAX_RESET_TEXT)
}

/// `take_resets` with `cap` in place of MAX_RESET_TEXT.
fn take_resets_within(doc: &mut Document, cap: usize) -> Result<Resets, Error> {
    let mut pass = ResetPass {
        doc,
        paths: Vec::new(),
        visited: HashMap::new(),
        resolved: HashMap::new(),
        visits: 0,
        text: 0,
        cap,
    };
    let root = pass.doc.root;
    pass.doc.root = pass.resolve(root, Path::root())?.unwrap_or(NO_ROOT);
    let mut paths = pass.paths;
    // Earlier files hold an attribute under its own name once their extensions are
    // promoted.
    for i in 0..paths.len() {
        let resolved = super::aliases::resolve_alias_path(&paths[i]);
        if resolved != paths[i] {
            paths.push(resolved);
        }
    }
    Ok(Resets { paths })
}

struct ResetPass<'a> {
    doc: &'a mut Document,
    paths: Vec<Path>,
    visited: HashMap<NodeId, Vec<Path>>,
    resolved: HashMap<NodeId, (NodeId, Vec<Path>)>,
    visits: usize,
    /// The bytes of the paths recorded so far, and how many they may come to.
    text: usize,
    cap: usize,
}

impl ResetPass<'_> {
    fn record(&mut self, path: Path) -> Result<(), Error> {
        self.spend(path.raw().len() + PATH_COST)?;
        self.paths.push(path);
        Ok(())
    }

    fn spend(&mut self, bytes: usize) -> Result<(), Error> {
        self.text = self.text.saturating_add(bytes);
        if self.text > self.cap {
            return Err(error(format!(
                "the !reset and !override paths of this file come to more than {} MiB once its aliases are followed",
                self.cap >> 20
            )));
        }
        Ok(())
    }

    fn resolve(&mut self, id: NodeId, path: Path) -> Result<Option<NodeId>, Error> {
        self.visits += 1;
        if self.visits > MAX_NODE_VISITS {
            return Err(error(format!(
                "compose file exceeds maximum node visit limit ({MAX_NODE_VISITS})"
            )));
        }
        let path = path.without_merge_key();
        match self.doc.nodes[id].tag.as_str() {
            "!reset" => {
                self.record(path)?;
                return Ok(None);
            }
            "!override" => {
                self.record(path)?;
                return Ok(Some(id));
            }
            _ => {}
        }
        match self.doc.nodes[id].kind {
            Kind::Alias(target) => {
                self.check_for_cycle(target, &path)?;
                match self.doc.nodes[target].tag.as_str() {
                    "!reset" => {
                        self.record(path)?;
                        Ok(None)
                    }
                    "!override" => {
                        self.record(path)?;
                        Ok(Some(target))
                    }
                    _ => self.cached(target, path).map(Some),
                }
            }
            Kind::Sequence(_) | Kind::Mapping(_) => self.cached(id, path).map(Some),
            Kind::Scalar(..) => Ok(Some(id)),
        }
    }

    fn cached(&mut self, id: NodeId, path: Path) -> Result<NodeId, Error> {
        // A node reached again replays the paths it recorded, which compose-go counts as
        // no visits; their text is counted before any is made.
        let hit = self.resolved.get(&id).map(|(node, relative)| {
            let bytes: usize = relative
                .iter()
                .map(|rel| path.raw().len() + 1 + rel.raw().len() + PATH_COST)
                .sum();
            (*node, bytes)
        });
        if let Some((node, bytes)) = hit {
            self.spend(bytes)?;
            if let Some((_, relative)) = self.resolved.get(&id) {
                for rel in relative {
                    self.paths.push(path.join(rel));
                }
            }
            return Ok(node);
        }
        let start = self.paths.len();
        let node = self.container(id, &path)?;
        let mut relative = Vec::new();
        for added in &self.paths[start..] {
            relative.push(added.relative_to(&path).ok_or_else(|| {
                error(format!(
                    "internal error: path {} is not a sub-path of {}",
                    super::gostr::quote(added.raw()),
                    super::gostr::quote(path.raw())
                ))
            })?);
        }
        self.resolved.insert(id, (node, relative));
        Ok(node)
    }

    fn container(&mut self, id: NodeId, path: &Path) -> Result<NodeId, Error> {
        match self.doc.nodes[id].kind.clone() {
            Kind::Sequence(items) => {
                let mut kept = Vec::new();
                for (index, item) in items.into_iter().enumerate() {
                    let Some(resolved) = self.resolve(item, path.next(&index.to_string()))? else {
                        continue;
                    };
                    let alias = matches!(self.doc.nodes[item].kind, Kind::Alias(_));
                    kept.push(if alias { item } else { resolved });
                }
                self.doc.nodes[id].kind = Kind::Sequence(kept);
            }
            Kind::Mapping(pairs) => {
                let mut lines: HashMap<String, usize> = HashMap::new();
                let mut kept = Vec::new();
                for (key, value) in pairs {
                    // Node.Value: an alias is its anchor's name.
                    let name = match &self.doc.nodes[key].kind {
                        Kind::Scalar(text, _) => text.clone(),
                        Kind::Alias(_) => self.doc.nodes[key].name.clone(),
                        _ => String::new(),
                    };
                    let line = self.doc.nodes[key].line;
                    if let Some(first) = lines.get(&name) {
                        return Err(error(format!(
                            "line {line}: mapping key {} already defined at line {first}",
                            super::gostr::quote(&name)
                        )));
                    }
                    lines.insert(name.clone(), line);
                    let Some(resolved) = self.resolve(value, path.next(&name))? else {
                        continue;
                    };
                    // go-yaml takes an alias under a merge key only to a mapping: the
                    // node it stands for goes there instead.
                    let alias = matches!(self.doc.nodes[value].kind, Kind::Alias(_));
                    kept.push((
                        key,
                        if alias && name != "<<" {
                            value
                        } else {
                            resolved
                        },
                    ));
                }
                self.doc.nodes[id].kind = Kind::Mapping(kept);
            }
            _ => {}
        }
        Ok(id)
    }

    fn check_for_cycle(&mut self, node: NodeId, path: &Path) -> Result<(), Error> {
        let seen = self.visited.entry(node).or_default();
        for previous in seen.iter() {
            if previous == path {
                continue;
            }
            let (here, there) = (path.raw(), previous.raw());
            if here.contains("<<") || there.contains("<<") {
                continue;
            }
            let nested =
                here.starts_with(&format!("{there}.")) || there.starts_with(&format!("{here}."));
            if nested && !path.in_different_services(previous) {
                return Err(error(format!(
                    "cycle detected: node at path {path} references node at path {previous}"
                )));
            }
        }
        seen.push(path.clone());
        Ok(())
    }
}

/// Removes from a model what `!reset` and `!override` paths name, before a file that
/// carries them is merged over it. Items of a list stay: compose-go matches them by an
/// index the paths never carry.
pub fn apply_resets(resets: &Resets, value: &mut Value, path: &Path) {
    match value {
        Value::Map(map) => {
            map.retain(|key, _| {
                let next = path.next(key);
                !resets.paths.iter().any(|pattern| next.matches(pattern))
            });
            for (key, entry) in map.iter_mut() {
                apply_resets(resets, entry, &path.next(key));
            }
        }
        Value::Seq(items) => {
            for (index, entry) in items.iter_mut().enumerate() {
                apply_resets(resets, entry, &path.next(&format!("[{index}]")));
            }
        }
        _ => {}
    }
}

/// Why a document did not read: go-yaml's or compose-go's error while decoding it, or a
/// key that is not a string.
#[derive(Debug)]
pub enum ReadError {
    Yaml(String),
    Key(String),
}

/// What compose-go makes of a document: its ResetProcessor decodes the node it holds
/// (an alias's anchor, for an alias), takes out its `!reset` nodes and remembers their
/// paths, then go-yaml decodes what is left into an interface{} whose maps compose-go
/// turns into string-keyed ones. A node tagged `!!null` skips all that: the processor
/// is never called and the document is nothing.
pub fn read(doc: &mut Document) -> Result<(Resets, Value), ReadError> {
    let mut root = doc.root;
    while let Kind::Alias(target) = doc.nodes[root].kind {
        root = target;
    }
    if super::construct::short_tag(doc, root) == "!!null" {
        return Ok((Resets::default(), Value::Null));
    }
    // go-yaml reports what the processor returns at the node it was called with.
    let line = doc.nodes[root].line;
    let wrap =
        |e: &dyn fmt::Display| ReadError::Yaml(format!("yaml: construct errors: line {line}: {e}"));
    doc.root = root;
    let resets = take_resets(doc).map_err(|e| wrap(&e))?;
    if doc.root == NO_ROOT {
        return Ok((resets, Value::Null));
    }
    let mut constructor = Constructor::new(doc, false, MAX_VALUES);
    let value = match constructor.any(doc.root) {
        Ok(value) => value,
        Err(Stop::Yaml(e)) => return Err(wrap(&e)),
        Err(Stop::TooLarge) => return Err(ReadError::Yaml(too_large())),
        Err(Stop::TooMuchText) => return Err(ReadError::Yaml(too_much_text())),
    };
    if !constructor.errors.is_empty() {
        return Err(ReadError::Yaml(type_errors(&constructor.errors)));
    }
    let value = string_keys(value.unwrap_or(Go::Nil), "").map_err(ReadError::Key)?;
    Ok((resets, value))
}

fn too_large() -> String {
    format!("compose file holds more than {MAX_VALUES} values once its aliases are followed")
}

fn too_much_text() -> String {
    format!(
        "compose file holds more than {} MiB of text once its aliases are followed",
        super::construct::MAX_TEXT >> 20
    )
}

/// LoadErrors: go-yaml's errors of type, each with its line.
fn type_errors(errors: &[LoadError]) -> String {
    format!(
        "yaml: construct errors: {}",
        errors
            .iter()
            .map(LoadError::simple)
            .collect::<Vec<_>>()
            .join("; ")
    )
}

/// The value of a document as tests see it: what `read` gives.
#[cfg(test)]
pub fn construct(doc: &Document) -> Result<Value, Error> {
    let mut doc = doc.clone();
    read(&mut doc).map(|(_, v)| v).map_err(|e| match e {
        ReadError::Yaml(e) | ReadError::Key(e) => error(e),
    })
}

/// What go-yaml makes of a document decoded into compose-go's `struct { Name string
/// `yaml:"name"` }`, as it reads the name of a project before loading it: the name,
/// or the error go-yaml gives.
pub fn decode_name(doc: &Document) -> Result<String, String> {
    let mut constructor = Constructor::new(doc, true, MAX_VALUES);
    match constructor.named(doc.root) {
        Err(Stop::Yaml(e)) => Err(e.to_string()),
        Err(Stop::TooLarge) => Err(too_large()),
        Err(Stop::TooMuchText) => Err(too_much_text()),
        Ok(_) if !constructor.errors.is_empty() => Err(type_errors(&constructor.errors)),
        Ok(name) => Ok(name.unwrap_or_default()),
    }
}

/// Whether a plain scalar of this text reads back as the same string.
pub fn plain_resolves_to_string(text: &str) -> bool {
    !text.is_empty() && super::construct::resolve_tag(text) == "!!str"
}

/// compose-go's convertToStringKeysRecursive.
fn string_keys(value: Go, prefix: &str) -> Result<Value, String> {
    let next = |key: &str| {
        if prefix.is_empty() {
            key.to_string()
        } else {
            format!("{prefix}.{key}")
        }
    };
    Ok(match value {
        Go::Nil => Value::Null,
        Go::Bool(b) => Value::Bool(b),
        Go::Int(i) => Value::Int(i),
        Go::Uint(u) => Value::Uint(u),
        Go::Float(x) => Value::Float(x),
        Go::Str(s) => Value::Str(s),
        Go::Time(t) => Value::Time(t),
        Go::Seq(items) => Value::Seq(
            items
                .into_iter()
                .enumerate()
                .map(|(i, item)| string_keys(item, &format!("{prefix}[{i}]")))
                .collect::<Result<_, _>>()?,
        ),
        Go::Map(GoMap::Str(entries)) => {
            let mut map = Map::new();
            for (key, entry) in entries {
                let value = string_keys(entry, &next(&key))?;
                map.insert(key, value);
            }
            Value::Map(map)
        }
        Go::Map(GoMap::Any(entries)) => {
            let mut map = Map::new();
            for (key, entry) in entries {
                let Go::Str(key) = key else {
                    let location = if prefix.is_empty() {
                        "at top level".to_string()
                    } else {
                        format!("in {prefix}")
                    };
                    return Err(format!(
                        "non-string key {location}: {}",
                        super::construct::sharp_v(&key, false)
                    ));
                };
                let value = string_keys(entry, &next(&key))?;
                map.insert(key, value);
            }
            Value::Map(map)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compose::value::Time;

    fn load(text: &str) -> Result<Value, Error> {
        let mut docs = parse(text)?;
        let mut doc = docs.remove(0);
        take_resets(&mut doc)?;
        construct(&doc)
    }

    fn map(text: &str) -> Map {
        match load(text).unwrap() {
            Value::Map(m) => m,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn scalars_resolve_as_go_yaml_resolves_them() {
        let m = map(
            "a: 0755\nb: 08\nc: yes\nd: ~\ne:\nf: 0x1F\ng: 1_000\nh: .5\ni: -0\n\
             j: 1e3\nk: +12\nl: 0o17\nm: 99999999999999999999\nn: '12'\no: !!str 12\n\
             p: 2001-12-14\nq: 0b101\nr: true\ns: False\nt: 12345678901234567890\n",
        );
        let expect = [
            ("a", Value::Int(493)),
            ("b", Value::Float(8.0)),
            ("c", Value::str("yes")),
            ("d", Value::Null),
            ("e", Value::Null),
            ("f", Value::Int(31)),
            ("g", Value::Int(1000)),
            ("h", Value::Float(0.5)),
            ("i", Value::Float(-0.0)),
            ("j", Value::Float(1000.0)),
            ("k", Value::Int(12)),
            ("l", Value::Int(15)),
            ("m", Value::Float(1e20)),
            ("n", Value::str("12")),
            ("o", Value::str("12")),
            ("p", Value::Time(Time::parse("2001-12-14").unwrap())),
            ("q", Value::Int(5)),
            ("r", Value::Bool(true)),
            ("s", Value::Bool(false)),
            ("t", Value::Uint(12345678901234567890)),
        ];
        for (key, value) in expect {
            assert_eq!(m[key], value, "{key}");
        }
    }

    #[test]
    fn merge_keys_bring_what_the_mapping_lacks() {
        let m = map("x: &x {a: 1, b: 2}\ny: &y {b: 3, c: 4}\n\
             z:\n  <<: [*x, *y]\n  a: 0\nw:\n  <<: *y\n");
        let z = m["z"].as_map().unwrap();
        assert_eq!(z["a"], Value::Int(0));
        // The first mapping of a merge list wins over the next.
        assert_eq!(z["b"], Value::Int(2));
        assert_eq!(z["c"], Value::Int(4));
        assert_eq!(m["w"].as_map().unwrap()["c"], Value::Int(4));
        let err = load("x: &x [1]\ny:\n  <<: *x\n").unwrap_err();
        // The alias under the merge key gives way to the list it stands for, whose item
        // is what go-yaml refuses.
        assert_eq!(
            err.0,
            "yaml: construct errors: line 1: go-yaml load error in constructor at L1.C8: \
             map merge requires map or sequence of maps as the value"
        );
    }

    #[test]
    fn keys_are_strings_and_unique() {
        assert_eq!(
            load("services:\n  web:\n    labels:\n      1: a\n")
                .unwrap_err()
                .0,
            "non-string key in services.web.labels: 1"
        );
        assert_eq!(
            load("services:\n  1:\n    image: x\n").unwrap_err().0,
            "non-string key in services: 1"
        );
        assert_eq!(
            load("a:\n  - b: 1\n    ~: 2\n").unwrap_err().0,
            "non-string key in a[0]: <nil>"
        );
        assert_eq!(
            load("services:\n  web:\n    image: a\n    image: b\n")
                .unwrap_err()
                .0,
            "line 4: mapping key \"image\" already defined at line 3"
        );
    }

    #[test]
    fn resets_are_taken_out_and_remembered() {
        let mut docs = parse(
            "services:\n  web:\n    ports: !reset []\n    command: !override [a]\n    \
             volumes:\n      - !reset a\n      - b\n",
        )
        .unwrap();
        let mut doc = docs.remove(0);
        let resets = take_resets(&mut doc).unwrap();
        let paths: Vec<String> = resets.paths.iter().map(|p| p.to_string()).collect();
        assert_eq!(
            paths,
            [
                "services.web.ports",
                "services.web.command",
                "services.web.volumes.0"
            ]
        );
        let value = construct(&doc).unwrap();
        let web = value.get(&["services", "web"]).unwrap().as_map().unwrap();
        assert!(!web.contains_key("ports"));
        assert_eq!(web["volumes"], Value::Seq(vec![Value::str("b")]));
        let mut model = Value::Map(Map::new());
        let services = load("services:\n  web:\n    ports: [1]\n    image: x\n").unwrap();
        model = super::super::merge::merge(model, services).unwrap();
        apply_resets(&resets, &mut model, &Path::root());
        let web = model.get(&["services", "web"]).unwrap().as_map().unwrap();
        assert!(!web.contains_key("ports"));
        assert!(web.contains_key("image"));
    }

    /// The reset pass counts the nodes it visits, aliases followed, and stops at
    /// compose-go's limit, the error at the line of the node the root alias stands for.
    #[test]
    fn the_reset_pass_stops_at_its_limit() {
        let text = format!(
            "x-a: !override\n  y: &a [{}]\n---\n*a\n",
            vec!["0"; 100_001].join(",")
        );
        let mut documents = Documents::new(text.as_bytes());
        documents.next_document().unwrap();
        let doc = documents.next_document().unwrap().unwrap();
        match read(doc) {
            Err(ReadError::Yaml(e)) => assert_eq!(
                e,
                "yaml: construct errors: line 2: compose file exceeds maximum node visit limit (100000)"
            ),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_anchor_cannot_hold_itself() {
        assert!(load("a: &a\n  b: *a\n").is_err());
    }

    #[test]
    fn an_alias_bomb_is_stopped() {
        let mut text = String::from("a0: &a0 [x, x, x, x, x, x, x, x, x, x]\n");
        for i in 1..12 {
            let prev = format!("*a{}", i - 1);
            text += &format!("a{i}: &a{i} [{}]\n", vec![prev; 10].join(", "));
        }
        let err = load(&text).unwrap_err();
        assert!(err.0.contains("once its aliases are followed"), "{err}");
        // Few values, each a long text: stopped by what the text comes to.
        let mut text = format!(
            "s: &s \"{}\"\na0: &a0 [{}]\n",
            "A".repeat(4096),
            ["*s"; 10].join(", ")
        );
        for i in 1..6 {
            let prev = format!("*a{}", i - 1);
            text += &format!("a{i}: &a{i} [{}]\n", vec![prev; 10].join(", "));
        }
        let err = load(&text).unwrap_err();
        assert!(
            err.0.contains("MiB of text once its aliases are followed"),
            "{err}"
        );
    }

    #[test]
    fn replayed_resets_are_held_to_their_text() {
        // Nested aliases multiply the paths a node recorded: a billion of them here,
        // in a few hundred bytes.
        let mut text = format!("a0: &a0 [{}]\n", ["!reset 0"; 10].join(", "));
        for i in 1..9 {
            let prev = format!("*a{}", i - 1);
            text += &format!("a{i}: &a{i} [{}]\n", vec![prev; 10].join(", "));
        }
        let mut doc = parse(&text).unwrap().remove(0);
        let err = take_resets_within(&mut doc, 1 << 20).unwrap_err();
        assert!(
            err.0.contains("paths of this file come to more than 1 MiB"),
            "{err}"
        );
    }

    #[test]
    fn replayed_resets_are_no_visits() {
        // compose-go counts no visit for a path a node reached again replays: this file
        // loads there, with its hundred thousand paths.
        let keys: Vec<String> = (0..1000).map(|i| format!("k{i}: !reset 0")).collect();
        let text = format!(
            "x-a0: &a0 {{{}}}\nx-b: [{}]\nservices: {{}}\n",
            keys.join(", "),
            ["*a0"; 101].join(", ")
        );
        let mut doc = parse(&text).unwrap().remove(0);
        let resets = take_resets(&mut doc).unwrap();
        assert_eq!(resets.paths.len(), 1000 + 101 * 1000);
    }

    #[test]
    fn documents_come_one_by_one() {
        assert_eq!(parse("").unwrap().len(), 0);
        assert_eq!(parse("# nothing\n").unwrap().len(), 0);
        let docs = parse("a: 1\n---\nb: 2\n").unwrap();
        assert_eq!(docs.len(), 2);
        assert_eq!(
            parse("a: [1\n").unwrap_err().0,
            "go-yaml load error in parser (while parsing a flow sequence) at L1.C4-L2.C1: \
             did not find expected ',' or ']'"
        );
    }
}
