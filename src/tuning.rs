//! docker's per-container knobs beyond the network, volumes, limits and healthcheck:
//! hostname, user, capabilities, mounts, devices, DNS, ulimits, signals. Remembered with
//! the machine, most of them one line of its settings file.

use std::collections::BTreeMap;
use std::net::IpAddr;

use anyhow::{bail, Context as _, Result};
use serde::{Deserialize, Serialize};

/// A device node handed to the machine, as docker's --device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub host: String,
    pub container: String,
    /// r, w and m, as DeviceAllow= takes them.
    pub permissions: String,
}

/// A secret handed to the machine, as docker's long --secret form has it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretRef {
    pub name: String,
    /// An absolute path inside the machine.
    pub target: String,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
}

/// An entry of --add-host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtraHost {
    pub host: String,
    /// An address, or "host-gateway" for the gateway of the machine's network.
    pub ip: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tuning {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// Overrides the image's user (a name or a uid).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    /// CAP_ names without the prefix, upper case; ALL for every one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cap_add: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cap_drop: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub privileged: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
    /// PATH[:options], as --tmpfs and TemporaryFileSystem= take them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tmpfs: Vec<String>,
    /// Bytes for /dev/shm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shm_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<Device>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dns: Vec<IpAddr>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dns_search: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_hosts: Vec<ExtraHost>,
    /// Lower-case resource names (nofile, nproc, ...) to (soft, hard).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub ulimits: BTreeMap<String, (u64, u64)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oom_score_adj: Option<i32>,
    /// Overrides the image's stop signal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_signal: Option<String>,
    /// Seconds between the stop signal and SIGKILL, the default of `stop -t`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_timeout: Option<u64>,
    /// Accepted for docker's sake: the stub init reaps anyway.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub init: bool,
    /// net.* keys, applied in an app machine's network namespace.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sysctls: BTreeMap<String, String>,
    /// --secret: decrypted for the machine while it runs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<SecretRef>,
}

impl Tuning {
    pub fn is_default(&self) -> bool {
        *self == Tuning::default()
    }

    /// The [Exec] lines: Hostname=, Capability=, DropCapability=, OOMScoreAdjust=, the
    /// rlimits, LinkJournal=. User= and WorkingDirectory= are the caller's, image
    /// defaults included.
    pub fn exec_lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(hostname) = &self.hostname {
            out.push(format!("Hostname={hostname}"));
        }
        if self.privileged {
            out.push("Capability=all".to_string());
        } else if !self.cap_add.is_empty() {
            out.push(format!("Capability={}", capabilities(&self.cap_add)));
        }
        if !self.cap_drop.is_empty() && !self.privileged {
            // systemd-nspawn lets a drop win over an add, docker the other way round:
            // "--cap-drop ALL --cap-add X" keeps X, and "--cap-add ALL" keeps all but
            // the named drops, so those drops are spelled out.
            let add_all = self.cap_add.iter().any(|c| c == "ALL");
            let drop_all = self.cap_drop.iter().any(|c| c == "ALL");
            let dropped = if add_all {
                let named: Vec<String> = self
                    .cap_drop
                    .iter()
                    .filter(|c| *c != "ALL")
                    .cloned()
                    .collect();
                capabilities(&named)
            } else if drop_all && !self.cap_add.is_empty() {
                ALL_CAPABILITIES
                    .iter()
                    .filter(|c| !self.cap_add.iter().any(|a| **c == format!("CAP_{a}")))
                    .copied()
                    .collect::<Vec<_>>()
                    .join(" ")
            } else {
                capabilities(&self.cap_drop)
            };
            if !dropped.is_empty() {
                out.push(format!("DropCapability={dropped}"));
            }
        }
        if let Some(adj) = self.oom_score_adj {
            out.push(format!("OOMScoreAdjust={adj}"));
        }
        for (name, (soft, hard)) in &self.ulimits {
            out.push(format!("Limit{}={soft}:{hard}", name.to_ascii_uppercase()));
        }
        // Linking the journal makes a directory in the root, which a read-only one
        // refuses at every start.
        if self.read_only {
            out.push("LinkJournal=no".to_string());
        }
        out
    }

    /// The [Files] lines: ReadOnly=, TemporaryFileSystem= (the tmpfs mounts and
    /// /dev/shm), Bind= of the device nodes.
    pub fn files_lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.read_only {
            out.push("ReadOnly=yes".to_string());
        }
        for mount in &self.tmpfs {
            out.push(format!("TemporaryFileSystem={mount}"));
        }
        if let Some(size) = self.shm_size {
            out.push(format!("TemporaryFileSystem=/dev/shm:size={size}"));
        }
        for device in &self.devices {
            out.push(format!("Bind={}:{}", device.host, device.container));
        }
        out
    }

    /// DeviceAllow= for the unit, so that the machine's cgroup may open the nodes.
    pub fn unit_lines(&self) -> Vec<String> {
        self.devices
            .iter()
            .map(|d| format!("DeviceAllow={} {}", d.host, d.permissions))
            .collect()
    }
}

/// Every capability of the kernel, as systemd-nspawn names them.
const ALL_CAPABILITIES: [&str; 41] = [
    "CAP_AUDIT_CONTROL",
    "CAP_AUDIT_READ",
    "CAP_AUDIT_WRITE",
    "CAP_BLOCK_SUSPEND",
    "CAP_BPF",
    "CAP_CHECKPOINT_RESTORE",
    "CAP_CHOWN",
    "CAP_DAC_OVERRIDE",
    "CAP_DAC_READ_SEARCH",
    "CAP_FOWNER",
    "CAP_FSETID",
    "CAP_IPC_LOCK",
    "CAP_IPC_OWNER",
    "CAP_KILL",
    "CAP_LEASE",
    "CAP_LINUX_IMMUTABLE",
    "CAP_MAC_ADMIN",
    "CAP_MAC_OVERRIDE",
    "CAP_MKNOD",
    "CAP_NET_ADMIN",
    "CAP_NET_BIND_SERVICE",
    "CAP_NET_BROADCAST",
    "CAP_NET_RAW",
    "CAP_PERFMON",
    "CAP_SETFCAP",
    "CAP_SETGID",
    "CAP_SETPCAP",
    "CAP_SETUID",
    "CAP_SYSLOG",
    "CAP_SYS_ADMIN",
    "CAP_SYS_BOOT",
    "CAP_SYS_CHROOT",
    "CAP_SYS_MODULE",
    "CAP_SYS_NICE",
    "CAP_SYS_PACCT",
    "CAP_SYS_PTRACE",
    "CAP_SYS_RAWIO",
    "CAP_SYS_RESOURCE",
    "CAP_SYS_TIME",
    "CAP_SYS_TTY_CONFIG",
    "CAP_WAKE_ALARM",
];

fn capabilities(names: &[String]) -> String {
    names
        .iter()
        .map(|n| {
            if n == "ALL" {
                "all".to_string()
            } else {
                format!("CAP_{n}")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The flags as given: each one replaces its part of what the machine had; "none" alone
/// clears a list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overrides {
    pub hostname: Option<String>,
    pub user: Option<String>,
    pub working_dir: Option<String>,
    pub cap_add: Vec<String>,
    pub cap_drop: Vec<String>,
    pub privileged: Option<bool>,
    pub read_only: Option<bool>,
    pub tmpfs: Vec<String>,
    pub shm_size: Option<u64>,
    pub devices: Vec<String>,
    pub dns: Vec<String>,
    pub dns_search: Vec<String>,
    pub extra_hosts: Vec<String>,
    pub ulimits: Vec<String>,
    pub oom_score_adj: Option<i32>,
    pub stop_signal: Option<String>,
    pub stop_timeout: Option<u64>,
    pub init: Option<bool>,
    pub sysctls: Vec<String>,
    pub secrets: Vec<String>,
}

fn cleared(values: &[String]) -> bool {
    values.len() == 1 && values[0] == "none"
}

fn list<T>(values: &[String], parse: impl Fn(&str) -> Result<T>) -> Result<Vec<T>> {
    if cleared(values) {
        return Ok(Vec::new());
    }
    values.iter().map(|v| parse(v)).collect()
}

impl Overrides {
    pub fn is_empty(&self) -> bool {
        *self == Overrides::default()
    }

    pub fn apply(&self, t: &mut Tuning) -> Result<()> {
        if let Some(hostname) = &self.hostname {
            if !hostname.is_empty() {
                validate_hostname(hostname)?;
            }
            t.hostname = some_unless_empty(hostname);
        }
        if let Some(user) = &self.user {
            if user.contains(':') {
                bail!("--user {user}: systemd-nspawn takes a user name or a uid, without a group");
            }
            reject_whitespace("--user", user)?;
            t.user = some_unless_empty(user);
        }
        if let Some(dir) = &self.working_dir {
            if !dir.is_empty() && !dir.starts_with('/') {
                bail!("--workdir {dir}: an absolute path inside the machine");
            }
            reject_whitespace("--workdir", dir)?;
            t.working_dir = some_unless_empty(dir);
        }
        if !self.cap_add.is_empty() {
            t.cap_add = list(&self.cap_add, capability)?;
        }
        if !self.cap_drop.is_empty() {
            t.cap_drop = list(&self.cap_drop, capability)?;
        }
        if let Some(privileged) = self.privileged {
            t.privileged = privileged;
        }
        if let Some(read_only) = self.read_only {
            t.read_only = read_only;
        }
        if !self.tmpfs.is_empty() {
            t.tmpfs = list(&self.tmpfs, tmpfs)?;
        }
        if let Some(size) = self.shm_size {
            t.shm_size = (size > 0).then_some(size);
        }
        if !self.devices.is_empty() {
            t.devices = list(&self.devices, device)?;
        }
        if !self.dns.is_empty() {
            t.dns = list(&self.dns, |s| {
                s.parse::<IpAddr>()
                    .with_context(|| format!("--dns {s}: not an IP address"))
            })?;
        }
        if !self.dns_search.is_empty() {
            t.dns_search = list(&self.dns_search, |s| {
                validate_hostname(s)?;
                Ok(s.to_string())
            })?;
        }
        if !self.extra_hosts.is_empty() {
            t.extra_hosts = list(&self.extra_hosts, extra_host)?;
        }
        if !self.ulimits.is_empty() {
            t.ulimits = if cleared(&self.ulimits) {
                BTreeMap::new()
            } else {
                self.ulimits
                    .iter()
                    .map(|u| ulimit(u))
                    .collect::<Result<_>>()?
            };
        }
        if let Some(adj) = self.oom_score_adj {
            if !(-1000..=1000).contains(&adj) {
                bail!("--oom-score-adj {adj}: between -1000 and 1000");
            }
            t.oom_score_adj = Some(adj);
        }
        if let Some(signal) = &self.stop_signal {
            if !signal.is_empty() {
                crate::api::machines::signal_number(signal)?;
            }
            t.stop_signal = some_unless_empty(signal);
        }
        if let Some(timeout) = self.stop_timeout {
            if timeout > 86_400 {
                bail!("--stop-timeout {timeout}: a day at most");
            }
            t.stop_timeout = Some(timeout);
        }
        if let Some(init) = self.init {
            t.init = init;
        }
        if !self.sysctls.is_empty() {
            t.sysctls = if cleared(&self.sysctls) {
                BTreeMap::new()
            } else {
                self.sysctls
                    .iter()
                    .map(|s| sysctl(s))
                    .collect::<Result<_>>()?
            };
        }
        if !self.secrets.is_empty() {
            t.secrets = list(&self.secrets, crate::api::secrets::parse_ref)?;
            let mut targets = std::collections::BTreeSet::new();
            for secret in &t.secrets {
                if !targets.insert(&secret.target) {
                    bail!("--secret: two secrets at {}", secret.target);
                }
            }
        }
        Ok(())
    }
}

fn some_unless_empty(text: &str) -> Option<String> {
    (!text.is_empty()).then(|| text.to_string())
}

fn reject_whitespace(flag: &str, text: &str) -> Result<()> {
    if text.chars().any(|c| c.is_whitespace() || c.is_control()) {
        bail!("{flag} {text:?}: no whitespace");
    }
    Ok(())
}

/// A hostname (or a search domain) as the kernel takes it: labels of letters, digits and
/// hyphens, dots between them, 64 characters at most.
pub fn validate_hostname(name: &str) -> Result<()> {
    let label_ok = |l: &str| {
        !l.is_empty()
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    };
    if name.is_empty() || name.len() > 64 || !name.split('.').all(label_ok) {
        bail!("{name:?} is not a hostname: letters, digits and hyphens, dots between labels, 64 characters at most");
    }
    Ok(())
}

/// A capability as docker spells it (NET_ADMIN, CAP_NET_ADMIN, all), kept without the
/// prefix in upper case.
fn capability(text: &str) -> Result<String> {
    let upper = text.to_ascii_uppercase();
    let name = upper.strip_prefix("CAP_").unwrap_or(&upper);
    if name == "ALL" {
        return Ok("ALL".to_string());
    }
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    {
        bail!("{text}: not a capability name (NET_ADMIN, SYS_PTRACE, ALL)");
    }
    Ok(name.to_string())
}

/// --tmpfs PATH[:options]; the options go to the mount as they are.
fn tmpfs(text: &str) -> Result<String> {
    let path = text.split_once(':').map_or(text, |(p, _)| p);
    if !path.starts_with('/') || path == "/" || path.chars().any(char::is_whitespace) {
        bail!("--tmpfs {text}: an absolute path inside the machine, optionally :OPTIONS");
    }
    Ok(text.to_string())
}

/// --device HOST[:CONTAINER[:PERMISSIONS]], as docker takes it.
fn device(text: &str) -> Result<Device> {
    let fields: Vec<&str> = text.split(':').collect();
    let (host, container, permissions) = match fields[..] {
        [host] => (host, host, "rwm"),
        [host, container] => (host, container, "rwm"),
        [host, container, permissions] => (host, container, permissions),
        _ => bail!("--device {text}: HOST[:CONTAINER[:PERMISSIONS]]"),
    };
    if !host.starts_with("/dev/") || !container.starts_with('/') {
        bail!("--device {text}: a node under /dev, and an absolute path inside the machine");
    }
    if text.chars().any(char::is_whitespace) {
        bail!("--device {text}: no whitespace");
    }
    if permissions.is_empty()
        || permissions.len() > 3
        || !permissions.chars().all(|c| matches!(c, 'r' | 'w' | 'm'))
    {
        bail!("--device {text}: permissions are r, w and m");
    }
    Ok(Device {
        host: host.to_string(),
        container: container.to_string(),
        permissions: permissions.to_string(),
    })
}

/// --add-host HOST:IP (or HOST=IP); "host-gateway" stands for the machine's gateway.
fn extra_host(text: &str) -> Result<ExtraHost> {
    let (host, ip) = text
        .split_once(':')
        .or_else(|| text.split_once('='))
        .with_context(|| format!("--add-host {text}: HOST:IP"))?;
    validate_hostname(host)?;
    if ip != "host-gateway" {
        ip.parse::<IpAddr>()
            .with_context(|| format!("--add-host {text}: {ip} is not an IP address"))?;
    }
    Ok(ExtraHost {
        host: host.to_string(),
        ip: ip.to_string(),
    })
}

/// --ulimit NAME=SOFT[:HARD], docker's names (nofile, nproc, core, ...).
fn ulimit(text: &str) -> Result<(String, (u64, u64))> {
    let (name, values) = text
        .split_once('=')
        .with_context(|| format!("--ulimit {text}: NAME=SOFT[:HARD]"))?;
    let name = name.to_ascii_lowercase();
    const NAMES: [&str; 16] = [
        "core",
        "cpu",
        "data",
        "fsize",
        "locks",
        "memlock",
        "msgqueue",
        "nice",
        "nofile",
        "nproc",
        "rss",
        "rtprio",
        "rttime",
        "sigpending",
        "stack",
        "as",
    ];
    if !NAMES.contains(&name.as_str()) {
        bail!("--ulimit {text}: unknown resource {name}");
    }
    let limit = |s: &str| -> Result<u64> {
        if s == "unlimited" || s == "-1" {
            return Ok(u64::MAX);
        }
        s.parse()
            .with_context(|| format!("--ulimit {text}: {s} is not a number"))
    };
    let (soft, hard) = match values.split_once(':') {
        Some((soft, hard)) => (limit(soft)?, limit(hard)?),
        None => {
            let both = limit(values)?;
            (both, both)
        }
    };
    if soft > hard {
        bail!("--ulimit {text}: the soft limit is above the hard one");
    }
    Ok((name, (soft, hard)))
}

/// --sysctl KEY=VALUE, net.* only: what an app machine's own network namespace takes.
fn sysctl(text: &str) -> Result<(String, String)> {
    let (key, value) = text
        .split_once('=')
        .with_context(|| format!("--sysctl {text}: KEY=VALUE"))?;
    if !key.starts_with("net.")
        || key.contains("..")
        || !key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        bail!("--sysctl {text}: net.* keys only, set in the machine's network namespace");
    }
    if value.is_empty() || value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        bail!("--sysctl {text}: a value without whitespace");
    }
    Ok((key.to_string(), value.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_are_parsed_and_rendered() {
        let flags = Overrides {
            hostname: Some("web-1".into()),
            user: Some("1000".into()),
            working_dir: Some("/srv".into()),
            cap_add: vec!["net_admin".into(), "CAP_SYS_PTRACE".into()],
            cap_drop: vec!["ALL".into()],
            read_only: Some(true),
            tmpfs: vec!["/run:size=64m".into(), "/tmp".into()],
            shm_size: Some(1 << 26),
            devices: vec!["/dev/null:/dev/nullo:r".into(), "/dev/dri".into()],
            dns: vec!["10.0.0.53".into()],
            dns_search: vec!["example.org".into()],
            extra_hosts: vec!["db:10.1.1.1".into(), "gw=host-gateway".into()],
            ulimits: vec!["nofile=64:128".into(), "nproc=unlimited".into()],
            oom_score_adj: Some(-500),
            stop_signal: Some("SIGINT".into()),
            stop_timeout: Some(2),
            init: Some(true),
            sysctls: vec!["net.ipv4.ip_forward=1".into()],
            secrets: vec!["pw".into(), "tls:/etc/key:0400:1000:1000".into()],
            privileged: None,
        };
        let mut t = Tuning::default();
        flags.apply(&mut t).unwrap();
        // Dropping all but the added ones is spelled out, since systemd-nspawn would
        // let the drop win.
        let dropped = ALL_CAPABILITIES
            .iter()
            .filter(|c| !["CAP_NET_ADMIN", "CAP_SYS_PTRACE"].contains(c))
            .copied()
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(dropped.split(' ').count(), 39);
        assert_eq!(
            t.exec_lines(),
            [
                "Hostname=web-1",
                "Capability=CAP_NET_ADMIN CAP_SYS_PTRACE",
                &format!("DropCapability={dropped}"),
                "OOMScoreAdjust=-500",
                "LimitNOFILE=64:128",
                "LimitNPROC=18446744073709551615:18446744073709551615",
                "LinkJournal=no",
            ]
        );
        assert_eq!(
            t.files_lines(),
            [
                "ReadOnly=yes",
                "TemporaryFileSystem=/run:size=64m",
                "TemporaryFileSystem=/tmp",
                "TemporaryFileSystem=/dev/shm:size=67108864",
                "Bind=/dev/null:/dev/nullo",
                "Bind=/dev/dri:/dev/dri",
            ]
        );
        assert_eq!(
            t.unit_lines(),
            ["DeviceAllow=/dev/null r", "DeviceAllow=/dev/dri rwm"]
        );
        assert_eq!(t.extra_hosts[1].ip, "host-gateway");
        assert_eq!(t.sysctls["net.ipv4.ip_forward"], "1");
        assert_eq!((t.user.as_deref(), t.stop_timeout), (Some("1000"), Some(2)));
        assert_eq!(t.secrets[1].target, "/etc/key");
        assert!(Overrides {
            secrets: vec!["a:/x".into(), "b:/x".into()],
            ..Overrides::default()
        }
        .apply(&mut Tuning::default())
        .is_err());
        let text = serde_json::to_string(&t).unwrap();
        assert_eq!(serde_json::from_str::<Tuning>(&text).unwrap(), t);
        assert_eq!(serde_json::to_string(&Tuning::default()).unwrap(), "{}");
        // "none" clears a list, a flag given empties an option, the rest stays.
        let cleared = Overrides {
            cap_add: vec!["none".into()],
            hostname: Some(String::new()),
            privileged: Some(true),
            ..Overrides::default()
        };
        cleared.apply(&mut t).unwrap();
        assert!(t.cap_add.is_empty() && t.hostname.is_none() && t.privileged);
        assert_eq!(t.exec_lines()[0], "Capability=all");
        assert_eq!(t.tmpfs.len(), 2);
        // ALL on both sides keeps every capability, as docker reads it.
        let mut caps = Tuning::default();
        Overrides {
            cap_drop: vec!["ALL".into()],
            ..Default::default()
        }
        .apply(&mut caps)
        .unwrap();
        assert_eq!(caps.exec_lines(), ["DropCapability=all"]);
        Overrides {
            cap_add: vec!["ALL".into()],
            ..Default::default()
        }
        .apply(&mut caps)
        .unwrap();
        assert_eq!(caps.exec_lines(), ["Capability=all"]);
    }

    #[test]
    fn bad_flags_are_refused() {
        for (field, value) in [
            ("hostname", "bad host"),
            ("hostname", "-x"),
            ("user", "1000:1000"),
            ("workdir", "srv"),
            ("cap", "NET ADMIN"),
            ("tmpfs", "run"),
            ("device", "/etc/passwd:/x"),
            ("device", "/dev/null:/x:q"),
            ("dns", "10.0.0"),
            ("host", "db"),
            ("host", "db:10.0.0"),
            ("ulimit", "nofile=128:64"),
            ("ulimit", "bogus=1"),
            ("oom", "5000"),
            ("signal", "SIGBOGUS"),
            ("sysctl", "kernel.shmmax=1"),
            ("sysctl", "net.ipv4.ip_forward"),
        ] {
            let mut flags = Overrides::default();
            match field {
                "hostname" => flags.hostname = Some(value.into()),
                "user" => flags.user = Some(value.into()),
                "workdir" => flags.working_dir = Some(value.into()),
                "cap" => flags.cap_add = vec![value.into()],
                "tmpfs" => flags.tmpfs = vec![value.into()],
                "device" => flags.devices = vec![value.into()],
                "dns" => flags.dns = vec![value.into()],
                "host" => flags.extra_hosts = vec![value.into()],
                "ulimit" => flags.ulimits = vec![value.into()],
                "oom" => flags.oom_score_adj = Some(5000),
                "signal" => flags.stop_signal = Some(value.into()),
                _ => flags.sysctls = vec![value.into()],
            }
            assert!(
                flags.apply(&mut Tuning::default()).is_err(),
                "{field} {value:?}"
            );
        }
    }
}
