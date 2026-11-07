//! The shape of a JSON document, to hold this API's answers against the recorded ones: the paths
//! of its keys, whatever the values.

use std::collections::BTreeSet;

use serde_json::Value;

/// The key paths of a JSON document, an array's elements under `[]`.
pub fn paths(value: &Value, prefix: &str, out: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                // Keys that are names (labels, networks, ports) are not the shape.
                let key = if [
                    "Labels",
                    "Networks",
                    "Ports",
                    "Containers",
                    "PortBindings",
                    "ExposedPorts",
                    "Volumes",
                    "Options",
                    "Tmpfs",
                    "Sysctls",
                ]
                .iter()
                .any(|p| prefix.ends_with(p))
                {
                    "*"
                } else {
                    k.as_str()
                };
                let path = format!("{prefix}.{key}");
                out.insert(path.clone());
                paths(v, &path, out);
            }
        }
        Value::Array(list) => {
            for v in list.iter().take(1) {
                paths(v, &format!("{prefix}[]"), out);
            }
        }
        _ => {}
    }
}

/// The paths of `ours` that `recorded` lacks, and those it has that `ours` lacks.
pub fn diff(ours: &Value, recorded: &Value) -> (Vec<String>, Vec<String>) {
    let (mut a, mut b) = (BTreeSet::new(), BTreeSet::new());
    paths(ours, "", &mut a);
    paths(recorded, "", &mut b);
    (
        a.difference(&b).cloned().collect(),
        b.difference(&a).cloned().collect(),
    )
}

/// An answer of the Docker Engine recorded under tests/dockerapi/golden (HACKING.md says
/// how).
pub fn golden(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}
