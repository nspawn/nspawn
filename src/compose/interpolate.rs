//! compose-go's interpolation of a model: every string substituted with the project's
//! environment, then cast to the type its attribute takes (a bool, an integer, a float)
//! where that attribute is not a string. The errors of every value are collected and
//! given together, sorted by path.

use super::gostr;
use super::path::Path;
use super::template::{self, Lookup};
use super::value::{Map, Value};

#[derive(Clone, Copy)]
enum Cast {
    Bool,
    Int,
    Int64,
    Float,
    Float32,
}

/// The attributes cast after interpolation: those of a container wherever one is
/// declared (a service, a job, a pre_start container), those of a workload (a service
/// or a job), and the service's own.
fn cast_for(path: &Path) -> Option<Cast> {
    const TOP: &[(&str, Cast)] = &[
        ("networks.*.external", Cast::Bool),
        ("networks.*.internal", Cast::Bool),
        ("networks.*.attachable", Cast::Bool),
        ("networks.*.enable_ipv4", Cast::Bool),
        ("networks.*.enable_ipv6", Cast::Bool),
        ("volumes.*.external", Cast::Bool),
        ("secrets.*.external", Cast::Bool),
        ("configs.*.external", Cast::Bool),
    ];
    const CONTAINER: &[(&str, Cast)] = &[
        ("cpu_count", Cast::Int64),
        ("cpu_percent", Cast::Float),
        ("cpu_period", Cast::Int64),
        ("cpu_quota", Cast::Int64),
        ("cpu_rt_period", Cast::Int64),
        ("cpu_rt_runtime", Cast::Int64),
        ("cpus", Cast::Float32),
        ("cpu_shares", Cast::Int64),
        ("init", Cast::Bool),
        ("oom_kill_disable", Cast::Bool),
        ("oom_score_adj", Cast::Int64),
        ("pids_limit", Cast::Int64),
        ("privileged", Cast::Bool),
        ("read_only", Cast::Bool),
        ("ulimits.*", Cast::Int),
        ("ulimits.*.hard", Cast::Int),
        ("ulimits.*.soft", Cast::Int),
        ("volumes.[].read_only", Cast::Bool),
        ("volumes.[].volume.nocopy", Cast::Bool),
    ];
    const WORKLOAD: &[(&str, Cast)] = &[
        ("depends_on.*.required", Cast::Bool),
        ("depends_on.*.restart", Cast::Bool),
        ("healthcheck.retries", Cast::Int),
        ("healthcheck.disable", Cast::Bool),
        ("ports.[].target", Cast::Int),
        ("stdin_open", Cast::Bool),
        ("tty", Cast::Bool),
    ];
    const SERVICE: &[(&str, Cast)] = &[
        ("deploy.replicas", Cast::Int),
        ("deploy.update_config.parallelism", Cast::Int),
        ("deploy.update_config.max_failure_ratio", Cast::Float),
        ("deploy.rollback_config.parallelism", Cast::Int),
        ("deploy.rollback_config.max_failure_ratio", Cast::Float),
        ("deploy.restart_policy.max_attempts", Cast::Int),
        ("deploy.placement.max_replicas_per_node", Cast::Int),
        ("scale", Cast::Int),
    ];
    let find = |table: &[(&str, Cast)], rest: &[&str]| {
        let rest = Path::new(rest);
        table
            .iter()
            .find(|(pattern, _)| rest.matches_str(pattern))
            .map(|(_, cast)| *cast)
    };
    if let Some((_, cast)) = TOP.iter().find(|(p, _)| path.matches_str(p)) {
        return Some(*cast);
    }
    let parts = path.parts();
    match parts.as_slice() {
        ["services", _, "pre_start", _, rest @ ..] if !rest.is_empty() => find(CONTAINER, rest),
        [kind @ ("services" | "jobs"), _, rest @ ..] if !rest.is_empty() => find(CONTAINER, rest)
            .or_else(|| find(WORKLOAD, rest))
            .or_else(|| (*kind == "services").then(|| find(SERVICE, rest)).flatten()),
        _ => None,
    }
}

/// The errors of an interpolation as compose-go collects them: one per value, joined
/// per mapping sorted by the path of their first, per list in its order.
enum Failure {
    One(Path, String),
    Joined(Vec<Failure>),
}

impl Failure {
    /// errorPath: the path of the first error it holds.
    fn key(&self) -> Vec<&str> {
        match self {
            Failure::One(path, _) => path.parts(),
            Failure::Joined(all) => all.first().map(Failure::key).unwrap_or_default(),
        }
    }

    fn lines(&self, out: &mut Vec<String>) {
        match self {
            Failure::One(_, message) => out.push(message.clone()),
            Failure::Joined(all) => all.iter().for_each(|f| f.lines(out)),
        }
    }
}

/// joinErrors: sorted by their paths, keeping the order of equal ones.
fn joined(mut errors: Vec<Failure>) -> Option<Failure> {
    if errors.is_empty() {
        return None;
    }
    errors.sort_by(|a, b| a.key().cmp(&b.key()));
    Some(Failure::Joined(errors))
}

/// Interpolates a model. `warn` hears of unset variables and of YAML 1.1 booleans.
pub fn interpolate(
    model: Map,
    lookup: &dyn Lookup,
    warn: &mut dyn FnMut(String),
) -> Result<Map, String> {
    let mut errors = Vec::new();
    let mut out = Map::new();
    for (key, value) in model {
        match walk(value, &Path::root().next(&key), lookup, warn) {
            Ok(value) => {
                out.insert(key, value);
            }
            Err(e) => errors.push(e),
        }
    }
    match joined(errors) {
        None => Ok(out),
        Some(failure) => {
            let mut lines = Vec::new();
            failure.lines(&mut lines);
            Err(lines.join("\n"))
        }
    }
}

fn walk(
    value: Value,
    path: &Path,
    lookup: &dyn Lookup,
    warn: &mut dyn FnMut(String),
) -> Result<Value, Failure> {
    match value {
        Value::Str(text) => {
            let mut unset = |name: &str| {
                warn(format!(
                    "The {} variable is not set. Defaulting to a blank string.",
                    gostr::quote(name)
                ))
            };
            let substituted = match template::substitute(&text, lookup, &mut unset) {
                Ok(substituted) => substituted,
                Err(template::Error::Invalid(template)) => {
                    return Err(Failure::One(
                        path.clone(),
                        format!(
                            "invalid interpolation format for {path}.\nYou may need to escape any $ with another $.\n{template}"
                        ),
                    ));
                }
                Err(error) => {
                    return Err(Failure::One(
                        path.clone(),
                        format!("error while interpolating {path}: {error}"),
                    ));
                }
            };
            let Some(cast) = cast_for(path) else {
                return Ok(Value::Str(substituted));
            };
            apply(cast, &substituted, warn).map_err(|error| {
                Failure::One(
                    path.clone(),
                    format!(
                        "error while interpolating {path}: failed to cast to expected type: {error}"
                    ),
                )
            })
        }
        Value::Map(map) => {
            let mut out = Map::new();
            let mut errors = Vec::new();
            for (key, entry) in map {
                match walk(entry, &path.next(&key), lookup, warn) {
                    Ok(entry) => {
                        out.insert(key, entry);
                    }
                    Err(e) => errors.push(e),
                }
            }
            match joined(errors) {
                None => Ok(Value::Map(out)),
                Some(failure) => Err(failure),
            }
        }
        Value::Seq(items) => {
            let next = path.next("[]");
            let mut out = Vec::with_capacity(items.len());
            let mut errors = Vec::new();
            for item in items {
                match walk(item, &next, lookup, warn) {
                    Ok(item) => out.push(item),
                    Err(e) => {
                        errors.push(e);
                        out.push(Value::Null);
                    }
                }
            }
            if errors.is_empty() {
                Ok(Value::Seq(out))
            } else {
                Err(Failure::Joined(errors))
            }
        }
        other => Ok(other),
    }
}

fn apply(cast: Cast, text: &str, warn: &mut dyn FnMut(String)) -> Result<Value, String> {
    match cast {
        Cast::Bool => to_bool(text, warn).map(Value::Bool),
        Cast::Int => gostr::atoi(text)
            .map(Value::Int)
            .map_err(|e| gostr::num_error("Atoi", text, e)),
        Cast::Int64 => gostr::parse_int(text, 10, 64)
            .map(Value::Int)
            .map_err(|e| gostr::num_error("ParseInt", text, e)),
        Cast::Float => gostr::parse_float(text, 64)
            .map(Value::Float)
            .map_err(|e| gostr::num_error("ParseFloat", text, e)),
        // The shortest digits of the float32 read back as a float64 print the same.
        Cast::Float32 => gostr::parse_float(text, 32)
            .map(|x| {
                let narrow = x as f32;
                Value::Float(narrow.to_string().parse().unwrap_or(x))
            })
            .map_err(|e| gostr::num_error("ParseFloat", text, e)),
    }
}

/// YAML 1.1's booleans, with a warning for those YAML 1.2 dropped.
fn to_bool(text: &str, warn: &mut dyn FnMut(String)) -> Result<bool, String> {
    match gostr::to_lower_str(text).as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        "y" | "yes" | "on" => {
            warn(format!(
                "{} for boolean is not supported by YAML 1.2, please use `true`",
                gostr::quote(text)
            ));
            Ok(true)
        }
        "n" | "no" | "off" => {
            warn(format!(
                "{} for boolean is not supported by YAML 1.2, please use `false`",
                gostr::quote(text)
            ));
            Ok(false)
        }
        _ => Err(format!("invalid boolean: {text}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compose::value::go_float;
    use std::collections::HashMap;

    fn model(text: &str) -> Map {
        let mut docs = super::super::yaml::parse(text).unwrap();
        let mut doc = docs.remove(0);
        super::super::yaml::take_resets(&mut doc).unwrap();
        match super::super::yaml::construct(&doc).unwrap() {
            Value::Map(m) => m,
            _ => unreachable!(),
        }
    }

    fn run(text: &str) -> (Result<Map, String>, Vec<String>) {
        let env: HashMap<&str, &str> = HashMap::from([("TAG", "1.2"), ("CPU", "0.1")]);
        let lookup = |name: &str| env.get(name).map(|v| v.to_string());
        let mut warnings = Vec::new();
        let result = interpolate(model(text), &lookup, &mut |w| warnings.push(w));
        (result, warnings)
    }

    #[test]
    fn strings_are_substituted_and_cast() {
        let (result, warnings) = run(
            "services:\n  web:\n    image: app:${TAG}\n    init: yes\n    cpus: ${CPU}\n    \
             ports:\n      - target: \"80\"\n    environment:\n      X: ${UNSET}\n",
        );
        let model = Value::Map(result.unwrap());
        let web = model.get(&["services", "web"]).unwrap();
        assert_eq!(web.get(&["image"]), Some(&Value::str("app:1.2")));
        assert_eq!(web.get(&["init"]), Some(&Value::Bool(true)));
        assert_eq!(
            go_float(match web.get(&["cpus"]) {
                Some(Value::Float(x)) => *x,
                other => panic!("{other:?}"),
            }),
            "0.1"
        );
        let port = &web.get(&["ports"]).unwrap().as_seq().unwrap()[0];
        assert_eq!(port.get(&["target"]), Some(&Value::Int(80)));
        assert_eq!(
            warnings,
            [
                "The \"UNSET\" variable is not set. Defaulting to a blank string.",
                "\"yes\" for boolean is not supported by YAML 1.2, please use `true`",
            ]
        );
    }

    #[test]
    fn errors_come_together_sorted_by_path() {
        let (result, _) = run(
            "services:\n  web:\n    image: ${IMG:?need it}\n    command: ${A:?a}\n    \
             pids_limit: \"x\"\n    ports: [\"${P:?p}\"]\n    labels: [\"${\"]\n",
        );
        assert_eq!(
            result.unwrap_err(),
            "error while interpolating services.web.command: required variable A is missing a value: a\n\
             error while interpolating services.web.image: required variable IMG is missing a value: need it\n\
             invalid interpolation format for services.web.labels.[].\nYou may need to escape any $ with another $.\n${\n\
             error while interpolating services.web.pids_limit: failed to cast to expected type: strconv.ParseInt: parsing \"x\": invalid syntax\n\
             error while interpolating services.web.ports.[]: required variable P is missing a value: p"
        );
    }
}
