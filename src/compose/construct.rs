//! go-yaml's Constructor (internal/libyaml/constructor.go of go.yaml.in/yaml/v4) for
//! what compose-go decodes a document into: an interface{}, which takes Go's nil,
//! bool, int, uint64, float64, string, time.Time, []interface{} and maps keyed by
//! string or by anything, and the struct with one string field it reads a project's
//! name with. An error of type is kept and the value goes on without what failed, as
//! go-yaml does; any other error stops it.

use std::cell::RefCell;
use std::rc::Rc;

use super::gostr;
use super::libyaml::{LoadError, Mark, Stage};
use super::value::Time;
use super::yaml::{Document, Kind, Node, NodeId, Style};

/// A value in an interface{}, as go-yaml makes it.
#[derive(Debug, Clone)]
pub enum Go {
    Nil,
    Bool(bool),
    Int(i64),
    Uint(u64),
    Float(f64),
    Str(String),
    Time(Time),
    Seq(Vec<Go>),
    Map(GoMap),
}

/// A Go map, its entries in the order they were first set.
#[derive(Debug, Clone)]
pub enum GoMap {
    /// map[string]interface{}
    Str(Vec<(String, Go)>),
    /// map[interface{}]interface{}
    Any(Vec<(Go, Go)>),
}

impl Go {
    /// Go's == on two interface values of hashable types.
    fn same(&self, other: &Go) -> bool {
        match (self, other) {
            (Go::Nil, Go::Nil) => true,
            (Go::Bool(a), Go::Bool(b)) => a == b,
            (Go::Int(a), Go::Int(b)) => a == b,
            (Go::Uint(a), Go::Uint(b)) => a == b,
            (Go::Float(a), Go::Float(b)) => a == b,
            (Go::Str(a), Go::Str(b)) => a == b,
            // A time.Time is equal to another with the same zone, and a zone of an
            // offset Go keeps one of only for whole hours (and for the local zone).
            (Go::Time(a), Go::Time(b)) => a == b && a.shared_zone(),
            _ => false,
        }
    }

    /// The name of the type of an unhashable value, as Go's runtime panic gives it.
    fn unhashable(&self) -> Option<&'static str> {
        match self {
            Go::Seq(_) => Some("[]interface {}"),
            Go::Map(GoMap::Str(_)) => Some("map[string]interface {}"),
            Go::Map(GoMap::Any(_)) => Some("map[interface {}]interface {}"),
            _ => None,
        }
    }
}

impl GoMap {
    fn contains(&self, key: &Go) -> bool {
        match (self, key) {
            (GoMap::Str(entries), Go::Str(k)) => entries.iter().any(|(e, _)| e == k),
            (GoMap::Str(_), _) => false,
            (GoMap::Any(entries), k) => entries.iter().any(|(e, _)| e.same(k)),
        }
    }

    /// SetMapIndex: a key already there takes the new value.
    fn set(&mut self, key: Go, value: Go) {
        match self {
            GoMap::Str(entries) => {
                let Go::Str(key) = key else {
                    unreachable!("a string map takes string keys");
                };
                match entries.iter_mut().find(|(e, _)| *e == key) {
                    Some(slot) => slot.1 = value,
                    None => entries.push((key, value)),
                }
            }
            GoMap::Any(entries) => match entries.iter_mut().find(|(e, _)| e.same(&key)) {
                Some(slot) => slot.1 = value,
                None => entries.push((key, value)),
            },
        }
    }

    fn type_name(&self) -> &'static str {
        match self {
            GoMap::Str(_) => "map[string]interface {}",
            GoMap::Any(_) => "map[interface {}]interface {}",
        }
    }
}

/// Go's %#v of a value an interface{} holds; `nested` inside a container, where a nil
/// is `interface {}(nil)`.
pub fn sharp_v(value: &Go, nested: bool) -> String {
    match value {
        Go::Nil if nested => "interface {}(nil)".to_string(),
        Go::Nil => "<nil>".to_string(),
        Go::Bool(b) => b.to_string(),
        Go::Int(i) => i.to_string(),
        Go::Uint(u) => format!("0x{u:x}"),
        Go::Float(x) => super::value::go_float(*x),
        Go::Str(s) => gostr::quote(s),
        Go::Time(t) => t.go_string(),
        Go::Seq(items) => format!(
            "[]interface {{}}{{{}}}",
            items
                .iter()
                .map(|v| sharp_v(v, true))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Go::Map(map) => {
            // fmt prints a map sorted by its keys (internal/fmtsort).
            let entries: Vec<(String, String)> = match map {
                GoMap::Str(e) => {
                    let mut sorted: Vec<&(String, Go)> = e.iter().collect();
                    sorted.sort_by_key(|(k, _)| gostr::to_bytes(k));
                    sorted
                        .into_iter()
                        .map(|(k, v)| (gostr::quote(k), sharp_v(v, true)))
                        .collect()
                }
                GoMap::Any(e) => {
                    let mut sorted: Vec<&(Go, Go)> = e.iter().collect();
                    sorted.sort_by(|a, b| fmt_order(&a.0, &b.0));
                    sorted
                        .into_iter()
                        .map(|(k, v)| (sharp_v(k, true), sharp_v(v, true)))
                        .collect()
                }
            };
            format!(
                "{}{{{}}}",
                map.type_name(),
                entries
                    .iter()
                    .map(|(k, v)| format!("{k}:{v}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    }
}

/// internal/fmtsort's order of the keys of a map[interface{}]interface{}: nil first,
/// then by type, in the order the Go binary lays its types out in, then
/// by value; a time by its nanoseconds, then its seconds.
fn fmt_order(a: &Go, b: &Go) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let rank = |v: &Go| match v {
        Go::Nil => 0,
        Go::Str(_) => 1,
        Go::Uint(_) => 2,
        Go::Int(_) => 3,
        Go::Float(_) => 4,
        Go::Bool(_) => 5,
        Go::Time(_) => 6,
        Go::Seq(_) | Go::Map(_) => 7,
    };
    match (a, b) {
        (Go::Bool(x), Go::Bool(y)) => x.cmp(y),
        (Go::Int(x), Go::Int(y)) => x.cmp(y),
        (Go::Uint(x), Go::Uint(y)) => x.cmp(y),
        // cmp.Compare: a NaN before every number.
        (Go::Float(x), Go::Float(y)) => match (x.is_nan(), y.is_nan()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            (false, false) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
        },
        (Go::Str(x), Go::Str(y)) => gostr::to_bytes(x).cmp(&gostr::to_bytes(y)),
        // The wall clock's nanoseconds, the seconds, then the *Location's address:
        // UTC's is nil, Local's is static, any other is on the heap.
        (Go::Time(x), Go::Time(y)) => {
            let zone = |t: &super::value::Time| match t.offset {
                None => 0,
                Some(_) if t.is_local() => 1,
                Some(_) => 2,
            };
            (x.nanosecond, x.unix(), zone(x)).cmp(&(y.nanosecond, y.unix(), zone(y)))
        }
        _ => rank(a).cmp(&rank(b)),
    }
}

/// Why a construction stopped.
pub enum Stop {
    Yaml(LoadError),
    /// nspawn's cap on the values one document builds.
    TooLarge,
}

impl From<LoadError> for Stop {
    fn from(e: LoadError) -> Self {
        Stop::Yaml(e)
    }
}

/// Where a node is constructed into.
enum Out<'o> {
    /// An interface{}.
    Any(&'o mut Option<Go>),
    /// A string: a key of a string map, or the name field.
    Str(&'o mut Option<String>),
    /// A map that is there already, as a merge fills its parent.
    Map(&'o mut GoMap),
    /// The struct with the name of the project.
    Named(&'o mut Option<String>),
}

impl Out<'_> {
    fn type_name(&self) -> &'static str {
        match self {
            Out::Any(_) => "interface {}",
            Out::Str(_) => "string",
            Out::Map(map) => map.type_name(),
            Out::Named(_) => "cli.named",
        }
    }
}

fn mark(node: &Node) -> Mark {
    Mark {
        index: 0,
        line: node.line,
        column: node.column,
    }
}

fn constructor_error(node: &Node, message: impl Into<String>) -> LoadError {
    LoadError::new(Stage::Constructor, message, mark(node))
}

fn want_map(node: &Node) -> Stop {
    Stop::Yaml(constructor_error(
        node,
        "map merge requires map or sequence of maps as the value",
    ))
}

/// Node.Value: a scalar's text, an alias's name, nothing for a collection.
fn node_value(node: &Node) -> &str {
    match &node.kind {
        Kind::Scalar(text, _) => text,
        Kind::Alias(_) => &node.name,
        _ => "",
    }
}

fn indicated_string(node: &Node) -> bool {
    match &node.kind {
        Kind::Scalar(_, style) => {
            node.tag == "!!str"
                || ((node.tag.is_empty() || node.tag == "!") && *style != Style::Plain)
        }
        _ => false,
    }
}

/// Node.ShortTag.
pub fn short_tag(doc: &Document, id: NodeId) -> String {
    let node = &doc.nodes[id];
    if indicated_string(node) {
        return "!!str".to_string();
    }
    if node.tag.is_empty() || node.tag == "!" {
        return match node.kind {
            Kind::Mapping(_) => "!!map".to_string(),
            Kind::Sequence(_) => "!!seq".to_string(),
            Kind::Alias(target) => short_tag(doc, target),
            Kind::Scalar(..) => "!!str".to_string(),
        };
    }
    node.tag.clone()
}

/// The tag go-yaml's Resolver stage gives a node without one.
pub fn resolver_tag(node: &Node) -> String {
    if !node.tag.is_empty() {
        return node.tag.clone();
    }
    match &node.kind {
        Kind::Scalar(_, style) if *style != Style::Plain => "!!str".to_string(),
        Kind::Scalar(text, _) => resolve_tag(text),
        Kind::Sequence(_) => "!!seq".to_string(),
        Kind::Mapping(_) => "!!map".to_string(),
        Kind::Alias(_) => String::new(),
    }
}

/// The tag a plain scalar resolves to.
pub fn resolve_tag(text: &str) -> String {
    resolve("", text).map_or_else(|_| "!!str".to_string(), |(tag, _)| tag)
}

/// go-yaml's resolve: what a scalar of a tag stands for, or the resolver's error when
/// the tag does not fit it.
pub fn resolve(tag: &str, text: &str) -> Result<(String, Go), LoadError> {
    if !matches!(
        tag,
        "" | "!!str" | "!!bool" | "!!int" | "!!float" | "!!null" | "!!timestamp"
    ) {
        return Ok((tag.to_string(), Go::Str(text.to_string())));
    }
    let (rtag, out) = resolve_plain(tag, text);
    match tag {
        "" | "!!str" => Ok((rtag, out)),
        t if t == rtag => Ok((rtag, out)),
        "!!float" if rtag == "!!int" => match out {
            Go::Int(i) => Ok(("!!float".to_string(), Go::Float(i as f64))),
            _ => Err(mismatch(&rtag, text, tag)),
        },
        _ => Err(mismatch(&rtag, text, tag)),
    }
}

fn mismatch(rtag: &str, text: &str, tag: &str) -> LoadError {
    LoadError::new(
        Stage::Resolver,
        format!("cannot construct {rtag} `{text}` as a {tag}"),
        Mark::default(),
    )
}

fn resolve_plain(tag: &str, text: &str) -> (String, Go) {
    let hint = match text.as_bytes().first() {
        None => b'N',
        Some(b'+' | b'-') => b'S',
        Some(b'0'..=b'9') => b'D',
        Some(b'y' | b'Y' | b'n' | b'N' | b't' | b'T' | b'f' | b'F' | b'o' | b'O' | b'~' | b'<') => {
            b'M'
        }
        Some(b'.') => b'.',
        Some(_) => 0,
    };
    let str_ = || ("!!str".to_string(), Go::Str(text.to_string()));
    if hint == 0 || tag == "!!str" {
        return str_();
    }
    let found = match text {
        "true" | "True" | "TRUE" => Some(("!!bool", Go::Bool(true))),
        "false" | "False" | "FALSE" => Some(("!!bool", Go::Bool(false))),
        "" | "~" | "null" | "Null" | "NULL" => Some(("!!null", Go::Nil)),
        ".nan" | ".NaN" | ".NAN" => Some(("!!float", Go::Float(f64::NAN))),
        ".inf" | ".Inf" | ".INF" | "+.inf" | "+.Inf" | "+.INF" => {
            Some(("!!float", Go::Float(f64::INFINITY)))
        }
        "-.inf" | "-.Inf" | "-.INF" => Some(("!!float", Go::Float(f64::NEG_INFINITY))),
        "-0" | "-0.0" => Some(("!!float", Go::Float(-0.0))),
        "<<" => Some(("!!merge", Go::Str("<<".to_string()))),
        _ => None,
    };
    if let Some((tag, value)) = found {
        return (tag.to_string(), value);
    }
    match hint {
        b'.' => {
            if let Ok(x) = gostr::parse_float(text, 64) {
                return ("!!float".to_string(), Go::Float(x));
            }
        }
        b'D' | b'S' => {
            if tag.is_empty() || tag == "!!timestamp" {
                if let Some(time) = Time::parse(text) {
                    return ("!!timestamp".to_string(), Go::Time(time));
                }
            }
            let plain = text.replace('_', "");
            if let Ok(i) = gostr::parse_int(&plain, 0, 64) {
                return ("!!int".to_string(), Go::Int(i));
            }
            if let Ok(u) = gostr::parse_uint(&plain, 0, 64) {
                return ("!!int".to_string(), Go::Uint(u));
            }
            if yaml_style_float(&plain) {
                if let Ok(x) = gostr::parse_float(&plain, 64) {
                    return ("!!float".to_string(), Go::Float(x));
                }
            }
        }
        _ => {}
    }
    str_()
}

/// `^[-+]?(?:\.[0-9]+|[0-9]+(?:\.[0-9]*)?)(?:[eE][-+]?[0-9]+)?$`
fn yaml_style_float(text: &str) -> bool {
    let body = text.strip_prefix(['-', '+']).unwrap_or(text);
    let (number, exponent) = match body.find(['e', 'E']) {
        Some(at) => (&body[..at], Some(&body[at + 1..])),
        None => (body, None),
    };
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    let number_ok = match number.split_once('.') {
        Some(("", fraction)) => !fraction.is_empty() && digits(fraction),
        Some((whole, fraction)) => !whole.is_empty() && digits(whole) && digits(fraction),
        None => !number.is_empty() && digits(number),
    };
    let exponent_ok = exponent.is_none_or(|e| {
        let e = e.strip_prefix(['-', '+']).unwrap_or(e);
        !e.is_empty() && digits(e)
    });
    number_ok && exponent_ok
}

/// Go's base64.StdEncoding.DecodeString: padded, line breaks skipped, the bits past
/// the last byte not checked.
fn decode_base64(text: &str) -> Option<Vec<u8>> {
    let value = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    };
    let input: Vec<u8> = text.bytes().filter(|&b| b != b'\r' && b != b'\n').collect();
    if !input.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::new();
    for (i, quad) in input.chunks(4).enumerate() {
        let last = i + 1 == input.len() / 4;
        let pad = quad.iter().rev().take_while(|&&b| b == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return None;
        }
        let mut n = 0u32;
        for &c in &quad[..4 - pad] {
            n = n << 6 | value(c)?;
        }
        n <<= 6 * pad as u32;
        let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(&bytes[..3 - pad]);
    }
    Some(out)
}

/// DefaultAliasCheck of yaml.Unmarshal: too many values reached through aliases for the
/// values built.
fn excessive_aliasing(aliases: usize, built: usize) -> bool {
    if aliases <= 100 || built <= 1000 {
        return false;
    }
    let (low, high) = (400_000.0, 4_000_000.0);
    let built_f = built as f64;
    let allowed = if built_f <= low {
        0.99
    } else if built_f >= high {
        0.10
    } else {
        0.99 - 0.89 * ((built_f - low) / (high - low))
    };
    aliases as f64 / built_f > allowed
}

/// The keys a merge has set, shared by every mapping it brings in, as go-yaml's
/// mergedFields map is.
type Merged = Rc<RefCell<Vec<Go>>>;

pub struct Constructor<'d> {
    doc: &'d Document,
    /// go-yaml's TypeErrors.
    pub errors: Vec<LoadError>,
    following: Vec<NodeId>,
    alias_depth: usize,
    alias_count: usize,
    count: usize,
    alias_check: bool,
    merged: Option<Merged>,
    limit: usize,
}

impl<'d> Constructor<'d> {
    /// `alias_check` for yaml.Unmarshal and Decoder.Decode; Node.Decode has none.
    /// `limit` caps the values built.
    pub fn new(doc: &'d Document, alias_check: bool, limit: usize) -> Self {
        Constructor {
            doc,
            errors: Vec::new(),
            following: Vec::new(),
            alias_depth: 0,
            alias_count: 0,
            count: 0,
            alias_check,
            merged: None,
            limit,
        }
    }

    /// A node into an interface{}: the value, None where go-yaml sets nothing.
    pub fn any(&mut self, id: NodeId) -> Result<Option<Go>, Stop> {
        let mut slot = None;
        self.construct(id, Out::Any(&mut slot))?;
        Ok(slot)
    }

    /// A node into the struct with the project's name, as yaml.Unmarshal decodes a
    /// document into it: the name, None when not set.
    pub fn named(&mut self, id: NodeId) -> Result<Option<String>, Stop> {
        // The document node counts as built, before what it holds.
        self.count += 1;
        let mut slot = None;
        self.construct(id, Out::Named(&mut slot))?;
        Ok(slot)
    }

    fn type_error(&mut self, node: &Node, tag: &str, into: &str) {
        let tag = if node.tag.is_empty() { tag } else { &node.tag };
        let value = if tag == "!!seq" || tag == "!!map" {
            // Node.Value as it is: nothing for a collection, the text of a scalar
            // tagged as one.
            node_value(node).to_string()
        } else {
            let text = node_value(node);
            let bytes = gostr::to_bytes(text);
            if bytes.len() > 10 {
                format!(" `{}...`", gostr::from_bytes(&bytes[..7]))
            } else {
                format!(" `{text}`")
            }
        };
        self.errors.push(constructor_error(
            node,
            format!("cannot construct {tag}{value} into {into}"),
        ));
    }

    fn construct(&mut self, id: NodeId, out: Out) -> Result<bool, Stop> {
        let doc = self.doc;
        let node = &doc.nodes[id];
        self.count += 1;
        if self.alias_depth > 0 {
            self.alias_count += 1;
        }
        if self.alias_check && excessive_aliasing(self.alias_count, self.count) {
            return Err(Stop::Yaml(constructor_error(
                node,
                "document contains excessive aliasing",
            )));
        }
        if self.count > self.limit {
            return Err(Stop::TooLarge);
        }
        match &node.kind {
            Kind::Alias(target) => {
                if self.following.contains(&id) {
                    return Err(Stop::Yaml(LoadError::new(
                        Stage::Composer,
                        format!("anchor '{}' value contains itself", node.name),
                        mark(node),
                    )));
                }
                self.following.push(id);
                self.alias_depth += 1;
                let good = self.construct(*target, out);
                self.alias_depth -= 1;
                self.following.pop();
                good
            }
            Kind::Scalar(text, _) => self.scalar(node, text, out),
            Kind::Sequence(items) => match out {
                Out::Any(slot) => {
                    let mut values = Vec::with_capacity(items.len());
                    for &item in items {
                        if let Some(value) = self.any(item)? {
                            values.push(value);
                        }
                    }
                    *slot = Some(Go::Seq(values));
                    Ok(true)
                }
                other => {
                    let into = other.type_name();
                    self.type_error(node, "!!seq", into);
                    Ok(false)
                }
            },
            Kind::Mapping(_) => self.mapping(id, out),
        }
    }

    fn scalar(&mut self, node: &Node, text: &str, out: Out) -> Result<bool, Stop> {
        let (tag, resolved) = if indicated_string(node) {
            ("!!str".to_string(), Go::Str(text.to_string()))
        } else {
            let (tag, resolved) = resolve(&node.tag, text)?;
            if tag == "!!binary" {
                let data = decode_base64(text).ok_or_else(|| {
                    constructor_error(node, "!!binary value contains invalid base64 data")
                })?;
                (tag, Go::Str(gostr::from_bytes(&data)))
            } else {
                (tag, resolved)
            }
        };
        if matches!(resolved, Go::Nil) {
            return Ok(self.null(out));
        }
        // The same type: taken as it is.
        let out = match (out, &resolved) {
            (Out::Str(slot), Go::Str(s)) => {
                *slot = Some(s.clone());
                return Ok(true);
            }
            (out, _) => out,
        };
        match tag.as_str() {
            "!!str" | "!!int" | "!!bool" | "!!float" => match out {
                Out::Str(slot) => {
                    *slot = Some(text.to_string());
                    Ok(true)
                }
                Out::Any(slot) => {
                    *slot = Some(resolved);
                    Ok(true)
                }
                other => {
                    let into = other.type_name();
                    self.type_error(node, &tag, into);
                    Ok(false)
                }
            },
            "!!null" => Ok(self.null(out)),
            "!!timestamp" => match out {
                Out::Any(slot) => {
                    *slot = Some(resolved);
                    Ok(true)
                }
                other => {
                    let into = other.type_name();
                    self.type_error(node, &tag, into);
                    Ok(false)
                }
            },
            "!!binary" => match (out, resolved) {
                (Out::Str(slot), Go::Str(s)) => {
                    *slot = Some(s);
                    Ok(true)
                }
                (Out::Any(slot), resolved) => {
                    *slot = Some(resolved);
                    Ok(true)
                }
                (other, _) => {
                    let into = other.type_name();
                    self.type_error(node, &tag, into);
                    Ok(false)
                }
            },
            // A merge key is no value.
            "!!merge" => Ok(false),
            _ => match out {
                Out::Any(slot) => {
                    *slot = Some(resolved);
                    Ok(true)
                }
                other => {
                    let into = other.type_name();
                    self.type_error(node, &tag, into);
                    Ok(false)
                }
            },
        }
    }

    /// null(): only what can be nil takes it.
    fn null(&mut self, out: Out) -> bool {
        match out {
            Out::Any(slot) => {
                *slot = Some(Go::Nil);
                true
            }
            Out::Map(_) => true,
            Out::Str(_) | Out::Named(_) => false,
        }
    }

    fn is_merge(&self, id: NodeId) -> bool {
        let node = &self.doc.nodes[id];
        matches!(node.kind, Kind::Scalar(..)) && node.tag == "!!merge"
    }

    /// Whether every key of a mapping is a string or a merge key: a map[string]any then.
    fn is_string_map(&self, pairs: &[(NodeId, NodeId)]) -> bool {
        pairs.iter().all(|&(k, _)| {
            let tag = short_tag(self.doc, k);
            tag == "!!str" || tag == "!!merge"
        })
    }

    fn mapping(&mut self, id: NodeId, out: Out) -> Result<bool, Stop> {
        let doc = self.doc;
        let node = &doc.nodes[id];
        let Kind::Mapping(pairs) = &node.kind else {
            unreachable!("a mapping");
        };
        // UniqueKeys.
        let before = self.errors.len();
        for i in 0..pairs.len() {
            let ni = &doc.nodes[pairs[i].0];
            for pair in &pairs[i + 1..] {
                let nj = &doc.nodes[pair.0];
                if std::mem::discriminant(&ni.kind) == std::mem::discriminant(&nj.kind)
                    && node_value(ni) == node_value(nj)
                {
                    self.errors.push(constructor_error(
                        nj,
                        format!(
                            "mapping key {} already defined at line {}",
                            gostr::quote(node_value(nj)),
                            ni.line
                        ),
                    ));
                }
            }
        }
        if self.errors.len() > before {
            return Ok(false);
        }
        match out {
            Out::Named(slot) => self.mapping_struct(id, slot),
            Out::Str(_) => {
                self.type_error(node, "!!map", "string");
                Ok(false)
            }
            Out::Any(slot) => {
                let mut map = if self.is_string_map(pairs) {
                    GoMap::Str(Vec::new())
                } else {
                    GoMap::Any(Vec::new())
                };
                let result = self.fill(id, &mut map);
                *slot = Some(Go::Map(map));
                result.map(|()| true)
            }
            Out::Map(map) => self.fill(id, map).map(|()| true),
        }
    }

    /// A key of a mapping, into the key type of the map it goes to.
    fn key(&mut self, id: NodeId, string: bool) -> Result<Option<Go>, Stop> {
        if string {
            let mut slot = None;
            self.construct(id, Out::Str(&mut slot))?;
            Ok(slot.map(Go::Str))
        } else {
            self.any(id)
        }
    }

    /// Whether a merge set has a key, putting it in when not: Go's map of interfaces,
    /// which cannot hold a list or a map.
    fn seen(&mut self, merged: &Merged, key: &Go, at: NodeId) -> Result<bool, Stop> {
        if let Some(name) = key.unhashable() {
            return Err(Stop::Yaml(constructor_error(
                &self.doc.nodes[at],
                format!("runtime error: hash of unhashable type {name}"),
            )));
        }
        let mut set = merged.borrow_mut();
        if set.iter().any(|k| k.same(key)) {
            return Ok(true);
        }
        set.push(key.clone());
        Ok(false)
    }

    fn fill(&mut self, id: NodeId, map: &mut GoMap) -> Result<(), Stop> {
        let doc = self.doc;
        let Kind::Mapping(pairs) = &doc.nodes[id].kind else {
            unreachable!("a mapping");
        };
        let merged = self.merged.take();
        let mut merge = None;
        let string = matches!(map, GoMap::Str(_));
        for &(k, v) in pairs {
            if self.is_merge(k) {
                merge = Some(v);
                continue;
            }
            let Some(key) = self.key(k, string)? else {
                continue;
            };
            if let Some(merged) = &merged {
                if self.seen(merged, &key, k)? {
                    continue;
                }
            }
            if matches!(key, Go::Seq(_) | Go::Map(_)) {
                return Err(Stop::Yaml(constructor_error(
                    &doc.nodes[k],
                    format!(
                        "cannot use '{}' as a map key; try decoding into yaml.Node",
                        sharp_v(&key, false)
                    ),
                )));
            }
            let mut slot = None;
            let good = self.construct(v, Out::Any(&mut slot))?;
            if good || (short_tag(doc, v) == "!!null" && !map.contains(&key)) {
                map.set(key, slot.unwrap_or(Go::Nil));
            }
        }
        self.merged = merged;
        if let Some(merge) = merge {
            self.merge(id, merge, &mut |c, item| c.construct(item, Out::Map(map)))?;
        }
        Ok(())
    }

    fn mapping_struct(&mut self, id: NodeId, field: &mut Option<String>) -> Result<bool, Stop> {
        let doc = self.doc;
        let Kind::Mapping(pairs) = &doc.nodes[id].kind else {
            unreachable!("a mapping");
        };
        let merged = self.merged.take();
        let mut merge = None;
        let mut done = false;
        for &(k, v) in pairs {
            if self.is_merge(k) {
                merge = Some(v);
                continue;
            }
            let mut name = None;
            if !self.construct(k, Out::Str(&mut name))? {
                continue;
            }
            let name = name.unwrap_or_default();
            if let Some(merged) = &merged {
                if self.seen(merged, &Go::Str(name.clone()), k)? {
                    continue;
                }
            }
            if name == "name" {
                if done {
                    self.errors.push(constructor_error(
                        &doc.nodes[k],
                        "field name already set in type cli.named",
                    ));
                    continue;
                }
                done = true;
                self.construct(v, Out::Str(field))?;
            }
        }
        self.merged = merged;
        if let Some(merge) = merge {
            self.merge(id, merge, &mut |c, item| {
                c.construct(item, Out::Named(field))
            })?;
        }
        Ok(true)
    }

    /// merge(): the mappings under a merge key, each constructed into what holds the
    /// key, past the keys it set itself and the earlier mappings set.
    fn merge(
        &mut self,
        parent: NodeId,
        merge: NodeId,
        into: &mut dyn FnMut(&mut Self, NodeId) -> Result<bool, Stop>,
    ) -> Result<(), Stop> {
        let doc = self.doc;
        let saved = self.merged.clone();
        if self.merged.is_none() {
            let set: Merged = Rc::new(RefCell::new(Vec::new()));
            self.merged = Some(set.clone());
            let Kind::Mapping(pairs) = &doc.nodes[parent].kind else {
                unreachable!("a mapping");
            };
            for &(k, _) in pairs {
                if let Some(key) = self.any(k)? {
                    self.seen(&set, &key, k)?;
                }
            }
        }
        let is_mapping = |id: NodeId| matches!(doc.nodes[id].kind, Kind::Mapping(_));
        match &doc.nodes[merge].kind {
            Kind::Mapping(_) => {
                into(self, merge)?;
            }
            Kind::Alias(target) => {
                if !is_mapping(*target) {
                    return Err(want_map(&doc.nodes[*target]));
                }
                into(self, merge)?;
            }
            Kind::Sequence(items) => {
                for &item in items {
                    match doc.nodes[item].kind {
                        Kind::Alias(target) if !is_mapping(target) => {
                            return Err(want_map(&doc.nodes[target]))
                        }
                        Kind::Alias(_) => {}
                        _ if !is_mapping(item) => return Err(want_map(&doc.nodes[item])),
                        _ => {}
                    }
                    into(self, item)?;
                }
            }
            Kind::Scalar(..) => return Err(want_map(&doc.nodes[merge])),
        }
        self.merged = saved;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_scalars_resolve_as_go_yaml_resolves_them() {
        let tag = |text: &str| resolve_tag(text);
        assert_eq!(tag("1_0"), "!!int");
        assert_eq!(tag("08"), "!!float");
        assert_eq!(tag("1e400"), "!!str");
        assert_eq!(tag("._5"), "!!str");
        assert_eq!(tag(".5_"), "!!str");
        assert_eq!(tag(".5"), "!!float");
        assert_eq!(tag("yes"), "!!str");
        assert_eq!(tag("<<"), "!!merge");
        assert_eq!(tag("2001-12-14"), "!!timestamp");
        assert!(matches!(
            resolve("!!int", "18446744073709551615"),
            Ok((_, Go::Uint(u64::MAX)))
        ));
        assert!(resolve("!!float", "18446744073709551615").is_err());
        assert!(resolve("!!float", "1e400").is_err());
        assert!(matches!(resolve("!!float", "1"), Ok((_, Go::Float(x))) if x == 1.0));
    }

    #[test]
    fn base64_reads_as_go_reads_it() {
        assert_eq!(decode_base64("Zm9v\nYmFy"), Some(b"foobar".to_vec()));
        assert_eq!(decode_base64("Zm9v YmFy"), None);
        // Bits past the last byte are not checked.
        assert_eq!(decode_base64("Zh=="), Some(b"f".to_vec()));
        assert_eq!(decode_base64("Zg="), None);
    }

    #[test]
    fn values_print_as_sharp_v() {
        let seq = Go::Seq(vec![Go::Nil, Go::Str("a\u{7}".into()), Go::Int(1)]);
        assert_eq!(
            sharp_v(&seq, false),
            "[]interface {}{interface {}(nil), \"a\\a\", 1}"
        );
        let map = Go::Map(GoMap::Any(vec![
            (Go::Int(10), Go::Nil),
            (Go::Int(2), Go::Bool(true)),
        ]));
        assert_eq!(
            sharp_v(&map, false),
            "map[interface {}]interface {}{2:true, 10:interface {}(nil)}"
        );
        assert_eq!(sharp_v(&Go::Uint(255), false), "0xff");
    }

    #[test]
    fn keys_of_several_types_sort_as_fmtsort_sorts_them() {
        let map = Go::Map(GoMap::Any(vec![
            (Go::Bool(true), Go::Nil),
            (Go::Float(f64::NAN), Go::Nil),
            (Go::Float(1.5), Go::Nil),
            (Go::Int(-1), Go::Nil),
            (Go::Uint(u64::MAX), Go::Nil),
            (Go::Str("a".into()), Go::Nil),
            (Go::Nil, Go::Nil),
        ]));
        assert_eq!(
            sharp_v(&map, false),
            "map[interface {}]interface {}{interface {}(nil):interface {}(nil), \
             \"a\":interface {}(nil), 0xffffffffffffffff:interface {}(nil), \
             -1:interface {}(nil), NaN:interface {}(nil), 1.5:interface {}(nil), \
             true:interface {}(nil)}"
        );
    }
}
