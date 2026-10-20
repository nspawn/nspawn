//! generate: the nspawn commands that make machines again, on another host or on this
//! one, from what nspawn keeps of them: the networks they join, then each machine after
//! the ones it depends on, `run -d` with every flag it keeps.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{bail, Result};

use crate::api::Context;
use crate::backend::BackendChoice;
use crate::bridge::{self, NetSpec, PortMap, Protocol};
use crate::oci::{detect_mode, Mode};
use crate::policy::Restart;
use crate::reference::ImageRef;
use crate::settings::Network;
use crate::store::ImageRecord;

/// One command without the program's name: its words in groups, each group a line of
/// the script.
pub type Command = Vec<Vec<String>>;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Script {
    /// What the commands cannot carry, said before them.
    pub notes: Vec<String>,
    pub commands: Vec<Command>,
}

impl Script {
    /// A shell script: the notes as comments, then one command per paragraph, a group
    /// per line.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for note in &self.notes {
            out.push_str(&format!("# {note}\n"));
        }
        for command in &self.commands {
            let lines: Vec<String> = command
                .iter()
                .map(|group| group.iter().map(|w| quote(w)).collect::<Vec<_>>().join(" "))
                .collect();
            out.push_str("nspawn ");
            out.push_str(&lines.join(" \\\n  "));
            out.push('\n');
        }
        out
    }
}

/// A word as a POSIX shell reads it back: as it is when nothing in it is special, in
/// single quotes otherwise.
pub fn quote(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c));
    if plain {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

/// The script for `names`, every machine of nspawn's when there are none, with the
/// machines they depend on or take their network from.
pub fn generate(ctx: &Context, names: &[String]) -> Result<Script> {
    let records = ctx.store.list_images()?;
    let by_name: BTreeMap<&str, &ImageRecord> =
        records.iter().map(|r| (r.name.as_str(), r)).collect();
    for name in names {
        if !by_name.contains_key(name.as_str()) {
            bail!("{name} is not a machine of nspawn's (nspawn ps -a lists them)");
        }
    }
    let wanted: Vec<&str> = if names.is_empty() {
        by_name.keys().copied().collect()
    } else {
        names.iter().map(String::as_str).collect()
    };
    let machines = order(&wanted, &by_name);
    let mut script = Script::default();
    let networks: BTreeSet<&str> = machines
        .iter()
        .filter(|r| bridge::bridge_kind(r))
        .flat_map(|r| bridge::networks_of(r))
        .filter(|n| *n != bridge::DEFAULT_NETWORK)
        .collect();
    for network in networks {
        let spec = crate::api::network::find(&ctx.store, &ctx.config, network)?;
        script.commands.push(network_command(&spec));
    }
    // A machine another one takes its network from has to run before that one starts.
    let owners: BTreeSet<&str> = machines
        .iter()
        .filter_map(|r| r.network_container.as_deref())
        .collect();
    // An image built here, which another host can only pull once it is pushed: its
    // record says build, the records of the machines made from it create.
    let built: BTreeSet<&str> = records
        .iter()
        .filter(|r| r.origin == "build")
        .map(|r| r.reference.as_str())
        .collect();
    let mut secrets = BTreeSet::new();
    let mut pushed = BTreeSet::new();
    let mut runs = Vec::new();
    for r in &machines {
        let registry = ImageRef::parse(&r.reference, &ctx.config.registry)
            .map(|i| i.registry)
            .unwrap_or_default();
        // Pulled without a signature where one is required: --no-verify, as it was.
        let required = crate::verify::Policy::for_registry(&ctx.config, &registry)
            .ok()
            .flatten()
            .is_some_and(|p| p.required);
        let command = machine_command(
            r,
            &ctx.config.registry,
            required && r.signed_by.is_none(),
            owners.contains(r.name.as_str()),
        );
        // The pulls first: a run --rm keeps the image it pulls under the image's own
        // name, which a pull of that name later would find taken.
        if command[0][0] == "pull" {
            script.commands.push(command);
        } else {
            runs.push(command);
        }
        if built.contains(r.reference.as_str()) && pushed.insert(r.reference.as_str()) {
            script.notes.push(format!(
                "{} was built on this host: push it (nspawn push {}) before this script runs elsewhere",
                r.reference, r.name
            ));
        }
        for volume in r
            .volumes
            .iter()
            .filter(|v| v.source.starts_with('/') && !v.is_api_socket())
        {
            script.notes.push(format!(
                "{} mounts {} of this host, which has to exist where this script runs",
                r.name, volume.source
            ));
        }
        for secret in &r.tuning.secrets {
            if secrets.insert(secret.name.clone()) {
                script.notes.push(format!(
                    "secret {} is encrypted for this host: nspawn secret create {} where this script runs, before it",
                    secret.name, secret.name
                ));
            }
        }
    }
    script.commands.extend(runs);
    Ok(script)
}

/// The machines to make, each after the ones it depends on or takes its network from.
fn order<'a>(
    wanted: &[&'a str],
    records: &BTreeMap<&'a str, &'a ImageRecord>,
) -> Vec<&'a ImageRecord> {
    fn visit<'a>(
        name: &'a str,
        records: &BTreeMap<&'a str, &'a ImageRecord>,
        seen: &mut BTreeSet<&'a str>,
        out: &mut Vec<&'a ImageRecord>,
    ) {
        if !seen.insert(name) {
            return;
        }
        let Some(r) = records.get(name) else {
            return;
        };
        for dep in &r.tuning.depends_on {
            visit(dep.name.as_str(), records, seen, out);
        }
        if let Some(owner) = &r.network_container {
            visit(owner.as_str(), records, seen, out);
        }
        out.push(r);
    }
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for name in wanted {
        visit(name, records, &mut seen, &mut out);
    }
    out
}

/// `network create` with the network's subnet, internal flag and labels.
pub fn network_command(spec: &NetSpec) -> Command {
    let mut head = words(&["network", "create", &spec.name]);
    head.extend(words(&["--subnet", &spec.subnet.to_string()]));
    if spec.internal {
        head.push("--internal".to_string());
    }
    let mut command = vec![head];
    for (key, value) in &spec.labels {
        command.push(option("--label", &format!("{key}={value}")));
    }
    command
}

/// `run -d` with every flag the machine keeps, or `pull` for an image with none that
/// need not run. `no_verify`: the image was pulled without the signature its registry
/// requires. `run`: another machine takes its network, so it has to run, flags or not.
///
/// `run` pulls each machine on its own (`--pull always`): made from a local image of the
/// same reference instead, it would take that image's network kind, mode and backend
/// wherever its own flags leave them out. Blobs already there are not downloaded again.
pub fn machine_command(r: &ImageRecord, registry: &str, no_verify: bool, run: bool) -> Command {
    let mut pull_flags = Vec::new();
    match r.backend {
        BackendChoice::Flat => pull_flags.extend(words(&["--backend", "flat"])),
        BackendChoice::Mstack => pull_flags.extend(words(&["--backend", "mstack"])),
        BackendChoice::Overlay | BackendChoice::Auto => {}
    }
    // The mode is detected again, unless it is the one detection would not give.
    let detected = detect_mode(true, &r.run.argv());
    match (r.mode, detected) {
        (Mode::App, Mode::Boot) => pull_flags.extend(words(&["--mode", "app"])),
        (Mode::Boot, Mode::App) => pull_flags.extend(words(&["--mode", "boot"])),
        _ => {}
    }
    if no_verify {
        pull_flags.push("--no-verify".to_string());
    }
    let (flags, tail) = flags(r);
    if flags.is_empty() && tail.is_empty() && !r.remove_on_exit && !run {
        let mut head = words(&["pull", &r.reference]);
        let own_name = ImageRef::parse(&r.reference, registry).map(|i| i.local_name());
        if own_name.ok().as_deref() != Some(r.name.as_str()) {
            head.extend(words(&["--name", &r.name]));
        }
        head.extend(pull_flags);
        return vec![head];
    }
    let mut head = words(&["run", "-d", "--name", &r.name, "--pull", "always"]);
    head.extend(pull_flags);
    if r.remove_on_exit {
        head.push("--rm".to_string());
    }
    head.push(r.reference.clone());
    let mut command = vec![head];
    command.extend(flags);
    if !tail.is_empty() {
        let mut last = vec!["--".to_string()];
        last.extend(tail);
        command.push(last);
    }
    command
}

fn words(list: &[&str]) -> Vec<String> {
    list.iter().map(|w| w.to_string()).collect()
}

/// A long option and its value, in one word when the value starts with a dash, which
/// clap would read as another option otherwise.
fn option(name: &str, value: &str) -> Vec<String> {
    if value.starts_with('-') {
        vec![format!("{name}={value}")]
    } else {
        words(&[name, value])
    }
}

/// The flags of start and run that make `r` what it is, a group each, and the command
/// that follows `--`.
pub(crate) fn flags(r: &ImageRecord) -> (Vec<Vec<String>>, Vec<String>) {
    let mut out: Vec<Vec<String>> = Vec::new();
    let flag = |out: &mut Vec<Vec<String>>, name: &str, value: &str| out.push(option(name, value));
    if r.no_network {
        flag(&mut out, "--network", "none");
    } else if let Some(owner) = &r.network_container {
        flag(&mut out, "--network", &format!("container:{owner}"));
    } else {
        match r.network {
            Network::Host => flag(&mut out, "--network", "host"),
            Network::Veth => flag(&mut out, "--network", "veth"),
            Network::Bridge => {
                let networks = bridge::networks_of(r);
                if networks.len() > 1 || networks[0] != bridge::DEFAULT_NETWORK {
                    for network in networks {
                        flag(&mut out, "--network", network);
                    }
                }
            }
        }
    }
    // The addresses asked for, as they were asked: the primary network's alone, the
    // others by name.
    if bridge::bridge_kind(r) {
        let networks = bridge::networks_of(r);
        for (i, network) in networks.iter().enumerate() {
            if !r.fixed_addresses.contains(*network) {
                continue;
            }
            if let Some(addr) = bridge::address_on(r, network) {
                let value = if i == 0 {
                    addr.to_string()
                } else {
                    format!("{network}={addr}")
                };
                flag(&mut out, "--ip", &value);
            }
        }
    }
    // Aliases stay in the record of a machine that left the bridge, where they mean
    // nothing and start refuses them.
    for (network, aliases) in r.aliases.iter().filter(|_| bridge::bridge_kind(r)) {
        for alias in aliases {
            flag(&mut out, "--network-alias", &format!("{network}={alias}"));
        }
    }
    for port in &r.ports {
        flag(&mut out, "--publish", &publish_text(port));
    }
    // An entrypoint of more than one word: the rest goes before the arguments, which
    // the image's own then stand for.
    let mut tail = Vec::new();
    match r.entrypoint.as_deref() {
        None => {}
        Some([]) => flag(&mut out, "--entrypoint", ""),
        Some([program, rest @ ..]) => {
            flag(&mut out, "--entrypoint", program);
            if !rest.is_empty() {
                tail.extend(rest.iter().cloned());
                if r.cmd.is_none() {
                    tail.extend(r.run.cmd().iter().cloned());
                }
            }
        }
    }
    if let Some(cmd) = &r.cmd {
        tail.extend(cmd.iter().cloned());
    }
    for var in &r.env {
        flag(&mut out, "--env", var);
    }
    for volume in &r.volumes {
        flag(&mut out, "--volume", &volume.to_string());
    }
    for (key, value) in &r.labels {
        flag(&mut out, "--label", &format!("{key}={value}"));
    }
    if r.restart != Restart::No {
        flag(&mut out, "--restart", r.restart.name());
    }
    let limits = &r.limits;
    if limits.memory > 0 {
        flag(&mut out, "--memory", &size_text(limits.memory));
    }
    if limits.memory_swap == -1 {
        flag(&mut out, "--memory-swap", "-1");
    } else if limits.memory_swap > 0 {
        flag(
            &mut out,
            "--memory-swap",
            &size_text(limits.memory_swap as u64),
        );
    }
    if limits.milli_cpus > 0 {
        flag(&mut out, "--cpus", &limits.cpus().to_string());
    }
    if limits.pids > 0 {
        flag(&mut out, "--pids-limit", &limits.pids.to_string());
    }
    out.extend(health_flags(r));
    out.extend(tuning_flags(&r.tuning));
    (out, tail)
}

/// The --health-* flags that turn the image's healthcheck into the machine's.
fn health_flags(r: &ImageRecord) -> Vec<Vec<String>> {
    let Some(hc) = &r.healthcheck else {
        return Vec::new();
    };
    if hc.disabled() {
        return vec![words(&["--no-healthcheck"])];
    }
    let base = r.run.healthcheck.clone().unwrap_or_default();
    let mut out = Vec::new();
    if hc.test != base.test {
        let line = match hc.test.as_slice() {
            [kind, line] if kind == "CMD-SHELL" => line.clone(),
            [_, argv @ ..] => argv.iter().map(|w| quote(w)).collect::<Vec<_>>().join(" "),
            [] => String::new(),
        };
        out.push(option("--health-cmd", &line));
    }
    for (name, value, image) in [
        ("--health-interval", hc.interval, base.interval),
        ("--health-timeout", hc.timeout, base.timeout),
        ("--health-start-period", hc.start_period, base.start_period),
        (
            "--health-start-interval",
            hc.start_interval,
            base.start_interval,
        ),
    ] {
        if value != image {
            out.push(words(&[name, &crate::health::format_duration(value)]));
        }
    }
    if hc.retries != base.retries {
        out.push(words(&["--health-retries", &hc.retries.to_string()]));
    }
    out
}

/// docker's other flags, as the machine keeps them.
fn tuning_flags(t: &crate::tuning::Tuning) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = Vec::new();
    let flag = |out: &mut Vec<Vec<String>>, name: &str, value: &str| out.push(option(name, value));
    if let Some(hostname) = &t.hostname {
        flag(&mut out, "--hostname", hostname);
    }
    if let Some(user) = &t.user {
        flag(&mut out, "--user", user);
    }
    if let Some(dir) = &t.working_dir {
        flag(&mut out, "--workdir", dir);
    }
    for cap in &t.cap_add {
        flag(&mut out, "--cap-add", cap);
    }
    for cap in &t.cap_drop {
        flag(&mut out, "--cap-drop", cap);
    }
    for mount in &t.tmpfs {
        flag(&mut out, "--tmpfs", mount);
    }
    if let Some(size) = t.shm_size {
        flag(&mut out, "--shm-size", &size_text(size));
    }
    for device in &t.devices {
        flag(
            &mut out,
            "--device",
            &format!(
                "{}:{}:{}",
                device.host, device.container, device.permissions
            ),
        );
    }
    for name in &t.cdi_devices {
        flag(&mut out, "--device", name);
    }
    for request in &t.gpus {
        flag(&mut out, "--gpus", &request.to_string());
    }
    for iface in &t.interfaces {
        flag(&mut out, "--interface", iface);
    }
    for server in &t.dns {
        flag(&mut out, "--dns", &server.to_string());
    }
    for domain in &t.dns_search {
        flag(&mut out, "--dns-search", domain);
    }
    for host in &t.extra_hosts {
        // HOST=IP for an IPv6 address, whose colons HOST:IP would split wrong.
        let sep = if host.ip.contains(':') { '=' } else { ':' };
        flag(
            &mut out,
            "--add-host",
            &format!("{}{sep}{}", host.host, host.ip),
        );
    }
    for (name, (soft, hard)) in &t.ulimits {
        let limit = |n: u64| {
            if n == u64::MAX {
                "unlimited".to_string()
            } else {
                n.to_string()
            }
        };
        flag(
            &mut out,
            "--ulimit",
            &format!("{name}={}:{}", limit(*soft), limit(*hard)),
        );
    }
    if let Some(adj) = t.oom_score_adj {
        flag(&mut out, "--oom-score-adj", &adj.to_string());
    }
    if let Some(signal) = &t.stop_signal {
        flag(&mut out, "--stop-signal", signal);
    }
    if let Some(timeout) = t.stop_timeout {
        flag(&mut out, "--stop-timeout", &timeout.to_string());
    }
    if let Some(mode) = &t.timezone {
        flag(&mut out, "--timezone", mode);
    }
    if let Some(driver) = &t.log_driver {
        flag(&mut out, "--log-driver", driver);
    }
    for (key, value) in &t.sysctls {
        flag(&mut out, "--sysctl", &format!("{key}={value}"));
    }
    for s in &t.secrets {
        flag(
            &mut out,
            "--secret",
            &format!("{}:{}:{:04o}:{}:{}", s.name, s.target, s.mode, s.uid, s.gid),
        );
    }
    for dep in &t.depends_on {
        flag(&mut out, "--depends-on", &dep.to_string());
    }
    for (on, name) in [
        (t.privileged, "--privileged"),
        (t.read_only, "--read-only"),
        (t.init == Some(true), "--init"),
        (t.init == Some(false), "--init=false"),
    ] {
        if on {
            out.push(words(&[name]));
        }
    }
    out
}

/// -p as it is given: [IP:]HOST:CONTAINER[/udp].
fn publish_text(port: &PortMap) -> String {
    let mut text = match port.host_ip {
        Some(ip) => format!("{ip}:{}:{}", port.host, port.container),
        None => format!("{}:{}", port.host, port.container),
    };
    if port.protocol == Protocol::Udp {
        text.push_str("/udp");
    }
    text
}

/// Bytes in the largest unit that holds them whole: 512m, 2g, 1536k.
fn size_text(bytes: u64) -> String {
    for (shift, unit) in [(40, "t"), (30, "g"), (20, "m"), (10, "k")] {
        if bytes >= 1 << shift && bytes.is_multiple_of(1 << shift) {
            return format!("{}{unit}", bytes >> shift);
        }
    }
    bytes.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(name: &str, reference: &str) -> ImageRecord {
        serde_json::from_value(serde_json::json!({
            "name": name,
            "reference": reference,
            "manifest_digest": "sha256:00",
            "layers": [],
            "backend": "overlay",
            "created": 0,
        }))
        .unwrap()
    }

    #[test]
    fn words_are_quoted_only_when_a_shell_would_change_them() {
        assert_eq!(
            quote("docker.io/library/nginx:1.27"),
            "docker.io/library/nginx:1.27"
        );
        assert_eq!(quote("A=1"), "A=1");
        assert_eq!(quote(""), "''");
        assert_eq!(quote("two words"), "'two words'");
        assert_eq!(quote("it's"), r"'it'\''s'");
        assert_eq!(quote("$HOME"), "'$HOME'");
        assert_eq!(size_text(512 << 20), "512m");
        assert_eq!(size_text(3 << 30), "3g");
        assert_eq!(size_text(1536 << 10), "1536k");
        assert_eq!(size_text(1000), "1000");
    }

    #[test]
    fn an_image_without_flags_is_pulled_again() {
        let plain = record("fedora-44", "hub.example/fedora:44");
        assert_eq!(
            machine_command(&plain, "hub.example", false, false),
            [["pull", "hub.example/fedora:44"]]
        );
        let mut named = record("base", "hub.example/fedora:44");
        named.backend = BackendChoice::Flat;
        assert_eq!(
            machine_command(&named, "hub.example", true, false),
            [[
                "pull",
                "hub.example/fedora:44",
                "--name",
                "base",
                "--backend",
                "flat",
                "--no-verify"
            ]]
        );
        // An image detection would boot, run as an app.
        let mut app = record("fedora-44", "hub.example/fedora:44");
        app.mode = Mode::App;
        assert_eq!(
            machine_command(&app, "hub.example", false, false)[0][2..],
            ["--mode", "app"]
        );
        let mut once = record("job", "hub.example/fedora:44");
        once.remove_on_exit = true;
        assert_eq!(
            machine_command(&once, "hub.example", false, false),
            [[
                "run",
                "-d",
                "--name",
                "job",
                "--pull",
                "always",
                "--rm",
                "hub.example/fedora:44"
            ]]
        );
        // A machine another one takes its network from runs, flags or not.
        let owner = record("redis", "docker.io/library/redis:7");
        assert_eq!(
            machine_command(&owner, "hub.example", false, true),
            [[
                "run",
                "-d",
                "--name",
                "redis",
                "--pull",
                "always",
                "docker.io/library/redis:7"
            ]]
        );
    }

    #[test]
    fn a_network_is_made_with_its_subnet_and_labels() {
        let spec: NetSpec = serde_json::from_value(serde_json::json!({
            "name": "backend",
            "interface": "nsbr-backend",
            "subnet": "10.99.7.0/24",
            "internal": true,
            "labels": {"team": "a b"},
        }))
        .unwrap();
        let script = Script {
            notes: vec!["secret pw is encrypted for this host".into()],
            commands: vec![network_command(&spec)],
        };
        assert_eq!(
            script.render(),
            "# secret pw is encrypted for this host\nnspawn network create backend --subnet 10.99.7.0/24 --internal \\\n  --label 'team=a b'\n"
        );
        let mut dashed = spec.clone();
        dashed.labels = [("-k".to_string(), "v".to_string())].into();
        assert_eq!(network_command(&dashed)[1], ["--label=-k=v"]);
    }

    #[test]
    fn addresses_asked_for_are_asked_for_again() {
        let mut r = record("web", "hub.example/app:1");
        r.network_name = Some("front".into());
        r.address = Some("10.97.42.50".parse().unwrap());
        r.extra_networks.push(crate::bridge::Attachment {
            network: "back".into(),
            address: Some("10.97.43.7".parse().unwrap()),
        });
        // A dynamic address is given out again, not asked for.
        let words = |r: &ImageRecord| flags(r).0.concat();
        assert!(!words(&r).contains(&"--ip".to_string()));
        r.fixed_addresses = ["front".to_string(), "back".to_string()].into();
        let w = words(&r);
        let ips: Vec<&String> = w
            .iter()
            .enumerate()
            .filter(|(i, _)| *i > 0 && w[i - 1] == "--ip")
            .map(|(_, v)| v)
            .collect();
        assert_eq!(ips, ["10.97.42.50", "back=10.97.43.7"]);
    }

    #[test]
    fn machines_come_after_what_they_depend_on() {
        let mut web = record("web", "r");
        web.tuning.depends_on = vec![crate::tuning::dependency("db:healthy").unwrap()];
        let mut side = record("side", "r");
        side.network_container = Some("web".into());
        let mut db = record("db", "r");
        db.tuning.depends_on = vec![crate::tuning::dependency("disk").unwrap()];
        let disk = record("disk", "r");
        let other = record("other", "r");
        let all = [web, side, db, disk, other];
        let by_name: BTreeMap<&str, &ImageRecord> =
            all.iter().map(|r| (r.name.as_str(), r)).collect();
        let names = |list: Vec<&ImageRecord>| -> Vec<String> {
            list.iter().map(|r| r.name.clone()).collect()
        };
        assert_eq!(
            names(order(&["side"], &by_name)),
            ["disk", "db", "web", "side"]
        );
        assert_eq!(
            names(order(&["other", "web", "db"], &by_name)),
            ["other", "disk", "db", "web"]
        );
    }

    #[test]
    fn values_that_look_like_options_and_stale_aliases() {
        let mut r = record("web", "hub.example/app:1");
        r.mode = Mode::App;
        r.env = vec!["-x=1".into()];
        r.labels = [("-k".to_string(), "v".to_string())].into();
        r.tuning.user = Some("-1".into());
        r.network = Network::Host;
        r.aliases = [("bridge".to_string(), vec!["www".to_string()])].into();
        let command = machine_command(&r, "hub.example", false, false);
        let flat = command.concat();
        assert!(flat.contains(&"--env=-x=1".to_string()), "{flat:?}");
        assert!(flat.contains(&"--label=-k=v".to_string()), "{flat:?}");
        assert!(flat.contains(&"--user=-1".to_string()), "{flat:?}");
        assert!(!flat.iter().any(|w| w == "--network-alias"), "{flat:?}");
        let mut argv = vec!["nspawn".to_string()];
        argv.extend(flat);
        let cli = <crate::cli::Cli as clap::Parser>::try_parse_from(&argv).unwrap();
        let crate::cli::Command::Run(run) = cli.command else {
            panic!("not run");
        };
        assert_eq!(run.options.env, ["-x=1"]);
        assert_eq!(run.options.tuning.user.as_deref(), Some("-1"));
        assert_eq!(run.options.network, ["host"]);
    }
}
