//! A service of the project as the options of CreateMachine: a container's
//! configuration in the words of nspawn's flags.

use std::collections::BTreeMap;

use anyhow::{bail, Result};
use zbus::zvariant::Value as Bus;

use super::label;
use crate::client::Options;
use crate::compose::project::Project;
use crate::compose::value::{Map, Value};

/// One machine to make: a replica of a service, or a one-off of `run`.
pub struct Spec<'a> {
    pub project: &'a Project,
    pub service: &'a str,
    pub number: u32,
    pub name: String,
    pub oneoff: bool,
    /// The digest of the image the machine is made from.
    pub image_digest: String,
    /// The services it depends on are left alone (--no-deps).
    pub no_deps: bool,
    /// The env files the project was given, for its label.
    pub env_files: Vec<String>,
}

/// What a machine takes: the options, and what to see to first.
pub struct Made {
    pub options: Options<'static>,
    /// Warnings: what of the service the machine does without.
    pub notes: Vec<String>,
    /// Directories to make on the host before the machine starts: bind sources that are
    /// not there.
    pub host_paths: Vec<String>,
    /// Ports published without a host port, which took a free one of the host.
    pub ephemeral: Vec<String>,
}

fn s<'a>(map: &'a Map, key: &str) -> &'a str {
    map.get(key).and_then(Value::as_str).unwrap_or("")
}

fn seq<'a>(map: &'a Map, key: &str) -> &'a [Value] {
    map.get(key)
        .and_then(Value::as_seq)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn sub<'a>(map: &'a Map, key: &str) -> Option<&'a Map> {
    map.get(key).and_then(Value::as_map)
}

fn int(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Int(i) => Some(*i),
        Value::Uint(u) => i64::try_from(*u).ok(),
        Value::Float(f) => Some(*f as i64),
        Value::Str(s) => s.parse().ok(),
        _ => None,
    }
}

fn float(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        Value::Str(s) => s.parse().ok(),
        _ => None,
    }
}

fn strings(values: &[Value]) -> Vec<String> {
    values.iter().map(|v| v.to_string()).collect()
}

/// The service as a mapping.
pub fn service<'a>(project: &'a Project, name: &str) -> Result<&'a Map> {
    match project
        .services
        .get(name)
        .or_else(|| project.jobs.get(name))
        .and_then(Value::as_map)
    {
        Some(map) => Ok(map),
        None => bail!("no such service: {name}"),
    }
}

/// How many machines a service has: --scale, then scale, then deploy.replicas, else one.
pub fn replicas(service: &Map, overrides: &BTreeMap<String, u32>, name: &str) -> u32 {
    if let Some(n) = overrides.get(name) {
        return *n;
    }
    if let Some(n) = int(service.get("scale")) {
        return n.max(0) as u32;
    }
    if let Some(n) = sub(service, "deploy").and_then(|d| int(d.get("replicas"))) {
        return n.max(0) as u32;
    }
    1
}

/// The name of a network of the project as nspawn knows it.
pub fn network_name(project: &Project, key: &str) -> String {
    project
        .networks
        .get(key)
        .and_then(Value::as_map)
        .map(|n| s(n, "name").to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| format!("{}_{key}", project.name))
}

/// The name of a volume of the project as nspawn knows it.
pub fn volume_name(project: &Project, key: &str) -> String {
    project
        .volumes
        .get(key)
        .and_then(Value::as_map)
        .map(|n| s(n, "name").to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| format!("{}_{key}", project.name))
}

/// The nspawn secret that carries a secret or config of the project with no file
/// behind it (environment or content).
pub fn secret_name(project: &str, kind: &str, key: &str) -> String {
    let name = format!("{project}_{kind}_{key}");
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The networks of a service in the order nspawn takes them, the primary first: the
/// highest priority, then by name.
pub fn service_networks(service: &Map) -> Vec<(String, Option<Map>)> {
    let mut networks: Vec<(String, Option<Map>)> = sub(service, "networks")
        .map(|n| {
            n.iter()
                .map(|(k, v)| (k.clone(), v.as_map().cloned()))
                .collect()
        })
        .unwrap_or_default();
    let priority = |v: &Option<Map>| {
        v.as_ref()
            .and_then(|m| int(m.get("gw_priority")).or_else(|| int(m.get("priority"))))
            .unwrap_or(0)
    };
    networks.sort_by(|a, b| priority(&b.1).cmp(&priority(&a.1)).then(a.0.cmp(&b.0)));
    networks
}

fn put(options: &mut Options<'static>, key: &'static str, value: impl Into<Bus<'static>>) {
    options.insert(key, value.into());
}

fn put_list(options: &mut Options<'static>, key: &'static str, values: Vec<String>) {
    if !values.is_empty() {
        options.insert(key, Bus::from(values));
    }
}

/// The CreateMachine options of a machine of a service.
pub fn options(spec: &Spec) -> Result<Made> {
    let project = spec.project;
    let service = service(project, spec.service)?;
    let mut made = Made {
        options: Options::new(),
        notes: Vec::new(),
        host_paths: Vec::new(),
        ephemeral: Vec::new(),
    };
    network(spec, service, &mut made)?;
    ports(spec, service, &mut made)?;
    volumes(spec, service, &mut made)?;
    secrets(spec, service, &mut made);
    program(spec, service, &mut made);
    limits(service, &mut made);
    health(service, &mut made);
    settings(spec, service, &mut made);
    dependencies(spec, service, &mut made)?;
    labels(spec, service, &mut made)?;
    Ok(made)
}

fn network(spec: &Spec, service: &Map, made: &mut Made) -> Result<()> {
    let project = spec.project;
    let mode = s(service, "network_mode");
    if !mode.is_empty() {
        let network = match mode {
            "host" | "none" | "bridge" => mode.to_string(),
            m if m.starts_with("service:") => {
                let owner = &m["service:".len()..];
                let other = self::service(project, owner)?;
                format!(
                    "container:{}",
                    super::machine_name(&project.name, owner, s(other, "container_name"), 1)
                )
            }
            m if m.starts_with("container:") => {
                format!("container:{}", m["container:".len()..].replace('_', "-"))
            }
            other => bail!(
                "service {:?}: network_mode {other} is not one nspawn has",
                spec.service
            ),
        };
        put(&mut made.options, "network", network);
        return Ok(());
    }
    let mut names = Vec::new();
    let mut aliases = Vec::new();
    let container_name = s(service, "container_name");
    for (key, opts) in service_networks(service) {
        let name = network_name(project, &key);
        if spec.oneoff {
            names.push(name);
            continue;
        }
        let mut own = vec![spec.service.to_string()];
        // container_name stays a name on the service's networks where the machine's
        // differs.
        if !container_name.is_empty() && container_name != spec.name {
            own.push(container_name.to_string());
        }
        if let Some(opts) = &opts {
            own.extend(strings(seq(opts, "aliases")));
        }
        own.dedup();
        for alias in own {
            if alias != spec.name {
                aliases.push(format!("{name}={alias}"));
            }
        }
        names.push(name);
    }
    put_list(&mut made.options, "networks", names);
    put_list(&mut made.options, "aliases", aliases);
    Ok(())
}

fn ports(spec: &Spec, service: &Map, made: &mut Made) -> Result<()> {
    if spec.oneoff {
        return Ok(());
    }
    let mut publish = Vec::new();
    for port in seq(service, "ports") {
        let Some(port) = port.as_map() else { continue };
        let target = int(port.get("target")).unwrap_or(0);
        let protocol = match s(port, "protocol") {
            "" | "tcp" => "tcp",
            "udp" => "udp",
            other => bail!(
                "service {:?}: ports of protocol {other}: nspawn publishes tcp and udp ports",
                spec.service
            ),
        };
        let host_ip = s(port, "host_ip");
        if host_ip.contains(':') {
            bail!(
                "service {:?}: port {target} on {host_ip}: nspawn publishes on IPv4 addresses",
                spec.service
            );
        }
        let mut published = s(port, "published").to_string();
        if published.contains('-') {
            bail!(
                "service {:?}: port {target} on {published}: a range of host ports for one port of the machine is not supported",
                spec.service
            );
        }
        if published.is_empty() {
            published = free_port(protocol)?.to_string();
            made.ephemeral
                .push(format!("{published}->{target}/{protocol}"));
        }
        let mut text = format!("{published}:{target}");
        if !host_ip.is_empty() && host_ip != "0.0.0.0" {
            text = format!("{host_ip}:{text}");
        }
        if protocol == "udp" {
            text.push_str("/udp");
        }
        if !publish.contains(&text) {
            publish.push(text);
        }
    }
    put_list(&mut made.options, "publish", publish);
    Ok(())
}

/// A port of the host nobody listens on, as the kernel hands out one to bind to.
fn free_port(protocol: &str) -> Result<u16> {
    let port = if protocol == "udp" {
        std::net::UdpSocket::bind("0.0.0.0:0")?.local_addr()?.port()
    } else {
        std::net::TcpListener::bind("0.0.0.0:0")?
            .local_addr()?
            .port()
    };
    Ok(port)
}

fn volumes(spec: &Spec, service: &Map, made: &mut Made) -> Result<()> {
    let project = spec.project;
    let mut binds = Vec::new();
    let mut tmpfs: Vec<String> = strings(seq(service, "tmpfs"));
    for volume in seq(service, "volumes") {
        let Some(volume) = volume.as_map() else {
            continue;
        };
        let target = s(volume, "target");
        let ro = if volume.get("read_only") == Some(&Value::Bool(true)) {
            ":ro"
        } else {
            ""
        };
        match s(volume, "type") {
            "bind" => {
                let source = s(volume, "source");
                let create = sub(volume, "bind")
                    .and_then(|b| b.get("create_host_path"))
                    .is_none_or(|v| v != &Value::Bool(false));
                if create && !std::path::Path::new(source).exists() {
                    made.host_paths.push(source.to_string());
                }
                binds.push(format!("{source}:{target}{ro}"));
            }
            "volume" => {
                let source = s(volume, "source");
                if source.is_empty() {
                    made.notes.push(format!(
                        "service {:?}: {target} has no source: on nspawn its data lives in the machine and goes with it",
                        spec.service
                    ));
                    continue;
                }
                binds.push(format!("{}:{target}{ro}", volume_name(project, source)));
            }
            "tmpfs" => {
                let mut opts = Vec::new();
                if let Some(t) = sub(volume, "tmpfs") {
                    if let Some(size) = int(t.get("size")).filter(|n| *n > 0) {
                        opts.push(format!("size={size}"));
                    }
                    if let Some(mode) = int(t.get("mode")).filter(|n| *n > 0) {
                        opts.push(format!("mode={mode:o}"));
                    }
                }
                if opts.is_empty() {
                    tmpfs.push(target.to_string());
                } else {
                    tmpfs.push(format!("{target}:{}", opts.join(",")));
                }
            }
            other => bail!(
                "service {:?}: a volume of type {other} is not supported",
                spec.service
            ),
        }
    }
    put_list(&mut made.options, "volume", binds);
    put_list(&mut made.options, "tmpfs", tmpfs);
    Ok(())
}

/// Secrets and configs: a file is bound read-only where the machine reads it; one from
/// the environment or written in the file is an nspawn secret, made by up before the
/// machine.
fn secrets(spec: &Spec, service: &Map, made: &mut Made) {
    let project = spec.project;
    let mut binds = Vec::new();
    let mut secrets = Vec::new();
    for (kind, top, default_dir) in [
        ("secret", &project.secrets, "/run/secrets/"),
        ("config", &project.configs, "/"),
    ] {
        for entry in seq(service, &format!("{kind}s")) {
            let Some(entry) = entry.as_map() else {
                continue;
            };
            let source = s(entry, "source");
            let mut target = s(entry, "target").to_string();
            if target.is_empty() {
                target = format!("{default_dir}{source}");
            } else if !target.starts_with('/') {
                target = format!("{default_dir}{target}");
            }
            let Some(def) = top.get(source).and_then(Value::as_map) else {
                continue;
            };
            let file = s(def, "file");
            if !file.is_empty() {
                binds.push(format!("{file}:{target}:ro"));
            } else {
                secrets.push(format!(
                    "{}:{target}",
                    secret_name(&project.name, kind, source)
                ));
            }
        }
    }
    if let Some(Bus::Array(existing)) = made.options.remove("volume") {
        let mut all: Vec<String> = existing
            .iter()
            .filter_map(|v| String::try_from(v.clone()).ok())
            .collect();
        all.extend(binds);
        put_list(&mut made.options, "volume", all);
    } else {
        put_list(&mut made.options, "volume", binds);
    }
    put_list(&mut made.options, "secrets", secrets);
}

/// What the machine runs, and its environment. An entrypoint of the file replaces the
/// image's and drops its command: the whole command line goes to the
/// machine as its command, after an empty entrypoint.
fn program(spec: &Spec, service: &Map, made: &mut Made) {
    let entrypoint = service.get("entrypoint").and_then(Value::as_seq);
    let command = service.get("command").and_then(Value::as_seq);
    if let Some(entrypoint) = entrypoint {
        let mut argv = strings(entrypoint);
        if let Some(command) = command {
            argv.extend(strings(command));
        }
        put(&mut made.options, "entrypoint", String::new());
        put_list(&mut made.options, "command", argv);
    } else if let Some(command) = command {
        put_list(&mut made.options, "command", strings(command));
    }
    let mut env = Vec::new();
    if let Some(environment) = sub(service, "environment") {
        for (k, v) in environment {
            if !v.is_null() {
                env.push(format!("{k}={v}"));
            }
        }
    }
    let _ = spec;
    put_list(&mut made.options, "env", env);
}

fn limits(service: &Map, made: &mut Made) {
    let deploy_limits = sub(service, "deploy")
        .and_then(|d| sub(d, "resources"))
        .and_then(|r| sub(r, "limits"));
    let memory = int(service.get("mem_limit"))
        .filter(|n| *n > 0)
        .or_else(|| {
            deploy_limits
                .and_then(|l| int(l.get("memory")))
                .filter(|n| *n > 0)
        });
    if let Some(memory) = memory {
        put(&mut made.options, "memory", memory as u64);
    }
    if let Some(swap) = int(service.get("memswap_limit")).filter(|n| *n != 0) {
        put(&mut made.options, "memory_swap", swap);
    }
    let cpus = float(service.get("cpus")).filter(|n| *n > 0.0).or_else(|| {
        deploy_limits
            .and_then(|l| float(l.get("cpus")))
            .filter(|n| *n > 0.0)
    });
    if let Some(cpus) = cpus {
        put(&mut made.options, "cpus", cpus);
    }
    let pids = int(service.get("pids_limit"))
        .filter(|n| *n > 0)
        .or_else(|| {
            deploy_limits
                .and_then(|l| int(l.get("pids")))
                .filter(|n| *n > 0)
        });
    if let Some(pids) = pids {
        put(&mut made.options, "pids_limit", pids as u64);
    }
}

fn health(service: &Map, made: &mut Made) {
    let Some(hc) = sub(service, "healthcheck") else {
        return;
    };
    let test = strings(seq(hc, "test"));
    if hc.get("disable") == Some(&Value::Bool(true)) || test.first().is_some_and(|t| t == "NONE") {
        put(&mut made.options, "no_healthcheck", true);
        return;
    }
    match test.first().map(String::as_str) {
        Some("CMD-SHELL") => {
            put(&mut made.options, "health_cmd", test[1..].join(" "));
        }
        Some("CMD") => {
            put(&mut made.options, "health_cmd", shell_join(&test[1..]));
        }
        _ => {}
    }
    // Durations are nanoseconds in the model, microseconds on the bus.
    for (key, option) in [
        ("interval", "health_interval"),
        ("timeout", "health_timeout"),
        ("start_period", "health_start_period"),
        ("start_interval", "health_start_interval"),
    ] {
        if let Some(ns) = int(hc.get(key)).filter(|n| *n > 0) {
            put(&mut made.options, option, (ns / 1000) as u64);
        }
    }
    if let Some(retries) = int(hc.get("retries")) {
        put(&mut made.options, "health_retries", retries.max(0) as u32);
    }
}

/// Arguments as one line /bin/sh -c runs as they were.
fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if !a.is_empty()
                && a.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_./=:,@%+".contains(&b))
            {
                a.clone()
            } else {
                format!("'{}'", a.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn settings(spec: &Spec, service: &Map, made: &mut Made) {
    let o = &mut made.options;
    for (key, option) in [
        ("hostname", "hostname"),
        ("user", "user"),
        ("working_dir", "working_dir"),
        ("stop_signal", "stop_signal"),
    ] {
        let value = s(service, key);
        if !value.is_empty() {
            put(o, option, value.to_string());
        }
    }
    let restart = match sub(service, "deploy")
        .and_then(|d| sub(d, "restart_policy"))
        .map(|r| s(r, "condition"))
        .unwrap_or("")
    {
        "none" | "no" => "no".to_string(),
        "any" => "always".to_string(),
        "on-failure" => "on-failure".to_string(),
        _ => s(service, "restart").to_string(),
    };
    let restart = match restart.as_str() {
        "" => String::new(),
        r if r.starts_with("on-failure") => {
            if r.contains(':') {
                made.notes.push(format!(
                    "service {:?}: restart {r}: restarted on every failure, systemd keeps no count",
                    spec.service
                ));
            }
            "on-failure".to_string()
        }
        r => r.to_string(),
    };
    if !restart.is_empty() && !spec.oneoff {
        put(o, "restart", restart);
    }
    if let Some(ns) = int(service.get("stop_grace_period")) {
        put(o, "stop_timeout", (ns / 1_000_000_000).max(0) as u64);
    }
    for (key, option) in [
        ("cap_add", "cap_add"),
        ("cap_drop", "cap_drop"),
        ("dns", "dns"),
        ("dns_search", "dns_search"),
    ] {
        put_list(o, option, strings(seq(service, key)));
    }
    for (key, option) in [
        ("privileged", "privileged"),
        ("read_only", "read_only"),
        ("init", "init"),
    ] {
        if service.get(key) == Some(&Value::Bool(true)) {
            put(o, option, true);
        }
    }
    if let Some(size) = int(service.get("shm_size")).filter(|n| *n > 0) {
        put(o, "shm_size", size as u64);
    }
    if let Some(adj) = int(service.get("oom_score_adj")).filter(|n| *n != 0) {
        put(o, "oom_score_adj", adj as i32);
    }
    let devices: Vec<String> = seq(service, "devices")
        .iter()
        .filter_map(Value::as_map)
        .map(|d| {
            let permissions = s(d, "permissions");
            format!(
                "{}:{}:{}",
                s(d, "source"),
                if s(d, "target").is_empty() {
                    s(d, "source")
                } else {
                    s(d, "target")
                },
                if permissions.is_empty() {
                    "rwm"
                } else {
                    permissions
                }
            )
        })
        .collect();
    put_list(o, "devices", devices);
    let mut hosts = Vec::new();
    if let Some(extra) = sub(service, "extra_hosts") {
        for (host, ips) in extra {
            for ip in ips.as_seq().map(Vec::as_slice).unwrap_or(&[]) {
                hosts.push(format!("{host}:{ip}"));
            }
        }
    }
    put_list(o, "extra_hosts", hosts);
    let mut ulimits = Vec::new();
    if let Some(limits) = sub(service, "ulimits") {
        for (name, limit) in limits {
            match limit {
                Value::Map(m) => ulimits.push(format!(
                    "{name}={}:{}",
                    int(m.get("soft")).unwrap_or(0),
                    int(m.get("hard")).unwrap_or(0)
                )),
                other => {
                    let n = int(Some(other)).unwrap_or(0);
                    ulimits.push(format!("{name}={n}:{n}"));
                }
            }
        }
    }
    put_list(o, "ulimits", ulimits);
    let sysctls: Vec<String> = sub(service, "sysctls")
        .map(|m| m.iter().map(|(k, v)| format!("{k}={v}")).collect())
        .unwrap_or_default();
    put_list(o, "sysctls", sysctls);
    if let Some(logging) = sub(service, "logging") {
        match s(logging, "driver") {
            "json-file" | "local" => put(o, "log_driver", "local".to_string()),
            "journald" => put(o, "log_driver", "journal".to_string()),
            "none" => put(o, "log_driver", "none".to_string()),
            _ => {}
        }
    }
}

fn dependencies(spec: &Spec, service: &Map, made: &mut Made) -> Result<()> {
    if spec.no_deps || spec.oneoff {
        return Ok(());
    }
    let project = spec.project;
    let mut deps = Vec::new();
    if let Some(depends) = sub(service, "depends_on") {
        for (name, dep) in depends {
            // A dependency out of the project (not selected, or in a disabled profile)
            // is none of the machine's.
            let Some(other) = project.services.get(name).and_then(Value::as_map) else {
                continue;
            };
            let dep = dep.as_map().cloned().unwrap_or_default();
            let condition = match s(&dep, "condition") {
                "service_healthy" => ":healthy",
                "service_completed_successfully" => ":completed",
                _ => "",
            };
            let optional = if dep.get("required") == Some(&Value::Bool(false)) {
                ":optional"
            } else {
                ""
            };
            for n in 1..=replicas(other, &BTreeMap::new(), name).max(1) {
                let machine =
                    super::machine_name(&project.name, name, s(other, "container_name"), n);
                deps.push(format!("{machine}{condition}{optional}"));
            }
        }
    }
    put_list(&mut made.options, "depends_on", deps);
    Ok(())
}

/// The service's labels, and those by which a project knows its machines again.
fn labels(spec: &Spec, service: &Map, made: &mut Made) -> Result<()> {
    let project = spec.project;
    let mut labels: BTreeMap<String, String> = sub(service, "labels")
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.to_string())).collect())
        .unwrap_or_default();
    let depends: Vec<String> = sub(service, "depends_on")
        .map(|m| {
            m.iter()
                .map(|(name, dep)| {
                    let dep = dep.as_map().cloned().unwrap_or_default();
                    let condition = match s(&dep, "condition") {
                        "" => "service_started",
                        c => c,
                    };
                    let restart = dep.get("restart") == Some(&Value::Bool(true));
                    format!("{name}:{condition}:{restart}")
                })
                .collect()
        })
        .unwrap_or_default();
    let hash = crate::compose::config::service_hash(&Value::Map(service.clone()))
        .map_err(anyhow::Error::msg)?;
    for (key, value) in [
        (label::PROJECT, project.name.clone()),
        (label::SERVICE, spec.service.to_string()),
        (label::NUMBER, spec.number.to_string()),
        (
            label::ONEOFF,
            if spec.oneoff { "True" } else { "False" }.to_string(),
        ),
        (label::CONFIG_HASH, hash),
        (label::WORKING_DIR, project.working_dir.clone()),
        (label::CONFIG_FILES, project.files.join(",")),
        (label::DEPENDS_ON, depends.join(",")),
        (label::IMAGE, spec.image_digest.clone()),
        (label::VERSION, super::compose_version().to_string()),
    ] {
        labels.insert(key.to_string(), value);
    }
    if !spec.env_files.is_empty() {
        labels.insert(label::ENV_FILE.to_string(), spec.env_files.join(","));
    }
    put_list(
        &mut made.options,
        "label",
        labels.iter().map(|(k, v)| format!("{k}={v}")).collect(),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// The project of a compose file, as up loads it.
    fn project(name: &str, yaml: &str) -> Project {
        let dir =
            std::env::temp_dir().join(format!("nspawn-convert-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("compose.yaml"), yaml).unwrap();
        let cli = crate::cli::Cli::try_parse_from(["nspawn", "compose", "up"]).unwrap();
        let crate::cli::Command::Compose(args) = cli.command else {
            unreachable!()
        };
        let env = crate::compose::dotenv::Env::from([("HOME".to_string(), "/nowhere".to_string())]);
        let host = crate::compose::config::Host::new(
            env,
            dir.to_string_lossy().into_owned(),
            crate::compose::config::Stdin::Process,
        );
        let flags = crate::compose::options::Flags::default();
        let project =
            crate::compose::config::project_for(&args, &flags, &host, &[], &mut |_| {}).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        project
    }

    fn made(project: &Project, service: &str, number: u32) -> Made {
        let s = service_of(project, service);
        let name = super::super::machine_name(
            &project.name,
            service,
            s.get("container_name")
                .and_then(Value::as_str)
                .unwrap_or(""),
            number,
        );
        options(&Spec {
            project,
            service,
            number,
            name,
            oneoff: false,
            image_digest: "sha256:abc".to_string(),
            no_deps: false,
            env_files: Vec::new(),
        })
        .unwrap()
    }

    fn service_of<'a>(project: &'a Project, name: &str) -> &'a Map {
        service(project, name).unwrap()
    }

    fn list(made: &Made, key: &str) -> Vec<String> {
        match made.options.get(key) {
            Some(Bus::Array(items)) => items
                .iter()
                .filter_map(|v| String::try_from(v.clone()).ok())
                .collect(),
            _ => Vec::new(),
        }
    }

    fn text(made: &Made, key: &str) -> Option<String> {
        made.options
            .get(key)
            .and_then(|v| String::try_from(v.clone()).ok())
    }

    const DEMO: &str = r#"
name: demo
services:
  db:
    image: postgres:18-alpine
    container_name: demo_db
    healthcheck:
      test: ["CMD", "pg_isready", "-U", "app"]
      interval: 5s
      retries: 3
  web:
    image: nginx:alpine
    restart: unless-stopped
    entrypoint: ["/docker-entrypoint.sh"]
    command: ["nginx", "-g", "daemon off;"]
    environment:
      A: "1"
      EMPTY:
    ports: ["127.0.0.1:8080:80", "8443:443/udp"]
    volumes:
      - data:/data
      - /srv/conf:/conf:ro
      - type: tmpfs
        target: /scratch
        tmpfs:
          size: 1048576
    networks:
      back:
        aliases: [www]
    depends_on:
      db:
        condition: service_healthy
    extra_hosts: ["gw=10.0.0.1"]
    ulimits:
      nofile:
        soft: 64
        hard: 128
    logging:
      driver: json-file
    stop_grace_period: 1m30s
  side:
    image: busybox
    network_mode: service:web
networks:
  back: {}
volumes:
  data: {}
"#;

    #[test]
    fn a_service_becomes_the_options_of_its_machine() {
        let project = project("demo", DEMO);
        let web = made(&project, "web", 1);
        assert_eq!(list(&web, "networks"), ["demo_back"]);
        assert_eq!(list(&web, "aliases"), ["demo_back=web", "demo_back=www"]);
        assert_eq!(list(&web, "publish"), ["127.0.0.1:8080:80", "8443:443/udp"]);
        assert_eq!(
            list(&web, "volume"),
            ["demo_data:/data", "/srv/conf:/conf:ro"]
        );
        assert_eq!(list(&web, "tmpfs"), ["/scratch:size=1048576"]);
        // The entrypoint drops the image's command: the whole line is the machine's.
        assert_eq!(text(&web, "entrypoint").as_deref(), Some(""));
        assert_eq!(
            list(&web, "command"),
            ["/docker-entrypoint.sh", "nginx", "-g", "daemon off;"]
        );
        // A variable without a value is none.
        assert_eq!(list(&web, "env"), ["A=1"]);
        assert_eq!(text(&web, "restart").as_deref(), Some("unless-stopped"));
        assert_eq!(list(&web, "depends_on"), ["demo-db:healthy"]);
        assert_eq!(list(&web, "extra_hosts"), ["gw:10.0.0.1"]);
        assert_eq!(list(&web, "ulimits"), ["nofile=64:128"]);
        assert_eq!(text(&web, "log_driver").as_deref(), Some("local"));
        assert_eq!(web.options.get("stop_timeout"), Some(&Bus::from(90u64)));
        let labels = list(&web, "label");
        for wanted in [
            "com.docker.compose.project=demo",
            "com.docker.compose.service=web",
            "com.docker.compose.container-number=1",
            "com.docker.compose.oneoff=False",
            "com.docker.compose.depends_on=db:service_healthy:false",
            "com.docker.compose.image=sha256:abc",
        ] {
            assert!(labels.iter().any(|l| l == wanted), "{wanted} in {labels:?}");
        }
        assert!(labels
            .iter()
            .any(|l| l.starts_with("com.docker.compose.config-hash=")));
        assert_eq!(web.host_paths, ["/srv/conf"]);
    }

    #[test]
    fn names_health_and_shared_networks_follow_docker_compose() {
        let project = project("names", DEMO);
        let db = made(&project, "db", 1);
        // container_name is the machine's name with `_` written `-`, and stays a name
        // on the network as it was written.
        assert_eq!(
            list(&db, "aliases"),
            ["demo_default=db", "demo_default=demo_db"]
        );
        assert_eq!(
            text(&db, "health_cmd").as_deref(),
            Some("pg_isready -U app")
        );
        assert_eq!(
            db.options.get("health_interval"),
            Some(&Bus::from(5_000_000u64))
        );
        assert_eq!(db.options.get("health_retries"), Some(&Bus::from(3u32)));
        let side = made(&project, "side", 1);
        assert_eq!(
            text(&side, "network").as_deref(),
            Some("container:demo-web-1")
        );
        assert!(!side.options.contains_key("networks"));
    }

    #[test]
    fn health_commands_keep_their_words() {
        assert_eq!(
            shell_join(&["pg_isready".into(), "-U".into(), "a b".into()]),
            "pg_isready -U 'a b'"
        );
        assert_eq!(
            shell_join(&["echo".into(), "it's".into()]),
            "echo 'it'\\''s'"
        );
    }

    #[test]
    fn secrets_of_the_project_get_a_name_of_their_own() {
        assert_eq!(
            secret_name("demo", "secret", "db pass"),
            "demo_secret_db_pass"
        );
    }
}
