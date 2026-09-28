//! Per-machine settings beyond the network, volumes, limits and healthcheck:
//! hostname, user, capabilities, mounts, devices, DNS, ulimits, signals. Remembered with
//! the machine, most of them one line of its settings file.

use std::collections::BTreeMap;
use std::net::IpAddr;

use anyhow::{bail, Context as _, Result};
use serde::{Deserialize, Serialize};

/// A device node handed to the machine (--device).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub host: String,
    pub container: String,
    /// r, w and m, as DeviceAllow= takes them.
    pub permissions: String,
}

/// A secret handed to the machine, with the fields of the long --secret form.
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
    /// Overrides the image's user: USER[:GROUP], each a name or a number of the image.
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
    /// Interfaces of the host moved into the machine while it runs, by name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub interfaces: Vec<String>,
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
    /// systemd-nspawn's Timezone= mode for /etc/localtime; None is its own default, auto.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
}

/// The modes of systemd-nspawn's Timezone= besides auto, its default: off leaves the
/// machine's /etc/localtime alone, so a zone set inside survives a restart.
pub const TIMEZONE_MODES: [&str; 5] = ["off", "copy", "bind", "symlink", "delete"];

impl Tuning {
    pub fn is_default(&self) -> bool {
        *self == Tuning::default()
    }

    /// The [Exec] lines: Hostname=, Capability=, DropCapability=, OOMScoreAdjust=, the
    /// rlimits, LinkJournal=. User= and WorkingDirectory= are the caller's, image
    /// defaults included. `confined`: the machine runs without a user namespace (an app
    /// on a bridge network), so it keeps the default capabilities instead of
    /// systemd-nspawn's.
    pub fn exec_lines(&self, confined: bool) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(hostname) = &self.hostname {
            out.push(format!("Hostname={hostname}"));
        }
        if self.privileged {
            out.push("Capability=all".to_string());
        } else if !self.cap_add.is_empty() {
            out.push(format!("Capability={}", capabilities(&self.cap_add)));
        }
        if !self.privileged {
            // systemd-nspawn lets a drop win over an add, the flags the other way round:
            // "--cap-drop ALL --cap-add X" keeps X, and "--cap-add ALL" keeps all but
            // the named drops, so those drops are spelled out.
            let add_all = self.cap_add.iter().any(|c| c == "ALL");
            let drop_all = self.cap_drop.iter().any(|c| c == "ALL");
            let added = |c: &str| self.cap_add.iter().any(|a| format!("CAP_{a}") == c);
            let mut dropped: Vec<String> = if add_all {
                self.cap_drop
                    .iter()
                    .filter(|c| *c != "ALL")
                    .map(|c| format!("CAP_{c}"))
                    .collect()
            } else if drop_all && self.cap_add.is_empty() {
                vec!["all".to_string()]
            } else if drop_all {
                ALL_CAPABILITIES
                    .iter()
                    .filter(|c| !added(c))
                    .map(|c| c.to_string())
                    .collect()
            } else {
                // The adds win over the drops: a capability in both is kept.
                self.cap_drop
                    .iter()
                    .filter(|c| !self.cap_add.contains(c))
                    .map(|c| format!("CAP_{c}"))
                    .collect()
            };
            // Without a user namespace, what systemd-nspawn retains beyond the app set
            // goes too, unless asked for: an interface given to the machine keeps
            // CAP_NET_ADMIN, since it is there to be configured.
            if confined && !add_all && !drop_all {
                for cap in NSPAWN_CAPABILITIES {
                    let kept =
                        added(cap) || (cap == "CAP_NET_ADMIN" && !self.interfaces.is_empty());
                    if !DEFAULT_CAPABILITIES.contains(&cap)
                        && !kept
                        && !dropped.iter().any(|d| d == cap)
                    {
                        dropped.push(cap.to_string());
                    }
                }
            }
            if !dropped.is_empty() {
                out.push(format!("DropCapability={}", dropped.join(" ")));
            }
        }
        if confined && !self.privileged {
            out.push(DEFAULT_SYSCALL_FILTER.to_string());
        }
        if let Some(adj) = self.oom_score_adj {
            out.push(format!("OOMScoreAdjust={adj}"));
        }
        if let Some(mode) = &self.timezone {
            out.push(format!("Timezone={mode}"));
        }
        // systemd's word for no limit; the number RLIM_INFINITY is would be refused.
        let limit = |v: &u64| {
            if *v == u64::MAX {
                "infinity".to_string()
            } else {
                v.to_string()
            }
        };
        for (name, (soft, hard)) in &self.ulimits {
            out.push(format!(
                "Limit{}={}:{}",
                name.to_ascii_uppercase(),
                limit(soft),
                limit(hard)
            ));
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

    /// DeviceAllow= for the unit, so that the machine's cgroup may open the nodes: each
    /// node under a directory given as a device (/dev/dri, /dev/snd), since DeviceAllow=
    /// takes a node and not a directory.
    pub fn unit_lines(&self) -> Vec<String> {
        self.unit_lines_with(device_nodes)
    }

    /// `unit_lines`, with `nodes` saying what device nodes a host path stands for.
    pub fn unit_lines_with(&self, nodes: impl Fn(&str) -> Vec<String>) -> Vec<String> {
        self.devices
            .iter()
            .flat_map(|d| {
                nodes(&d.host)
                    .into_iter()
                    .map(move |node| format!("DeviceAllow={node} {}", d.permissions))
            })
            .collect()
    }
}

/// The device nodes a --device path stands for: the node itself, or every character
/// and block device below a directory.
pub fn device_nodes(path: &str) -> Vec<String> {
    use std::os::unix::fs::FileTypeExt;
    fn walk(dir: &std::path::Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                walk(&entry.path(), out);
            } else if kind.is_char_device() || kind.is_block_device() {
                out.push(entry.path().to_string_lossy().into_owned());
            }
        }
    }
    let path = std::path::Path::new(path);
    if path.is_dir() {
        let mut out = Vec::new();
        walk(path, &mut out);
        out
    } else {
        vec![path.to_string_lossy().into_owned()]
    }
}

/// What an app without a user namespace keeps, instead of systemd-nspawn's set, whose
/// CAP_SYS_ADMIN is root on the host outside a user namespace: /proc/sys is one remount
/// away from writable, and kernel.core_pattern runs what it names as the host's root.
/// The least that lets images run as written (files owned, users switched, ports below
/// 1024 bound, raw sockets for ping, nodes made, chroots) plus CAP_SYS_BOOT, so that a
/// reboot asked from inside ends the machine as it does in a booted one: in a pid
/// namespace of its own, reboot(2) can only signal the machine's init, and the kexec
/// system calls the capability would also allow are filtered out
/// (`DEFAULT_SYSCALL_FILTER`).
pub const DEFAULT_CAPABILITIES: [&str; 15] = [
    "CAP_AUDIT_WRITE",
    "CAP_CHOWN",
    "CAP_DAC_OVERRIDE",
    "CAP_FOWNER",
    "CAP_FSETID",
    "CAP_KILL",
    "CAP_MKNOD",
    "CAP_NET_BIND_SERVICE",
    "CAP_NET_RAW",
    "CAP_SETFCAP",
    "CAP_SETGID",
    "CAP_SETPCAP",
    "CAP_SETUID",
    "CAP_SYS_BOOT",
    "CAP_SYS_CHROOT",
];

/// Kept out of an app without a user namespace whatever its capabilities: with
/// CAP_SYS_BOOT in the initial user namespace they would load a kernel into the host
/// for its next kexec or crash.
pub const DEFAULT_SYSCALL_FILTER: &str = "SystemCallFilter=~kexec_load kexec_file_load";

/// What systemd-nspawn retains by default (systemd-nspawn(1), --capability=), the
/// CAP_NET_ADMIN of a private network included.
const NSPAWN_CAPABILITIES: [&str; 27] = [
    "CAP_AUDIT_CONTROL",
    "CAP_AUDIT_WRITE",
    "CAP_CHOWN",
    "CAP_DAC_OVERRIDE",
    "CAP_DAC_READ_SEARCH",
    "CAP_FOWNER",
    "CAP_FSETID",
    "CAP_IPC_OWNER",
    "CAP_KILL",
    "CAP_LEASE",
    "CAP_LINUX_IMMUTABLE",
    "CAP_MKNOD",
    "CAP_NET_ADMIN",
    "CAP_NET_BIND_SERVICE",
    "CAP_NET_BROADCAST",
    "CAP_NET_RAW",
    "CAP_SETFCAP",
    "CAP_SETGID",
    "CAP_SETPCAP",
    "CAP_SETUID",
    "CAP_SYS_ADMIN",
    "CAP_SYS_BOOT",
    "CAP_SYS_CHROOT",
    "CAP_SYS_NICE",
    "CAP_SYS_PTRACE",
    "CAP_SYS_RESOURCE",
    "CAP_SYS_TTY_CONFIG",
];

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
    pub interfaces: Vec<String>,
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
    pub timezone: Option<String>,
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
            reject_whitespace("--user", user)?;
            validate_user(user)?;
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
        if !self.interfaces.is_empty() {
            let mut seen = std::collections::BTreeSet::new();
            t.interfaces = list(&self.interfaces, interface_name)?
                .into_iter()
                .filter(|iface| seen.insert(iface.clone()))
                .collect();
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
        if let Some(mode) = &self.timezone {
            t.timezone = match mode.as_str() {
                "" | "auto" => None,
                m if TIMEZONE_MODES.contains(&m) => Some(m.to_string()),
                other => bail!(
                    "--timezone {other:?}: one of auto, {}",
                    TIMEZONE_MODES.join(", ")
                ),
            };
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

/// USER[:GROUP]: neither side empty when the colon is there.
fn validate_user(text: &str) -> Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    match text.split(':').collect::<Vec<_>>().as_slice() {
        [user] if !user.is_empty() => Ok(()),
        [user, group] if !user.is_empty() && !group.is_empty() => Ok(()),
        _ => bail!("--user {text}: USER or USER:GROUP, each a name or a number of the image"),
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

/// An interface name as the kernel takes one: 1 to 15 bytes, no slash, colon or
/// whitespace; not lo, which every machine has of its own.
pub fn interface_name(text: &str) -> Result<String> {
    if text.is_empty() || text.len() > 15 || text == "." || text == ".." {
        bail!("--interface {text:?}: an interface name is 1 to 15 characters");
    }
    if text
        .chars()
        .any(|c| c == '/' || c == ':' || c.is_whitespace() || c.is_control())
    {
        bail!("--interface {text:?}: no slash, colon or whitespace in an interface name");
    }
    if text == "lo" {
        bail!("--interface lo: every machine has a loopback of its own");
    }
    Ok(text.to_string())
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

/// A capability with or without its prefix (NET_ADMIN, CAP_NET_ADMIN, all), kept without the
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
    if !crate::volume::plain_inside_path(path)
        || path == "/"
        || path.chars().any(char::is_whitespace)
    {
        bail!("--tmpfs {text}: an absolute path inside the machine, optionally :OPTIONS");
    }
    // The options become part of a settings line.
    reject_whitespace("--tmpfs", text)?;
    Ok(text.to_string())
}

/// --device HOST[:CONTAINER[:PERMISSIONS]].
fn device(text: &str) -> Result<Device> {
    let fields: Vec<&str> = text.split(':').collect();
    let (host, container, permissions) = match fields[..] {
        [host] => (host, host, "rwm"),
        [host, container] => (host, container, "rwm"),
        [host, container, permissions] => (host, container, permissions),
        _ => bail!("--device {text}: HOST[:CONTAINER[:PERMISSIONS]]"),
    };
    if !host.starts_with("/dev/") || !crate::volume::plain_inside_path(container) {
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
    // HOST=IP first, the form for IPv6 addresses, whose colons would split HOST:IP
    // wrong; a hostname never carries an equals sign.
    let (host, ip) = text
        .split_once('=')
        .or_else(|| text.split_once(':'))
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

/// --ulimit NAME=SOFT[:HARD], NAME one of nofile, nproc, core, ...
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
            interfaces: vec!["wlan0".into(), "eth1".into(), "wlan0".into()],
            dns: vec!["10.0.0.53".into()],
            dns_search: vec!["example.org".into()],
            extra_hosts: vec![
                "db:10.1.1.1".into(),
                "gw=host-gateway".into(),
                "v6=fd00::1".into(),
            ],
            ulimits: vec!["nofile=64:128".into(), "nproc=unlimited".into()],
            oom_score_adj: Some(-500),
            stop_signal: Some("SIGINT".into()),
            stop_timeout: Some(2),
            init: Some(true),
            sysctls: vec!["net.ipv4.ip_forward=1".into()],
            secrets: vec!["pw".into(), "tls:/etc/key:0400:1000:1000".into()],
            privileged: None,
            timezone: Some("off".into()),
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
            t.exec_lines(false),
            [
                "Hostname=web-1",
                "Capability=CAP_NET_ADMIN CAP_SYS_PTRACE",
                &format!("DropCapability={dropped}"),
                "OOMScoreAdjust=-500",
                "Timezone=off",
                "LimitNOFILE=64:128",
                "LimitNPROC=infinity:infinity",
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
        // A directory stands for the nodes below it, which DeviceAllow= takes one by one.
        let nodes = |path: &str| match path {
            "/dev/dri" => vec![
                "/dev/dri/card0".to_string(),
                "/dev/dri/renderD128".to_string(),
            ],
            other => vec![other.to_string()],
        };
        assert_eq!(
            t.unit_lines_with(nodes),
            [
                "DeviceAllow=/dev/null r",
                "DeviceAllow=/dev/dri/card0 rwm",
                "DeviceAllow=/dev/dri/renderD128 rwm"
            ]
        );
        assert_eq!(device_nodes("/dev/null"), ["/dev/null"]);
        let empty = tempfile::tempdir().unwrap();
        std::fs::write(empty.path().join("plain"), "").unwrap();
        assert!(device_nodes(&empty.path().to_string_lossy()).is_empty());
        assert_eq!(t.interfaces, ["wlan0", "eth1"], "once each, in order");
        assert_eq!(t.extra_hosts[1].ip, "host-gateway");
        assert_eq!(
            (t.extra_hosts[2].host.as_str(), t.extra_hosts[2].ip.as_str()),
            ("v6", "fd00::1")
        );
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
            interfaces: vec!["none".into()],
            hostname: Some(String::new()),
            privileged: Some(true),
            ..Overrides::default()
        };
        cleared.apply(&mut t).unwrap();
        assert!(t.cap_add.is_empty() && t.hostname.is_none() && t.privileged);
        assert!(t.interfaces.is_empty());
        assert_eq!(t.exec_lines(false)[0], "Capability=all");
        assert_eq!(t.tmpfs.len(), 2);
        // A capability both added and dropped is kept: the adds win.
        let mut both = Tuning::default();
        Overrides {
            cap_add: vec!["NET_ADMIN".into()],
            cap_drop: vec!["NET_ADMIN".into(), "MKNOD".into()],
            ..Default::default()
        }
        .apply(&mut both)
        .unwrap();
        assert_eq!(
            both.exec_lines(false),
            ["Capability=CAP_NET_ADMIN", "DropCapability=CAP_MKNOD"]
        );
        // ALL on both sides keeps every capability.
        let mut caps = Tuning::default();
        Overrides {
            cap_drop: vec!["ALL".into()],
            ..Default::default()
        }
        .apply(&mut caps)
        .unwrap();
        assert_eq!(caps.exec_lines(false), ["DropCapability=all"]);
        Overrides {
            cap_add: vec!["ALL".into()],
            ..Default::default()
        }
        .apply(&mut caps)
        .unwrap();
        assert_eq!(caps.exec_lines(false), ["Capability=all"]);
    }

    #[test]
    fn an_app_without_a_user_namespace_keeps_the_default_capabilities() {
        let beyond_default = "CAP_AUDIT_CONTROL CAP_DAC_READ_SEARCH CAP_IPC_OWNER CAP_LEASE CAP_LINUX_IMMUTABLE CAP_NET_ADMIN CAP_NET_BROADCAST CAP_SYS_ADMIN CAP_SYS_NICE CAP_SYS_PTRACE CAP_SYS_RESOURCE CAP_SYS_TTY_CONFIG";
        let plain = Tuning::default();
        assert_eq!(
            plain.exec_lines(true),
            [
                format!("DropCapability={beyond_default}"),
                DEFAULT_SYSCALL_FILTER.to_string()
            ]
        );
        assert!(
            DEFAULT_CAPABILITIES.contains(&"CAP_SYS_BOOT"),
            "a reboot from inside ends the machine"
        );
        assert!(
            plain.exec_lines(false).is_empty(),
            "with a user namespace, systemd-nspawn's set stays"
        );
        for cap in NSPAWN_CAPABILITIES {
            assert!(
                DEFAULT_CAPABILITIES.contains(&cap) || beyond_default.split(' ').any(|c| c == cap),
                "{cap}"
            );
        }
        // --cap-add keeps one of them, --cap-drop still drops one of the default set.
        let mut t = Tuning::default();
        Overrides {
            cap_add: vec!["SYS_PTRACE".into()],
            cap_drop: vec!["NET_RAW".into()],
            ..Overrides::default()
        }
        .apply(&mut t)
        .unwrap();
        assert_eq!(
            t.exec_lines(true),
            [
                "Capability=CAP_SYS_PTRACE".to_string(),
                format!(
                    "DropCapability=CAP_NET_RAW {}",
                    beyond_default.replace(" CAP_SYS_PTRACE", "")
                ),
                DEFAULT_SYSCALL_FILTER.to_string()
            ]
        );
        // An interface given to the machine keeps CAP_NET_ADMIN, to be configured.
        let mut t = Tuning::default();
        Overrides {
            interfaces: vec!["wlan0".into()],
            ..Overrides::default()
        }
        .apply(&mut t)
        .unwrap();
        assert_eq!(
            t.exec_lines(true),
            [
                format!(
                    "DropCapability={}",
                    beyond_default.replace(" CAP_NET_ADMIN", "")
                ),
                DEFAULT_SYSCALL_FILTER.to_string()
            ]
        );
        // --privileged, --cap-add ALL and --cap-drop ALL: everything, everything but the
        // named drops, nothing but the named adds.
        for (add, drop, expected) in [
            (
                vec![],
                vec!["ALL".into()],
                vec![
                    "DropCapability=all".to_string(),
                    DEFAULT_SYSCALL_FILTER.to_string(),
                ],
            ),
            (
                vec!["ALL".into()],
                vec!["SYS_ADMIN".into()],
                vec![
                    "Capability=all".to_string(),
                    "DropCapability=CAP_SYS_ADMIN".to_string(),
                    DEFAULT_SYSCALL_FILTER.to_string(),
                ],
            ),
            (
                vec!["NET_BIND_SERVICE".into()],
                vec!["ALL".into()],
                vec![
                    "Capability=CAP_NET_BIND_SERVICE".to_string(),
                    format!(
                        "DropCapability={}",
                        ALL_CAPABILITIES
                            .iter()
                            .filter(|c| **c != "CAP_NET_BIND_SERVICE")
                            .copied()
                            .collect::<Vec<_>>()
                            .join(" ")
                    ),
                    DEFAULT_SYSCALL_FILTER.to_string(),
                ],
            ),
        ] {
            let mut t = Tuning::default();
            Overrides {
                cap_add: add,
                cap_drop: drop,
                ..Overrides::default()
            }
            .apply(&mut t)
            .unwrap();
            assert_eq!(t.exec_lines(true), expected);
        }
        let mut t = Tuning::default();
        Overrides {
            privileged: Some(true),
            ..Overrides::default()
        }
        .apply(&mut t)
        .unwrap();
        assert_eq!(t.exec_lines(true), ["Capability=all"]);
    }

    #[test]
    fn bad_flags_are_refused() {
        for (field, value) in [
            ("hostname", "bad host"),
            ("hostname", "-x"),
            ("user", "1000:"),
            ("user", ":1000"),
            ("user", "a:b:c"),
            ("workdir", "srv"),
            ("cap", "NET ADMIN"),
            ("tmpfs", "run"),
            ("tmpfs", "/run/.."),
            ("device", "/dev/null:/dev/../x"),
            ("tmpfs", "/x:size=1m\nBind=/:/host"),
            ("device", "/etc/passwd:/x"),
            ("device", "/dev/null:/x:q"),
            ("interface", "lo"),
            ("interface", ""),
            ("interface", "."),
            ("interface", "sixteen-chars-xx"),
            ("interface", "a/b"),
            ("interface", "a:b"),
            ("interface", "wl an"),
            ("interface", "wl\nan"),
            ("dns", "10.0.0"),
            ("host", "db"),
            ("host", "db:10.0.0"),
            ("ulimit", "nofile=128:64"),
            ("ulimit", "bogus=1"),
            ("oom", "5000"),
            ("signal", "SIGBOGUS"),
            ("timezone", "utc"),
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
                "interface" => flags.interfaces = vec![value.into()],
                "dns" => flags.dns = vec![value.into()],
                "host" => flags.extra_hosts = vec![value.into()],
                "ulimit" => flags.ulimits = vec![value.into()],
                "oom" => flags.oom_score_adj = Some(5000),
                "signal" => flags.stop_signal = Some(value.into()),
                "timezone" => flags.timezone = Some(value.into()),
                _ => flags.sysctls = vec![value.into()],
            }
            assert!(
                flags.apply(&mut Tuning::default()).is_err(),
                "{field} {value:?}"
            );
        }
    }

    #[test]
    fn timezone_takes_the_modes_of_systemd_nspawn_and_auto_forgets() {
        let mut t = Tuning::default();
        for mode in TIMEZONE_MODES {
            let flags = Overrides {
                timezone: Some(mode.into()),
                ..Overrides::default()
            };
            flags.apply(&mut t).unwrap();
            assert!(t.exec_lines(false).contains(&format!("Timezone={mode}")));
        }
        let auto = Overrides {
            timezone: Some("auto".into()),
            ..Overrides::default()
        };
        auto.apply(&mut t).unwrap();
        assert_eq!(t.timezone, None);
        assert!(!t
            .exec_lines(false)
            .iter()
            .any(|l| l.starts_with("Timezone=")));
    }
}
