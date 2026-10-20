//! compose-go's EnforceUnicity: the lists of a service that name a thing once (a
//! mount by its target, a port, a label by its key) keep one entry per thing, the last
//! one written, at the place of the first.

use super::mount;
use super::path::Path;
use super::value::Value;

pub type Indexer = fn(&Value, &Path) -> Result<String, String>;

fn indexer(path: &Path) -> Option<Indexer> {
    const UNIQUE: &[(&str, Indexer)] = &[
        ("networks.*.labels", key_value_indexer),
        ("networks.*.ipam.options", key_value_indexer),
        ("services.*.annotations", key_value_indexer),
        ("services.*.build.args", key_value_indexer),
        ("services.*.build.additional_contexts", key_value_indexer),
        ("services.*.build.platform", key_value_indexer),
        ("services.*.build.tags", key_value_indexer),
        ("services.*.build.labels", key_value_indexer),
        ("services.*.cap_add", key_value_indexer),
        ("services.*.cap_drop", key_value_indexer),
        ("services.*.configs", config_indexer),
        ("services.*.deploy.labels", key_value_indexer),
        ("services.*.dns", key_value_indexer),
        ("services.*.dns_opt", key_value_indexer),
        ("services.*.dns_search", key_value_indexer),
        ("services.*.environment", key_value_indexer),
        ("services.*.env_file", env_file_indexer),
        ("services.*.expose", expose_indexer),
        ("services.*.labels", key_value_indexer),
        ("services.*.links", key_value_indexer),
        ("services.*.networks.*.aliases", key_value_indexer),
        ("services.*.networks.*.link_local_ips", key_value_indexer),
        ("services.*.ports", port_indexer),
        ("services.*.profiles", key_value_indexer),
        ("services.*.secrets", secret_indexer),
        ("services.*.sysctls", key_value_indexer),
        ("services.*.tmpfs", key_value_indexer),
        ("services.*.volumes", volume_indexer),
        ("services.*.devices", device_indexer),
    ];
    UNIQUE
        .iter()
        .find(|(pattern, _)| path.matches_str(pattern))
        .map(|(_, indexer)| *indexer)
}

pub fn enforce(value: Value) -> Result<Value, String> {
    enforce_at(value, &Path::root())
}

fn enforce_at(value: Value, path: &Path) -> Result<Value, String> {
    match value {
        Value::Map(map) => Ok(Value::Map(
            map.into_iter()
                .map(|(key, entry)| {
                    let next = path.next(&key);
                    enforce_at(entry, &next).map(|entry| (key, entry))
                })
                .collect::<Result<_, _>>()?,
        )),
        Value::Seq(items) => match indexer(path) {
            Some(indexer) => unique_entries(items, indexer, path).map(Value::Seq),
            None => Ok(Value::Seq(items)),
        },
        other => Ok(other),
    }
}

/// One entry per key: the last entry for a key, at the position of the first.
pub fn unique_entries(
    entries: Vec<Value>,
    indexer: Indexer,
    path: &Path,
) -> Result<Vec<Value>, String> {
    let mut seq: Vec<Value> = Vec::new();
    let mut keys: Vec<(String, usize)> = Vec::new();
    for (i, entry) in entries.into_iter().enumerate() {
        let key = indexer(&entry, &path.next(&format!("[{i}]")))?;
        match keys.iter().find(|(k, _)| *k == key) {
            Some((_, at)) => seq[*at] = entry,
            None => {
                keys.push((key, seq.len()));
                seq.push(entry);
            }
        }
    }
    Ok(seq)
}

/// Go's %s of a value, nil for none.
fn go_s(value: Option<&Value>) -> String {
    value.map_or_else(|| "%!s(<nil>)".to_string(), super::golib::go_s)
}

pub fn key_value_indexer(value: &Value, path: &Path) -> Result<String, String> {
    match value {
        Value::Str(s) => Ok(s.split_once('=').map_or(s.as_str(), |(k, _)| k).to_string()),
        other => Err(format!("{path}: unexpected type {}", other.go_type())),
    }
}

fn volume_indexer(value: &Value, path: &Path) -> Result<String, String> {
    match value {
        Value::Map(map) => match map.get("target") {
            Some(Value::Str(target)) => Ok(target.clone()),
            _ => Err(format!("service volume {path} is missing a mount target")),
        },
        Value::Str(spec) => mount::parse(spec).map(|volume| volume.target),
        _ => Ok(String::new()),
    }
}

fn device_indexer(value: &Value, path: &Path) -> Result<String, String> {
    match value {
        Value::Map(map) => match map.get("target") {
            Some(Value::Str(target)) => Ok(target.clone()),
            _ => Err(format!("service device {path} is missing a mount target")),
        },
        Value::Str(spec) => {
            let parts: Vec<&str> = spec.split(':').collect();
            Ok(parts.get(1).unwrap_or(&parts[0]).to_string())
        }
        _ => Ok(String::new()),
    }
}

fn expose_indexer(value: &Value, path: &Path) -> Result<String, String> {
    match value {
        Value::Str(s) => Ok(s.clone()),
        Value::Int(i) => Ok(i.to_string()),
        other => Err(format!(
            "{path}: unsupported expose value {}",
            go_s(Some(other))
        )),
    }
}

fn mount_indexer(default: &str, value: &Value, path: &Path) -> Result<String, String> {
    match value {
        Value::Str(s) => Ok(format!("{default}/{s}")),
        Value::Map(map) => match map.get("target") {
            Some(target) => Ok(go_s(Some(target))),
            None => Ok(format!("{default}/{}", go_s(map.get("source")))),
        },
        other => Err(format!(
            "{path}: unsupported expose value {}",
            go_s(Some(other))
        )),
    }
}

fn config_indexer(value: &Value, path: &Path) -> Result<String, String> {
    mount_indexer("", value, path)
}

fn secret_indexer(value: &Value, path: &Path) -> Result<String, String> {
    mount_indexer("/run/secrets", value, path)
}

fn port_indexer(value: &Value, path: &Path) -> Result<String, String> {
    match value {
        Value::Int(i) => Ok(i.to_string()),
        Value::Map(map) => {
            let Some(target) = map.get("target") else {
                return Err(format!("service ports {path} is missing a target port"));
            };
            let target = match target {
                Value::Int(i) => i.to_string(),
                other => format!("%!d({}={other})", other.go_type()),
            };
            Ok(format!(
                "{}:{}:{target}/{}",
                map.get("host_ip")
                    .map_or_else(|| "0.0.0.0".to_string(), |h| go_s(Some(h))),
                go_s(map.get("published")),
                map.get("protocol")
                    .map_or_else(|| "tcp".to_string(), |p| go_s(Some(p))),
            ))
        }
        Value::Str(s) => Ok(s.clone()),
        _ => Ok(String::new()),
    }
}

fn env_file_indexer(value: &Value, path: &Path) -> Result<String, String> {
    match value {
        Value::Str(s) => Ok(s.clone()),
        Value::Map(map) => match map.get("path") {
            Some(p) => Ok(go_s(Some(p))),
            None => Err(format!("environment path attribute {path} is missing")),
        },
        _ => Ok(String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(items: &[&str]) -> Value {
        Value::Seq(items.iter().map(|i| Value::str(*i)).collect())
    }

    #[test]
    fn the_last_entry_takes_the_place_of_the_first() {
        let model = Value::Map(
            [(
                "services".to_string(),
                Value::Map(
                    [(
                        "web".to_string(),
                        Value::Map(
                            [
                                ("environment".to_string(), s(&["A=1", "B=2", "A=3"])),
                                (
                                    "volumes".to_string(),
                                    s(&["./a:/data", "db:/var/lib", "./b:/data:ro"]),
                                ),
                                ("command".to_string(), s(&["x", "x"])),
                            ]
                            .into(),
                        ),
                    )]
                    .into(),
                ),
            )]
            .into(),
        );
        let model = enforce(model).unwrap();
        let web = model.get(&["services", "web"]).unwrap();
        assert_eq!(web.get(&["environment"]), Some(&s(&["A=3", "B=2"])));
        assert_eq!(
            web.get(&["volumes"]),
            Some(&s(&["./b:/data:ro", "db:/var/lib"]))
        );
        // Lists without a rule keep every entry.
        assert_eq!(web.get(&["command"]), Some(&s(&["x", "x"])));
    }

    #[test]
    fn a_volume_needs_a_target() {
        let err = volume_indexer(&Value::Map(Default::default()), &Path::new(&["v"])).unwrap_err();
        assert_eq!(err, "service volume v is missing a mount target");
    }
}
