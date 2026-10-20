//! How a compose file applies over the ones before it: compose-go's override.MergeYaml.
//! Mappings merge key by key, lists append what they do not hold yet, a scalar
//! replaces, and a null never clears. Some attributes have rules of their own: a
//! command replaces, environment and labels merge by key, depends_on and networks
//! merge as mappings, logging only with the same driver.

use super::path::Path;
use super::unicity::{key_value_indexer, unique_entries};
use super::value::{Map, Value};

type Merger = fn(Value, Value, &Path) -> Result<Value, String>;

fn special(path: &Path) -> Option<Merger> {
    const TOP: &[(&str, Merger)] = &[
        ("networks.*.ipam.config", merge_ipam_config),
        ("networks.*.labels", merge_key_value_sequence),
        ("volumes.*.labels", merge_key_value_sequence),
        ("secrets.*.labels", merge_key_value_sequence),
        ("configs.*.labels", merge_key_value_sequence),
        ("services.*.deploy.labels", merge_key_value_sequence),
    ];
    const WORKLOAD: &[(&str, Merger)] = &[
        ("annotations", merge_key_value_sequence),
        ("build", merge_build),
        ("build.args", merge_key_value_sequence),
        ("build.additional_contexts", merge_key_value_sequence),
        ("build.extra_hosts", merge_to_sequence),
        ("build.labels", merge_key_value_sequence),
        ("command", replace),
        ("depends_on", merge_depends_on),
        ("dns", merge_to_sequence),
        ("dns_opt", merge_to_sequence),
        ("dns_search", merge_to_sequence),
        ("entrypoint", replace),
        ("env_file", merge_to_sequence),
        ("label_file", merge_to_sequence),
        ("environment", merge_key_value_sequence),
        ("extra_hosts", merge_to_sequence),
        ("healthcheck.test", replace),
        ("labels", merge_key_value_sequence),
        ("volumes.*.volume.labels", merge_key_value_sequence),
        ("logging", merge_logging),
        ("models", merge_as_mapping_without_defaults),
        ("networks", merge_as_mapping_without_defaults),
        ("sysctls", merge_key_value_sequence),
        ("tmpfs", merge_to_sequence),
        ("ulimits.*", merge_ulimit),
    ];
    if let Some((_, merger)) = TOP.iter().find(|(p, _)| path.matches_str(p)) {
        return Some(*merger);
    }
    let parts = path.parts();
    if parts.len() > 2 && matches!(parts[0], "services" | "jobs") {
        let rest = Path::new(&parts[2..]);
        return WORKLOAD
            .iter()
            .find(|(p, _)| rest.matches_str(p))
            .map(|(_, m)| *m);
    }
    None
}

/// Applies `over` over `base`, the model of the files before it.
pub fn merge(base: Value, over: Value) -> Result<Value, String> {
    merge_at(base, over, &Path::root())
}

pub fn merge_at(base: Value, over: Value, path: &Path) -> Result<Value, String> {
    if let Some(merger) = special(path) {
        return merger(base, over, path);
    }
    if over.is_null() {
        return Ok(base);
    }
    match base {
        Value::Map(base) => match over {
            Value::Map(over) => merge_mappings(base, over, path).map(Value::Map),
            _ => Err(format!("cannot override {path}")),
        },
        Value::Seq(base) => match over {
            Value::Seq(over) => Ok(Value::Seq(append_without_duplicates(base, over))),
            _ => Err(format!("cannot override {path}")),
        },
        _ => Ok(over),
    }
}

fn merge_mappings(mut base: Map, over: Map, path: &Path) -> Result<Map, String> {
    for (key, value) in over {
        match base.remove(&key) {
            None => {
                base.insert(key, value);
            }
            Some(existing) => {
                let merged = merge_at(existing, value, &path.next(&key))?;
                base.insert(key, merged);
            }
        }
    }
    Ok(base)
}

/// The type of a value as Go's %T names it, for the messages that show it.
fn go_type(value: &Value) -> &'static str {
    value.go_type()
}

fn merge_logging(base: Value, over: Value, path: &Path) -> Result<Value, String> {
    let Value::Map(base) = base else {
        return Err(format!(
            "{path}: cannot merge logging: expected a mapping, got {}",
            go_type(&base)
        ));
    };
    let Value::Map(over) = over else {
        return Err(format!(
            "{path}: cannot merge logging: expected a mapping, got {}",
            go_type(&over)
        ));
    };
    // The options merge when both name the same driver, or one names none.
    match (over.get("driver"), base.get("driver")) {
        (Some(driver), Some(base_driver)) if driver != base_driver => Ok(Value::Map(over)),
        _ => merge_mappings(base, over, path).map(Value::Map),
    }
}

fn merge_build(base: Value, over: Value, path: &Path) -> Result<Value, String> {
    let as_build = |value: Value| match value {
        Value::Str(context) => Map::from([("context".to_string(), Value::Str(context))]),
        Value::Map(map) => map,
        _ => Map::new(),
    };
    merge_mappings(as_build(base), as_build(over), path).map(Value::Map)
}

fn merge_depends_on(base: Value, over: Value, path: &Path) -> Result<Value, String> {
    let defaults = Map::from([
        ("condition".to_string(), Value::str("service_started")),
        ("required".to_string(), Value::Bool(true)),
    ]);
    merge_as_mapping(base, over, Some(&defaults), path)
}

fn merge_as_mapping_without_defaults(
    base: Value,
    over: Value,
    path: &Path,
) -> Result<Value, String> {
    merge_as_mapping(base, over, None, path)
}

fn merge_as_mapping(
    base: Value,
    over: Value,
    defaults: Option<&Map>,
    path: &Path,
) -> Result<Value, String> {
    let base = into_mapping(null_as_empty(base), defaults, path)?;
    let over = into_mapping(null_as_empty(over), defaults, path)?;
    merge_mappings(base, over, path).map(Value::Map)
}

fn merge_to_sequence(base: Value, over: Value, _: &Path) -> Result<Value, String> {
    // Go's nil slice, which nothing appended to stays: a null the schema refuses.
    let mut merged = go_sequence(base);
    for value in go_sequence(over).unwrap_or_default() {
        let entries = merged.get_or_insert_with(Vec::new);
        if !entries.contains(&value) {
            entries.push(value);
        }
    }
    Ok(merged.map_or(Value::Null, Value::Seq))
}

/// convertIntoSequence, with Go's nil for a mapping without entries and for what is
/// neither a mapping, a list nor a string.
fn go_sequence(value: Value) -> Option<Vec<Value>> {
    match value {
        Value::Map(map) if map.is_empty() => None,
        Value::Map(_) | Value::Seq(_) | Value::Str(_) => Some(into_sequence(value)),
        _ => None,
    }
}

/// Appends the entries of `over` that `base` does not hold already: two identical
/// entries never mean more than one.
pub fn append_without_duplicates(mut base: Vec<Value>, over: Vec<Value>) -> Vec<Value> {
    for value in over {
        if !base.contains(&value) {
            base.push(value);
        }
    }
    base
}

/// KEY=VALUE lists merge by key: an entry of `over` replaces the one of the same key
/// in place, new keys come at the end.
fn merge_key_value_sequence(base: Value, over: Value, path: &Path) -> Result<Value, String> {
    let mut entries = into_sequence(base);
    entries.extend(into_sequence(over));
    unique_entries(entries, key_value_indexer, path).map(Value::Seq)
}

/// A mapping as the KEY=VALUE list it stands for (sorted, a null value as the key
/// alone, a list value as one entry per item); a string as a list of itself.
pub fn into_sequence(value: Value) -> Vec<Value> {
    match value {
        Value::Map(map) => {
            let mut seq: Vec<String> = Vec::new();
            for (key, value) in map {
                match value {
                    Value::Null => seq.push(key),
                    Value::Seq(items) => {
                        seq.extend(items.iter().map(|item| format!("{key}={item}")));
                    }
                    other => seq.push(format!("{key}={other}")),
                }
            }
            seq.sort();
            seq.into_iter().map(Value::Str).collect()
        }
        Value::Seq(items) => items,
        Value::Str(s) => vec![Value::Str(s)],
        _ => Vec::new(),
    }
}

fn merge_ulimit(base: Value, over: Value, path: &Path) -> Result<Value, String> {
    match (base, over) {
        (Value::Map(base), Value::Map(over)) => merge_mappings(base, over, path).map(Value::Map),
        (_, over) => Ok(over),
    }
}

/// IPAM config entries merge by subnet, as compose-go's mergeIPAMConfig does it with
/// Go's maps, which are references: an entry it keeps is the very map a later override
/// merges into, subnet and all.
fn merge_ipam_config(base: Value, over: Value, path: &Path) -> Result<Value, String> {
    use std::cell::RefCell;
    use std::rc::Rc;
    let Value::Seq(configs) = base else {
        return Err(format!("{path}: unexpected type {}", go_type(&base)));
    };
    let Value::Seq(overrides) = over else {
        return Err(format!("{path}: unexpected type {}", go_type(&over)));
    };
    let overrides = overrides
        .into_iter()
        .map(|o| into_mapping(o, None, path).map(|m| Rc::new(RefCell::new(m))))
        .collect::<Result<Vec<_>, _>>()?;
    let subnet = |m: &Rc<RefCell<Map>>| m.borrow().get("subnet").cloned().unwrap_or(Value::Null);
    let mut merged_configs: Vec<Rc<RefCell<Map>>> = Vec::new();
    for original in configs {
        let right = Rc::new(RefCell::new(into_mapping(original, None, path)?));
        for left in &overrides {
            let left_subnet = subnet(left);
            if left_subnet != subnet(&right)
                && !merged_configs.iter().any(|c| subnet(c) == left_subnet)
            {
                merged_configs.push(left.clone());
                continue;
            }
            let merged = merge_mappings(right.borrow().clone(), left.borrow().clone(), path)?;
            *right.borrow_mut() = merged;
            let merged_subnet = subnet(&right);
            match merged_configs
                .iter()
                .position(|c| subnet(c) == merged_subnet)
            {
                Some(at) => merged_configs[at] = right.clone(),
                None => merged_configs.push(right.clone()),
            }
        }
    }
    if merged_configs.is_empty() {
        // Go's nil slice.
        return Ok(Value::Null);
    }
    Ok(Value::Seq(
        merged_configs
            .iter()
            .map(|c| Value::Map(c.borrow().clone()))
            .collect(),
    ))
}

fn null_as_empty(value: Value) -> Value {
    if value.is_null() {
        Value::Map(Map::new())
    } else {
        value
    }
}

fn into_mapping(value: Value, defaults: Option<&Map>, path: &Path) -> Result<Map, String> {
    let entry = || defaults.map_or(Value::Null, |d| Value::Map(d.clone()));
    match value {
        Value::Map(map) => Ok(map),
        Value::Str(key) => Ok(Map::from([(key, entry())])),
        Value::Seq(items) => {
            let mut map = Map::new();
            for item in items {
                let Value::Str(key) = item else {
                    return Err(format!(
                        "{path}: cannot use {} as a mapping key",
                        go_type(&item)
                    ));
                };
                map.insert(key, entry());
            }
            Ok(map)
        }
        other => Err(format!(
            "{path}: cannot convert {} into a mapping",
            go_type(&other)
        )),
    }
}

fn replace(_: Value, over: Value, _: &Path) -> Result<Value, String> {
    Ok(over)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yaml(text: &str) -> Value {
        let mut docs = super::super::yaml::parse(text).unwrap();
        let mut doc = docs.remove(0);
        super::super::yaml::take_resets(&mut doc).unwrap();
        super::super::yaml::construct(&doc).unwrap()
    }

    fn merged(base: &str, over: &str) -> Value {
        merge(yaml(base), yaml(over)).unwrap()
    }

    #[test]
    fn scalars_replace_and_nulls_keep() {
        let m = merged(
            "services:\n  web:\n    image: a\n    init: true\n",
            "services:\n  web:\n    image: b\n    init:\n",
        );
        let web = m.get(&["services", "web"]).unwrap();
        assert_eq!(web.get(&["image"]), Some(&Value::str("b")));
        assert_eq!(web.get(&["init"]), Some(&Value::Bool(true)));
    }

    #[test]
    fn commands_replace_and_lists_append() {
        let m = merged(
            "services:\n  web:\n    command: [a, b]\n    cap_add: [A, B]\n",
            "services:\n  web:\n    command: [c]\n    cap_add: [B, C]\n",
        );
        let web = m.get(&["services", "web"]).unwrap();
        assert_eq!(
            web.get(&["command"]),
            Some(&Value::Seq(vec![Value::str("c")]))
        );
        assert_eq!(
            web.get(&["cap_add"]),
            Some(&Value::Seq(vec![
                Value::str("A"),
                Value::str("B"),
                Value::str("C")
            ]))
        );
    }

    #[test]
    fn environments_merge_by_key() {
        let m = merged(
            "services:\n  web:\n    environment:\n      A: 1\n      MODE: release\n",
            "services:\n  web:\n    environment: [MODE=debug, B=2]\n",
        );
        assert_eq!(
            m.get(&["services", "web", "environment"]),
            Some(&Value::Seq(vec![
                Value::str("A=1"),
                Value::str("MODE=debug"),
                Value::str("B=2")
            ]))
        );
    }

    #[test]
    fn depends_on_merges_as_a_mapping() {
        let m = merged(
            "services:\n  web:\n    depends_on:\n      db:\n        condition: service_healthy\n",
            "services:\n  web:\n    depends_on: [db, cache]\n",
        );
        let deps = m.get(&["services", "web", "depends_on"]).unwrap();
        // A list item resets the long form to the defaults.
        assert_eq!(
            deps.get(&["db", "condition"]),
            Some(&Value::str("service_started"))
        );
        assert_eq!(deps.get(&["cache", "required"]), Some(&Value::Bool(true)));
    }

    #[test]
    fn logging_merges_with_the_same_driver_only() {
        let same = merged(
            "services:\n  web:\n    logging:\n      driver: local\n      options: {a: 1}\n",
            "services:\n  web:\n    logging:\n      options: {b: 2}\n",
        );
        let options = same
            .get(&["services", "web", "logging", "options"])
            .unwrap();
        assert_eq!(options.as_map().unwrap().len(), 2);
        let other = merged(
            "services:\n  web:\n    logging:\n      driver: local\n      options: {a: 1}\n",
            "services:\n  web:\n    logging:\n      driver: syslog\n",
        );
        assert!(other
            .get(&["services", "web", "logging", "options"])
            .is_none());
    }

    #[test]
    fn a_type_change_is_refused() {
        let err = merge(
            yaml("services:\n  web:\n    ports: [80]\n"),
            yaml("services:\n  web:\n    ports: {a: 1}\n"),
        )
        .unwrap_err();
        assert_eq!(err, "cannot override services.web.ports");
    }

    #[test]
    fn ipam_configs_merge_by_subnet() {
        let m = merged(
            "networks:\n  n:\n    ipam:\n      config:\n        - subnet: 10.0.0.0/24\n          gateway: 10.0.0.1\n",
            "networks:\n  n:\n    ipam:\n      config:\n        - subnet: 10.0.0.0/24\n          ip_range: 10.0.0.0/25\n        - subnet: 10.1.0.0/24\n",
        );
        let configs = m
            .get(&["networks", "n", "ipam", "config"])
            .unwrap()
            .as_seq()
            .unwrap();
        assert_eq!(configs.len(), 2);
        assert_eq!(configs[0].get(&["gateway"]), Some(&Value::str("10.0.0.1")));
        assert_eq!(
            configs[0].get(&["ip_range"]),
            Some(&Value::str("10.0.0.0/25"))
        );
    }
}
