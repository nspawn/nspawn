//! nspawn's machines and networks as the Docker Engine API describes containers and
//! networks.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;

use anyhow::Result;
use serde_json::{json, Map, Value};

use super::util::{
    container_id, endpoint_id, familiar, human_duration, network_id, rfc3339_nano, short, ZERO_TIME,
};
use crate::api::machines::MachineSummary;
use crate::api::Context;
use crate::bridge::{self, NetSpec, DEFAULT_NETWORK};
use crate::oci::Mode;
use crate::settings::Network;
use crate::store::{now_unix, ImageRecord};

/// A machine nspawn keeps a record of; others machined lists are no containers here.
pub struct Machine {
    pub id: String,
    pub record: ImageRecord,
    pub summary: MachineSummary,
    /// When its last run ended, in microseconds, for one that ended.
    pub finished_usec: Option<u64>,
    /// How many times its restart policy brought it back.
    pub restarts: u32,
}

/// Everything at the time of one request.
pub struct Snapshot {
    pub machines: Vec<Machine>,
    pub networks: Vec<NetSpec>,
}

/// Whether a machine in `state` has a run that ended, whose end docker shows: one
/// stopped, or between two runs of its restart policy.
fn ended(state: &str) -> bool {
    matches!(state, "stopped" | "restarting")
}

impl Snapshot {
    pub async fn take(ctx: &Context) -> Result<Snapshot> {
        let sd = ctx.sd().await?;
        let mut machines = Vec::new();
        for summary in crate::api::machines::list(ctx, true).await? {
            let Some(record) = summary.record.clone() else {
                continue;
            };
            let unit = format!("systemd-nspawn@{}.service", record.name);
            let finished_usec = if ended(&summary.state) {
                sd.exec_main_exit_usec(&unit).await.ok().filter(|t| *t > 0)
            } else {
                None
            };
            let restarts = sd.n_restarts(&unit).await.unwrap_or(0);
            machines.push(Machine {
                id: container_id(&record.name, record.created),
                record,
                summary,
                finished_usec,
                restarts,
            });
        }
        let networks = crate::api::network::all(&ctx.store, &ctx.config)?;
        Ok(Snapshot { machines, networks })
    }

    /// By ID, by name with or without docker's leading slash, or by a unique prefix of
    /// the ID, in that order, as docker looks one up.
    /// A container by its ID, its name or a prefix of its ID, as docker's GetContainer
    /// finds it, and its words when there is none.
    pub fn find(&self, key: &str) -> Result<&Machine, String> {
        if let Some(m) = self.machines.iter().find(|m| m.id == key) {
            return Ok(m);
        }
        let name = key.strip_prefix('/').unwrap_or(key);
        if let Some(m) = self.machines.iter().find(|m| m.record.name == name) {
            return Ok(m);
        }
        let mut found = self
            .machines
            .iter()
            .filter(|m| !key.is_empty() && m.id.starts_with(key));
        match (found.next(), found.next()) {
            (Some(m), None) => Ok(m),
            (Some(_), Some(_)) => Err(format!("multiple IDs found with provided prefix: {key}")),
            (None, _) => Err(format!("No such container: {key}")),
        }
    }

    pub fn network(&self, key: &str) -> Option<NetworkView<'_>> {
        let all = self.network_views();
        if let Some(n) = all.iter().find(|n| n.id == key) {
            return Some(n.clone());
        }
        if let Some(n) = all.iter().find(|n| n.name == key) {
            return Some(n.clone());
        }
        unique_prefix(all.iter(), key, |n| &n.id).cloned()
    }

    /// nspawn's bridge networks, then the host's and none, which docker lists too.
    pub fn network_views(&self) -> Vec<NetworkView<'_>> {
        let mut views: Vec<NetworkView> = self
            .networks
            .iter()
            .map(|spec| NetworkView {
                id: network_id(&spec.name, spec.created),
                name: spec.name.clone(),
                spec: Some(spec),
            })
            .collect();
        for name in ["host", "none"] {
            views.push(NetworkView {
                id: network_id(name, 0),
                name: name.to_string(),
                spec: None,
            });
        }
        views
    }

    /// The bridge networks a machine joins, by name and ID, from its record.
    pub fn networks_of(ctx: &Context, machine: &str) -> Option<Vec<(String, String)>> {
        let record = ctx.store.load_image(machine).ok()??;
        if !bridge::bridge_kind(&record) {
            return Some(Vec::new());
        }
        let specs = crate::api::network::nets_of(&ctx.store, &ctx.config, &record).ok()?;
        Some(
            specs
                .iter()
                .map(|s| (s.name.clone(), network_id(&s.name, s.created)))
                .collect(),
        )
    }

    fn spec(&self, name: &str) -> Option<&NetSpec> {
        self.networks.iter().find(|n| n.name == name)
    }
}

fn unique_prefix<'a, T>(
    items: impl Iterator<Item = &'a T>,
    prefix: &str,
    id: impl Fn(&T) -> &String,
) -> Option<&'a T> {
    if prefix.is_empty() || !prefix.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut found = items.filter(|item| id(item).starts_with(prefix));
    let first = found.next()?;
    found.next().is_none().then_some(first)
}

#[derive(Clone)]
pub struct NetworkView<'a> {
    pub id: String,
    pub name: String,
    /// None for host and none.
    pub spec: Option<&'a NetSpec>,
}

impl Machine {
    pub fn name(&self) -> &str {
        &self.record.name
    }

    pub fn running(&self) -> bool {
        matches!(
            self.summary.state.as_str(),
            "running" | "opening" | "closing" | "paused"
        )
    }

    /// docker's state word: created, running, paused, restarting or exited.
    pub fn state(&self) -> &'static str {
        match self.summary.state.as_str() {
            "running" | "opening" | "closing" => "running",
            "paused" => "paused",
            "restarting" => "restarting",
            "stopped" if self.summary.exit_code.is_some() => "exited",
            _ => "created",
        }
    }

    /// docker's status line: "Up 3 minutes (healthy)", "Exited (1) 2 hours ago".
    pub fn status(&self, now: u64) -> String {
        let health = match self.summary.health.as_ref().map(|h| h.status.as_str()) {
            Some("healthy") => " (healthy)",
            Some("unhealthy") => " (unhealthy)",
            Some("starting") => " (health: starting)",
            _ => "",
        };
        let since = |t: u64| human_duration(now.saturating_sub(t));
        match self.state() {
            "running" => match self.summary.started {
                Some(started) => format!("Up {}{health}", since(started)),
                None => format!("Up{health}"),
            },
            "paused" => match self.summary.started {
                Some(started) => format!("Up {} (Paused)", since(started)),
                None => "Up (Paused)".to_string(),
            },
            "restarting" => format!(
                "Restarting ({}) {} ago",
                self.summary.exit_code.unwrap_or(0),
                since(self.finished_usec.map_or(now, |u| u / 1_000_000))
            ),
            "exited" => {
                let code = self.summary.exit_code.unwrap_or(0);
                match self.finished_usec {
                    Some(usec) => format!("Exited ({code}) {} ago", since(usec / 1_000_000)),
                    None => format!("Exited ({code})"),
                }
            }
            _ => "Created".to_string(),
        }
    }

    /// The program and its arguments.
    pub fn argv(&self) -> Vec<String> {
        match self.record.mode {
            Mode::App => self.record.effective_command(),
            Mode::Boot => vec!["/sbin/init".to_string()],
        }
    }

    fn command_line(&self) -> String {
        super::util::command_line(&self.argv())
    }

    /// What `HostConfig.NetworkMode` says: the primary network, host, none, or the
    /// machine whose namespace it shares.
    pub(super) fn network_mode(&self, snapshot: &Snapshot) -> String {
        let r = &self.record;
        if let Some(owner) = bridge::shares_network(r) {
            let id = snapshot
                .find(owner)
                .map_or_else(|_| owner.to_string(), |m| m.id.clone());
            return format!("container:{id}");
        }
        if r.no_network {
            return "none".to_string();
        }
        match r.network {
            Network::Host => "host".to_string(),
            Network::Veth => "default".to_string(),
            Network::Bridge => bridge::networks_of(r)[0].to_string(),
        }
    }

    /// The bridge networks it joins, each with its address there.
    fn attachments<'s>(&self, snapshot: &'s Snapshot) -> Vec<(&'s NetSpec, Option<Ipv4Addr>)> {
        if !bridge::bridge_kind(&self.record) {
            return Vec::new();
        }
        bridge::networks_of(&self.record)
            .into_iter()
            .filter_map(|net| {
                snapshot
                    .spec(net)
                    .map(|spec| (spec, bridge::address_on(&self.record, net)))
            })
            .collect()
    }

    fn mac(&self, address: Ipv4Addr) -> String {
        // Booted machines get theirs from systemd-nspawn, which nspawn does not set.
        if self.record.mode == Mode::App {
            bridge::mac_of(address)
        } else {
            String::new()
        }
    }

    fn endpoint(&self, snapshot: &Snapshot, spec: &NetSpec, address: Option<Ipv4Addr>) -> Value {
        let running = self.running();
        let address = address.filter(|_| running);
        let aliases: Vec<&String> = self
            .record
            .aliases
            .get(&spec.name)
            .map(|a| a.iter().collect())
            .unwrap_or_default();
        let mut dns_names = vec![self.name().to_string()];
        dns_names.extend(aliases.iter().map(|a| (*a).clone()));
        dns_names.push(short(&self.id).to_string());
        let _ = snapshot;
        json!({
            "IPAMConfig": null,
            "Links": null,
            "Aliases": if aliases.is_empty() { Value::Null } else { json!(aliases) },
            "MacAddress": address.map(|a| self.mac(a)).unwrap_or_default(),
            "DriverOpts": null,
            "GwPriority": 0,
            "NetworkID": network_id(&spec.name, spec.created),
            "EndpointID": if running { endpoint_id(self.name(), &spec.name) } else { String::new() },
            "Gateway": address.map(|_| spec.subnet.gateway().to_string()).unwrap_or_default(),
            "IPAddress": address.map(|a| a.to_string()).unwrap_or_default(),
            "IPPrefixLen": if address.is_some() { spec.subnet.prefix } else { 0 },
            "IPv6Gateway": "",
            "GlobalIPv6Address": "",
            "GlobalIPv6PrefixLen": 0,
            "DNSNames": if running { json!(dns_names) } else { Value::Null },
        })
    }

    fn networks(&self, snapshot: &Snapshot) -> Map<String, Value> {
        let mut out = Map::new();
        for (spec, address) in self.attachments(snapshot) {
            out.insert(spec.name.clone(), self.endpoint(snapshot, spec, address));
        }
        if out.is_empty() {
            let mode = self.network_mode(snapshot);
            if mode == "host" || mode == "none" {
                out.insert(mode.clone(), host_endpoint(&mode));
            }
        }
        out
    }

    /// Published ports, then the image's exposed ones nothing publishes.
    fn port_list(&self, exposed: &[String]) -> Vec<Value> {
        let mut out = Vec::new();
        for p in &self.record.ports {
            out.push(json!({
                "IP": p.host_ip.map_or_else(|| "0.0.0.0".to_string(), |ip| ip.to_string()),
                "PrivatePort": p.container,
                "PublicPort": p.host,
                "Type": p.protocol.name(),
            }));
        }
        for e in exposed {
            let (port, protocol) = e.split_once('/').unwrap_or((e, "tcp"));
            let Ok(port) = port.parse::<u16>() else {
                continue;
            };
            let published = self
                .record
                .ports
                .iter()
                .any(|p| p.container == port && p.protocol.name() == protocol);
            if !published {
                out.push(json!({ "PrivatePort": port, "Type": protocol }));
            }
        }
        out
    }

    fn mounts(&self, ctx: &Context) -> Vec<Value> {
        self.record
            .volumes
            .iter()
            .map(|v| {
                if v.is_named() {
                    json!({
                        "Type": "volume",
                        "Name": v.source,
                        "Source": v.host_path(&ctx.store.volumes_dir()).to_string_lossy(),
                        "Destination": v.target,
                        "Driver": "local",
                        "Mode": "z",
                        "RW": !v.read_only,
                        "Propagation": "",
                    })
                } else {
                    json!({
                        "Type": "bind",
                        "Source": v.source,
                        "Destination": v.target,
                        "Mode": if v.read_only { "ro" } else { "" },
                        "RW": !v.read_only,
                        "Propagation": "rprivate",
                    })
                }
            })
            .collect()
    }

    /// An entry of GET /containers/json.
    pub fn summary_json(&self, ctx: &Context, snapshot: &Snapshot, image: &ImageInfo) -> Value {
        json!({
            "Id": self.id,
            "Names": [format!("/{}", self.name())],
            "Image": familiar(&self.record.reference),
            "ImageID": image.id,
            "Command": self.command_line(),
            "Created": self.record.created,
            // docker lists the ports of a running container only.
            "Ports": if self.running() { self.port_list(&image.exposed_ports) } else { Vec::new() },
            "Labels": self.record.effective_labels(),
            "State": self.state(),
            "Status": self.status(now_unix()),
            "HostConfig": { "NetworkMode": self.network_mode(snapshot) },
            "NetworkSettings": { "Networks": self.networks(snapshot) },
            "Mounts": self.mounts(ctx),
        })
    }

    /// GET /containers/{id}/json.
    pub fn inspect_json(&self, ctx: &Context, snapshot: &Snapshot, image: &ImageInfo) -> Value {
        let r = &self.record;
        let argv = self.argv();
        let health = self.summary.health.as_ref().map(|h| {
            json!({
                "Status": h.status,
                "FailingStreak": h.failing_streak,
                "Log": h.log.iter().map(|p| json!({
                    "Start": rfc3339_nano(p.start * 1_000_000, false),
                    "End": rfc3339_nano(p.end * 1_000_000, false),
                    "ExitCode": p.exit_code,
                    "Output": p.output,
                })).collect::<Vec<_>>(),
            })
        });
        // docker's Running stays true between two runs of a restart policy.
        let mut state = json!({
            "Status": self.state(),
            "Running": self.running() || self.state() == "restarting",
            "Paused": self.state() == "paused",
            "Restarting": self.state() == "restarting",
            "OOMKilled": false,
            "Dead": false,
            "Pid": if self.running() { self.summary.leader.unwrap_or(0) } else { 0 },
            "ExitCode": self.summary.exit_code.unwrap_or(0),
            "Error": "",
            "StartedAt": self.summary.started.map_or_else(|| ZERO_TIME.to_string(), |s| rfc3339_nano(s * 1_000_000, false)),
            "FinishedAt": self.finished_usec.map_or_else(|| ZERO_TIME.to_string(), |u| rfc3339_nano(u, false)),
        });
        if let Some(health) = health {
            state["Health"] = health;
        }
        let ports = self.port_bindings(&image.exposed_ports);
        let hostname = r.tuning.hostname.clone().unwrap_or_else(|| r.name.clone());
        let env = crate::volume::merge_env(&r.run.env, &r.env);
        // Fields left at their default are not written, as docker leaves them out.
        let healthcheck = r.effective_healthcheck().map(|h| {
            let mut check = json!({ "Test": h.test });
            for (key, usec) in [
                ("Interval", h.interval),
                ("Timeout", h.timeout),
                ("StartPeriod", h.start_period),
                ("StartInterval", h.start_interval),
            ] {
                if usec > 0 {
                    check[key] = json!(usec * 1000);
                }
            }
            if h.retries > 0 {
                check["Retries"] = json!(h.retries);
            }
            check
        });
        let exposed: Map<String, Value> = image
            .exposed_ports
            .iter()
            .map(|p| (p.clone(), json!({})))
            .collect();
        let volumes: Map<String, Value> = r
            .run
            .volumes
            .iter()
            .map(|v| (v.clone(), json!({})))
            .collect();
        // null for none, as docker writes a nil list.
        let list = |words: &[String]| match words {
            [] => Value::Null,
            words => json!(words),
        };
        let entrypoint = list(r.effective_entrypoint());
        let mut config = json!({
            "Hostname": hostname,
            "Domainname": "",
            "User": r.effective_user().unwrap_or(""),
            "AttachStdin": false,
            "AttachStdout": false,
            "AttachStderr": false,
            "Tty": false,
            "OpenStdin": false,
            "StdinOnce": false,
            "Env": env,
            "Cmd": list(r.effective_cmd()),
            "Image": familiar(&r.reference),
            "Volumes": if volumes.is_empty() { Value::Null } else { Value::Object(volumes) },
            "WorkingDir": r.effective_working_dir().unwrap_or(""),
            "Entrypoint": entrypoint,
            "OnBuild": null,
            "Labels": r.effective_labels(),
        });
        if !exposed.is_empty() {
            config["ExposedPorts"] = Value::Object(exposed);
        }
        if let Some(signal) = r.effective_stop_signal() {
            config["StopSignal"] = json!(signal);
        }
        if let Some(timeout) = r.tuning.stop_timeout {
            config["StopTimeout"] = json!(crate::policy::shown_stop_timeout(timeout));
        }
        if let Some(h) = healthcheck {
            config["Healthcheck"] = h;
        }
        let machine_dir = ctx.store.machines_private_dir().join(&r.name);
        let binds: Vec<String> = r
            .volumes
            .iter()
            .map(|v| {
                let mut text = format!("{}:{}", v.source, v.target);
                if v.read_only {
                    text.push_str(":ro");
                }
                text
            })
            .collect();
        let restart = match r.restart {
            crate::policy::Restart::No => "no",
            crate::policy::Restart::OnFailure => "on-failure",
            crate::policy::Restart::Always => "always",
            crate::policy::Restart::UnlessStopped => "unless-stopped",
        };
        let mut port_bindings = Map::new();
        for p in &r.ports {
            let key = format!("{}/{}", p.container, p.protocol.name());
            let entry = port_bindings.entry(key).or_insert_with(|| json!([]));
            if let Some(list) = entry.as_array_mut() {
                list.push(json!({
                    "HostIp": p.host_ip.map(|ip| ip.to_string()).unwrap_or_default(),
                    "HostPort": p.host.to_string(),
                }));
            }
        }
        let t = &r.tuning;
        let host_config = json!({
            "Binds": if binds.is_empty() { Value::Null } else { json!(binds) },
            "ContainerIDFile": "",
            "LogConfig": { "Type": if t.log_driver.as_deref() == Some("none") { "none" } else { "journald" }, "Config": {} },
            "NetworkMode": self.network_mode(snapshot),
            "PortBindings": port_bindings,
            "RestartPolicy": { "Name": restart, "MaximumRetryCount": 0 },
            "AutoRemove": r.remove_on_exit,
            "VolumeDriver": "",
            "VolumesFrom": null,
            "CapAdd": if t.cap_add.is_empty() { Value::Null } else { json!(t.cap_add) },
            "CapDrop": if t.cap_drop.is_empty() { Value::Null } else { json!(t.cap_drop) },
            "CgroupnsMode": "private",
            "Dns": t.dns,
            "DnsOptions": [],
            "DnsSearch": t.dns_search,
            "ExtraHosts": if t.extra_hosts.is_empty() { Value::Null } else { json!(t.extra_hosts.iter().map(|h| format!("{}:{}", h.host, h.ip)).collect::<Vec<_>>()) },
            "GroupAdd": null,
            "IpcMode": "private",
            "Cgroup": "",
            "Links": null,
            "OomScoreAdj": t.oom_score_adj.unwrap_or(0),
            "PidMode": "",
            "Privileged": t.privileged,
            "PublishAllPorts": false,
            "ReadonlyRootfs": t.read_only,
            "SecurityOpt": null,
            "UTSMode": "",
            "UsernsMode": "",
            "ShmSize": t.shm_size.unwrap_or(64 * 1024 * 1024),
            "Runtime": "systemd-nspawn",
            "Isolation": "",
            "Memory": r.limits.memory,
            "MemorySwap": r.limits.memory_swap,
            "NanoCpus": r.limits.milli_cpus * 1_000_000,
            "PidsLimit": if r.limits.pids == 0 { Value::Null } else { json!(r.limits.pids) },
            "Devices": t.devices.iter().map(|d| json!({
                "PathOnHost": d.host,
                "PathInContainer": d.container,
                "CgroupPermissions": d.permissions,
            })).collect::<Vec<_>>(),
            "Ulimits": [],
        });
        // What docker writes for the settings nspawn has no counterpart of.
        let unset = json!({
            "BlkioWeight": 0,
            "BlkioWeightDevice": [],
            "BlkioDeviceReadBps": [],
            "BlkioDeviceWriteBps": [],
            "BlkioDeviceReadIOps": [],
            "BlkioDeviceWriteIOps": [],
            "CgroupParent": "",
            "ConsoleSize": [0, 0],
            "CpuCount": 0,
            "CpuPercent": 0,
            "CpuPeriod": 0,
            "CpuQuota": 0,
            "CpuRealtimePeriod": 0,
            "CpuRealtimeRuntime": 0,
            "CpuShares": 0,
            "CpusetCpus": "",
            "CpusetMems": "",
            "DeviceCgroupRules": null,
            "DeviceRequests": null,
            "IOMaximumBandwidth": 0,
            "IOMaximumIOps": 0,
            "MaskedPaths": [],
            "MemoryReservation": 0,
            "MemorySwappiness": null,
            "OomKillDisable": null,
            "ReadonlyPaths": [],
        });
        let mut host_config = host_config;
        if let (Some(config), Value::Object(unset)) = (host_config.as_object_mut(), unset) {
            config.extend(unset);
        }
        if let Some(init) = t.init {
            host_config["Init"] = json!(init);
        }
        // --gpus and CDI names as device requests.
        let mut requests: Vec<Value> = t
            .gpus
            .iter()
            .map(|g| {
                let mut capabilities = g.capabilities.clone();
                capabilities.push("gpu".to_string());
                json!({
                    "Driver": g.driver,
                    "Count": g.count,
                    "DeviceIDs": if g.device_ids.is_empty() { Value::Null } else { json!(g.device_ids) },
                    "Capabilities": [capabilities],
                    "Options": g.options,
                })
            })
            .collect();
        if !t.cdi_devices.is_empty() {
            requests.push(json!({
                "Driver": "cdi",
                "Count": 0,
                "DeviceIDs": t.cdi_devices,
                "Capabilities": null,
                "Options": null,
            }));
        }
        if !requests.is_empty() {
            host_config["DeviceRequests"] = Value::Array(requests);
        }
        if !t.tmpfs.is_empty() {
            host_config["Tmpfs"] = t
                .tmpfs
                .iter()
                .map(|text| {
                    let (path, options) = text.split_once(':').unwrap_or((text, ""));
                    (path.to_string(), json!(options))
                })
                .collect::<Map<String, Value>>()
                .into();
        }
        if !t.sysctls.is_empty() {
            host_config["Sysctls"] = json!(t.sysctls);
        }
        let machine_root = ctx.config.machines_dir.join(&r.name);
        json!({
            "Id": self.id,
            "Created": rfc3339_nano(r.created * 1_000_000, false),
            "Path": argv.first().cloned().unwrap_or_default(),
            "Args": argv.get(1..).unwrap_or(&[]).to_vec(),
            "State": state,
            "Image": image.id,
            "ResolvConfPath": machine_dir.join("resolv.conf").to_string_lossy(),
            "HostnamePath": "",
            "HostsPath": machine_dir.join("hosts").to_string_lossy(),
            "LogPath": "",
            "Name": format!("/{}", r.name),
            "RestartCount": self.restarts,
            "Driver": format!("{:?}", r.backend).to_lowercase(),
            "Platform": "linux",
            "MountLabel": "",
            "ProcessLabel": "",
            "AppArmorProfile": "",
            "ExecIDs": null,
            "HostConfig": host_config,
            "GraphDriver": {
                "Data": {
                    "ID": self.id,
                    "LowerDir": "",
                    "MergedDir": machine_root.to_string_lossy(),
                    "UpperDir": machine_dir.join("upper").to_string_lossy(),
                    "WorkDir": machine_dir.join("work").to_string_lossy(),
                },
                "Name": format!("{:?}", r.backend).to_lowercase(),
            },
            "Mounts": self.mounts(ctx),
            "Config": config,
            "NetworkSettings": {
                "Bridge": "",
                "SandboxID": "",
                "SandboxKey": if self.running() && bridge::namespaced(r) { format!("/run/netns/nspawn-{}", bridge::shares_network(r).unwrap_or(&r.name)) } else { String::new() },
                "Ports": ports,
                "HairpinMode": false,
                "LinkLocalIPv6Address": "",
                "LinkLocalIPv6PrefixLen": 0,
                "SecondaryIPAddresses": null,
                "SecondaryIPv6Addresses": null,
                "EndpointID": "",
                "Gateway": "",
                "GlobalIPv6Address": "",
                "GlobalIPv6PrefixLen": 0,
                "IPAddress": self.legacy_address(snapshot),
                "IPPrefixLen": 0,
                "IPv6Gateway": "",
                "MacAddress": "",
                "Networks": self.networks(snapshot),
            },
        })
    }

    /// NetworkSettings.Ports: each published port with its binding, an exposed one with
    /// none, as long as the machine runs.
    fn port_bindings(&self, exposed: &[String]) -> Map<String, Value> {
        let mut out = Map::new();
        if !self.running() {
            return out;
        }
        for p in &self.record.ports {
            let key = format!("{}/{}", p.container, p.protocol.name());
            let entry = out.entry(key).or_insert_with(|| json!([]));
            if let Some(list) = entry.as_array_mut() {
                list.push(json!({
                    "HostIp": p.host_ip.map_or_else(|| "0.0.0.0".to_string(), |ip| ip.to_string()),
                    "HostPort": p.host.to_string(),
                }));
            }
        }
        // The image's, from its config for a record written before they were kept.
        for e in exposed {
            out.entry(e.clone()).or_insert(Value::Null);
        }
        out
    }

    /// The top-level address docker still fills for a container of its default
    /// network.
    fn legacy_address(&self, snapshot: &Snapshot) -> String {
        if !self.running() || bridge::networks_of(&self.record).first() != Some(&DEFAULT_NETWORK) {
            return String::new();
        }
        self.attachments(snapshot)
            .first()
            .and_then(|(_, a)| *a)
            .map(|a| a.to_string())
            .unwrap_or_default()
    }
}

fn host_endpoint(name: &str) -> Value {
    json!({
        "IPAMConfig": null,
        "Links": null,
        "Aliases": null,
        "MacAddress": "",
        "DriverOpts": null,
        "GwPriority": 0,
        "NetworkID": network_id(name, 0),
        "EndpointID": "",
        "Gateway": "",
        "IPAddress": "",
        "IPPrefixLen": 0,
        "IPv6Gateway": "",
        "GlobalIPv6Address": "",
        "GlobalIPv6PrefixLen": 0,
        "DNSNames": null,
    })
}

/// What a container's image is to the API: its ID (the config's digest) and the ports
/// its config exposes.
pub struct ImageInfo {
    pub id: String,
    pub exposed_ports: Vec<String>,
}

impl ImageInfo {
    pub fn of(ctx: &Context, record: &ImageRecord) -> ImageInfo {
        let config = ctx
            .store
            .load_manifest(&record.name)
            .ok()
            .and_then(|bytes| {
                serde_json::from_slice::<oci_client::manifest::OciImageManifest>(&bytes).ok()
            })
            .map(|m| m.config.digest);
        let exposed_ports = if record.run.exposed_ports.is_empty() {
            // Records written before the ports were kept: the config still has them.
            config
                .as_deref()
                .and_then(|d| std::fs::read(ctx.store.blob_path(d)).ok())
                .map(|blob| crate::oci::RunSpec::exposed_ports_of(&blob))
                .unwrap_or_default()
        } else {
            record.run.exposed_ports.clone()
        };
        ImageInfo {
            id: config.unwrap_or_else(|| record.manifest_digest.clone()),
            exposed_ports,
        }
    }
}

/// GET /networks/{id}: the network with the running machines on it.
pub fn network_json(snapshot: &Snapshot, view: &NetworkView) -> Value {
    let mut containers = Map::new();
    for m in snapshot.machines.iter().filter(|m| m.running()) {
        match view.spec {
            Some(spec) => {
                if let Some((_, Some(address))) = m
                    .attachments(snapshot)
                    .into_iter()
                    .find(|(s, _)| s.name == spec.name)
                {
                    containers.insert(
                        m.id.clone(),
                        json!({
                            "Name": m.name(),
                            "EndpointID": endpoint_id(m.name(), &spec.name),
                            "MacAddress": m.mac(address),
                            "IPv4Address": format!("{address}/{}", spec.subnet.prefix),
                            "IPv6Address": "",
                        }),
                    );
                }
            }
            None => {
                if m.network_mode(snapshot) == view.name {
                    containers.insert(
                        m.id.clone(),
                        json!({
                            "Name": m.name(),
                            "EndpointID": "",
                            "MacAddress": "",
                            "IPv4Address": "",
                            "IPv6Address": "",
                        }),
                    );
                }
            }
        }
    }
    let (driver, ipam, created, internal, labels, options) = match view.spec {
        Some(spec) => (
            "bridge",
            json!([{ "Subnet": format!("{}/{}", spec.subnet.network, spec.subnet.prefix), "Gateway": spec.subnet.gateway().to_string() }]),
            spec.created,
            spec.internal,
            spec.labels.clone(),
            json!({}),
        ),
        None => (
            if view.name == "host" { "host" } else { "null" },
            json!([]),
            0,
            false,
            BTreeMap::new(),
            json!({}),
        ),
    };
    json!({
        "Name": view.name,
        "Id": view.id,
        "Created": rfc3339_nano(created * 1_000_000, false),
        "Scope": "local",
        "Driver": driver,
        "EnableIPv4": view.spec.is_some(),
        "EnableIPv6": false,
        "IPAM": { "Driver": "default", "Options": {}, "Config": ipam },
        "Internal": internal,
        "Attachable": false,
        "Ingress": false,
        "ConfigFrom": { "Network": "" },
        "ConfigOnly": false,
        "Containers": containers,
        "Options": options,
        "Labels": labels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// The key paths of a JSON document, an array's elements under `[]`.
    fn paths(value: &Value, prefix: &str, out: &mut BTreeSet<String>) {
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

    fn fixture() -> (Context, Snapshot) {
        let record: ImageRecord = serde_json::from_value(json!({
            "name": "web", "reference": "docker.io/library/busybox:1.37", "manifest_digest": "sha256:aa",
            "layers": [], "backend": "overlay", "created": 1_792_344_103u64, "mode": "app",
            "run": { "cmd": ["httpd", "-f", "-p", "80"], "env": ["PATH=/bin"], "exposed_ports": ["80/tcp"],
                     "healthcheck": { "test": ["CMD-SHELL", "true"], "interval": 2_000_000 } },
            "network": "bridge", "network_name": "front", "address": "10.99.1.2",
            "aliases": { "front": ["www"] },
            "ports": [{ "host": 8080, "container": 80, "protocol": "tcp" }],
            "env": ["GREETING=hello"], "volumes": [{ "source": "data", "target": "/data" }],
            "labels": { "caddy": "http://web.test" }, "restart": "unless-stopped",
        }))
        .unwrap();
        let summary = MachineSummary {
            name: "web".into(),
            record: Some(record.clone()),
            state: "running".into(),
            started: Some(1_792_344_103),
            leader: Some(593),
            os: None,
            health: Some(crate::health::Status {
                status: "healthy".into(),
                failing_streak: 0,
                log: vec![crate::health::Probe {
                    start: 1,
                    end: 2,
                    exit_code: 0,
                    output: String::new(),
                }],
            }),
            exit_code: None,
            finished: None,
        };
        let front: NetSpec = serde_json::from_value(json!({
            "name": "front", "interface": "nsbr-front", "subnet": "10.99.1.0/24", "created": 5, "labels": { "e2e": "yes" },
        }))
        .unwrap();
        let config = Config::merge(Default::default(), None, None).unwrap();
        let snapshot = Snapshot {
            machines: vec![Machine {
                id: container_id("web", record.created),
                record,
                summary,
                finished_usec: None,
                restarts: 0,
            }],
            networks: vec![config.default_network(), front],
        };
        (Context::new(config), snapshot)
    }

    use crate::config::Config;

    #[test]
    fn the_program_reads_as_docker_merged_it() {
        let (ctx, mut snapshot) = fixture();
        let image = ImageInfo::of(&ctx, &snapshot.machines[0].record);
        let config = |snapshot: &Snapshot| {
            snapshot.machines[0].inspect_json(&ctx, snapshot, &image)["Config"].clone()
        };
        assert_eq!(config(&snapshot)["Cmd"], json!(["httpd", "-f", "-p", "80"]));
        assert_eq!(config(&snapshot)["Entrypoint"], Value::Null);
        // An entrypoint given alone: docker's Cmd is null, not the image's.
        snapshot.machines[0].record.entrypoint = Some(vec!["/bin/sh".into()]);
        snapshot.machines[0].record.cmd = Some(Vec::new());
        assert_eq!(config(&snapshot)["Cmd"], Value::Null);
        assert_eq!(config(&snapshot)["Entrypoint"], json!(["/bin/sh"]));
        let m = &snapshot.machines[0];
        assert_eq!(
            m.summary_json(&ctx, &snapshot, &image)["Command"],
            "/bin/sh"
        );
        // The arguments with a space in quotes, as docker lists a command.
        snapshot.machines[0].record.cmd = Some(vec!["-c".into(), "echo a; sleep 1".into()]);
        let m = &snapshot.machines[0];
        assert_eq!(
            m.summary_json(&ctx, &snapshot, &image)["Command"],
            "/bin/sh -c 'echo a; sleep 1'"
        );
    }

    #[test]
    fn a_machine_between_restarts_is_running_to_docker() {
        let (ctx, mut snapshot) = fixture();
        snapshot.machines[0].summary.state = "restarting".into();
        snapshot.machines[0].summary.exit_code = Some(1);
        let m = &snapshot.machines[0];
        let image = ImageInfo::of(&ctx, &m.record);
        let state = &m.inspect_json(&ctx, &snapshot, &image)["State"];
        assert_eq!(state["Running"], true);
        assert_eq!(state["Restarting"], true);
        assert_eq!(state["ExitCode"], 1);
        // A stopped one lists no ports.
        let running = m.summary_json(&ctx, &snapshot, &image)["Ports"].clone();
        assert!(
            running.as_array().is_some_and(|p| p.is_empty()),
            "{running}"
        );
        // Its run's end is read, as a stopped one's.
        assert!(ended("restarting") && ended("stopped") && !ended("running"));
        let now = 1_792_344_200;
        snapshot.machines[0].finished_usec = Some((now - 3) * 1_000_000);
        assert_eq!(
            snapshot.machines[0].status(now),
            "Restarting (1) 3 seconds ago"
        );
    }

    #[test]
    fn host_machines_are_found_under_the_host_network() {
        let (ctx, mut snapshot) = fixture();
        let filter = |net: &str| {
            crate::dockerapi::filters::Filters::parse(Some(&format!(r#"{{"network":["{net}"]}}"#)))
                .unwrap()
        };
        let matches = |snapshot: &Snapshot, net: &str| {
            crate::dockerapi::container_matches(&ctx, snapshot, &filter(net), &snapshot.machines[0])
        };
        assert!(matches(&snapshot, "front"));
        assert!(!matches(&snapshot, "host"));
        snapshot.machines[0].record.network = Network::Host;
        assert!(matches(&snapshot, "host"));
        assert!(!matches(&snapshot, "front"));
        let host_id = snapshot.network("host").unwrap().id;
        assert!(matches(&snapshot, &host_id[..12]));
    }

    fn diff(ours: &Value, docker: &Value) -> (Vec<String>, Vec<String>) {
        let (mut a, mut b) = (BTreeSet::new(), BTreeSet::new());
        paths(ours, "", &mut a);
        paths(docker, "", &mut b);
        (
            a.difference(&b).cloned().collect(),
            b.difference(&a).cloned().collect(),
        )
    }

    #[test]
    fn shapes_against_docker() {
        let (ctx, snapshot) = fixture();
        let m = &snapshot.machines[0];
        let image = ImageInfo {
            id: "sha256:30ec".into(),
            exposed_ports: vec!["80/tcp".into()],
        };
        let golden: Value = serde_json::from_str(include_str!(
            "../../tests/dockerapi/golden/containers-all.body"
        ))
        .unwrap();
        let web = golden
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["Names"][0] == "/web")
            .unwrap();
        let (extra, missing) = diff(&m.summary_json(&ctx, &snapshot, &image), web);
        assert!(
            extra.is_empty() && missing.is_empty(),
            "summary: extra {extra:?}, missing {missing:?}"
        );
        let golden: Value = serde_json::from_str(include_str!(
            "../../tests/dockerapi/golden/inspect-web.body"
        ))
        .unwrap();
        let (extra, missing) = diff(&m.inspect_json(&ctx, &snapshot, &image), &golden);
        assert!(
            extra.is_empty() && missing.is_empty(),
            "inspect: extra {extra:?}, missing {missing:?}"
        );
        let golden: Value = serde_json::from_str(include_str!(
            "../../tests/dockerapi/golden/network-front.body"
        ))
        .unwrap();
        let view = snapshot.network("front").unwrap();
        let (extra, missing) = diff(&network_json(&snapshot, &view), &golden);
        assert!(
            extra.is_empty() && missing.is_empty(),
            "network: extra {extra:?}, missing {missing:?}"
        );
    }
}
