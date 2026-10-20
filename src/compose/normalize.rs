//! compose-go's Normalize: the default network of the services that name none,
//! `pull_policy: if_not_present` read as missing, the environment and build arguments
//! filled from the project's environment, the dependencies that links, shared
//! namespaces and volumes_from imply, volume targets cleaned, pre_start hooks given
//! their service's attributes, and resources named after the project.

use super::canonical::clean;
use super::dotenv::Env;
use super::merge;
use super::value::{Map, Value};

/// The attributes of a container, which a pre_start hook takes from its service
/// (compose-go's types.ContainerSpecAttributes).
const CONTAINER_SPEC: &[&str] = &[
    "annotations",
    "blkio_config",
    "cap_add",
    "cap_drop",
    "cgroup",
    "cgroup_parent",
    "command",
    "configs",
    "cpu_count",
    "cpu_percent",
    "cpu_period",
    "cpu_quota",
    "cpu_rt_period",
    "cpu_rt_runtime",
    "cpu_shares",
    "cpus",
    "cpuset",
    "credential_spec",
    "device_cgroup_rules",
    "devices",
    "dns",
    "dns_opt",
    "dns_search",
    "domainname",
    "entrypoint",
    "env_file",
    "environment",
    "extra_hosts",
    "gpus",
    "group_add",
    "hostname",
    "image",
    "init",
    "ipc",
    "isolation",
    "label_file",
    "labels",
    "log_driver",
    "log_opt",
    "logging",
    "mac_address",
    "mem_limit",
    "mem_reservation",
    "mem_swappiness",
    "memswap_limit",
    "models",
    "network_mode",
    "networks",
    "oom_kill_disable",
    "oom_score_adj",
    "pid",
    "pids_limit",
    "platform",
    "privileged",
    "pull_policy",
    "pull_refresh_after",
    "read_only",
    "runtime",
    "secrets",
    "security_opt",
    "shm_size",
    "stop_grace_period",
    "stop_signal",
    "storage_opt",
    "sysctls",
    "tmpfs",
    "ulimits",
    "use_api_socket",
    "user",
    "userns_mode",
    "uts",
    "volume_driver",
    "volumes",
    "volumes_from",
    "working_dir",
];

fn started(restart: bool) -> Value {
    Value::Map(Map::from([
        ("condition".to_string(), Value::str("service_started")),
        ("restart".to_string(), Value::Bool(restart)),
        ("required".to_string(), Value::Bool(true)),
    ]))
}

pub fn normalize(mut model: Map, env: &Env) -> Result<Map, String> {
    normalize_networks(&mut model);
    for key in ["services", "jobs"] {
        let Some(Value::Map(containers)) = model.get_mut(key) else {
            continue;
        };
        for container in containers.values_mut() {
            let Value::Map(c) = container else { continue };
            if c.get("pull_policy") == Some(&Value::str("if_not_present")) {
                c.insert("pull_policy".to_string(), Value::str("missing"));
            }
            if let Some(Value::Map(build)) = c.get_mut("build") {
                build
                    .entry("context".to_string())
                    .and_modify(|v| {
                        if v.is_null() {
                            *v = Value::str(".");
                        }
                    })
                    .or_insert_with(|| Value::str("."));
                let no_dockerfile = build.get("dockerfile").is_none_or(Value::is_null)
                    && build.get("dockerfile_inline").is_none_or(Value::is_null);
                if no_dockerfile {
                    build.insert("dockerfile".to_string(), Value::str("Dockerfile"));
                }
                if let Some(args) = build.remove("args") {
                    build.insert("args".to_string(), resolve(args, env, false).0);
                }
            }
            if let Some(environment) = c.remove("environment") {
                c.insert("environment".to_string(), resolve(environment, env, true).0);
            }
            let mut depends_on = match c.get("depends_on") {
                Some(Value::Map(d)) => d.clone(),
                _ => Map::new(),
            };
            if let Some(Value::Seq(links)) = c.get("links") {
                for link in links {
                    let link = link.to_string();
                    let parts: Vec<&str> = link.split(':').collect();
                    let name = if parts.len() == 2 {
                        parts[0]
                    } else {
                        link.as_str()
                    };
                    depends_on
                        .entry(name.to_string())
                        .or_insert_with(|| started(true));
                }
            }
            for namespace in ["network_mode", "ipc", "pid", "uts", "cgroup"] {
                if let Some(Value::Str(reference)) = c.get(namespace) {
                    if let Some(shared) = reference.strip_prefix("service:") {
                        depends_on
                            .entry(shared.to_string())
                            .or_insert_with(|| started(true));
                    }
                }
            }
            if let Some(Value::Seq(volumes)) = c.get_mut("volumes") {
                for volume in volumes.iter_mut() {
                    if let Some(Value::Str(target)) =
                        volume.as_map_mut().and_then(|v| v.get_mut("target"))
                    {
                        *target = clean(target);
                    }
                }
            }
            if let Some(Value::Seq(from)) = c.get("volumes_from") {
                for v in from {
                    let v = v.to_string();
                    if !v.starts_with("container:") {
                        let source = v.split(':').next().unwrap_or("").to_string();
                        depends_on.entry(source).or_insert_with(|| started(false));
                    }
                }
            }
            if !depends_on.is_empty() {
                c.insert("depends_on".to_string(), Value::Map(depends_on));
            }
            resolve_pre_start_hooks(c)?;
        }
    }
    name_from_key(&mut model);
    Ok(model)
}

fn resolve_pre_start_hooks(service: &mut Map) -> Result<(), String> {
    let Some(Value::Seq(hooks)) = service.get("pre_start") else {
        return Ok(());
    };
    if hooks.is_empty() {
        return Ok(());
    }
    let base: Map = service
        .iter()
        .filter(|(k, _)| k.as_str() != "volumes" && CONTAINER_SPEC.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let wrap = |v: Value| {
        Value::Map(Map::from([(
            "services".to_string(),
            Value::Map(Map::from([("hook".to_string(), v)])),
        )]))
    };
    let mut resolved = Vec::new();
    for hook in hooks.clone() {
        if !matches!(hook, Value::Map(_)) {
            resolved.push(hook);
            continue;
        }
        let merged = merge::merge(wrap(Value::Map(base.clone())), wrap(hook))
            .map_err(|e| format!("resolving pre_start hook specification: {e}"))?;
        resolved.push(
            merged
                .get(&["services", "hook"])
                .cloned()
                .unwrap_or(Value::Null),
        );
    }
    service.insert("pre_start".to_string(), Value::Seq(resolved));
    Ok(())
}

fn normalize_networks(model: &mut Map) {
    let mut networks = match model.get("networks") {
        Some(Value::Map(n)) => n.clone(),
        _ => Map::new(),
    };
    let mut uses_default = false;
    for key in ["services", "jobs"] {
        let Some(Value::Map(containers)) = model.get_mut(key) else {
            continue;
        };
        for container in containers.values_mut() {
            let Value::Map(c) = container else { continue };
            if c.contains_key("provider") || c.contains_key("network_mode") {
                continue;
            }
            match c.get("networks") {
                Some(Value::Map(n)) if !n.is_empty() => {
                    if n.contains_key("default") {
                        uses_default = true;
                    }
                }
                _ => {
                    c.insert(
                        "networks".to_string(),
                        Value::Map(Map::from([("default".to_string(), Value::Null)])),
                    );
                    uses_default = true;
                }
            }
        }
    }
    if uses_default && !networks.contains_key("default") {
        networks.insert("default".to_string(), Value::Null);
    }
    if !networks.is_empty() {
        model.insert("networks".to_string(), Value::Map(networks));
    }
}

/// compose-go's resolve: a variable without a value takes the environment's, or stays
/// (an environment) or goes (build arguments).
fn resolve(value: Value, env: &Env, keep_empty: bool) -> (Value, bool) {
    match value {
        Value::Seq(items) => {
            let resolved: Vec<Value> = items
                .into_iter()
                .filter_map(|v| {
                    let (r, ok) = resolve(v, env, keep_empty);
                    ok.then_some(r)
                })
                .collect();
            // compose-go's resolved list is nil when empty.
            if resolved.is_empty() {
                (Value::Null, true)
            } else {
                (Value::Seq(resolved), true)
            }
        }
        Value::Map(map) => {
            let mut resolved = Map::new();
            for (key, val) in map {
                if !val.is_null() {
                    resolved.insert(key, val);
                } else if let Some(found) = env.get(&key) {
                    resolved.insert(key, Value::Str(found.clone()));
                } else if keep_empty {
                    resolved.insert(key, Value::Null);
                }
            }
            (Value::Map(resolved), true)
        }
        Value::Str(s) => {
            if s.contains('=') {
                return (Value::Str(s), true);
            }
            match env.get(&s) {
                Some(found) => (Value::Str(format!("{s}={found}")), true),
                None if keep_empty => (Value::Str(s), true),
                None => (Value::str(""), false),
            }
        }
        other => (other, false),
    }
}

fn name_from_key(model: &mut Map) {
    let project = model.get("name").map(|n| n.to_string()).unwrap_or_default();
    for kind in ["networks", "volumes", "configs", "secrets"] {
        let Some(Value::Map(resources)) = model.get_mut(kind) else {
            continue;
        };
        for (key, resource) in resources.iter_mut() {
            let mut map = match resource {
                Value::Map(m) => m.clone(),
                _ => Map::new(),
            };
            if map.get("name").is_none_or(Value::is_null) {
                let external = map.get("external").is_some_and(|x| {
                    matches!(
                        x.to_string().as_str(),
                        "1" | "t" | "T" | "TRUE" | "true" | "True"
                    )
                });
                let name = if external {
                    key.clone()
                } else {
                    format!("{project}_{key}")
                };
                map.insert("name".to_string(), Value::Str(name));
            }
            *resource = Value::Map(map);
        }
    }
}
