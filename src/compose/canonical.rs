//! What compose-go does to a merged model before binding it: the long form of every
//! short syntax (transform.Canonical), the defaults (transform.SetDefaultValues), the
//! empty `dns` entries dropped (OmitEmpty) and the checks the schema cannot express
//! (validation.Validate).

use super::golib;
use super::mount;
use super::path::Path;
use super::value::{Map, Value};

type Transform = fn(Value, &Path, bool, &mut dyn FnMut(String)) -> Result<Value, String>;

fn transformer(path: &Path) -> Option<Transform> {
    const CONTAINER: &[(&str, Transform)] = &[
        ("env_file", env_file),
        ("label_file", string_or_list),
        ("gpus", gpus),
        ("networks", string_slice_to_map),
        ("models", string_slice_to_map),
        ("volumes.*", volume_mount),
        ("dns", string_or_list),
        ("devices.*", device_mapping),
        ("secrets.*", file_mount),
        ("configs.*", file_mount),
        ("ulimits.*", ulimits),
    ];
    const WORKLOAD: &[(&str, Transform)] = &[
        ("build.secrets.*", file_mount),
        ("build.provenance", string_or_x),
        ("build.sbom", string_or_x),
        ("build.additional_contexts", key_value),
        ("depends_on", depends_on),
        ("extends", extends),
        ("ports", ports),
        ("build", build),
        ("build.ssh", ssh),
        ("build.ulimits.*", ulimits),
    ];
    const TOP: &[(&str, Transform)] = &[
        ("jobs.*.triggers.schedule.*", schedule),
        ("services.*.develop.watch.*.ignore", string_or_list),
        ("services.*.develop.watch.*.include", string_or_list),
        ("volumes.*", maybe_external),
        ("networks.*", maybe_external),
        ("secrets.*", maybe_external),
        ("configs.*", maybe_external),
        ("include.*", include),
    ];
    let parts = path.parts();
    let find = |table: &[(&str, Transform)], rest: &[&str]| {
        let rest = Path::new(rest);
        table
            .iter()
            .find(|(p, _)| rest.matches_str(p))
            .map(|(_, t)| *t)
    };
    if let Some((_, t)) = TOP.iter().find(|(p, _)| path.matches_str(p)) {
        return Some(*t);
    }
    match parts.as_slice() {
        ["services" | "jobs", _] => Some(service),
        ["services", _, "pre_start", _, rest @ ..] if !rest.is_empty() => find(CONTAINER, rest),
        ["services" | "jobs", _, rest @ ..] => {
            find(CONTAINER, rest).or_else(|| find(WORKLOAD, rest))
        }
        _ => None,
    }
}

/// transform.Canonical.
pub fn canonical(
    model: Map,
    ignore_errors: bool,
    warn: &mut dyn FnMut(String),
) -> Result<Map, String> {
    match transform(Value::Map(model), &Path::root(), ignore_errors, warn)? {
        Value::Map(m) => Ok(m),
        _ => unreachable!("a mapping stays one"),
    }
}

fn transform(
    value: Value,
    path: &Path,
    ignore: bool,
    warn: &mut dyn FnMut(String),
) -> Result<Value, String> {
    if let Some(t) = transformer(path) {
        return t(value, path, ignore, warn);
    }
    walk(value, path, ignore, warn)
}

fn walk(
    value: Value,
    path: &Path,
    ignore: bool,
    warn: &mut dyn FnMut(String),
) -> Result<Value, String> {
    match value {
        Value::Map(map) => Ok(Value::Map(
            map.into_iter()
                .map(|(k, v)| {
                    let next = path.next(&k);
                    transform(v, &next, ignore, warn).map(|v| (k, v))
                })
                .collect::<Result<_, _>>()?,
        )),
        Value::Seq(items) => {
            let next = path.next("[]");
            Ok(Value::Seq(
                items
                    .into_iter()
                    .map(|i| transform(i, &next, ignore, warn))
                    .collect::<Result<_, _>>()?,
            ))
        }
        other => Ok(other),
    }
}

fn service(
    value: Value,
    path: &Path,
    ignore: bool,
    warn: &mut dyn FnMut(String),
) -> Result<Value, String> {
    match value {
        Value::Map(_) => walk(value, path, ignore, warn),
        other => Ok(other),
    }
}

fn string_or_list(
    value: Value,
    _: &Path,
    _: bool,
    _: &mut dyn FnMut(String),
) -> Result<Value, String> {
    Ok(match value {
        Value::Str(s) => Value::Seq(vec![Value::Str(s)]),
        other => other,
    })
}

fn string_or_x(
    value: Value,
    _: &Path,
    _: bool,
    _: &mut dyn FnMut(String),
) -> Result<Value, String> {
    Ok(match value {
        Value::Str(s) => Value::Str(s),
        other => Value::Str(other.to_string()),
    })
}

fn string_slice_to_map(
    value: Value,
    _: &Path,
    _: bool,
    _: &mut dyn FnMut(String),
) -> Result<Value, String> {
    Ok(match value {
        Value::Seq(items) => Value::Map(
            items
                .into_iter()
                .map(|i| (i.to_string(), Value::Null))
                .collect(),
        ),
        other => other,
    })
}

fn key_value(
    value: Value,
    path: &Path,
    ignore: bool,
    _: &mut dyn FnMut(String),
) -> Result<Value, String> {
    match value {
        Value::Map(_) => Ok(value),
        Value::Seq(ref items) => {
            let mut map = Map::new();
            for item in items {
                let text = item.to_string();
                let Some((k, v)) = text.split_once('=') else {
                    if ignore {
                        return Ok(value.clone());
                    }
                    return Err(format!("{path}: invalid value {text}, expected key=value"));
                };
                map.insert(k.to_string(), Value::str(v));
            }
            Ok(Value::Map(map))
        }
        other => Err(format!(
            "{path}: invalid type {} for key=value",
            other.go_type()
        )),
    }
}

fn env_file(
    value: Value,
    path: &Path,
    _: bool,
    _: &mut dyn FnMut(String),
) -> Result<Value, String> {
    let entry = |v: Value| match v {
        Value::Str(s) => Value::Map(Map::from([
            ("path".to_string(), Value::Str(s)),
            ("required".to_string(), Value::Bool(true)),
        ])),
        Value::Map(mut m) => {
            m.entry("required".to_string())
                .or_insert_with(|| Value::Bool(true));
            Value::Map(m)
        }
        _ => Value::Null,
    };
    match value {
        Value::Str(_) => Ok(Value::Seq(vec![entry(value)])),
        Value::Seq(items) => Ok(Value::Seq(items.into_iter().map(entry).collect())),
        other => Err(format!(
            "{path}: invalid type {} for env_file",
            other.go_type()
        )),
    }
}

fn gpus(
    value: Value,
    path: &Path,
    ignore: bool,
    warn: &mut dyn FnMut(String),
) -> Result<Value, String> {
    match value {
        Value::Seq(_) => walk(value, path, ignore, warn),
        Value::Str(_) => Ok(Value::Seq(vec![Value::Map(Map::from([(
            "count".to_string(),
            Value::str("all"),
        )]))])),
        other => Err(format!("{path}: invalid type {} for gpus", other.go_type())),
    }
}

fn volume_mount(
    value: Value,
    path: &Path,
    ignore: bool,
    _: &mut dyn FnMut(String),
) -> Result<Value, String> {
    match value {
        Value::Map(_) => Ok(value),
        Value::Str(ref spec) => {
            let parsed = match mount::parse(spec) {
                Ok(parsed) => parsed,
                Err(_) if ignore => return Ok(value.clone()),
                Err(e) => return Err(e),
            };
            Ok(Value::Map(encode_volume(&parsed)))
        }
        other => Err(format!(
            "{path}: invalid type {} for service volume mount",
            other.go_type()
        )),
    }
}

/// A parsed short volume as compose-go's mapstructure encodes its struct: the fields
/// that are set.
fn encode_volume(spec: &mount::Spec) -> Map {
    let mut map = Map::new();
    map.insert("type".to_string(), Value::str(spec.kind));
    if !spec.source.is_empty() {
        map.insert("source".to_string(), Value::str(&spec.source));
    }
    let target = if spec.target.is_empty() {
        String::new()
    } else {
        clean(&spec.target)
    };
    if !target.is_empty() {
        map.insert("target".to_string(), Value::Str(target));
    }
    if spec.read_only {
        map.insert("read_only".to_string(), Value::Bool(true));
    }
    if let Some(bind) = &spec.bind {
        let mut b = Map::new();
        if !bind.selinux.is_empty() {
            b.insert("selinux".to_string(), Value::str(&bind.selinux));
        }
        if !bind.propagation.is_empty() {
            b.insert("propagation".to_string(), Value::str(&bind.propagation));
        }
        // compose-go's encode leaves out what is zero.
        if bind.create_host_path {
            b.insert("create_host_path".to_string(), Value::Bool(true));
        }
        map.insert("bind".to_string(), Value::Map(b));
    }
    if let Some(nocopy) = spec.nocopy {
        let mut v = Map::new();
        if nocopy {
            v.insert("nocopy".to_string(), Value::Bool(true));
        }
        map.insert("volume".to_string(), Value::Map(v));
    }
    map
}

/// Go's path.Clean.
pub fn clean(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let rooted = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|p| *p != "..") {
                    parts.pop();
                } else if !rooted {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    let joined = parts.join("/");
    match (rooted, joined.is_empty()) {
        (true, _) => format!("/{joined}"),
        (false, true) => ".".to_string(),
        (false, false) => joined,
    }
}

fn device_mapping(
    value: Value,
    path: &Path,
    ignore: bool,
    _: &mut dyn FnMut(String),
) -> Result<Value, String> {
    match value {
        Value::Map(_) => Ok(value),
        Value::Str(s) => {
            let parts: Vec<&str> = s.split(':').collect();
            let (mut source, mut target, mut permissions) = ("", "", "rwm");
            match parts.len() {
                3 => (source, target, permissions) = (parts[0], parts[1], parts[2]),
                2 => (source, target) = (parts[0], parts[1]),
                1 => source = parts[0],
                _ if !ignore => {
                    return Err(format!(
                        "confusing device mapping, please use long syntax: {s}"
                    ))
                }
                _ => {}
            }
            if target.is_empty() {
                target = source;
            }
            Ok(Value::Map(Map::from([
                ("source".to_string(), Value::str(source)),
                ("target".to_string(), Value::str(target)),
                ("permissions".to_string(), Value::str(permissions)),
            ])))
        }
        other => Err(format!(
            "{path}: invalid type {} for service volume mount",
            other.go_type()
        )),
    }
}

fn file_mount(
    value: Value,
    path: &Path,
    _: bool,
    _: &mut dyn FnMut(String),
) -> Result<Value, String> {
    match value {
        Value::Map(_) => Ok(value),
        Value::Str(s) => Ok(Value::Map(Map::from([(
            "source".to_string(),
            Value::Str(s),
        )]))),
        other => Err(format!("{path}: unsupported type {}", other.go_type())),
    }
}

fn ulimits(
    value: Value,
    path: &Path,
    ignore: bool,
    _: &mut dyn FnMut(String),
) -> Result<Value, String> {
    match value {
        Value::Map(_) | Value::Int(_) => Ok(value),
        Value::Str(_) if ignore => Ok(value),
        other => Err(format!(
            "{path}: invalid type {} for ulimits",
            other.go_type()
        )),
    }
}

fn depends_on(
    value: Value,
    path: &Path,
    _: bool,
    _: &mut dyn FnMut(String),
) -> Result<Value, String> {
    let defaults = || {
        Map::from([
            ("condition".to_string(), Value::str("service_started")),
            ("required".to_string(), Value::Bool(true)),
        ])
    };
    match value {
        Value::Map(map) => {
            let mut out = Map::new();
            for (name, entry) in map {
                let Value::Map(mut dep) = entry else {
                    return Err(format!("{path}.{name}: unsupported value {}", entry));
                };
                dep.entry("condition".to_string())
                    .or_insert_with(|| Value::str("service_started"));
                dep.entry("required".to_string())
                    .or_insert_with(|| Value::Bool(true));
                out.insert(name, Value::Map(dep));
            }
            Ok(Value::Map(out))
        }
        Value::Seq(items) => Ok(Value::Map(
            items
                .into_iter()
                .map(|i| (i.to_string(), Value::Map(defaults())))
                .collect(),
        )),
        other => Err(format!(
            "{path}: invalid type {} for depend_on",
            other.go_type()
        )),
    }
}

fn extends(
    value: Value,
    path: &Path,
    ignore: bool,
    warn: &mut dyn FnMut(String),
) -> Result<Value, String> {
    match value {
        Value::Map(_) => walk(value, path, ignore, warn),
        Value::Str(s) => Ok(Value::Map(Map::from([(
            "service".to_string(),
            Value::Str(s),
        )]))),
        other => Err(format!(
            "{path}: invalid type {} for extends",
            other.go_type()
        )),
    }
}

fn build(
    value: Value,
    path: &Path,
    ignore: bool,
    warn: &mut dyn FnMut(String),
) -> Result<Value, String> {
    match value {
        Value::Map(_) => walk(value, path, ignore, warn),
        Value::Str(s) => Ok(Value::Map(Map::from([(
            "context".to_string(),
            Value::Str(s),
        )]))),
        other => Err(format!(
            "{path}: invalid type {} for build",
            other.go_type()
        )),
    }
}

fn ssh(
    value: Value,
    path: &Path,
    ignore: bool,
    _: &mut dyn FnMut(String),
) -> Result<Value, String> {
    match value {
        Value::Map(_) => Ok(value),
        Value::Seq(ref items) => {
            let mut out = Map::new();
            for item in items {
                let Value::Str(s) = item else {
                    return Err(format!("invalid ssh key type {}", item.go_type()));
                };
                match s.split_once('=') {
                    Some((id, p)) => {
                        out.insert(id.to_string(), Value::str(p));
                    }
                    None if s == "default" => {
                        out.insert(s.clone(), Value::Null);
                    }
                    None if ignore => return Ok(value.clone()),
                    None => return Err(format!("invalid ssh key {}", golib::go_quote(s))),
                }
            }
            Ok(Value::Map(out))
        }
        other => Err(format!("{path}: invalid type {} for ssh", other.go_type())),
    }
}

fn schedule(
    value: Value,
    path: &Path,
    _: bool,
    _: &mut dyn FnMut(String),
) -> Result<Value, String> {
    match value {
        Value::Str(s) => Ok(Value::Map(Map::from([("cron".to_string(), Value::Str(s))]))),
        Value::Map(_) => Ok(value),
        other => Err(format!(
            "{path}: invalid type {} for schedule entry",
            other.go_type()
        )),
    }
}

fn include(value: Value, path: &Path, _: bool, _: &mut dyn FnMut(String)) -> Result<Value, String> {
    match value {
        Value::Map(_) => Ok(value),
        Value::Str(s) => Ok(Value::Map(Map::from([("path".to_string(), Value::Str(s))]))),
        other => Err(format!(
            "{path}: invalid type {} for external",
            other.go_type()
        )),
    }
}

/// A top-level resource, `external: {name: x}` rewritten as `external: true` and
/// `name: x`.
fn maybe_external(
    value: Value,
    path: &Path,
    ignore: bool,
    warn: &mut dyn FnMut(String),
) -> Result<Value, String> {
    if value.is_null() {
        return Ok(Value::Null);
    }
    if !matches!(value, Value::Map(_)) {
        return Err(format!("{path}: must be a mapping"));
    }
    let Value::Map(mut resource) = walk(value, path, ignore, warn)? else {
        unreachable!("a mapping stays one");
    };
    if let Some(Value::Map(external)) = resource.get("external").cloned() {
        resource.insert("external".to_string(), Value::Bool(true));
        if let Some(name) = external.get("name") {
            warn(format!(
                "{path}: external.name is deprecated. Please set name and external: true"
            ));
            match resource.get("name") {
                Some(own) if own != name => {
                    return Err(format!(
                        "{path}: name and external.name conflict; only use name"
                    ))
                }
                Some(_) => {}
                None => {
                    resource.insert("name".to_string(), name.clone());
                }
            }
        }
    }
    Ok(Value::Map(resource))
}

fn ports(
    value: Value,
    path: &Path,
    ignore: bool,
    _: &mut dyn FnMut(String),
) -> Result<Value, String> {
    let Value::Seq(entries) = value else {
        return Err(format!("{path}: invalid type {} for port", value.go_type()));
    };
    let mut out = Vec::new();
    for entry in &entries {
        let spec = match entry {
            Value::Int(i) => i.to_string(),
            Value::Str(s) => s.clone(),
            Value::Map(_) => {
                out.push(entry.clone());
                continue;
            }
            other => return Err(format!("{path}: invalid type {} for port", other.go_type())),
        };
        let parsed = match golib::parse_port_spec(&spec) {
            Ok(parsed) => parsed,
            Err(_) if ignore && matches!(entry, Value::Str(_)) => return Ok(Value::Seq(entries)),
            Err(e) => return Err(e),
        };
        for binding in parsed {
            let mut map = Map::new();
            map.insert("mode".to_string(), Value::str("ingress"));
            if !binding.host_ip.is_empty() {
                map.insert("host_ip".to_string(), Value::Str(binding.host_ip));
            }
            if binding.target != 0 {
                map.insert("target".to_string(), Value::Int(binding.target as i64));
            }
            if !binding.host_port.is_empty() {
                map.insert("published".to_string(), Value::Str(binding.host_port));
            }
            map.insert("protocol".to_string(), Value::Str(binding.protocol));
            out.push(Value::Map(map));
        }
    }
    Ok(Value::Seq(out))
}

type Default = fn(Value, &Path) -> Result<Value, String>;

fn default_for(path: &Path) -> Option<Default> {
    let parts = path.parts();
    match parts.as_slice() {
        ["services", _, "deploy", "resources", "reservations", "devices", _] => {
            Some(device_request)
        }
        ["services" | "jobs", _, rest @ ..] => match rest {
            ["build"] => Some(build_context),
            ["secrets", _] => Some(secret_mount),
            ["ports", _] => Some(port_defaults),
            ["gpus", _] => Some(device_request),
            ["volumes", _, "bind"] => Some(volume_bind),
            _ => None,
        },
        _ => None,
    }
}

/// transform.SetDefaultValues.
pub fn set_defaults(model: Map) -> Result<Map, String> {
    match defaults(Value::Map(model), &Path::root())? {
        Value::Map(m) => Ok(m),
        _ => unreachable!("a mapping stays one"),
    }
}

fn defaults(value: Value, path: &Path) -> Result<Value, String> {
    if let Some(d) = default_for(path) {
        return d(value, path);
    }
    match value {
        Value::Map(map) => Ok(Value::Map(
            map.into_iter()
                .map(|(k, v)| {
                    let next = path.next(&k);
                    defaults(v, &next).map(|v| (k, v))
                })
                .collect::<Result<_, _>>()?,
        )),
        Value::Seq(items) => {
            let next = path.next("[]");
            Ok(Value::Seq(
                items
                    .into_iter()
                    .map(|i| defaults(i, &next))
                    .collect::<Result<_, _>>()?,
            ))
        }
        other => Ok(other),
    }
}

fn build_context(value: Value, _: &Path) -> Result<Value, String> {
    Ok(match value {
        Value::Map(mut m) => {
            m.entry("context".to_string())
                .or_insert_with(|| Value::str("."));
            Value::Map(m)
        }
        other => other,
    })
}

fn secret_mount(value: Value, path: &Path) -> Result<Value, String> {
    match value {
        Value::Map(mut m) => {
            if !m.contains_key("target") {
                let source = m
                    .get("source")
                    .map_or_else(|| "%!s(<nil>)".to_string(), |s| s.to_string());
                m.insert(
                    "target".to_string(),
                    Value::Str(format!("/run/secrets/{source}")),
                );
            }
            Ok(Value::Map(m))
        }
        other => Err(format!("{path}: unsupported type {}", other.go_type())),
    }
}

fn port_defaults(value: Value, _: &Path) -> Result<Value, String> {
    Ok(match value {
        Value::Map(mut m) => {
            m.entry("protocol".to_string())
                .or_insert_with(|| Value::str("tcp"));
            m.entry("mode".to_string())
                .or_insert_with(|| Value::str("ingress"));
            Value::Map(m)
        }
        other => other,
    })
}

fn device_request(value: Value, path: &Path) -> Result<Value, String> {
    match value {
        Value::Map(mut m) => {
            if !m.contains_key("count") && !m.contains_key("device_ids") {
                m.insert("count".to_string(), Value::str("all"));
            }
            Ok(Value::Map(m))
        }
        other => Err(format!(
            "{path}: invalid type {} for device request",
            other.go_type()
        )),
    }
}

fn volume_bind(value: Value, path: &Path) -> Result<Value, String> {
    match value {
        Value::Map(mut m) => {
            m.entry("create_host_path".to_string())
                .or_insert_with(|| Value::Bool(true));
            Ok(Value::Map(m))
        }
        other => Err(format!(
            "{path}: invalid type {} for service volume bind",
            other.go_type()
        )),
    }
}

/// OmitEmpty: the empty entries of a service's dns.
pub fn omit_empty(mut model: Map) -> Map {
    for key in ["services", "jobs"] {
        if let Some(Value::Map(containers)) = model.get_mut(key) {
            for container in containers.values_mut() {
                if let Some(Value::Seq(dns)) = container.as_map_mut().and_then(|c| c.get_mut("dns"))
                {
                    dns.retain(|e| !(e.is_null() || e.as_str() == Some("")));
                }
            }
        }
    }
    model
}

/// validation.Validate: what the schema cannot say.
pub fn validate(model: &Map) -> Result<(), String> {
    check(&Value::Map(model.clone()), &Path::root())
}

fn check(value: &Value, path: &Path) -> Result<(), String> {
    let parts = path.parts();
    match parts.as_slice() {
        ["volumes", _] => return check_volume(value, path),
        ["configs", _] => {
            return check_file_object(value, path, &["file", "environment", "content"])
        }
        ["secrets", _] => return check_file_object(value, path, &["file", "environment"]),
        ["services", _, "ports", _] => return check_ip_address(value, path),
        ["services", _, "develop", "watch", _, "path"] => {
            if value.as_str() == Some("") {
                return Err(format!("{path}: value can't be blank"));
            }
            return Ok(());
        }
        ["services", _, "deploy", "resources", "reservations", "devices", _]
        | ["services", _, "gpus", _] => {
            if let Some(m) = value.as_map() {
                if m.contains_key("count") && m.contains_key("device_ids") {
                    return Err(format!(
                        "{path}: \"count\" and \"device_ids\" attributes are exclusive"
                    ));
                }
            }
            return Ok(());
        }
        _ => {}
    }
    match value {
        Value::Map(map) => {
            for (k, v) in map {
                check(v, &path.next(k))?;
            }
        }
        Value::Seq(items) => {
            for item in items {
                check(item, &path.next("[]"))?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn check_volume(value: &Value, path: &Path) -> Result<(), String> {
    let Some(map) = value.as_map() else {
        return if value.is_null() {
            Ok(())
        } else {
            Err(format!("expected volume, got {value}"))
        };
    };
    if map.get("external") != Some(&Value::Bool(true)) {
        return Ok(());
    }
    for key in map.keys() {
        if key == "name"
            || key == "external"
            || key == super::model::EXTENSIONS
            || key.starts_with("x-")
        {
            continue;
        }
        return Err(format!(
            "{path}: conflicting parameters \"external\" and {} specified",
            golib::go_quote(key)
        ));
    }
    Ok(())
}

fn check_file_object(value: &Value, path: &Path, keys: &[&str]) -> Result<(), String> {
    let Some(map) = value.as_map() else {
        return Ok(());
    };
    let count = keys.iter().filter(|k| map.contains_key(**k)).count();
    if count > 1 {
        return Err(format!(
            "{path}: {} attributes are mutually exclusive",
            keys.join("|")
        ));
    }
    if count == 0 && !map.contains_key("driver") && !map.contains_key("external") {
        return Err(format!("{path}: one of {} must be set", keys.join("|")));
    }
    Ok(())
}

fn check_ip_address(value: &Value, path: &Path) -> Result<(), String> {
    if let Some(Value::Str(ip)) = value.as_map().and_then(|m| m.get("host_ip")) {
        if ip.parse::<std::net::IpAddr>().is_err() {
            return Err(format!("{path}: invalid ip address: {ip}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_clean_as_go_cleans_them() {
        for (path, clean_path) in [
            ("/data/", "/data"),
            ("/a/./b/../c", "/a/c"),
            ("a//b", "a/b"),
            ("../x", "../x"),
            ("/..", "/"),
            ("", "."),
        ] {
            assert_eq!(clean(path), clean_path, "{path}");
        }
    }

    #[test]
    fn short_syntaxes_take_their_long_form() {
        let mut docs = super::super::yaml::parse(
            "services:\n  web:\n    ports: [\"127.0.0.1:8080:80\", 9000]\n    volumes: [\"./data:/data/:ro\"]\n    \
             depends_on: [db]\n    env_file: .env\n    devices: [\"/dev/a\"]\n    secrets: [s]\n",
        )
        .unwrap();
        let mut doc = docs.remove(0);
        super::super::yaml::take_resets(&mut doc).unwrap();
        let Value::Map(model) = super::super::yaml::construct(&doc).unwrap() else {
            unreachable!()
        };
        let model = set_defaults(canonical(model, false, &mut |_| {}).unwrap()).unwrap();
        let model = Value::Map(model);
        let web = model.get(&["services", "web"]).unwrap();
        let ports = web.get(&["ports"]).unwrap().as_seq().unwrap();
        assert_eq!(ports[0].get(&["published"]), Some(&Value::str("8080")));
        assert_eq!(ports[0].get(&["target"]), Some(&Value::Int(80)));
        assert_eq!(ports[1].get(&["protocol"]), Some(&Value::str("tcp")));
        let volume = &web.get(&["volumes"]).unwrap().as_seq().unwrap()[0];
        assert_eq!(volume.get(&["target"]), Some(&Value::str("/data")));
        assert_eq!(
            volume.get(&["bind"]),
            Some(&Value::Map(Map::from([(
                "create_host_path".to_string(),
                Value::Bool(true)
            )])))
        );
        assert_eq!(
            web.get(&["depends_on", "db", "condition"]),
            Some(&Value::str("service_started"))
        );
        assert_eq!(
            web.get(&["env_file"]).unwrap().as_seq().unwrap()[0].get(&["required"]),
            Some(&Value::Bool(true))
        );
        assert_eq!(
            web.get(&["devices"]).unwrap().as_seq().unwrap()[0].get(&["permissions"]),
            Some(&Value::str("rwm"))
        );
        assert_eq!(
            web.get(&["secrets"]).unwrap().as_seq().unwrap()[0].get(&["target"]),
            Some(&Value::str("/run/secrets/s"))
        );
    }
}
