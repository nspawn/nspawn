//! The typed model of a project, as compose-go binds it to its structs: each struct is
//! a table of fields (src/compose/types) with its YAML and JSON names, and one decoder
//! and one encoder follow the tables. Decoding is mapstructure's with compose-go's
//! hooks (each type's DecodeMapstructure, strings cast to the field's type); a decoded
//! value is a `Value` in a fixed form per type (a duration in nanoseconds, a command
//! as a list), absent where the Go field is zero. Encoding follows go-yaml and
//! encoding/json: field order, omitempty, the types' own marshallers.

use super::emit::Out;
use super::golib;
use super::gostr;
use super::value::{Map, Value};

/// The type of a field.
#[derive(Debug)]
pub enum Ty {
    Str,
    Bool,
    Int,
    Int64,
    Uint64,
    Uint32,
    Uint16,
    Float32,
    Duration,
    UnitBytes,
    NanoCpus,
    ShellCommand,
    MappingWithEquals,
    Mapping,
    Labels,
    Options,
    MultiOptions,
    HostsList,
    StringList,
    StringOrNumberList,
    HealthCheckTest,
    FileMode,
    OptOut,
    DeviceCount,
    Ulimit,
    SshConfig,
    Slice(&'static Ty),
    Map(&'static Ty),
    Ptr(&'static Ty),
    Struct(&'static Def),
}

/// The JSON name of a field.
#[derive(Debug, Clone, Copy)]
pub enum Json {
    Skip,
    Key {
        name: &'static str,
        omitempty: bool,
        omitzero: bool,
    },
}

#[derive(Debug)]
pub struct Field {
    pub key: &'static str,
    pub ty: Ty,
    /// YAML's omitempty.
    pub omit: bool,
    pub json: Json,
}

#[derive(Debug)]
pub enum Member {
    F(Field),
    /// An embedded struct (`yaml:",inline" mapstructure:",squash"`).
    Embed(&'static Def),
    /// The `x-*` extensions, inline.
    Extensions,
}

/// A struct marshaller of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Custom {
    None,
    /// ServiceConfig: its name is not written.
    Service,
    /// SecretConfig: its content is not written.
    Secret,
    /// ConfigObjConfig: its content is not written when it came from the environment.
    Config,
}

#[derive(Debug)]
pub struct Def {
    pub members: &'static [Member],
    pub custom: Custom,
}

/// The key extensions sit under in a decoded struct, compose-go's consts.Extensions.
pub const EXTENSIONS: &str = "#extensions";

/// A decoding error as mapstructure v2 gives it: one, named by the path of what failed
/// (`'services[web].ports[0].target' ...`), or those of a struct, a map or a list
/// joined.
#[derive(Debug)]
pub enum DecodeError {
    One(String),
    Joined(Vec<DecodeError>),
}

impl DecodeError {
    fn lines(&self, out: &mut Vec<String>) {
        match self {
            DecodeError::One(text) => out.push(text.clone()),
            DecodeError::Joined(all) => all.iter().for_each(|e| e.lines(out)),
        }
    }

    /// The text Decode returns: joined errors under mapstructure's heading.
    pub fn message(&self) -> String {
        let mut lines = Vec::new();
        self.lines(&mut lines);
        let text = lines.join("\n");
        match self {
            DecodeError::Joined(_) => {
                format!("decoding failed due to the following error(s):\n\n{text}")
            }
            DecodeError::One(_) => text,
        }
    }
}

fn one(path: &str, message: impl std::fmt::Display) -> DecodeError {
    DecodeError::One(format!("'{path}' {message}"))
}

/// mapstructure's UnconvertibleTypeError.
fn unconvertible(path: &str, want: &str, value: &Value) -> DecodeError {
    one(
        path,
        format!(
            "expected type '{want}', got unconvertible type '{}'",
            value.go_type()
        ),
    )
}

/// errors.Join of what a struct, map or list gave.
fn joined(errors: Vec<DecodeError>) -> Result<(), DecodeError> {
    if errors.is_empty() {
        Ok(())
    } else {
        Err(DecodeError::Joined(errors))
    }
}

fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}.{key}")
    }
}

/// The name a Go type of an unsigned field has, and its width.
fn uint_type(ty: &Ty) -> (&'static str, u32) {
    match ty {
        Ty::Uint16 => ("uint16", 16),
        Ty::Uint32 => ("uint32", 32),
        _ => ("uint64", 64),
    }
}

/// reflect's SetUint: the value cut to the field's width.
fn truncate(value: u64, bits: u32) -> i64 {
    let value = if bits == 64 {
        value
    } else {
        value & ((1 << bits) - 1)
    };
    value as i64
}

/// Decodes `value` as `ty`, with compose-go's hooks (each type's DecodeMapstructure,
/// strings cast to the field's type, an int to a string) then mapstructure's rules;
/// `warn` hears of YAML 1.1 booleans cast to a bool.
pub fn decode(
    value: &Value,
    ty: &Ty,
    path: &str,
    warn: &mut dyn FnMut(String),
) -> Result<Value, DecodeError> {
    if value.is_null() {
        // mapstructure leaves a field zero for a null; a pointer stays nil.
        return Ok(Value::Null);
    }
    let gostr_error = |e| one(path, e);
    match ty {
        Ty::Str => match value {
            Value::Str(_) => Ok(value.clone()),
            Value::Int(i) => Ok(Value::Str(i.to_string())),
            other => Err(unconvertible(path, "string", other)),
        },
        Ty::Bool | Ty::OptOut => match value {
            Value::Bool(_) => Ok(value.clone()),
            Value::Str(s) => cast_bool(s, warn).map(Value::Bool).map_err(gostr_error),
            other => Err(unconvertible(path, "bool", other)),
        },
        Ty::Int | Ty::Int64 => match value {
            Value::Int(_) => Ok(value.clone()),
            Value::Uint(u) => Ok(Value::Int(*u as i64)),
            Value::Float(x) => Ok(Value::Int(gostr::f64_to_i64(*x))),
            Value::Str(s) => {
                let parsed = if matches!(ty, Ty::Int) {
                    gostr::atoi(s).map_err(|e| gostr::num_error("Atoi", s, e))
                } else {
                    gostr::parse_int(s, 10, 64).map_err(|e| gostr::num_error("ParseInt", s, e))
                };
                parsed.map(Value::Int).map_err(gostr_error)
            }
            other => Err(unconvertible(
                path,
                if matches!(ty, Ty::Int) {
                    "int"
                } else {
                    "int64"
                },
                other,
            )),
        },
        Ty::Uint64 | Ty::Uint32 | Ty::Uint16 => {
            let (name, bits) = uint_type(ty);
            match value {
                Value::Int(i) if *i < 0 => Err(one(
                    path,
                    format!("cannot parse value as '{name}': {i} overflows uint"),
                )),
                Value::Int(i) => Ok(Value::Int(truncate(*i as u64, bits))),
                Value::Uint(u) => Ok(Value::Int(truncate(*u, bits))),
                Value::Float(x) if *x < 0.0 => Err(one(
                    path,
                    format!("cannot parse value as '{name}': {x:.6} overflows uint"),
                )),
                Value::Float(x) => Ok(Value::Int(truncate(gostr::f64_to_u64(*x), bits))),
                other => Err(unconvertible(path, name, other)),
            }
        }
        Ty::Float32 => match value {
            Value::Int(i) => Ok(Value::Float(round32(*i as f64))),
            Value::Uint(u) => Ok(Value::Float(round32(*u as f64))),
            Value::Float(x) => Ok(Value::Float(round32(*x))),
            Value::Str(s) => gostr::parse_float(s, 32)
                .map(|x| Value::Float(round32(x)))
                .map_err(|e| gostr_error(gostr::num_error("ParseFloat", s, e))),
            other => Err(unconvertible(path, "float32", other)),
        },
        // NanoCPUs' DecodeMapstructure.
        Ty::NanoCpus => match value {
            Value::Int(i) => Ok(Value::Float(round32(*i as f64))),
            Value::Float(x) => Ok(Value::Float(round32(*x))),
            Value::Str(s) => gostr::parse_float(s, 64)
                .map(|x| Value::Float(round32(x)))
                .map_err(|e| gostr_error(gostr::num_error("ParseFloat", s, e))),
            other => Err(one(
                path,
                format!("unexpected value type {} for cpus", other.go_type()),
            )),
        },
        // Duration's: whatever the value prints as, read as a duration.
        Ty::Duration => golib::parse_duration(&value.to_string())
            .map(Value::Int)
            .map_err(gostr_error),
        // UnitBytes': an int, or a string read as an int or a size; anything else
        // leaves it zero.
        Ty::UnitBytes => match value {
            Value::Int(i) => Ok(Value::Int(*i)),
            Value::Str(s) => match gostr::parse_int(s, 10, 64) {
                Ok(n) => Ok(Value::Int(n)),
                Err(_) => golib::ram_in_bytes(s).map(Value::Int).map_err(gostr_error),
            },
            _ => Ok(Value::Null),
        },
        Ty::ShellCommand => match value {
            Value::Str(s) => golib::shell_words(s)
                .map(|words| Value::Seq(words.into_iter().map(Value::Str).collect()))
                .map_err(gostr_error),
            Value::Seq(items) => Ok(Value::Seq(
                items.iter().map(|i| Value::Str(i.to_string())).collect(),
            )),
            _ => Ok(Value::Null),
        },
        Ty::MappingWithEquals => match value {
            Value::Map(map) => Ok(Value::Map(
                map.iter()
                    .map(|(k, v)| {
                        let v = if v.is_null() {
                            Value::Null
                        } else {
                            Value::Str(v.to_string())
                        };
                        (k.clone(), v)
                    })
                    .collect(),
            )),
            Value::Seq(items) => {
                let mut out = Map::new();
                for item in items {
                    let text = item.to_string();
                    let (key, rest) = match text.split_once('=') {
                        Some((k, v)) => (k.to_string(), Some(v.to_string())),
                        None => (text.clone(), None),
                    };
                    // unicode.IsSpace of the last byte of the key, as a rune.
                    let last = gostr::to_bytes(&key).last().copied();
                    if last.is_some_and(|b| gostr::is_space(char::from(b))) {
                        return Err(one(
                            path,
                            format!("environment variable {key} is declared with a trailing space"),
                        ));
                    }
                    out.insert(key, rest.map_or(Value::Null, Value::Str));
                }
                Ok(Value::Map(out))
            }
            other => Err(one(
                path,
                format!("unexpected value type {} for mapping", other.go_type()),
            )),
        },
        Ty::Mapping | Ty::Labels => match value {
            Value::Map(map) => Ok(Value::Map(
                map.iter()
                    .map(|(k, v)| {
                        let text = if v.is_null() {
                            String::new()
                        } else {
                            v.to_string()
                        };
                        (k.clone(), Value::Str(text))
                    })
                    .collect(),
            )),
            Value::Seq(items) => Ok(Value::Map(
                items
                    .iter()
                    .map(|item| {
                        let text = item.to_string();
                        let (k, v) = text.split_once('=').unwrap_or((&text, ""));
                        (k.to_string(), Value::str(v))
                    })
                    .collect(),
            )),
            other => {
                let what = if matches!(ty, Ty::Labels) {
                    "labels"
                } else {
                    "mapping"
                };
                Err(one(
                    path,
                    format!("unexpected value type {} for {what}", other.go_type()),
                ))
            }
        },
        Ty::Options => match value {
            Value::Map(map) => Ok(Value::Map(
                map.iter()
                    .map(|(k, v)| {
                        let text = if v.is_null() {
                            String::new()
                        } else {
                            v.to_string()
                        };
                        (k.clone(), Value::Str(text))
                    })
                    .collect(),
            )),
            other => Err(one(
                path,
                format!("invalid type {} for options", other.go_type()),
            )),
        },
        Ty::MultiOptions => match value {
            Value::Map(map) => Ok(Value::Map(
                map.iter()
                    .map(|(k, v)| {
                        let values = match v {
                            Value::Seq(items) => {
                                items.iter().map(|i| Value::Str(i.to_string())).collect()
                            }
                            other => vec![Value::Str(other.to_string())],
                        };
                        (k.clone(), Value::Seq(values))
                    })
                    .collect(),
            )),
            other => Err(one(
                path,
                format!("invalid type {} for options", other.go_type()),
            )),
        },
        Ty::HostsList => decode_hosts(value).map_err(gostr_error),
        Ty::StringList => match value {
            Value::Str(s) => Ok(Value::Seq(vec![Value::Str(s.clone())])),
            Value::Seq(items) => {
                let mut out = Vec::new();
                for item in items {
                    match item {
                        Value::Str(_) => out.push(item.clone()),
                        _ => {
                            return Err(one(
                                path,
                                format!("invalid type {} for string list", value.go_type()),
                            ))
                        }
                    }
                }
                Ok(Value::Seq(out))
            }
            other => Err(one(
                path,
                format!("invalid type {} for string list", other.go_type()),
            )),
        },
        Ty::StringOrNumberList => match value {
            Value::Str(s) => Ok(Value::Seq(vec![Value::Str(s.clone())])),
            Value::Seq(items) => Ok(Value::Seq(
                items.iter().map(|i| Value::Str(i.to_string())).collect(),
            )),
            other => Err(one(
                path,
                format!("invalid type {} for string list", other.go_type()),
            )),
        },
        Ty::HealthCheckTest => match value {
            Value::Str(s) => Ok(Value::Seq(vec![
                Value::str("CMD-SHELL"),
                Value::Str(s.clone()),
            ])),
            Value::Seq(items) => Ok(Value::Seq(
                items.iter().map(|i| Value::Str(i.to_string())).collect(),
            )),
            other => Err(one(
                path,
                format!(
                    "unexpected value type {} for healthcheck.test",
                    other.go_type()
                ),
            )),
        },
        Ty::FileMode => match value {
            Value::Str(s) => gostr::parse_int(s, 8, 64)
                .map(Value::Int)
                .map_err(|e| gostr_error(gostr::num_error("ParseInt", s, e))),
            Value::Int(i) => Ok(Value::Int(*i)),
            other => Err(one(
                path,
                format!("unexpected value type {} for mode", other.go_type()),
            )),
        },
        Ty::DeviceCount => match value {
            Value::Int(i) => Ok(Value::Int(*i)),
            Value::Str(s) if gostr::to_lower_str(s) == "all" => Ok(Value::Int(-1)),
            Value::Str(s) => gostr::parse_int(s, 10, 64).map(Value::Int).map_err(|_| {
                one(
                    path,
                    format!(
                        "invalid value {}, the only value allowed is 'all' or a number",
                        golib::go_quote(s)
                    ),
                )
            }),
            other => Err(one(
                path,
                format!("invalid type {} for device count", other.go_type()),
            )),
        },
        Ty::Ulimit => match value {
            Value::Int(i) => Ok(Value::Int(*i)),
            Value::Map(map) => {
                let get = |key: &str| match map.get(key) {
                    Some(Value::Int(i)) => Value::Int(*i),
                    _ => Value::Int(0),
                };
                Ok(Value::Map(Map::from([
                    ("soft".to_string(), get("soft")),
                    ("hard".to_string(), get("hard")),
                ])))
            }
            other => Err(one(
                path,
                format!("unexpected value type {} for ulimit", other.go_type()),
            )),
        },
        Ty::SshConfig => match value {
            Value::Map(map) => Ok(Value::Seq(
                map.iter()
                    .map(|(id, path)| {
                        let mut key = Map::from([("id".to_string(), Value::str(id))]);
                        if !path.is_null() {
                            key.insert("path".to_string(), Value::Str(path.to_string()));
                        }
                        Value::Map(key)
                    })
                    .collect(),
            )),
            other => Err(one(
                path,
                format!("invalid ssh config type {}", other.go_type()),
            )),
        },
        Ty::Slice(item) => match value {
            Value::Seq(items) => {
                let mut out = Vec::with_capacity(items.len());
                let mut errors = Vec::new();
                for (i, v) in items.iter().enumerate() {
                    match decode(v, item, &format!("{path}[{i}]"), warn) {
                        Ok(v) => out.push(v),
                        Err(e) => {
                            errors.push(e);
                            out.push(Value::Null);
                        }
                    }
                }
                joined(errors)?;
                Ok(Value::Seq(out))
            }
            other => Err(one(
                path,
                format!(
                    "source data must be an array or slice, got {}",
                    go_kind(other)
                ),
            )),
        },
        Ty::Map(entry) => match value {
            Value::Map(map) => {
                let mut out = Map::new();
                let mut errors = Vec::new();
                for (k, v) in map {
                    match decode(v, entry, &format!("{path}[{k}]"), warn) {
                        Ok(v) => {
                            out.insert(k.clone(), v);
                        }
                        Err(e) => errors.push(e),
                    }
                }
                joined(errors)?;
                Ok(Value::Map(out))
            }
            other => Err(one(
                path,
                format!(
                    "expected type 'map[string]interface {{}}', got unconvertible type '{}'",
                    other.go_type()
                ),
            )),
        },
        Ty::Ptr(inner) => decode(value, inner, path, warn),
        Ty::Struct(def) => decode_struct(value, def, path, warn),
    }
}

fn go_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "invalid",
        Value::Bool(_) => "bool",
        Value::Int(_) => "int",
        Value::Uint(_) => "uint64",
        Value::Float(_) => "float64",
        Value::Str(_) => "string",
        Value::Time(_) => "struct",
        Value::Seq(_) => "slice",
        Value::Map(_) => "map",
    }
}

/// A float64 read back as the float32 it becomes, at the digits the float32 prints.
fn round32(x: f64) -> f64 {
    let narrow = x as f32;
    if !narrow.is_finite() {
        return narrow as f64;
    }
    format!("{narrow:e}").parse().unwrap_or(x)
}

fn cast_bool(text: &str, warn: &mut dyn FnMut(String)) -> Result<bool, String> {
    match text.to_lowercase().as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        "y" | "yes" | "on" => {
            warn(format!(
                "{} for boolean is not supported by YAML 1.2, please use `true`",
                golib::go_quote(text)
            ));
            Ok(true)
        }
        "n" | "no" | "off" => {
            warn(format!(
                "{} for boolean is not supported by YAML 1.2, please use `false`",
                golib::go_quote(text)
            ));
            Ok(false)
        }
        _ => Err(format!("invalid boolean: {text}")),
    }
}

/// HostsList's DecodeMapstructure: `host=ip`, `host:ip` or a mapping, several
/// addresses of a host joined, IPv6 brackets dropped.
fn decode_hosts(value: &Value) -> Result<Value, String> {
    let mut hosts: Vec<(String, Vec<String>)> = Vec::new();
    match value {
        Value::Map(map) => {
            for (host, entry) in map {
                let ips = match entry {
                    Value::Null => vec![String::new()],
                    Value::Str(s) => vec![s.clone()],
                    Value::Seq(items) => items.iter().map(|i| i.to_string()).collect(),
                    _ => {
                        return Err(format!(
                            "unexpected value type {} for extra_hosts entry",
                            value.go_type()
                        ))
                    }
                };
                hosts.push((host.clone(), ips));
            }
        }
        Value::Seq(items) => {
            for item in items {
                let text = item.to_string();
                let Some((host, ip)) = text.split_once('=').or_else(|| text.split_once(':')) else {
                    return Err(format!("invalid additional host, missing IP: {text}"));
                };
                let ips: Vec<String> = ip.split(',').map(String::from).collect();
                match hosts.iter_mut().find(|(h, _)| h == host) {
                    Some((_, existing)) => existing.extend(ips),
                    None => hosts.push((host.to_string(), ips)),
                }
            }
        }
        other => {
            return Err(format!(
                "unexpected value type {} for extra_hosts",
                other.go_type()
            ))
        }
    }
    let mut out = Map::new();
    for (host, ips) in hosts {
        if host.is_empty() || host.contains([':', '=']) {
            return Err(format!("bad host name '{host}'"));
        }
        let ips = ips
            .into_iter()
            .map(|ip| {
                if ip.len() > 2 && ip.starts_with('[') && ip.ends_with(']') {
                    ip[1..ip.len() - 1].to_string()
                } else {
                    ip
                }
            })
            .map(Value::Str)
            .collect();
        out.insert(host, Value::Seq(ips));
    }
    Ok(Value::Map(out))
}

/// Every field of a struct, embedded ones included, in their order.
fn fields(def: &'static Def, out: &mut Vec<&'static Field>) {
    for member in def.members {
        match member {
            Member::F(field) => out.push(field),
            Member::Embed(inner) => fields(inner, out),
            Member::Extensions => {}
        }
    }
}

/// The fields of a struct as mapstructure walks them: its own first, then those of
/// the structs it embeds, one after the other.
fn struct_fields(def: &'static Def, out: &mut Vec<&'static Field>) {
    let mut structs = vec![def];
    while !structs.is_empty() {
        let current = structs.remove(0);
        for member in current.members {
            match member {
                Member::F(field) => out.push(field),
                Member::Embed(inner) => structs.push(inner),
                Member::Extensions => {}
            }
        }
    }
}

fn has_extensions(def: &'static Def) -> bool {
    def.members.iter().any(|m| match m {
        Member::Extensions => true,
        Member::Embed(inner) => has_extensions(inner),
        Member::F(_) => false,
    })
}

fn decode_struct(
    value: &Value,
    def: &'static Def,
    path: &str,
    warn: &mut dyn FnMut(String),
) -> Result<Value, DecodeError> {
    let Value::Map(map) = value else {
        return Err(one(
            path,
            format!(
                "expected a map or struct, got {}",
                gostr::quote(go_kind(value))
            ),
        ));
    };
    let mut all = Vec::new();
    struct_fields(def, &mut all);
    let mut out = Map::new();
    if has_extensions(def) {
        if let Some(extensions) = map.get(EXTENSIONS) {
            out.insert(EXTENSIONS.to_string(), extensions.clone());
        }
    }
    // Field by field, in the struct's order: the key of the same name, else the first
    // equal to it but for case.
    let mut errors = Vec::new();
    for field in all {
        let entry = map.get(field.key).or_else(|| {
            map.iter()
                .find(|(k, _)| *k != EXTENSIONS && k.eq_ignore_ascii_case(field.key))
                .map(|(_, v)| v)
        });
        let Some(entry) = entry else {
            continue;
        };
        match decode(entry, &field.ty, &join(path, field.key), warn) {
            Ok(decoded) => {
                if !decoded.is_null() || matches!(field.ty, Ty::Ptr(_)) {
                    out.insert(field.key.to_string(), decoded);
                }
            }
            Err(e) => errors.push(e),
        }
    }
    joined(errors)?;
    Ok(Value::Map(out))
}

/// The output formats, which name and omit fields differently.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Yaml,
    Json,
}

/// Whether a decoded value is its type's zero, as omitempty sees it.
fn is_zero(value: Option<&Value>, ty: &Ty) -> bool {
    let Some(value) = value else {
        return true;
    };
    match ty {
        Ty::Ptr(_) => false,
        Ty::OptOut => matches!(value, Value::Bool(true)),
        Ty::ShellCommand => value.is_null(),
        Ty::Struct(def) => match value {
            Value::Map(map) => {
                let mut all = Vec::new();
                fields(def, &mut all);
                all.iter().all(|f| is_zero(map.get(f.key), &f.ty))
                    && map
                        .get(EXTENSIONS)
                        .is_none_or(|e| e.as_map().is_none_or(Map::is_empty))
            }
            _ => true,
        },
        _ => match value {
            Value::Null => true,
            Value::Bool(b) => !b,
            Value::Int(i) => *i == 0,
            Value::Uint(u) => *u == 0,
            Value::Float(x) => *x == 0.0,
            Value::Str(s) => s.is_empty(),
            Value::Seq(items) => items.is_empty(),
            Value::Map(map) => map.is_empty(),
            Value::Time(_) => false,
        },
    }
}

/// encoding/json's omitempty, which never omits a struct.
fn json_empty(value: Option<&Value>, ty: &Ty) -> bool {
    match ty {
        Ty::Struct(_) => false,
        Ty::Ptr(_) => value.is_none(),
        _ => is_zero(value, ty),
    }
}

/// Encodes a decoded value of `ty`.
pub fn encode(value: Option<&Value>, ty: &Ty, format: Format) -> Out {
    match ty {
        Ty::Ptr(inner) => match value {
            None | Some(Value::Null) => Out::Null,
            Some(v) => encode(Some(v), inner, format),
        },
        Ty::Struct(def) => encode_struct(value.and_then(Value::as_map), def, format),
        Ty::Slice(item) => match value {
            Some(Value::Seq(items)) => Out::Seq(
                items
                    .iter()
                    .map(|i| encode(Some(i), item, format))
                    .collect(),
            ),
            _ => nil_slice(format),
        },
        Ty::Map(entry) => match value {
            Some(Value::Map(map)) => Out::Dict(
                map.iter()
                    .map(|(k, v)| (k.clone(), encode(Some(v), entry, format)))
                    .collect(),
            ),
            _ => Out::Null,
        },
        Ty::Str => Out::Str(value.and_then(Value::as_str).unwrap_or("").to_string()),
        Ty::Bool | Ty::OptOut => Out::Bool(matches!(value, Some(Value::Bool(true)))),
        Ty::Int | Ty::Int64 | Ty::DeviceCount => Out::Int(int(value)),
        Ty::Uint64 | Ty::Uint32 | Ty::Uint16 => Out::Uint(int(value) as u64),
        Ty::Float32 | Ty::NanoCpus => Out::Float32(float(value) as f32),
        Ty::Duration => Out::Str(golib::duration_string(int(value))),
        Ty::UnitBytes => Out::Str(int(value).to_string()),
        Ty::FileMode => {
            // fmt's %o of an int64: a sign, then the octal digits.
            let mode = int(value);
            let sign = if mode < 0 { "-" } else { "" };
            Out::Str(format!("0{sign}{:o}", mode.unsigned_abs()))
        }
        Ty::ShellCommand | Ty::StringList | Ty::StringOrNumberList | Ty::HealthCheckTest => {
            match value {
                Some(Value::Seq(items)) => {
                    Out::Seq(items.iter().map(|i| Out::Str(i.to_string())).collect())
                }
                _ => nil_slice(format),
            }
        }
        Ty::MappingWithEquals => match value {
            Some(Value::Map(map)) => Out::Dict(
                map.iter()
                    .map(|(k, v)| {
                        let out = match v {
                            Value::Null => Out::Null,
                            other => Out::Str(other.to_string()),
                        };
                        (k.clone(), out)
                    })
                    .collect(),
            ),
            _ => Out::Null,
        },
        Ty::Mapping | Ty::Labels | Ty::Options => match value {
            Some(Value::Map(map)) => Out::Dict(
                map.iter()
                    .map(|(k, v)| (k.clone(), Out::Str(v.to_string())))
                    .collect(),
            ),
            _ => Out::Null,
        },
        Ty::MultiOptions => match value {
            Some(Value::Map(map)) => Out::Dict(
                map.iter()
                    .map(|(k, v)| {
                        let items = v
                            .as_seq()
                            .map(|s| s.iter().map(|i| Out::Str(i.to_string())).collect());
                        (k.clone(), Out::Seq(items.unwrap_or_default()))
                    })
                    .collect(),
            ),
            _ => Out::Null,
        },
        Ty::HostsList => {
            let mut list: Vec<String> = Vec::new();
            if let Some(Value::Map(map)) = value {
                for (host, ips) in map {
                    for ip in ips.as_seq().into_iter().flatten() {
                        list.push(format!("{host}={ip}"));
                    }
                }
            }
            list.sort();
            Out::Seq(list.into_iter().map(Out::Str).collect())
        }
        Ty::Ulimit => match value {
            Some(Value::Int(single)) if *single != 0 => Out::Int(*single),
            // A single 0 is no single limit: the soft and hard ones, both 0.
            Some(Value::Int(_)) => encode(Some(&Value::Map(Map::new())), ty, format),
            Some(Value::Map(map)) => {
                let get = |k: &str| match map.get(k) {
                    Some(Value::Int(i)) => *i,
                    _ => 0,
                };
                match format {
                    Format::Yaml => Out::Struct(vec![
                        ("soft".to_string(), Out::Int(get("soft"))),
                        ("hard".to_string(), Out::Int(get("hard"))),
                    ]),
                    Format::Json => Out::Struct(
                        [("soft", get("soft")), ("hard", get("hard"))]
                            .into_iter()
                            .filter(|(_, v)| *v != 0)
                            .map(|(k, v)| (k.to_string(), Out::Int(v)))
                            .collect(),
                    ),
                }
            }
            _ => Out::Struct(Vec::new()),
        },
        Ty::SshConfig => match value {
            Some(Value::Seq(keys)) => {
                let mut keys: Vec<(String, String)> = keys
                    .iter()
                    .map(|k| {
                        let get = |f: &str| k.get(&[f]).map(|v| v.to_string()).unwrap_or_default();
                        (get("id"), get("path"))
                    })
                    .collect();
                keys.sort();
                Out::Seq(
                    keys.into_iter()
                        .map(|(id, path)| {
                            Out::Str(if path.is_empty() {
                                id
                            } else {
                                format!("{id}: {path}")
                            })
                        })
                        .collect(),
                )
            }
            _ => nil_slice(format),
        },
    }
}

fn nil_slice(format: Format) -> Out {
    match format {
        Format::Yaml => Out::Seq(Vec::new()),
        Format::Json => Out::Null,
    }
}

fn int(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Int(i)) => *i,
        Some(Value::Uint(u)) => *u as i64,
        Some(Value::Float(x)) => *x as i64,
        _ => 0,
    }
}

fn float(value: Option<&Value>) -> f64 {
    match value {
        Some(Value::Int(i)) => *i as f64,
        Some(Value::Uint(u)) => *u as f64,
        Some(Value::Float(x)) => *x,
        _ => 0.0,
    }
}

/// A value of no fixed type: maps as Go maps, a timestamp as RFC 3339.
pub fn any(value: &Value) -> Out {
    match value {
        Value::Null => Out::Null,
        Value::Bool(b) => Out::Bool(*b),
        Value::Int(i) => Out::Int(*i),
        Value::Uint(u) => Out::Uint(*u),
        Value::Float(x) => Out::Float(*x),
        Value::Str(s) => Out::Str(s.clone()),
        Value::Time(t) => Out::Time(*t),
        Value::Seq(items) => Out::Seq(items.iter().map(any).collect()),
        Value::Map(map) => Out::Dict(map.iter().map(|(k, v)| (k.clone(), any(v))).collect()),
    }
}

fn encode_struct(map: Option<&Map>, def: &'static Def, format: Format) -> Out {
    let empty = Map::new();
    let map = map.unwrap_or(&empty);
    let mut entries = Vec::new();
    write_members(map, def, format, &mut entries);
    Out::Struct(entries)
}

fn write_members(map: &Map, def: &'static Def, format: Format, entries: &mut Vec<(String, Out)>) {
    for member in def.members {
        match member {
            Member::Embed(inner) => write_members(map, inner, format, entries),
            Member::Extensions => {
                // Inline, in go-yaml's order of map keys; JSON leaves them out.
                if let (Format::Yaml, Some(Value::Map(ext))) = (format, map.get(EXTENSIONS)) {
                    let mut items: Vec<(String, Out)> =
                        ext.iter().map(|(k, v)| (k.clone(), any(v))).collect();
                    items.sort_by(|a, b| super::emit::natural_order(&a.0, &b.0));
                    entries.extend(items);
                }
            }
            Member::F(field) => {
                let value = map.get(field.key);
                let hidden = match def.custom {
                    Custom::Service => field.key == "name",
                    Custom::Secret => field.key == "content",
                    Custom::Config => {
                        field.key == "content"
                            && map
                                .get("environment")
                                .and_then(Value::as_str)
                                .is_some_and(|e| !e.is_empty())
                    }
                    Custom::None => false,
                };
                let value = if hidden { None } else { value };
                match format {
                    Format::Yaml => {
                        if field.omit && is_zero(value, &field.ty) {
                            continue;
                        }
                        entries.push((field.key.to_string(), encode(value, &field.ty, format)));
                    }
                    Format::Json => {
                        let Json::Key {
                            name,
                            omitempty,
                            omitzero,
                        } = field.json
                        else {
                            continue;
                        };
                        if omitempty && json_empty(value, &field.ty) {
                            continue;
                        }
                        if omitzero && is_zero(value, &field.ty) {
                            continue;
                        }
                        entries.push((name.to_string(), encode(value, &field.ty, format)));
                    }
                }
            }
        }
    }
}
