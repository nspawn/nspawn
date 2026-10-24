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
    /// That image's own command.
    pub image_cmd: Vec<String>,
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

/// A host directory a volume of the project binds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub path: String,
    /// `ro` among the mount options: read-only whatever the service's mount says.
    pub read_only: bool,
}

/// The host directory a volume of the project binds, as the local driver's `type: none`
/// with `o: bind` and a `device` does; None for one nspawn makes, and an error for
/// driver options nothing here honours, a mount option among them: dropping noexec,
/// say, would give the machine what the file meant to keep from it.
pub fn volume_device(project: &Project, key: &str) -> Result<Option<Device>> {
    let Some(opts) = project
        .volumes
        .get(key)
        .and_then(Value::as_map)
        .and_then(|v| v.get("driver_opts"))
        .and_then(Value::as_map)
        .filter(|o| !o.is_empty())
    else {
        return Ok(None);
    };
    let device = s(opts, "device");
    let options: Vec<&str> = s(opts, "o").split(',').filter(|o| !o.is_empty()).collect();
    let bind = options.iter().any(|o| matches!(*o, "bind" | "rbind"));
    if s(opts, "type") != "none" || !bind || device.is_empty() {
        bail!("volume {key:?}: driver_opts are taken only as type: none, o: bind and a device, which binds that host directory");
    }
    if let Some(other) = options
        .iter()
        .find(|o| !matches!(**o, "bind" | "rbind" | "ro" | "rw"))
    {
        bail!("volume {key:?}: the mount option {other} of o is not taken; bind, rbind, ro and rw are");
    }
    if !device.starts_with('/') {
        bail!("volume {key:?}: device {device:?} is no absolute path of the host");
    }
    // The last of ro and rw wins, as mount(8) reads its options.
    let read_only = options.iter().rev().find(|o| matches!(**o, "ro" | "rw")) == Some(&"ro");
    Ok(Some(Device {
        path: device.to_string(),
        read_only,
    }))
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

/// A device request of compose (gpus, deploy.resources.reservations.devices) as --gpus
/// takes it.
fn gpu_request(device: &Map) -> crate::cdi::GpuRequest {
    let strings = |key: &str| -> Vec<String> {
        seq(device, key)
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    };
    let count = match device.get("count") {
        Some(Value::Str(all)) if all.eq_ignore_ascii_case("all") => -1,
        other => int(other).unwrap_or(0),
    };
    crate::cdi::GpuRequest {
        driver: s(device, "driver").to_string(),
        count,
        device_ids: strings("device_ids"),
        capabilities: strings("capabilities")
            .into_iter()
            .filter(|c| c != "gpu")
            .collect(),
        options: sub(device, "options")
            .map(|m| {
                m.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                    .collect()
            })
            .unwrap_or_default(),
    }
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
    platform(spec, service, &mut made);
    ports(spec, service, &mut made)?;
    volumes(spec, service, &mut made)?;
    secrets(spec, service, &mut made);
    program(spec, service, &mut made);
    limits(service, &mut made);
    health(service, &mut made);
    settings(spec, service, &mut made);
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
                // With --no-deps the owner is left out of the selection, not out of the
                // project: its machine is there all the same.
                let other = match self::service(project, owner) {
                    Ok(other) => other,
                    Err(e) => project
                        .disabled_services
                        .get(owner)
                        .and_then(Value::as_map)
                        .ok_or(e)?,
                };
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
    let mut addresses = Vec::new();
    let container_name = s(service, "container_name");
    for (key, opts) in service_networks(service) {
        let name = network_name(project, &key);
        // A one-off machine runs beside the service's own, whose fixed address it
        // cannot take as well.
        if spec.oneoff {
            names.push(name);
            continue;
        }
        if let Some(opts) = &opts {
            let ip = s(opts, "ipv4_address");
            if !ip.is_empty() {
                addresses.push(format!("{name}={ip}"));
            }
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
        // links: SERVICE:ALIAS of the services that link to this one, an alias on the
        // default network; of the services not selected too, which may come up later.
        if key == "default" && !spec.oneoff {
            let services = project
                .services
                .values()
                .chain(project.disabled_services.values());
            for other in services.filter_map(Value::as_map) {
                for link in strings(seq(other, "links")) {
                    if let Some((linked, alias)) = link.split_once(':') {
                        if linked == spec.service && !alias.is_empty() {
                            own.push(alias.to_string());
                        }
                    }
                }
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        own.retain(|alias| seen.insert(alias.clone()));
        for alias in own {
            if alias != spec.name {
                aliases.push(format!("{name}={alias}"));
            }
        }
        names.push(name);
    }
    put_list(&mut made.options, "networks", names);
    put_list(&mut made.options, "aliases", aliases);
    put_list(&mut made.options, "addresses", addresses);
    Ok(())
}

/// platform: this host's image is pulled whatever it names; another architecture is
/// said.
fn platform(spec: &Spec, service: &Map, made: &mut Made) {
    let platform = s(service, "platform");
    let arch = platform.split('/').nth(1).unwrap_or_default();
    let host = super::info::go_arch();
    if !arch.is_empty() && arch != host {
        made.notes.push(format!(
            "service {:?}: platform {platform}: the image for this host's {host} is used",
            spec.service
        ));
    }
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
        // 0.0.0.0 is IPv4 alone, :: IPv6 alone.
        let mut text = format!("{published}:{target}");
        if host_ip.contains(':') {
            text = format!("[{host_ip}]:{text}");
        } else if !host_ip.is_empty() {
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
                // A machine's Docker API socket is nspawn's: a host without one of its own
                // must not get a directory there.
                let api = crate::volume::API_SOCKET_PATHS.contains(&source);
                if create && !api && absent(source) {
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
                match volume_device(project, source)? {
                    Some(device) => {
                        let ro = if device.read_only { ":ro" } else { ro };
                        binds.push(format!("{}:{target}{ro}", device.path));
                    }
                    None => binds.push(format!("{}:{target}{ro}", volume_name(project, source))),
                }
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
    // use_api_socket: the Docker API socket where its clients look for it, unless the
    // service mounts something there itself.
    if service.get("use_api_socket") == Some(&Value::Bool(true))
        && !binds
            .iter()
            .any(|b| b.split(':').nth(1) == Some(crate::volume::API_SOCKET_PATHS[0]))
    {
        let socket = crate::volume::API_SOCKET_PATHS[0];
        binds.push(format!("{socket}:{socket}"));
    }
    put_list(&mut made.options, "volume", binds);
    put_list(&mut made.options, "tmpfs", tmpfs);
    Ok(())
}

/// Whether a bind's source is to be made: only one known not to be there. The client
/// runs as its caller, who may not look into a directory only root reads
/// (/etc/letsencrypt/live): such a source is the service's to find, which runs as root,
/// and making it here would fail or shadow what is there.
fn absent(path: &str) -> bool {
    matches!(std::fs::metadata(path), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
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
/// image's and drops its command: the whole command line goes to the machine as its
/// command, after an empty entrypoint. An empty one with no command keeps the image's
/// command: compose gives [] for it where the command line gives [""].
fn program(spec: &Spec, service: &Map, made: &mut Made) {
    let entrypoint = service.get("entrypoint").and_then(Value::as_seq);
    let command = service.get("command").and_then(Value::as_seq);
    if let Some(entrypoint) = entrypoint {
        let mut argv = strings(entrypoint);
        if let Some(command) = command {
            argv.extend(strings(command));
        }
        if argv.is_empty() {
            argv.clone_from(&spec.image_cmd);
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
        // The exec form runs without a shell, which an image may not have.
        Some("CMD") => {
            put_list(&mut made.options, "health_test", test.clone());
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
    // getRestartPolicy: deploy.restart_policy, when there is one, decides alone, its
    // condition mapped as the engine's words (an empty one is no policy); else restart.
    let condition = |c: &str| -> String {
        match c {
            "none" | "no" => "no".to_string(),
            "any" | "always" => "always".to_string(),
            other => other.to_string(),
        }
    };
    let restart = match sub(service, "deploy").and_then(|d| sub(d, "restart_policy")) {
        Some(policy) => {
            let mapped = condition(s(policy, "condition"));
            match int(policy.get("max_attempts")) {
                Some(n) if n > 0 && mapped == "on-failure" => format!("on-failure:{n}"),
                _ => mapped,
            }
        }
        None => {
            let given = s(service, "restart");
            match given.split_once(':') {
                Some((mode, count)) => format!("{}:{count}", condition(mode)),
                None => condition(given),
            }
        }
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
        put(
            o,
            "stop_timeout",
            crate::policy::stop_seconds(ns / 1_000_000_000),
        );
    }
    for (key, option) in [
        ("cap_add", "cap_add"),
        ("cap_drop", "cap_drop"),
        ("dns", "dns"),
        ("dns_search", "dns_search"),
    ] {
        put_list(o, option, strings(seq(service, key)));
    }
    for (key, option) in [("privileged", "privileged"), ("read_only", "read_only")] {
        if service.get(key) == Some(&Value::Bool(true)) {
            put(o, option, true);
        }
    }
    // Either way it says something: false makes the program PID 1.
    if let Some(Value::Bool(init)) = service.get("init") {
        put(o, "init", *init);
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
            // A CDI device goes by its name alone.
            if crate::cdi::is_qualified(s(d, "source")) {
                return s(d, "source").to_string();
            }
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
    // GPUs and the devices deploy reserves, both device requests.
    let reserved = sub(service, "deploy")
        .and_then(|d| sub(d, "resources"))
        .and_then(|r| sub(r, "reservations"))
        .map(|r| seq(r, "devices"))
        .unwrap_or(&[]);
    let gpus: Vec<String> = reserved
        .iter()
        .chain(seq(service, "gpus"))
        .filter_map(Value::as_map)
        .map(|d| gpu_request(d).to_string())
        .collect();
    put_list(o, "gpus", gpus);
    let mut hosts = Vec::new();
    if let Some(extra) = sub(service, "extra_hosts") {
        for (host, ips) in extra {
            for ip in ips.as_seq().map(Vec::as_slice).unwrap_or(&[]) {
                hosts.push(format!("{host}:{ip}"));
            }
        }
    }
    put_list(o, "extra_hosts", hosts);
    put_list(o, "ulimits", ulimits(service));
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

/// The ulimits of a service or of its build section, as NAME=SOFT:HARD.
pub(super) fn ulimits(section: &Map) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(limits) = sub(section, "ulimits") {
        for (name, limit) in limits {
            match limit {
                Value::Map(m) => out.push(format!(
                    "{name}={}:{}",
                    int(m.get("soft")).unwrap_or(0),
                    int(m.get("hard")).unwrap_or(0)
                )),
                other => {
                    let n = int(Some(other)).unwrap_or(0);
                    out.push(format!("{name}={n}:{n}"));
                }
            }
        }
    }
    out
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

/// The names a machine made of `made` goes by on its networks, NETWORK=NAME: an existing
/// machine without one of them is stale, as another service's link may add one without
/// changing its own configuration.
pub fn aliases(made: &Made) -> std::collections::BTreeSet<String> {
    match made.options.get("aliases") {
        Some(Bus::Array(items)) => items
            .iter()
            .filter_map(|v| String::try_from(v.clone()).ok())
            .collect(),
        _ => std::collections::BTreeSet::new(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use clap::Parser;

    /// The project of a compose file, as up loads it.
    pub(crate) fn project(name: &str, yaml: &str) -> Project {
        project_of(name, yaml, &[])
    }

    /// `base` in a directory of this call's own: tests that load projects of one name at
    /// once would write and remove each other's files. The name is the project's.
    fn scratch(base: &str) -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir()
            .join(format!("nspawn-test-{}-{n}", std::process::id()))
            .join(base)
    }

    /// What loading these files (compose.yaml first) says is wrong with them.
    pub(crate) fn load_error(name: &str, files: &[(&str, &str)]) -> String {
        let dir = scratch(&format!("nspawn-load-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (file, text) in files {
            std::fs::write(dir.join(file), text).unwrap();
        }
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
        let error = crate::compose::config::project_for(&args, &flags, &host, &[], &mut |_| {})
            .err()
            .unwrap_or_default();
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
        error
    }

    #[test]
    fn what_docker_compose_crashes_on_or_fetches_is_refused() {
        for services in ["[a, b]", "null"] {
            let error = load_error(
                "include-list",
                &[
                    (
                        "compose.yaml",
                        &format!("include: [other.yaml]\nservices: {services}\n"),
                    ),
                    ("other.yaml", "services:\n  web:\n    image: nginx\n"),
                ],
            );
            assert_eq!(error, "services must be a mapping", "{services}");
        }
        for file in [
            "git@github.com:org/stack.git#main:base.yaml",
            "oci://example.invalid/stack:1",
        ] {
            let error = load_error(
                "extends-remote",
                &[(
                    "compose.yaml",
                    &format!("services:\n  web:\n    extends: {{file: '{file}', service: base}}\n"),
                )],
            );
            assert!(
                error.contains("services.web.extends.file") && error.contains(file),
                "{error}"
            );
        }
    }

    #[test]
    fn an_absolute_env_file_is_opened_as_written() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("app.env");
        std::fs::write(&file, "A=1\n").unwrap();
        let path = format!("{}/", file.display());
        let error = load_error(
            "envfile-slash",
            &[(
                "compose.yaml",
                &format!("services:\n  web:\n    image: nginx\n    env_file: ['{path}']\n"),
            )],
        );
        assert_eq!(error, format!("open {path}: not a directory"));
    }

    /// The project with only `services` selected, as `up SERVICE...` loads it.
    pub(crate) fn project_of(name: &str, yaml: &str, services: &[String]) -> Project {
        let dir = scratch(&format!("nspawn-convert-{name}-{}", std::process::id()));
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
            crate::compose::config::project_for(&args, &flags, &host, services, &mut |_| {})
                .unwrap();
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
        project
    }

    #[test]
    fn a_job_runs_with_its_env_file_and_the_jobs_it_needs() {
        let dir = std::env::temp_dir().join(format!("nspawn-job-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("compose.yaml"),
            "services:\n  web: {image: x, env_file: web.env, volumes: [\"pgdata:/data\"]}\njobs:\n  seed: {image: app, triggers: {manual: true}}\n  migrate: {image: app, env_file: migrate.env, depends_on: [seed], triggers: {schedule: [{cron: \"0 3 * * *\"}]}}\n  off: {image: app, triggers: {manual: false}}\n  uses-off: {image: app, depends_on: [off], triggers: {manual: true}}\nvolumes:\n  pgdata: {}\n",
        )
        .unwrap();
        std::fs::write(dir.join("migrate.env"), "DATABASE_URL=postgres://db\n").unwrap();
        let job = |name: &str| {
            let cli = crate::cli::Cli::try_parse_from(["nspawn", "compose", "run", name]).unwrap();
            let crate::cli::Command::Compose(args) = cli.command else {
                unreachable!()
            };
            let env =
                crate::compose::dotenv::Env::from([("HOME".to_string(), "/nowhere".to_string())]);
            let host = crate::compose::config::Host::new(
                env,
                dir.to_string_lossy().into_owned(),
                crate::compose::config::Stdin::Process,
            );
            let flags = crate::compose::options::Flags::default();
            crate::compose::config::job_project(&args, &flags, &host, name, &mut |_| {})
        };
        // web.env is not there, and web is not needed: it is not read.
        let project = job("migrate").unwrap().unwrap();
        assert!(project.services.contains_key("migrate"));
        assert!(project.services.contains_key("seed"), "a job it depends on");
        assert!(!project.services.contains_key("web"));
        let migrate = &project.services["migrate"];
        assert_eq!(
            migrate.get(&["environment", "DATABASE_URL"]),
            Some(&Value::Str("postgres://db".into()))
        );
        assert!(migrate.get(&["triggers"]).is_none(), "run as a service");
        // The resources the load kept stay, as runProject keeps them: `run -v pgdata:/x`
        // is the project's volume.
        assert!(project.volumes.contains_key("pgdata"));
        assert!(job("nope").unwrap().is_none());
        assert!(job("off").unwrap_err().contains("cannot be run manually"));
        assert!(job("uses-off")
            .unwrap_err()
            .contains("cannot be triggered even as a dependency of another job"));
        let _ = std::fs::remove_dir_all(&dir);
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
            image_cmd: vec!["nginx".into(), "-g".into(), "daemon off;".into()],
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

    #[test]
    fn restart_policies_as_compose_maps_them() {
        let p = project(
            "restarts",
            "services:\n  a:\n    image: x\n    deploy:\n      restart_policy:\n        condition: unless-stopped\n  b:\n    image: x\n    restart: always\n    deploy:\n      restart_policy:\n        max_attempts: 3\n  c:\n    image: x\n    restart: any\n",
        );
        assert_eq!(
            text(&made(&p, "a", 1), "restart").as_deref(),
            Some("unless-stopped")
        );
        assert_eq!(
            text(&made(&p, "b", 1), "restart"),
            None,
            "deploy's policy decides alone"
        );
        assert_eq!(
            text(&made(&p, "c", 1), "restart").as_deref(),
            Some("always")
        );
    }

    #[test]
    fn a_link_alias_names_the_linked_service() {
        let p = project(
            "links",
            "services:\n  db:\n    image: postgres\n  web:\n    image: nginx\n    links: [\"db:database\"]\n",
        );
        let db = made(&p, "db", 1);
        assert!(
            list(&db, "aliases")
                .iter()
                .any(|a| a.ends_with("=database")),
            "{:?}",
            list(&db, "aliases")
        );
        assert!(!list(&made(&p, "web", 1), "aliases")
            .iter()
            .any(|a| a.ends_with("=database")));
        // db brought up alone has it as well, for web to come up later.
        let alone = project_of(
            "links-alone",
            "services:\n  db:\n    image: postgres\n  web:\n    image: nginx\n    links: [\"db:database\"]\n",
            &["db".to_string()],
        );
        assert!(alone.disabled_services.contains_key("web"));
        assert!(aliases(&made(&alone, "db", 1))
            .iter()
            .any(|a| a.ends_with("=database")));
    }

    #[test]
    fn another_platform_is_said() {
        let other = if super::super::info::go_arch() == "arm64" {
            "amd64"
        } else {
            "arm64"
        };
        let p = project(
            "platform",
            &format!("services:\n  web:\n    image: nginx\n    platform: linux/{other}\n"),
        );
        let web = made(&p, "web", 1);
        assert!(
            web.notes.iter().any(|n| n.contains("platform")),
            "{:?}",
            web.notes
        );
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
    ports: ["127.0.0.1:8080:80", "8443:443/udp", "0.0.0.0:8081:81", "[::1]:8082:82"]
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
    fn an_empty_entrypoint_keeps_the_image_command_as_docker_merges_them() {
        // What docker compose v5.6.0 on docker 28.5.1 made of these with nginx:alpine.
        let project = project(
            "q",
            r#"
services:
  empty_list:
    image: nginx:alpine
    entrypoint: []
  empty_string:
    image: nginx:alpine
    entrypoint: ""
  both_empty:
    image: nginx:alpine
    entrypoint: []
    command: []
  sh_only:
    image: nginx:alpine
    entrypoint: ["/bin/sh"]
  cmd_empty:
    image: nginx:alpine
    command: []
"#,
        );
        for service in ["empty_list", "empty_string", "both_empty"] {
            let m = made(&project, service, 1);
            assert_eq!(text(&m, "entrypoint").as_deref(), Some(""), "{service}");
            assert_eq!(
                list(&m, "command"),
                ["nginx", "-g", "daemon off;"],
                "{service}"
            );
        }
        let m = made(&project, "sh_only", 1);
        assert_eq!(list(&m, "command"), ["/bin/sh"]);
        // An empty command alone is none: the image's entrypoint and command run.
        let m = made(&project, "cmd_empty", 1);
        assert_eq!(text(&m, "entrypoint"), None);
        assert!(list(&m, "command").is_empty());
    }

    #[test]
    fn a_service_becomes_the_options_of_its_machine() {
        let project = project("demo", DEMO);
        let web = made(&project, "web", 1);
        assert_eq!(list(&web, "networks"), ["demo_back"]);
        assert_eq!(list(&web, "aliases"), ["demo_back=web", "demo_back=www"]);
        // 0.0.0.0 is IPv4 alone, an IPv6 address goes in brackets.
        assert_eq!(
            list(&web, "publish"),
            [
                "127.0.0.1:8080:80",
                "8443:443/udp",
                "0.0.0.0:8081:81",
                "[::1]:8082:82"
            ]
        );
        for p in list(&web, "publish") {
            assert!(
                crate::bridge::parse_publish(std::slice::from_ref(&p)).is_ok(),
                "{p}"
            );
        }
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
        // depends_on orders compose's own starts: the machine carries no --depends-on,
        // which would tie it to db at boot and refuse db's removal.
        assert!(!web.options.contains_key("depends_on"));
        assert_eq!(list(&web, "extra_hosts"), ["gw:10.0.0.1"]);
        assert_eq!(list(&web, "ulimits"), ["nofile=64:128"]);
        assert_eq!(text(&web, "log_driver").as_deref(), Some("local"));
        assert_eq!(web.options.get("stop_timeout"), Some(&Bus::from(90u64)));
        // A negative grace period waits for as long as the program takes.
        let forever = self::project(
            "q",
            "services:\n  web:\n    image: nginx\n    stop_grace_period: -1s\n",
        );
        assert_eq!(
            made(&forever, "web", 1).options.get("stop_timeout"),
            Some(&Bus::from(crate::policy::NO_STOP_TIMEOUT))
        );
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
    fn only_a_bind_source_known_missing_is_made() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let here = tmp.path().join("here");
        std::fs::write(&here, "").unwrap();
        assert!(!absent(here.to_str().unwrap()));
        assert!(absent(tmp.path().join("gone").to_str().unwrap()));
        // A directory its caller cannot look into, as /etc/letsencrypt/live for a user:
        // what is under it is left to the service. Run as root, the file is seen.
        let closed = tmp.path().join("closed");
        std::fs::create_dir(&closed).unwrap();
        std::fs::write(closed.join("cert.pem"), "").unwrap();
        std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert!(!absent(closed.join("cert.pem").to_str().unwrap()));
        std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn scheduled_jobs_are_refused_by_up_and_an_empty_schedule_is_none() {
        let p = project(
            "sched",
            "services:\n  db: {image: postgres}\n\
             jobs:\n  sync: {image: app, triggers: {schedule: [{cron: \"0 * * * *\"}]}}\n\
             \x20 backup: {image: app, triggers: {schedule: [{cron: \"0 3 * * *\"}]}}\n\
             \x20 migrate: {image: app, triggers: {manual: true}}\n\
             \x20 never: {image: app, triggers: {schedule: []}}\n",
        );
        assert_eq!(
            super::super::up::reject_scheduled_jobs(&p)
                .unwrap_err()
                .to_string(),
            "scheduled jobs are not supported in this version: backup, sync"
        );
        // Refused by up, not by the support table: run takes a scheduled job.
        assert!(p.refused.iter().all(|r| r.service != "never"));
        let mut p = p;
        p.materialize_job("sync").unwrap();
        p = p
            .with_selected_services(&["sync".to_string()], false)
            .unwrap();
        super::super::up::check_refusals(&p).unwrap();
    }

    #[test]
    fn an_extension_under_jobs_is_no_job() {
        let dir = std::env::temp_dir().join(format!("nspawn-jobext-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("compose.yaml"),
            "services:\n  x-svc: {image: x, cpu_shares: 512}\n\
             jobs:\n  x-template: {image: alpine, cpu_shares: 512, triggers: {manual: true}}\n",
        )
        .unwrap();
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
        let mut warnings = Vec::new();
        crate::compose::config::project_for(&args, &flags, &host, &[], &mut |w| warnings.push(w))
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(warnings.iter().any(|w| w.contains("x-svc")), "{warnings:?}");
        assert!(
            !warnings.iter().any(|w| w.contains("x-template")),
            "{warnings:?}"
        );
    }

    #[test]
    fn run_makes_a_job_a_service() {
        let mut p = project(
            "jobs",
            "services:\n  db:\n    image: postgres\n\
             jobs:\n  migrate:\n    image: app\n    depends_on: [db]\n    triggers: {manual: true}\n\
             \x20 nightly:\n    image: app\n    triggers: {manual: false}\n",
        );
        p.materialize_job("migrate").unwrap();
        assert!(p.services.contains_key("migrate") && !p.jobs.contains_key("migrate"));
        let err = p.materialize_job("nightly").unwrap_err();
        assert_eq!(
            err,
            "job \"nightly\" is declared with manual: false, it cannot be run manually"
        );
        assert_eq!(
            p.materialize_job("nope").unwrap_err(),
            "no such service: nope"
        );
    }

    #[test]
    fn only_commands_that_make_machines_read_env_files() {
        let dir = std::env::temp_dir().join(format!("nspawn-envfiles-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("compose.yaml"),
            "services:\n  web:\n    image: nginx\n    env_file: gone.env\n",
        )
        .unwrap();
        let load = |command: &str| {
            let cli = crate::cli::Cli::try_parse_from(["nspawn", "compose", command]).unwrap();
            let crate::cli::Command::Compose(args) = cli.command else {
                unreachable!()
            };
            let host = crate::compose::config::Host::new(
                crate::compose::dotenv::Env::from([("HOME".to_string(), "/nowhere".to_string())]),
                dir.to_string_lossy().into_owned(),
                crate::compose::config::Stdin::Process,
            );
            let flags = crate::compose::options::Flags::default();
            crate::compose::config::project_for(&args, &flags, &host, &[], &mut |_| {})
        };
        assert!(load("down").is_ok());
        assert!(load("up").unwrap_err().contains("gone.env"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compatibility_leaves_the_project_normalized() {
        let dir = std::env::temp_dir().join(format!("nspawn-compat-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("compose.yaml"),
            "services:\n  web:\n    image: nginx\n",
        )
        .unwrap();
        let cli = crate::cli::Cli::try_parse_from(["nspawn", "compose", "--compatibility", "up"])
            .unwrap();
        let crate::cli::Command::Compose(args) = cli.command else {
            unreachable!()
        };
        let host = crate::compose::config::Host::new(
            crate::compose::dotenv::Env::from([("HOME".to_string(), "/nowhere".to_string())]),
            dir.to_string_lossy().into_owned(),
            crate::compose::config::Stdin::Process,
        );
        let flags = crate::compose::options::Flags::default();
        let p =
            crate::compose::config::project_for(&args, &flags, &host, &[], &mut |_| {}).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(p.networks.contains_key("default"), "{:?}", p.networks);
    }

    #[test]
    fn the_owner_of_a_network_is_found_without_its_dependencies() {
        let p = project(
            "owner",
            "services:\n  vpn:\n    image: gluetun\n  torrent:\n    image: qb\n    network_mode: service:vpn\n",
        )
        .with_selected_services(&["torrent".to_string()], true)
        .unwrap();
        let torrent = made(&p, "torrent", 1);
        let network = text(&torrent, "network").unwrap();
        assert!(
            network.starts_with("container:") && network.ends_with("-vpn-1"),
            "{network}"
        );
    }

    #[test]
    fn a_named_service_has_one_replica() {
        let p = project(
            "named",
            "services:\n  api:\n    image: nginx\n    container_name: api\n",
        );
        assert!(super::super::up::unique_names(&p, &BTreeMap::new()).is_ok());
        let two = BTreeMap::from([("api".to_string(), 2)]);
        assert!(super::super::up::unique_names(&p, &two).is_err());
        let p = project(
            "named",
            "services:\n  my_db:\n    image: postgres\n  my-db:\n    image: postgres\n",
        );
        let err = super::super::up::unique_names(&p, &BTreeMap::new()).unwrap_err();
        assert!(err.to_string().contains("-my-db-1: machine names"), "{err}");
        let p = project(
            "named",
            "services:\n  a:\n    image: x\n    container_name: app_db\n  b:\n    image: x\n    container_name: app-db\n",
        );
        assert!(super::super::up::unique_names(&p, &BTreeMap::new()).is_err());
    }

    #[test]
    fn neutral_values_are_not_refused() {
        let p = project(
            "neutral",
            "services:\n  web:\n    image: nginx\n    secrets: [s]\n    networks: [n]\n    volumes: [v:/v]\n\
             secrets:\n  s: {file: ./s.txt, external: false}\n\
             networks:\n  n: {driver: \"\"}\n\
             volumes:\n  v: {driver: \"\"}\n",
        );
        super::super::up::check_refusals(&p).unwrap();
    }

    #[test]
    fn refusals_reach_x_named_and_dotted_resources() {
        let p = project(
            "refusals",
            "services:\n  x-app:\n    image: alpine\n    volumes: [{type: volume, source: shared, target: /d, volume: {subpath: a}}]\n\
             \x20 db:\n    image: postgres\n    volumes: [app.data:/x]\n\
             volumes:\n  shared: {}\n  app.data:\n    driver: nfs\n",
        );
        let err = super::super::up::check_refusals(&p)
            .unwrap_err()
            .to_string();
        assert!(err.contains("subpath"), "{err}");
        assert!(err.contains("app.data.driver"), "{err}");
        // A resource whose name another one's starts is not that one: unused, its
        // refusal does not apply.
        let p = project(
            "prefixes",
            "services:\n  web:\n    image: nginx\n    networks: [front]\n    volumes: [data:/d]\n\
             networks:\n  front: {}\n  front.net: {driver: macvlan}\n\
             volumes:\n  data: {}\n  data.old: {driver: nfs}\n",
        );
        super::super::up::check_refusals(&p).unwrap();
        // Models only for the services to run that use them.
        let models = "services:\n  ai:\n    image: alpine\n    models: [llm]\n\
                      \x20 web:\n    image: nginx\n\
                      models:\n  llm: {model: ai/smollm2}\n";
        let web = project_of("models-web", models, &["web".to_string()]);
        super::super::up::check_refusals(&web).unwrap();
        let all = project("models-all", models);
        assert!(super::super::up::check_refusals(&all)
            .unwrap_err()
            .to_string()
            .contains("Docker Model Runner"));
    }

    #[test]
    fn depends_on_stays_compose_s_own() {
        let p = project(
            "deps",
            "services:\n  db:\n    image: postgres\n    deploy: {replicas: 2}\n\
             \x20 migrate:\n    image: app\n\
             \x20 web:\n    image: nginx\n    depends_on:\n\
             \x20     db: {condition: service_healthy}\n\
             \x20     migrate: {condition: service_completed_successfully}\n",
        );
        let made = options(&Spec {
            project: &p,
            service: "web",
            number: 1,
            name: "web".to_string(),
            oneoff: false,
            image_digest: String::new(),
            image_cmd: Vec::new(),
            env_files: Vec::new(),
        })
        .unwrap();
        // The machine knows nothing of depends_on: no --depends-on ties web to db at
        // boot or refuses db's removal, and compose orders its own starts...
        assert!(!made.options.contains_key("depends_on"));
        // ...by the label, from which it finds them without the file too.
        let labels = list(&made, "label");
        assert!(
            labels.contains(&"com.docker.compose.depends_on=db:service_healthy:false,migrate:service_completed_successfully:false".to_string()),
            "{labels:?}"
        );
    }

    #[test]
    fn a_volume_of_the_local_driver_binds_its_device() {
        let local = project(
            "devices",
            "services:\n  db:\n    image: postgres\n    volumes: [pg:/data, plain:/plain]\n\
             volumes:\n  pg:\n    driver: local\n    driver_opts: {type: none, o: bind, device: /srv/pg}\n\
             \x20 plain: {}\n",
        );
        assert_eq!(
            volume_device(&local, "pg").unwrap(),
            Some(Device {
                path: "/srv/pg".into(),
                read_only: false
            })
        );
        assert_eq!(volume_device(&local, "plain").unwrap(), None);
        let db = made(&local, "db", 1);
        let plain = format!("{}_plain:/plain", local.name);
        assert_eq!(list(&db, "volume"), ["/srv/pg:/data", plain.as_str()]);
        // ro among the options makes the bind read-only; options nspawn cannot apply
        // are refused rather than dropped.
        let ro = project(
            "devices-ro",
            "services:\n  app:\n    image: x\n    volumes: [conf:/etc/app]\n\
             volumes:\n  conf:\n    driver_opts: {type: none, o: \"bind,ro\", device: /srv/conf}\n",
        );
        assert_eq!(
            list(&made(&ro, "app", 1), "volume"),
            ["/srv/conf:/etc/app:ro"]
        );
        let noexec = project(
            "devices-noexec",
            "services:\n  app:\n    image: x\n    volumes: [conf:/etc/app]\n\
             volumes:\n  conf:\n    driver_opts: {type: none, o: \"bind,noexec\", device: /srv/conf}\n",
        );
        let e = volume_device(&noexec, "conf").unwrap_err().to_string();
        assert!(
            e.contains("the mount option noexec of o is not taken"),
            "{e}"
        );
        // Options nothing honours are refused, not taken for a directory of nspawn's.
        let nfs = project(
            "nfs",
            "services:\n  db:\n    image: postgres\n    volumes: [nfs:/nfs]\n\
             volumes:\n  nfs:\n    driver_opts: {type: nfs, o: addr=10.0.0.5, device: ':/x'}\n",
        );
        assert!(volume_device(&nfs, "nfs").is_err());
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
        assert_eq!(list(&db, "health_test"), ["CMD", "pg_isready", "-U", "app"]);
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
    fn gpus_and_cdi_devices_become_device_requests() {
        let project = project(
            "gpu",
            r#"
services:
  ml:
    image: busybox
    devices:
      - /dev/null:/dev/nullo:r
      - e2e.nspawn.test/dev=one
    gpus: all
  train:
    image: busybox
    gpus:
      - driver: nvidia
        device_ids: ["0", "1"]
        capabilities: [gpu, compute]
    deploy:
      resources:
        reservations:
          devices:
            - capabilities: [gpu]
              count: 1
"#,
        );
        let ml = made(&project, "ml", 1);
        assert_eq!(
            list(&ml, "devices"),
            ["/dev/null:/dev/nullo:r", "e2e.nspawn.test/dev=one"]
        );
        assert_eq!(list(&ml, "gpus"), ["all"]);
        let train = made(&project, "train", 1);
        assert_eq!(
            list(&train, "gpus"),
            [
                "count=1",
                r#""device=0,1",driver=nvidia,capabilities=compute"#
            ]
        );
        for request in list(&train, "gpus") {
            crate::cdi::parse_gpus(&request).unwrap();
        }
    }

    #[test]
    fn health_commands_keep_their_words() {
        let p = project(
            "health",
            "services:\n  web:\n    image: distroless\n    healthcheck:\n      test: [CMD, /app/check, a b]\n",
        );
        let web = made(&p, "web", 1);
        assert_eq!(list(&web, "health_test"), ["CMD", "/app/check", "a b"]);
        assert!(!web.options.contains_key("health_cmd"));
    }

    #[test]
    fn secrets_of_the_project_get_a_name_of_their_own() {
        assert_eq!(
            secret_name("demo", "secret", "db pass"),
            "demo_secret_db_pass"
        );
    }
}
