//! The structural check of a compose file against the Compose Specification's JSON
//! schema, the one under src/compose/upstream, with the words docker compose uses: its
//! validator (santhosh-tekuri/jsonschema v6, which compose-go runs) builds a tree of
//! errors and compose-go reports the one deepest in the file. The schema uses a small
//! part of JSON Schema 2020-12, which is what is evaluated here, in the same order.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use serde_json::Value as Json;

use super::value::{go_float, Value};

pub const SCHEMA: &str = include_str!("upstream/compose-go/compose-spec.json");

type Id = usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Type {
    Null,
    Boolean,
    Number,
    Integer,
    String,
    Array,
    Object,
}

impl Type {
    fn parse(name: &str) -> Option<Type> {
        Some(match name {
            "null" => Type::Null,
            "boolean" => Type::Boolean,
            "number" => Type::Number,
            "integer" => Type::Integer,
            "string" => Type::String,
            "array" => Type::Array,
            "object" => Type::Object,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Type::Null => "null",
            Type::Boolean => "boolean",
            Type::Number => "number",
            Type::Integer => "integer",
            Type::String => "string",
            Type::Array => "array",
            Type::Object => "object",
        }
    }
}

/// The patterns the schema uses, each matched as Go's regexp matches it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pattern {
    /// `^[a-zA-Z0-9._-]+$`
    Name,
    /// `^x-`
    Extension,
    /// `^.+$`
    Line,
    /// `.+`
    NotEmpty,
    /// `^[a-z]+$`
    Lowercase,
    /// `[a-zA-Z0-9][a-zA-Z0-9_.-]+`
    ContainerName,
    /// The pattern of pull_policy.
    PullPolicy,
}

impl Pattern {
    fn parse(text: &str) -> Option<Pattern> {
        Some(match text {
            "^[a-zA-Z0-9._-]+$" => Pattern::Name,
            "^x-" => Pattern::Extension,
            "^.+$" => Pattern::Line,
            ".+" => Pattern::NotEmpty,
            "^[a-z]+$" => Pattern::Lowercase,
            "[a-zA-Z0-9][a-zA-Z0-9_.-]+" => Pattern::ContainerName,
            "^(always|never|build|if_not_present|missing|refresh|daily|weekly|every_([0-9]+[wdhms])+)$" => {
                Pattern::PullPolicy
            }
            _ => return None,
        })
    }

    fn matches(self, s: &str) -> bool {
        let name_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-');
        match self {
            Pattern::Name => !s.is_empty() && s.chars().all(name_char),
            Pattern::Extension => s.starts_with("x-"),
            Pattern::Line => !s.is_empty() && !s.contains('\n'),
            Pattern::NotEmpty => s.chars().any(|c| c != '\n'),
            Pattern::Lowercase => !s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase()),
            Pattern::ContainerName => {
                let chars: Vec<char> = s.chars().collect();
                chars
                    .windows(2)
                    .any(|w| w[0].is_ascii_alphanumeric() && name_char(w[1]))
            }
            Pattern::PullPolicy => {
                matches!(
                    s,
                    "always"
                        | "never"
                        | "build"
                        | "if_not_present"
                        | "missing"
                        | "refresh"
                        | "daily"
                        | "weekly"
                ) || s.strip_prefix("every_").is_some_and(|rest| {
                    // ([0-9]+[wdhms])+
                    let mut digits = 0;
                    let mut groups = 0;
                    for c in rest.chars() {
                        if c.is_ascii_digit() {
                            digits += 1;
                        } else if digits > 0 && matches!(c, 'w' | 'd' | 'h' | 'm' | 's') {
                            digits = 0;
                            groups += 1;
                        } else {
                            return false;
                        }
                    }
                    digits == 0 && groups > 0
                })
            }
        }
    }

    fn text(self) -> &'static str {
        match self {
            Pattern::Name => "^[a-zA-Z0-9._-]+$",
            Pattern::Extension => "^x-",
            Pattern::Line => "^.+$",
            Pattern::NotEmpty => ".+",
            Pattern::Lowercase => "^[a-z]+$",
            Pattern::ContainerName => "[a-zA-Z0-9][a-zA-Z0-9_.-]+",
            Pattern::PullPolicy => {
                "^(always|never|build|if_not_present|missing|refresh|daily|weekly|every_([0-9]+[wdhms])+)$"
            }
        }
    }
}

#[derive(Debug, Clone)]
enum Additional {
    Bool(bool),
    Schema(Id),
}

#[derive(Debug, Default, Clone)]
struct Schema {
    boolean: Option<bool>,
    types: Vec<Type>,
    enumeration: Option<Vec<Json>>,
    reference: Option<Id>,
    required: Vec<String>,
    properties: Vec<(String, Id)>,
    pattern_properties: Vec<(Pattern, Id)>,
    additional: Option<Additional>,
    unevaluated: Option<Additional>,
    items: Option<Id>,
    unique_items: bool,
    minimum: Option<f64>,
    maximum: Option<f64>,
    pattern: Option<Pattern>,
    all_of: Vec<Id>,
    any_of: Vec<Id>,
    one_of: Vec<Id>,
}

struct Compiled {
    schemas: Vec<Schema>,
    root: Id,
}

fn compiled() -> &'static Compiled {
    static COMPILED: OnceLock<Compiled> = OnceLock::new();
    COMPILED.get_or_init(|| compile(SCHEMA).expect("the embedded schema compiles"))
}

/// Builds the schema, refusing a keyword or pattern this validator does not evaluate,
/// so that a new schema cannot pass with a part of it left out.
fn compile(text: &str) -> Result<Compiled, String> {
    let root: Json = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let defs = root
        .get("$defs")
        .and_then(Json::as_object)
        .cloned()
        .unwrap_or_default();
    let mut compiler = Compiler {
        schemas: Vec::new(),
        defs: Vec::new(),
    };
    // Each definition gets its id first, so that references can point at it.
    for (name, _) in &defs {
        let id = compiler.reserve();
        compiler.defs.push((format!("#/$defs/{name}"), id));
    }
    for (name, def) in &defs {
        let id = compiler.def(&format!("#/$defs/{name}")).unwrap();
        compiler.fill(id, def)?;
    }
    let id = compiler.reserve();
    compiler.fill(id, &root)?;
    Ok(Compiled {
        schemas: compiler.schemas,
        root: id,
    })
}

struct Compiler {
    schemas: Vec<Schema>,
    defs: Vec<(String, Id)>,
}

/// The keywords that only describe, which validation does not look at.
const ANNOTATIONS: &[&str] = &[
    "description",
    "deprecated",
    "default",
    "$schema",
    "$id",
    "title",
    "$defs",
];

impl Compiler {
    fn reserve(&mut self) -> Id {
        self.schemas.push(Schema::default());
        self.schemas.len() - 1
    }

    fn def(&self, reference: &str) -> Option<Id> {
        self.defs
            .iter()
            .find(|(r, _)| r == reference)
            .map(|(_, id)| *id)
    }

    fn sub(&mut self, json: &Json) -> Result<Id, String> {
        let id = self.reserve();
        self.fill(id, json)?;
        Ok(id)
    }

    fn fill(&mut self, id: Id, json: &Json) -> Result<(), String> {
        let mut schema = Schema::default();
        if let Some(b) = json.as_bool() {
            schema.boolean = Some(b);
            self.schemas[id] = schema;
            return Ok(());
        }
        let object = json
            .as_object()
            .ok_or("a schema that is neither an object nor a boolean")?;
        for (keyword, value) in object {
            match keyword.as_str() {
                k if ANNOTATIONS.contains(&k) => {}
                "type" => {
                    let names: Vec<&str> = match value {
                        Json::String(s) => vec![s.as_str()],
                        Json::Array(items) => items.iter().filter_map(Json::as_str).collect(),
                        _ => return Err("a type that is not a name".to_string()),
                    };
                    for name in names {
                        schema
                            .types
                            .push(Type::parse(name).ok_or_else(|| format!("unknown type {name}"))?);
                    }
                }
                "enum" => schema.enumeration = value.as_array().cloned(),
                "$ref" => {
                    let reference = value.as_str().unwrap_or("");
                    schema.reference = Some(
                        self.def(reference)
                            .ok_or_else(|| format!("unknown reference {reference}"))?,
                    );
                }
                "required" => {
                    schema.required = value
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect();
                }
                "properties" => {
                    for (name, sub) in value.as_object().into_iter().flatten() {
                        let sub = self.sub(sub)?;
                        schema.properties.push((name.clone(), sub));
                    }
                }
                "patternProperties" => {
                    for (pattern, sub) in value.as_object().into_iter().flatten() {
                        let pattern = Pattern::parse(pattern)
                            .ok_or_else(|| format!("a pattern not evaluated here: {pattern}"))?;
                        let sub = self.sub(sub)?;
                        schema.pattern_properties.push((pattern, sub));
                    }
                }
                "additionalProperties" | "unevaluatedProperties" => {
                    let additional = match value {
                        Json::Bool(b) => Additional::Bool(*b),
                        other => Additional::Schema(self.sub(other)?),
                    };
                    if keyword == "additionalProperties" {
                        schema.additional = Some(additional);
                    } else {
                        schema.unevaluated = Some(additional);
                    }
                }
                "items" => schema.items = Some(self.sub(value)?),
                "uniqueItems" => schema.unique_items = value.as_bool().unwrap_or(false),
                "minimum" => schema.minimum = value.as_f64(),
                "maximum" => schema.maximum = value.as_f64(),
                "pattern" => {
                    let text = value.as_str().unwrap_or("");
                    schema.pattern = Some(
                        Pattern::parse(text)
                            .ok_or_else(|| format!("a pattern not evaluated here: {text}"))?,
                    );
                }
                "allOf" | "anyOf" | "oneOf" => {
                    let mut subs = Vec::new();
                    for sub in value.as_array().into_iter().flatten() {
                        subs.push(self.sub(sub)?);
                    }
                    match keyword.as_str() {
                        "allOf" => schema.all_of = subs,
                        "anyOf" => schema.any_of = subs,
                        _ => schema.one_of = subs,
                    }
                }
                other => return Err(format!("a keyword not evaluated here: {other}")),
            }
        }
        self.schemas[id] = schema;
        Ok(())
    }
}

/// What failed, as the validator words it.
#[derive(Debug, Clone, PartialEq)]
enum Kind {
    Group,
    FalseSchema,
    Type { want: Vec<Type> },
    Enum { want: Vec<Json> },
    Required { missing: Vec<String> },
    AdditionalProperties { properties: Vec<String> },
    UniqueItems { first: usize, second: usize },
    Minimum { want: f64 },
    Maximum { want: f64 },
    Pattern { got: String, want: Pattern },
    AllOf,
    AnyOf,
    OneOf { matched: Option<(usize, usize)> },
    Reference,
}

#[derive(Debug, Clone)]
struct Error {
    location: Vec<String>,
    kind: Kind,
    causes: Vec<Error>,
}

/// The value as the validator sees it once compose-go turned it into JSON and back.
#[derive(Debug, Clone, PartialEq)]
enum Instance {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Instance>),
    /// Keys in order, as a mapping iterates.
    Object(Vec<(String, Instance)>),
}

impl Instance {
    fn from(value: &Value) -> Result<Instance, String> {
        Ok(match value {
            Value::Null => Instance::Null,
            Value::Bool(b) => Instance::Bool(*b),
            Value::Int(i) => Instance::Number(*i as f64),
            Value::Uint(u) => Instance::Number(*u as f64),
            Value::Float(x) if !x.is_finite() => {
                return Err(format!("json: unsupported value: {}", go_float(*x)))
            }
            Value::Float(x) => Instance::Number(*x),
            Value::Str(s) => Instance::String(s.clone()),
            Value::Time(t) => match t.json_error() {
                Some(e) => {
                    return Err(format!(
                        "json: error calling MarshalJSON for type time.Time: {e}"
                    ))
                }
                None => Instance::String(t.rfc3339()),
            },
            Value::Seq(items) => {
                Instance::Array(items.iter().map(Instance::from).collect::<Result<_, _>>()?)
            }
            Value::Map(map) => Instance::Object(
                map.iter()
                    .map(|(k, v)| Instance::from(v).map(|v| (k.clone(), v)))
                    .collect::<Result<_, _>>()?,
            ),
        })
    }

    fn type_of(&self) -> Type {
        match self {
            Instance::Null => Type::Null,
            Instance::Bool(_) => Type::Boolean,
            Instance::Number(_) => Type::Number,
            Instance::String(_) => Type::String,
            Instance::Array(_) => Type::Array,
            Instance::Object(_) => Type::Object,
        }
    }

    fn equals_json(&self, json: &Json) -> bool {
        match (self, json) {
            (Instance::Null, Json::Null) => true,
            (Instance::Bool(a), Json::Bool(b)) => a == b,
            (Instance::Number(a), Json::Number(b)) => Some(*a) == b.as_f64(),
            (Instance::String(a), Json::String(b)) => a == b,
            _ => false,
        }
    }
}

struct Validator<'a> {
    schemas: &'a [Schema],
}

/// What a validation left unevaluated of an object, when someone needs to know.
type Uneval = Option<BTreeSet<String>>;

struct Frame<'v> {
    value: &'v Instance,
    location: Vec<String>,
    uneval: Uneval,
    errors: Vec<Error>,
    bool_result: bool,
}

impl Validator<'_> {
    fn uneval_from(&self, value: &Instance, schema: Id, caller_needs: bool) -> Uneval {
        match value {
            Instance::Object(entries)
                if caller_needs || self.schemas[schema].unevaluated.is_some() =>
            {
                Some(entries.iter().map(|(k, _)| k.clone()).collect())
            }
            _ => None,
        }
    }

    fn error(&self, frame: &Frame, kind: Kind) -> Error {
        Error {
            location: frame.location.clone(),
            kind,
            causes: Vec::new(),
        }
    }

    /// jsonschema's validate: Ok with what stayed unevaluated, or the error.
    fn validate(&self, schema: Id, mut frame: Frame) -> Result<Uneval, Error> {
        let s = &self.schemas[schema];
        if let Some(b) = s.boolean {
            return if b {
                Ok(frame.uneval)
            } else {
                Err(self.error(&frame, Kind::FalseSchema))
            };
        }
        let value = frame.value;
        let t = value.type_of();
        if !s.types.is_empty() {
            let integer = matches!(value, Instance::Number(n) if n.fract() == 0.0);
            let matched = s.types.contains(&t) || (s.types.contains(&Type::Integer) && integer);
            if !matched {
                return Err(self.error(
                    &frame,
                    Kind::Type {
                        want: s.types.clone(),
                    },
                ));
            }
        }
        if let Some(want) = &s.enumeration {
            if !want.iter().any(|w| value.equals_json(w)) {
                return Err(self.error(&frame, Kind::Enum { want: want.clone() }));
            }
        }
        if let Some(target) = s.reference {
            if let Err(error) = self.validate_ref(target, &mut frame) {
                frame.errors.push(error);
            }
        }
        match value {
            Instance::Object(entries) => self.object(s, entries, &mut frame),
            Instance::Array(items) => self.array(s, items, &mut frame),
            Instance::String(text) => {
                if let Some(pattern) = s.pattern {
                    if !pattern.matches(text) {
                        let kind = Kind::Pattern {
                            got: text.clone(),
                            want: pattern,
                        };
                        frame.errors.push(self.error(&frame, kind));
                    }
                }
            }
            Instance::Number(n) => {
                if let Some(min) = s.minimum.filter(|min| n < min) {
                    frame
                        .errors
                        .push(self.error(&frame, Kind::Minimum { want: min }));
                }
                if let Some(max) = s.maximum.filter(|max| n > max) {
                    frame
                        .errors
                        .push(self.error(&frame, Kind::Maximum { want: max }));
                }
            }
            _ => {}
        }
        if frame.errors.is_empty() || !frame.bool_result {
            self.conditions(s, &mut frame);
            self.unevaluated(s, &mut frame);
        }
        match frame.errors.len() {
            0 => Ok(frame.uneval),
            1 => Err(frame.errors.pop().unwrap()),
            _ => {
                let mut group = self.error(&frame, Kind::Group);
                group.causes = frame.errors;
                Err(group)
            }
        }
    }

    fn object(&self, s: &Schema, entries: &[(String, Instance)], frame: &mut Frame) {
        let missing: Vec<String> = s
            .required
            .iter()
            .filter(|name| !entries.iter().any(|(k, _)| k == *name))
            .cloned()
            .collect();
        if !missing.is_empty() {
            frame
                .errors
                .push(self.error(frame, Kind::Required { missing }));
        }
        if frame.bool_result && !frame.errors.is_empty() {
            return;
        }
        let mut additional = Vec::new();
        for (name, value) in entries {
            if frame.bool_result && !frame.errors.is_empty() {
                return;
            }
            let mut evaluated = false;
            if let Some((_, sub)) = s.properties.iter().find(|(p, _)| p == name) {
                evaluated = true;
                if let Err(e) = self.validate_val(*sub, value, name, frame) {
                    frame.errors.push(e);
                }
            }
            for (pattern, sub) in &s.pattern_properties {
                if pattern.matches(name) {
                    evaluated = true;
                    if let Err(e) = self.validate_val(*sub, value, name, frame) {
                        frame.errors.push(e);
                    }
                }
            }
            if !evaluated {
                if let Some(a) = &s.additional {
                    evaluated = true;
                    match a {
                        Additional::Bool(false) => additional.push(name.clone()),
                        Additional::Bool(true) => {}
                        Additional::Schema(sub) => {
                            if let Err(e) = self.validate_val(*sub, value, name, frame) {
                                frame.errors.push(e);
                            }
                        }
                    }
                }
            }
            if evaluated {
                if let Some(uneval) = &mut frame.uneval {
                    uneval.remove(name);
                }
            }
        }
        if !additional.is_empty() {
            let kind = Kind::AdditionalProperties {
                properties: additional,
            };
            frame.errors.push(self.error(frame, kind));
        }
    }

    fn array(&self, s: &Schema, items: &[Instance], frame: &mut Frame) {
        if s.unique_items && items.len() > 1 {
            'outer: for i in 1..items.len() {
                for j in 0..i {
                    if items[i] == items[j] {
                        let kind = Kind::UniqueItems {
                            first: j,
                            second: i,
                        };
                        frame.errors.push(self.error(frame, kind));
                        break 'outer;
                    }
                }
            }
        }
        if let Some(sub) = s.items {
            for (i, item) in items.iter().enumerate() {
                if let Err(e) = self.validate_val(sub, item, &i.to_string(), frame) {
                    frame.errors.push(e);
                }
            }
        }
    }

    fn conditions(&self, s: &Schema, frame: &mut Frame) {
        if !s.all_of.is_empty() {
            let mut errors = Vec::new();
            for sub in &s.all_of {
                if let Err(e) = self.validate_self(*sub, false, frame) {
                    errors.push(e);
                    if frame.bool_result {
                        break;
                    }
                }
            }
            if !errors.is_empty() {
                let mut error = self.error(frame, Kind::AllOf);
                error.causes = errors;
                frame.errors.push(error);
            }
        }
        if !s.any_of.is_empty() {
            let mut matched = false;
            let mut errors = Vec::new();
            for sub in &s.any_of {
                match self.validate_self(*sub, false, frame) {
                    Err(e) => errors.push(e),
                    Ok(()) => {
                        matched = true;
                        if frame.uneval.as_ref().is_none_or(BTreeSet::is_empty) {
                            break;
                        }
                    }
                }
            }
            if !matched {
                let mut error = self.error(frame, Kind::AnyOf);
                error.causes = errors;
                frame.errors.push(error);
            }
        }
        if !s.one_of.is_empty() {
            let mut matched: Option<usize> = None;
            let mut errors = Vec::new();
            for (i, sub) in s.one_of.iter().enumerate() {
                match self.validate_self(*sub, matched.is_some(), frame) {
                    Err(e) => {
                        if matched.is_none() {
                            errors.push(e);
                        }
                    }
                    Ok(()) => match matched {
                        None => matched = Some(i),
                        Some(first) => {
                            let kind = Kind::OneOf {
                                matched: Some((first, i)),
                            };
                            frame.errors.push(self.error(frame, kind));
                            break;
                        }
                    },
                }
            }
            if matched.is_none() {
                let mut error = self.error(frame, Kind::OneOf { matched: None });
                error.causes = errors;
                frame.errors.push(error);
            }
        }
    }

    fn unevaluated(&self, s: &Schema, frame: &mut Frame) {
        let (Some(rule), Instance::Object(entries)) = (&s.unevaluated, frame.value) else {
            return;
        };
        let left: Vec<String> = frame
            .uneval
            .take()
            .unwrap_or_default()
            .into_iter()
            .collect();
        for name in left {
            let Some((_, value)) = entries.iter().find(|(k, _)| *k == name) else {
                continue;
            };
            let result = match rule {
                Additional::Bool(true) => Ok(()),
                Additional::Bool(false) => {
                    let mut location = frame.location.clone();
                    location.push(name.clone());
                    Err(Error {
                        location,
                        kind: Kind::FalseSchema,
                        causes: Vec::new(),
                    })
                }
                Additional::Schema(sub) => self.validate_val(*sub, value, &name, frame),
            };
            if let Err(e) = result {
                frame.errors.push(e);
            }
        }
        frame.uneval = Some(BTreeSet::new());
    }

    fn validate_self(&self, schema: Id, bool_result: bool, frame: &mut Frame) -> Result<(), Error> {
        let caller_needs = frame.uneval.as_ref().is_some_and(|u| !u.is_empty());
        let sub = Frame {
            value: frame.value,
            location: frame.location.clone(),
            uneval: self.uneval_from(frame.value, schema, caller_needs),
            errors: Vec::new(),
            bool_result: frame.bool_result || bool_result,
        };
        let uneval = self.validate(schema, sub)?;
        // What the subschema evaluated is evaluated for the caller too.
        if let Some(mine) = &mut frame.uneval {
            let theirs = uneval.unwrap_or_default();
            mine.retain(|k| theirs.contains(k));
        }
        Ok(())
    }

    fn validate_val(
        &self,
        schema: Id,
        value: &Instance,
        token: &str,
        frame: &Frame,
    ) -> Result<(), Error> {
        let mut location = frame.location.clone();
        location.push(token.to_string());
        let sub = Frame {
            value,
            location,
            uneval: self.uneval_from(value, schema, false),
            errors: Vec::new(),
            bool_result: frame.bool_result,
        };
        self.validate(schema, sub).map(|_| ())
    }

    fn validate_ref(&self, target: Id, frame: &mut Frame) -> Result<(), Error> {
        match self.validate_self(target, false, frame) {
            Ok(()) => Ok(()),
            Err(error) => {
                let mut reference = self.error(frame, Kind::Reference);
                reference.causes = if error.kind == Kind::Group {
                    error.causes
                } else {
                    vec![error]
                };
                Err(reference)
            }
        }
    }
}

fn most_specific(error: &Error) -> &Error {
    if error.causes.is_empty() {
        return error;
    }
    let mut best: Option<&Error> = None;
    for cause in &error.causes {
        let cause = most_specific(cause);
        if specificity(Some(cause)) > specificity(best) {
            best = Some(cause);
        }
    }
    best.unwrap()
}

fn specificity(error: Option<&Error>) -> isize {
    match error {
        None => -1,
        Some(e) if matches!(e.kind, Kind::AdditionalProperties { .. }) => {
            e.location.len() as isize + 1
        }
        Some(e) => e.location.len() as isize,
    }
}

/// jsonschema's quote: a Go-quoted string between single quotes.
fn quote(s: &str) -> String {
    let go = super::gostr::quote(s);
    let inner = &go[1..go.len() - 1];
    format!("'{}'", inner.replace("\\\"", "\"").replace('\'', "\\'"))
}

fn display(json: &Json) -> String {
    match json {
        Json::String(s) => quote(s),
        Json::Array(_) | Json::Object(_) => "value".to_string(),
        Json::Number(n) => n.as_f64().map_or_else(|| n.to_string(), go_float),
        other => other.to_string(),
    }
}

/// compose-go's humanReadableType.
fn human_type(want: &[Type]) -> String {
    let name = |t: &Type| match t {
        Type::Object => "mapping".to_string(),
        other => other.name().to_string(),
    };
    if want.len() == 1 {
        return name(&want[0]);
    }
    let mut names: Vec<String> = want.iter().map(name).collect();
    names.sort();
    let last = names.pop().unwrap();
    format!("{} or {last}", names.join(", "))
}

fn message(error: &Error) -> String {
    let path = error.location.join(".");
    let text = match &error.kind {
        Kind::Type { want } => return format!("{path} must be a {}", human_type(want)),
        Kind::Minimum { want } => {
            return format!(
                "{path} must be greater than or equal to {}",
                go_float(*want)
            )
        }
        Kind::Maximum { want } => {
            return format!("{path} must be less than or equal to {}", go_float(*want))
        }
        Kind::Group => "validation failed".to_string(),
        Kind::FalseSchema => "false schema".to_string(),
        Kind::Enum { want } => {
            if want.iter().any(|w| w.is_array() || w.is_object()) {
                "'enum' failed".to_string()
            } else if want.len() == 1 {
                format!("value must be {}", display(&want[0]))
            } else {
                let want: Vec<String> = want.iter().map(display).collect();
                format!("value must be one of {}", want.join(", "))
            }
        }
        Kind::Required { missing } if missing.len() == 1 => {
            format!("missing property {}", quote(&missing[0]))
        }
        Kind::Required { missing } => format!(
            "missing properties {}",
            missing
                .iter()
                .map(|m| quote(m))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Kind::AdditionalProperties { properties } => format!(
            "additional properties {} not allowed",
            properties
                .iter()
                .map(|p| quote(p))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Kind::UniqueItems { first, second } => format!("items at {first} and {second} are equal"),
        Kind::Pattern { got, want } => {
            format!(
                "{} does not match pattern {}",
                quote(got),
                quote(want.text())
            )
        }
        Kind::AllOf => "'allOf' failed".to_string(),
        Kind::AnyOf => "'anyOf' failed".to_string(),
        Kind::OneOf { matched: None } => "'oneOf' failed, none matched".to_string(),
        Kind::OneOf {
            matched: Some((a, b)),
        } => format!("'oneOf' failed, subschemas {a}, {b} matched"),
        Kind::Reference => "validation failed".to_string(),
    };
    format!("{path} {text}")
}

/// Checks a model against the schema; the error is compose-go's message for the most
/// specific failure.
pub fn validate(model: &Value) -> Result<(), String> {
    let instance = Instance::from(model)?;
    let compiled = compiled();
    let validator = Validator {
        schemas: &compiled.schemas,
    };
    let frame = Frame {
        value: &instance,
        location: Vec::new(),
        uneval: validator.uneval_from(&instance, compiled.root, false),
        errors: Vec::new(),
        bool_result: false,
    };
    match validator.validate(compiled.root, frame) {
        Ok(_) => Ok(()),
        Err(error) => {
            let root = Error {
                location: Vec::new(),
                kind: Kind::Group,
                causes: if error.kind == Kind::Group {
                    error.causes
                } else {
                    vec![error]
                },
            };
            Err(message(most_specific(&root)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(text: &str) -> Result<(), String> {
        let mut docs = super::super::yaml::parse(text).unwrap();
        let mut doc = docs.remove(0);
        super::super::yaml::take_resets(&mut doc).unwrap();
        validate(&super::super::yaml::construct(&doc).unwrap())
    }

    #[test]
    fn the_schema_uses_only_what_is_evaluated_here() {
        compile(SCHEMA).unwrap();
    }

    #[test]
    fn errors_read_as_docker_compose_s() {
        for (file, error) in [
            (
                "services:\n  web:\n    image: x\n    foo: 1\n",
                "services.web.foo false schema",
            ),
            (
                "services:\n  web:\n    image: x\nfoo: 1\n",
                " additional properties 'foo' not allowed",
            ),
            (
                "services:\n  web:\n    image: [x]\n",
                "services.web.image must be a string",
            ),
            (
                "services:\n  web:\n    image: x\n    depends_on:\n      db:\n        condition: bogus\nservices2: 1\n",
                "services.web.depends_on.db.condition value must be one of 'service_started', 'service_healthy', 'service_completed_successfully'",
            ),
            (
                "services:\n  web:\n    image: x\n    container_name: \"-x\"\n",
                "services.web.container_name '-x' does not match pattern '[a-zA-Z0-9][a-zA-Z0-9_.-]+'",
            ),
            (
                "services:\n  web:\n    image: x\n    provider:\n      options: {}\n",
                "services.web.provider missing property 'type'",
            ),
            (
                "services:\n  web:\n    image: x\n    healthcheck:\n      interval: 3\n",
                "services.web.healthcheck.interval must be a string",
            ),
            (
                "networks:\n  n:\n    foo: 1\n",
                "networks.n additional properties 'foo' not allowed",
            ),
            (
                "services:\n  web:\n    image: x\n    build:\n      foo: 1\n",
                "services.web.build additional properties 'foo' not allowed",
            ),
        ] {
            assert_eq!(check(file).unwrap_err(), error, "{file}");
        }
    }

    #[test]
    fn valid_files_pass() {
        check(
            "name: x\nservices:\n  web:\n    image: nginx\n    ports: [\"80:80\", {target: 81}]\n    \
             x-meta: 1\n    deploy:\n      replicas: 2\n    volumes: [\"./a:/a\"]\n    \
             depends_on: [db]\n  db:\n    image: postgres\nvolumes:\n  data: {}\nx-anchors: 1\n",
        )
        .unwrap();
    }

    #[test]
    fn infinity_is_no_json() {
        assert_eq!(
            check("services:\n  web:\n    image: x\n    labels:\n      a: .inf\n").unwrap_err(),
            "json: unsupported value: +Inf"
        );
    }
}
