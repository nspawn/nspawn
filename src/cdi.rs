//! Devices of the Container Device Interface: the specs vendors' tools write under
//! /etc/cdi and /var/run/cdi (nvidia-ctk cdi generate, amd-ctk cdi generate), read as
//! tags.cncf.io/container-device-interface v1.1.1 reads them, and what a machine takes
//! of the devices it names: nodes, mounts, environment, and the hooks that prepare its
//! root before it starts. --gpus names the devices of a GPU vendor's spec.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::settings::Bind;
use crate::tuning::{Device, Tuning};

/// Lowest priority first: a device of a later directory replaces one of an earlier.
pub const SPEC_DIRS: [&str; 2] = ["/etc/cdi", "/var/run/cdi"];

/// VENDOR/CLASS=NAME split into its parts, each validated as the CDI parser does.
pub fn parse_qualified(device: &str) -> Result<(&str, &str, &str), String> {
    let (vendor, class, name) = parse_device(device);
    if vendor.is_empty() {
        return Err(format!("unqualified device {device:?}, missing vendor"));
    }
    if class.is_empty() {
        return Err(format!("unqualified device {device:?}, missing class"));
    }
    if name.is_empty() {
        return Err(format!(
            "unqualified device {device:?}, missing device name"
        ));
    }
    let invalid = |e: String| format!("invalid device {device:?}: {e}");
    validate_vendor(vendor).map_err(invalid)?;
    validate_class(class).map_err(invalid)?;
    validate_device_name(name).map_err(invalid)?;
    Ok((vendor, class, name))
}

/// Whether --device names a CDI device rather than a node of the host.
pub fn is_qualified(device: &str) -> bool {
    parse_qualified(device).is_ok()
}

fn parse_device(device: &str) -> (&str, &str, &str) {
    if device.is_empty() || device.starts_with('/') {
        return ("", "", device);
    }
    let Some((qualifier, name)) = device.split_once('=') else {
        return ("", "", device);
    };
    if qualifier.is_empty() || name.is_empty() {
        return ("", "", device);
    }
    let (vendor, class) = parse_qualifier(qualifier);
    if vendor.is_empty() {
        return ("", "", device);
    }
    (vendor, class, name)
}

/// A spec's kind, VENDOR/CLASS.
fn parse_qualifier(kind: &str) -> (&str, &str) {
    match kind.split_once('/') {
        Some((vendor, class)) if !vendor.is_empty() && !class.is_empty() => (vendor, class),
        _ => ("", kind),
    }
}

fn validate_vendor(vendor: &str) -> Result<(), String> {
    vendor_or_class(vendor).map_err(|e| format!("invalid vendor. {e}"))
}

fn validate_class(class: &str) -> Result<(), String> {
    vendor_or_class(class).map_err(|e| format!("invalid class. {e}"))
}

fn vendor_or_class(name: &str) -> Result<(), String> {
    let bytes = name.as_bytes();
    let Some(&first) = bytes.first() else {
        return Err("empty name".to_string());
    };
    if !first.is_ascii_alphabetic() {
        return Err(format!("{name:?}, should start with letter"));
    }
    if bytes.len() == 1 {
        return Ok(());
    }
    for c in String::from_utf8_lossy(&bytes[1..bytes.len() - 1]).chars() {
        if !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')) {
            return Err(format!("invalid character '{c}' in name {name:?}"));
        }
    }
    if !bytes[bytes.len() - 1].is_ascii_alphanumeric() {
        return Err(format!("{name:?}, should end with a letter or digit"));
    }
    Ok(())
}

fn validate_device_name(name: &str) -> Result<(), String> {
    let bytes = name.as_bytes();
    let Some(&first) = bytes.first() else {
        return Err("invalid (empty) device name".to_string());
    };
    if !first.is_ascii_alphanumeric() {
        return Err(format!(
            "invalid class {name:?}, should start with a letter or digit"
        ));
    }
    if bytes.len() == 1 {
        return Ok(());
    }
    for c in String::from_utf8_lossy(&bytes[1..bytes.len() - 1]).chars() {
        if !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':')) {
            return Err(format!("invalid character '{c}' in device name {name:?}"));
        }
    }
    if !bytes[bytes.len() - 1].is_ascii_alphanumeric() {
        return Err(format!(
            "invalid name {name:?}, should end with a letter or digit"
        ));
    }
    Ok(())
}

// The spec, field by field as specs-go v1.1.0 has it; a field it does not know fails
// the whole file, as the reference decodes with KnownFields.

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSpec {
    #[serde(rename = "cdiVersion", default)]
    version: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    annotations: BTreeMap<String, String>,
    #[serde(default)]
    devices: Vec<RawDevice>,
    #[serde(rename = "containerEdits", default)]
    container_edits: Edits,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDevice {
    #[serde(default)]
    name: String,
    #[serde(default)]
    annotations: BTreeMap<String, String>,
    #[serde(rename = "containerEdits", default)]
    container_edits: Edits,
}

/// What a device or a whole spec asks of the machine.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Edits {
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub device_nodes: Vec<DeviceNode>,
    #[serde(default)]
    pub net_devices: Vec<NetDevice>,
    #[serde(default)]
    pub hooks: Vec<Hook>,
    #[serde(default)]
    pub mounts: Vec<Mount>,
    #[serde(default)]
    pub intel_rdt: Option<IntelRdt>,
    #[serde(default)]
    pub additional_gids: Vec<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DeviceNode {
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub host_path: String,
    /// b, c, u or p; empty takes the host node's.
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub major: i64,
    #[serde(default)]
    pub minor: i64,
    #[serde(default)]
    pub file_mode: Option<u32>,
    /// r, w and m; empty for all three, none for no access rule.
    #[serde(default)]
    pub permissions: String,
    #[serde(default)]
    pub uid: Option<u32>,
    #[serde(default)]
    pub gid: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Mount {
    #[serde(default)]
    pub host_path: String,
    #[serde(default)]
    pub container_path: String,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(rename = "type", default)]
    pub kind: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Hook {
    #[serde(default)]
    pub hook_name: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
    /// Seconds.
    #[serde(default)]
    pub timeout: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct IntelRdt {
    #[serde(rename = "closID", default)]
    pub clos_id: String,
    #[serde(default)]
    pub l3_cache_schema: String,
    #[serde(default)]
    pub mem_bw_schema: String,
    #[serde(default)]
    pub schemata: Option<Vec<String>>,
    #[serde(default)]
    pub enable_monitoring: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct NetDevice {
    #[serde(default)]
    pub host_interface_name: String,
    #[serde(default)]
    pub name: String,
}

const HOOK_NAMES: [&str; 6] = [
    "prestart",
    "createRuntime",
    "createContainer",
    "startContainer",
    "poststart",
    "poststop",
];

/// The spec versions the reference knows, oldest first.
const VERSIONS: [&str; 10] = [
    "0.1.0", "0.2.0", "0.3.0", "0.4.0", "0.5.0", "0.6.0", "0.7.0", "0.8.0", "1.0.0", "1.1.0",
];

impl Edits {
    fn is_empty(&self) -> bool {
        self.env.is_empty()
            && self.device_nodes.is_empty()
            && self.hooks.is_empty()
            && self.mounts.is_empty()
            && self.additional_gids.is_empty()
            && self.intel_rdt.is_none()
            && self.net_devices.is_empty()
    }

    fn validate(&self) -> Result<(), String> {
        validate_env(&self.env).map_err(|e| format!("invalid container edits: {e}"))?;
        for d in &self.device_nodes {
            if d.path.is_empty() {
                return Err("invalid (empty) device path".to_string());
            }
            if !matches!(d.kind.as_str(), "" | "b" | "c" | "u" | "p") {
                return Err(format!("device {:?}: invalid type {:?}", d.path, d.kind));
            }
            if !d.permissions.is_empty()
                && d.permissions != "none"
                && !d
                    .permissions
                    .trim_matches(|c| matches!(c, 'r' | 'w' | 'm'))
                    .is_empty()
            {
                return Err(format!(
                    "device {:?}: invalid permissions {:?}",
                    d.path, d.permissions
                ));
            }
        }
        for h in &self.hooks {
            if !HOOK_NAMES.contains(&h.hook_name.as_str()) {
                return Err(format!("invalid hook name {:?}", h.hook_name));
            }
            if h.path.is_empty() {
                return Err(format!("invalid hook {:?} with empty path", h.hook_name));
            }
            validate_env(&h.env).map_err(|e| format!("invalid hook {:?}: {e}", h.hook_name))?;
        }
        for m in &self.mounts {
            if m.host_path.is_empty() {
                return Err("invalid mount, empty host path".to_string());
            }
            if m.container_path.is_empty() {
                return Err("invalid mount, empty container path".to_string());
            }
        }
        if let Some(rdt) = &self.intel_rdt {
            let c = rdt.clos_id.as_str();
            if c != "/" && (c.len() >= 4096 || c == "." || c == ".." || c.contains(['/', '\n'])) {
                return Err("invalid ClosID".to_string());
            }
        }
        let (mut hosts, mut names) = (BTreeMap::new(), BTreeMap::new());
        for dev in &self.net_devices {
            if dev.host_interface_name.is_empty() {
                return Err("invalid linux net device, empty HostInterfaceName".to_string());
            }
            if dev.name.is_empty() {
                return Err("invalid linux net device, empty Name".to_string());
            }
            if let Some(other) = hosts.insert(&dev.host_interface_name, &dev.name) {
                return Err(format!(
                    "invalid linux net device, duplicate HostInterfaceName {:?} with names {:?} and {:?}",
                    dev.host_interface_name, dev.name, other
                ));
            }
            if let Some(other) = names.insert(&dev.name, &dev.host_interface_name) {
                return Err(format!(
                    "invalid linux net device, duplicate Name {:?} with HostInterfaceName {:?} and {:?}",
                    dev.name, dev.host_interface_name, other
                ));
            }
        }
        Ok(())
    }

    fn append(&mut self, other: &Edits) {
        self.env.extend(other.env.iter().cloned());
        self.device_nodes.extend(other.device_nodes.iter().cloned());
        self.net_devices.extend(other.net_devices.iter().cloned());
        self.hooks.extend(other.hooks.iter().cloned());
        self.mounts.extend(other.mounts.iter().cloned());
        if other.intel_rdt.is_some() {
            self.intel_rdt.clone_from(&other.intel_rdt);
        }
        self.additional_gids
            .extend(other.additional_gids.iter().copied());
    }
}

fn validate_env(env: &[String]) -> Result<(), String> {
    for v in env {
        if v.find('=').is_none_or(|i| i == 0) {
            return Err(format!("invalid environment variable {v:?}"));
        }
    }
    Ok(())
}

fn version_index(version: &str) -> Option<usize> {
    let bare = version.strip_prefix('v').unwrap_or(version);
    VERSIONS.iter().position(|v| *v == bare)
}

/// The oldest version that has everything the spec uses.
fn required_version(spec: &RawSpec) -> &'static str {
    let edits: Vec<&Edits> = spec
        .devices
        .iter()
        .map(|d| &d.container_edits)
        .chain([&spec.container_edits])
        .collect();
    let any = |test: &dyn Fn(&Edits) -> bool| edits.iter().any(|e| test(e));
    if any(&|e| {
        !e.net_devices.is_empty()
            || e.intel_rdt
                .as_ref()
                .is_some_and(|r| r.schemata.is_some() || r.enable_monitoring)
    }) {
        return "1.1.0";
    }
    if any(&|e| e.intel_rdt.is_some() || !e.additional_gids.is_empty()) {
        return "0.7.0";
    }
    let class_has_dot = spec
        .kind
        .split_once('/')
        .is_some_and(|(_, class)| class.contains('.'));
    if !spec.annotations.is_empty()
        || spec.devices.iter().any(|d| !d.annotations.is_empty())
        || class_has_dot
    {
        return "0.6.0";
    }
    if spec
        .devices
        .iter()
        .any(|d| d.name.as_bytes().first().is_some_and(u8::is_ascii_digit))
        || any(&|e| e.device_nodes.iter().any(|n| !n.host_path.is_empty()))
    {
        return "0.5.0";
    }
    if any(&|e| e.mounts.iter().any(|m| !m.kind.is_empty())) {
        return "0.4.0";
    }
    "0.3.0"
}

/// A spec read and validated.
#[derive(Debug, Clone)]
struct Spec {
    path: PathBuf,
    priority: usize,
    kind: String,
    edits: Edits,
    devices: BTreeMap<String, Edits>,
}

fn parse_spec(data: &[u8]) -> Result<RawSpec, String> {
    let value = crate::compose::yaml_to_json(data)
        .map_err(|e| format!("failed to unmarshal CDI Spec: {e}"))?;
    match value {
        None | Some(serde_json::Value::Null) => Err("no Spec data".to_string()),
        Some(value) => {
            serde_json::from_value(value).map_err(|e| format!("failed to unmarshal CDI Spec: {e}"))
        }
    }
}

fn new_spec(raw: RawSpec, path: &Path, priority: usize) -> Result<Spec, String> {
    let invalid = |e: String| format!("invalid CDI Spec: {e}");
    let Some(given) = version_index(&raw.version) else {
        return Err(invalid(format!("invalid version {:?}", raw.version)));
    };
    let required = required_version(&raw);
    if version_index(required).is_some_and(|r| r > given) {
        return Err(invalid(format!(
            "the spec version must be at least v{required}"
        )));
    }
    let (vendor, class) = parse_qualifier(&raw.kind);
    validate_vendor(vendor).map_err(invalid)?;
    validate_class(class).map_err(invalid)?;
    raw.container_edits.validate().map_err(invalid)?;
    let mut devices = BTreeMap::new();
    for d in &raw.devices {
        let device = || -> Result<(), String> {
            validate_device_name(&d.name)?;
            if d.container_edits.is_empty() {
                return Err("invalid device, empty device edits".to_string());
            }
            d.container_edits
                .validate()
                .map_err(|e| format!("invalid device {:?}: {e}", d.name))
        };
        device().map_err(|e| invalid(format!("failed add device {:?}: {e}", d.name)))?;
        if devices
            .insert(d.name.clone(), d.container_edits.clone())
            .is_some()
        {
            return Err(invalid(format!(
                "invalid spec, multiple device {:?}",
                d.name
            )));
        }
    }
    if devices.is_empty() {
        return Err(invalid("invalid spec, no devices".to_string()));
    }
    Ok(Spec {
        path: path.to_path_buf(),
        priority,
        kind: raw.kind,
        edits: raw.container_edits,
        devices,
    })
}

fn read_spec(path: &Path, priority: usize) -> Result<Spec, String> {
    let data = std::fs::read(path).map_err(|e| {
        format!(
            "failed to read CDI Spec {:?}: {e}",
            path.display().to_string()
        )
    })?;
    let raw = parse_spec(&data).map_err(|e| {
        format!(
            "failed to parse CDI Spec {:?}: {e}",
            path.display().to_string()
        )
    })?;
    new_spec(raw, path, priority)
}

/// The devices the host's specs offer.
#[derive(Debug, Default)]
pub struct Registry {
    specs: Vec<Spec>,
    /// Qualified name to the spec that has it.
    devices: BTreeMap<String, usize>,
    /// What could not be read or was given twice.
    pub errors: Vec<String>,
}

impl Registry {
    pub fn load() -> Registry {
        let dirs: Vec<&Path> = SPEC_DIRS.iter().map(Path::new).collect();
        Registry::load_from(&dirs)
    }

    pub fn load_from(dirs: &[&Path]) -> Registry {
        let mut registry = Registry::default();
        let mut conflicts = BTreeSet::new();
        'dirs: for (priority, dir) in dirs.iter().enumerate() {
            let mut entries: Vec<std::fs::DirEntry> = match std::fs::read_dir(dir) {
                Ok(entries) => entries.flatten().collect(),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                // The reference stops scanning at a directory it cannot list.
                Err(e) => {
                    registry
                        .errors
                        .push(format!("failed to list {}: {e}", dir.display()));
                    break 'dirs;
                }
            };
            entries.sort_by_key(std::fs::DirEntry::file_name);
            for entry in entries {
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    continue;
                }
                let path = entry.path();
                if !matches!(
                    path.extension().and_then(|e| e.to_str()),
                    Some("json" | "yaml")
                ) {
                    continue;
                }
                let spec = match read_spec(&path, priority) {
                    Ok(spec) => spec,
                    Err(e) => {
                        registry.errors.push(format!("failed to load CDI Spec {e}"));
                        continue;
                    }
                };
                let index = registry.specs.len();
                for device in spec.devices.keys() {
                    let qualified = format!("{}={device}", spec.kind);
                    if let Some(&other) = registry.devices.get(&qualified) {
                        let old = &registry.specs[other];
                        if priority == old.priority {
                            registry.errors.push(format!(
                                "conflicting device {qualified:?} (specs {:?}, {:?})",
                                path.display().to_string(),
                                old.path.display().to_string()
                            ));
                            conflicts.insert(qualified.clone());
                        }
                        if priority <= old.priority {
                            continue;
                        }
                    }
                    registry.devices.insert(qualified, index);
                }
                registry.specs.push(spec);
            }
        }
        for name in conflicts {
            registry.devices.remove(&name);
        }
        registry
    }

    /// Whether a spec of this kind (VENDOR/CLASS) was read.
    pub fn has_kind(&self, kind: &str) -> bool {
        self.specs.iter().any(|s| s.kind == kind)
    }

    /// The edits of the devices, in order: those of a spec once, before the first of
    /// its devices.
    pub fn edits(&self, names: &[String]) -> Result<Edits> {
        let mut edits = Edits::default();
        let mut seen_specs = BTreeSet::new();
        let mut unresolved = Vec::new();
        for name in names {
            let Some(&index) = self.devices.get(name) else {
                unresolved.push(name.as_str());
                continue;
            };
            let spec = &self.specs[index];
            if seen_specs.insert(index) {
                edits.append(&spec.edits);
            }
            let device = name.split_once('=').map_or("", |(_, d)| d);
            edits.append(&spec.devices[device]);
        }
        if !unresolved.is_empty() {
            let mut text = format!(
                "CDI device injection failed: unresolvable CDI devices {}",
                unresolved.join(", ")
            );
            if !self.errors.is_empty() {
                text.push_str(&format!(" ({})", self.errors.join("; ")));
            }
            bail!(text);
        }
        Ok(edits)
    }

    /// The CDI devices a --gpus request stands for, from the spec of the first vendor
    /// whose driver it names (any, when it names none) and that takes every capability
    /// it asks for.
    pub fn gpu_devices(&self, request: &GpuRequest) -> Result<Vec<String>> {
        if request.driver == "cdi" {
            if request.count != 0 {
                bail!("unexpected count in CDI device request");
            }
            if !request.options.is_empty() {
                bail!("unexpected options in CDI device request");
            }
            return Ok(request.device_ids.clone());
        }
        let mut capabilities = request.capabilities.clone();
        capabilities.push("gpu".to_string());
        let kind = GPU_VENDORS
            .iter()
            .find(|(driver, kind, set)| {
                (request.driver.is_empty() || request.driver == *driver)
                    && capabilities.iter().all(|c| set.contains(&c.as_str()))
                    && self.has_kind(kind)
            })
            .map(|(_, kind, _)| *kind)
            .ok_or_else(|| {
                let mut text = format!(
                    "could not select device driver {:?} with capabilities: [[{}]]",
                    request.driver,
                    capabilities.join(" ")
                );
                if !self.errors.is_empty() {
                    text.push_str(&format!(" ({})", self.errors.join("; ")));
                }
                anyhow!(text)
            })?;
        Ok(if !request.device_ids.is_empty() {
            request
                .device_ids
                .iter()
                .map(|id| format!("{kind}={id}"))
                .collect()
        } else if request.count < 0 {
            vec![format!("{kind}=all")]
        } else {
            (0..request.count).map(|i| format!("{kind}={i}")).collect()
        })
    }
}

/// The GPU drivers a request may name, the kind of their CDI specs, and the capabilities
/// each takes.
const GPU_VENDORS: [(&str, &str, &[&str]); 2] = [
    (
        "nvidia",
        "nvidia.com/gpu",
        &[
            "gpu", "nvidia", "compute", "compat32", "graphics", "utility", "video", "display",
        ],
    ),
    ("amd", "amd.com/gpu", &["gpu", "amd"]),
];

/// A --gpus request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuRequest {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub driver: String,
    /// -1 for all of them.
    #[serde(default)]
    pub count: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub device_ids: Vec<String>,
    /// As given, without gpu, which every request has.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub options: BTreeMap<String, String>,
}

fn gpu_count(text: &str) -> Result<i64> {
    if text == "all" {
        return Ok(-1);
    }
    text.parse::<i64>().map_err(|e| {
        let reason = match e.kind() {
            std::num::IntErrorKind::PosOverflow | std::num::IntErrorKind::NegOverflow => {
                "value out of range"
            }
            _ => "invalid syntax",
        };
        anyhow!(r#"invalid count ({text}): value must be either "all" or an integer: {reason}"#)
    })
}

/// --gpus all, 2, device=0,1, driver=nvidia,"capabilities=compute,utility": CSV fields,
/// a bare one being the count, one GPU when neither a count nor devices are given.
pub fn parse_gpus(value: &str) -> Result<GpuRequest> {
    if value.is_empty() {
        bail!("--gpus: a request such as all, 2 or device=0,1");
    }
    let fields = crate::mountopt::csv_fields(value)?;
    let mut request = GpuRequest::default();
    let mut seen = BTreeSet::new();
    let mut ids_given = false;
    for field in &fields {
        let (key, val) = match field.split_once('=') {
            Some((key, val)) => (key, Some(val)),
            None => (field.as_str(), None),
        };
        if !seen.insert(key.to_string()) {
            bail!("gpu request key '{key}' can be specified only once");
        }
        let Some(val) = val else {
            seen.insert("count".to_string());
            request.count = gpu_count(key)?;
            continue;
        };
        match key {
            "driver" => request.driver = val.to_string(),
            "count" => request.count = gpu_count(val)?,
            "device" => {
                ids_given = true;
                request.device_ids = val.split(',').map(str::to_string).collect();
            }
            "capabilities" => {
                request.capabilities = val.split(',').map(str::to_string).collect();
            }
            "options" => {
                let options = crate::mountopt::csv_fields(val)
                    .map_err(|e| anyhow!("failed to read gpu options: {e}"))?;
                request.options = options
                    .iter()
                    .map(|o| match o.split_once('=') {
                        Some((k, v)) => (k.to_string(), v.to_string()),
                        None => (o.clone(), String::new()),
                    })
                    .collect();
            }
            _ => bail!("unexpected key '{key}' in '{field}'"),
        }
    }
    if !seen.contains("count") && !ids_given {
        request.count = 1;
    }
    if request.count != 0 && ids_given {
        bail!("cannot set both Count and DeviceIDs on device request");
    }
    Ok(request)
}

/// One field of a CSV record, quoted when it has to be.
fn csv_field(text: &str) -> String {
    if text.contains([',', '"', '\n', '\r']) || text.starts_with(' ') {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text.to_string()
    }
}

/// The request as --gpus takes it back.
impl fmt::Display for GpuRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.count == -1
            && self.device_ids.is_empty()
            && self.driver.is_empty()
            && self.capabilities.is_empty()
            && self.options.is_empty()
        {
            return f.write_str("all");
        }
        let mut fields = Vec::new();
        if !self.device_ids.is_empty() {
            fields.push(format!("device={}", self.device_ids.join(",")));
        } else if self.count == -1 {
            fields.push("count=all".to_string());
        } else {
            fields.push(format!("count={}", self.count));
        }
        if !self.driver.is_empty() {
            fields.push(format!("driver={}", self.driver));
        }
        if !self.capabilities.is_empty() {
            fields.push(format!("capabilities={}", self.capabilities.join(",")));
        }
        if !self.options.is_empty() {
            let options: Vec<String> = self
                .options
                .iter()
                .map(|(k, v)| {
                    csv_field(&if v.is_empty() {
                        k.clone()
                    } else {
                        format!("{k}={v}")
                    })
                })
                .collect();
            fields.push(format!("options={}", options.join(",")));
        }
        let fields: Vec<String> = fields.iter().map(|f| csv_field(f)).collect();
        f.write_str(&fields.join(","))
    }
}

/// A device node as the machine gets it, the host's details filled in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// Inside the machine.
    pub path: String,
    pub host_path: String,
    /// b, c, u or p.
    pub kind: String,
    pub major: i64,
    pub minor: i64,
    pub file_mode: u32,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    /// What the machine's cgroup may do with it; empty for no rule.
    pub permissions: String,
    /// The host's node is the one the spec describes, and is bound as it is.
    pub on_host: bool,
}

/// fillMissingInfo: the type and numbers of the host's node where the spec leaves them
/// out.
fn complete(d: &DeviceNode) -> Result<Node> {
    let minimal = !d.kind.is_empty() && (d.major != 0 || d.kind == "p");
    let host_path = if d.host_path.is_empty() {
        d.path.clone()
    } else {
        d.host_path.clone()
    };
    let host = std::fs::symlink_metadata(&host_path).ok().and_then(|meta| {
        let t = meta.file_type();
        let kind = if t.is_block_device() {
            "b"
        } else if t.is_char_device() {
            "c"
        } else if t.is_fifo() {
            "p"
        } else {
            return None;
        };
        let rdev = meta.rdev();
        Some((
            kind,
            nix::sys::stat::major(rdev) as i64,
            nix::sys::stat::minor(rdev) as i64,
            meta.permissions().mode() & 0o7777,
        ))
    });
    let mut node = Node {
        path: d.path.clone(),
        host_path: host_path.clone(),
        kind: d.kind.clone(),
        major: d.major,
        minor: d.minor,
        file_mode: d.file_mode.unwrap_or(0o666),
        uid: d.uid,
        gid: d.gid,
        permissions: String::new(),
        on_host: false,
    };
    match host {
        None if !minimal => {
            let reason = if std::path::Path::new(&host_path).symlink_metadata().is_ok() {
                "not a device node".to_string()
            } else {
                "no such file or directory".to_string()
            };
            bail!("failed to stat CDI host device {host_path:?}: {reason}");
        }
        None => {}
        Some((kind, major, minor, mode)) => {
            node.file_mode = d.file_mode.unwrap_or(mode);
            if minimal {
                node.on_host =
                    kind == d.kind && (kind == "p" || (major, minor) == (d.major, d.minor));
            } else {
                if d.kind.is_empty() {
                    node.kind = kind.to_string();
                }
                if node.kind != kind {
                    bail!(
                        "CDI device ({:?}, {:?}), host type mismatch ({}, {kind})",
                        d.path,
                        host_path,
                        node.kind
                    );
                }
                if kind != "p" && d.major == 0 {
                    node.major = major;
                    node.minor = minor;
                }
                node.on_host = kind == "p" || (node.major, node.minor) == (major, minor);
            }
        }
    }
    if matches!(node.kind.as_str(), "b" | "c") {
        node.permissions = match d.permissions.as_str() {
            "" => "rwm".to_string(),
            "none" => String::new(),
            other => other.to_string(),
        };
    }
    Ok(node)
}

/// What a machine takes of its CDI devices on this host now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// The devices, qualified, in the order they were asked for.
    pub names: Vec<String>,
    /// After the machine's own: a variable set here wins.
    pub env: Vec<String>,
    /// One per path inside the machine, the last edit winning.
    pub nodes: Vec<Node>,
    /// Bind mounts, shallow ones first.
    pub mounts: Vec<Bind>,
    /// What runs before systemd-nspawn, in the order of the OCI lifecycle.
    pub hooks: Vec<Hook>,
    /// What nspawn leaves out, for the caller to say.
    pub notes: Vec<String>,
}

/// The devices of `tuning` resolved against `registry`, None when it names none.
pub fn plan_with(registry: &Registry, tuning: &Tuning) -> Result<Option<Plan>> {
    if tuning.cdi_devices.is_empty() && tuning.gpus.is_empty() {
        return Ok(None);
    }
    let mut names: Vec<String> = Vec::new();
    let add = |name: String, names: &mut Vec<String>| {
        if !names.contains(&name) {
            names.push(name);
        }
    };
    for name in &tuning.cdi_devices {
        add(name.clone(), &mut names);
    }
    for request in &tuning.gpus {
        for name in registry.gpu_devices(request)? {
            add(name, &mut names);
        }
    }
    let edits = registry.edits(&names)?;
    let mut plan = Plan {
        names,
        ..Plan::default()
    };
    // A later value of a variable replaces the earlier one where it stood.
    for var in &edits.env {
        let key = var.split_once('=').map_or(var.as_str(), |(k, _)| k);
        match plan
            .env
            .iter_mut()
            .find(|v| v.split_once('=').map_or(v.as_str(), |(k, _)| k) == key)
        {
            Some(slot) => slot.clone_from(var),
            None => plan.env.push(var.clone()),
        }
    }
    for d in &edits.device_nodes {
        let node = complete(d)?;
        plan.nodes.retain(|n| n.path != node.path);
        plan.nodes.push(node);
    }
    if !edits.net_devices.is_empty() {
        bail!(
            "CDI device {} moves network interfaces into the machine, which nspawn does not do from a spec; --interface does",
            plan.names.join(", ")
        );
    }
    for m in &edits.mounts {
        let bind = m.kind.is_empty()
            || m.kind == "bind"
            || m.options.iter().any(|o| o == "bind" || o == "rbind");
        if !bind {
            bail!(
                "CDI mount at {}: type {} is not supported; bind mounts are",
                m.container_path,
                m.kind
            );
        }
        if !crate::volume::plain_inside_path(&m.container_path) {
            bail!(
                "CDI mount at {}: an absolute path inside the machine",
                m.container_path
            );
        }
        if !Path::new(&m.host_path).exists() {
            bail!(
                "CDI mount of {}: it does not exist on the host",
                m.host_path
            );
        }
        plan.mounts.retain(|b| b.target != m.container_path);
        plan.mounts.push(Bind {
            source: PathBuf::from(&m.host_path),
            target: m.container_path.clone(),
            read_only: m.options.iter().any(|o| o == "ro"),
        });
    }
    let depth = |b: &Bind| b.target.split('/').filter(|c| !c.is_empty()).count();
    plan.mounts.sort_by_key(depth);
    for stage in [
        &["prestart", "createRuntime"][..],
        &["createContainer"],
        &["startContainer"],
    ] {
        plan.hooks.extend(
            edits
                .hooks
                .iter()
                .filter(|h| stage.contains(&h.hook_name.as_str()))
                .cloned(),
        );
    }
    for hook in edits
        .hooks
        .iter()
        .filter(|h| matches!(h.hook_name.as_str(), "poststart" | "poststop"))
    {
        plan.notes.push(format!(
            "the {} hook {} of the CDI specs is not run: nspawn runs the hooks that prepare a machine before it starts",
            hook.hook_name, hook.path
        ));
    }
    let gids: Vec<String> = edits
        .additional_gids
        .iter()
        .filter(|g| **g != 0)
        .map(u32::to_string)
        .collect();
    if !gids.is_empty() {
        plan.notes.push(format!(
            "the CDI specs add the groups {} to the program's, which systemd-nspawn cannot do; it runs with the groups of its user",
            gids.join(", ")
        ));
    }
    if edits.intel_rdt.is_some() {
        plan.notes.push(
            "the Intel RDT class of the CDI specs is left out: systemd-nspawn has no setting for it"
                .to_string(),
        );
    }
    Ok(Some(plan))
}

/// `plan_with` on the specs of this host.
pub fn plan(tuning: &Tuning) -> Result<Option<Plan>> {
    if tuning.cdi_devices.is_empty() && tuning.gpus.is_empty() {
        return Ok(None);
    }
    plan_with(&Registry::load(), tuning)
}

/// The device nodes as --device gives them: the host's node where it is the one the
/// spec describes, else one under `dir` that `devices` makes, which the machine's
/// cgroup lets it make again (m) when a start finds it gone.
pub fn device_paths(plan: &Plan, dir: &Path) -> Vec<Device> {
    plan.nodes
        .iter()
        .enumerate()
        .map(|(i, node)| {
            let mut permissions = node.permissions.clone();
            if !node.on_host && !permissions.is_empty() && !permissions.contains('m') {
                permissions.push('m');
            }
            Device {
                host: node_source(node, dir, i).to_string_lossy().into_owned(),
                container: node.path.clone(),
                permissions,
            }
        })
        .collect()
}

/// Where the machine's node `i` comes from: the host's, or the one made under `dir`.
fn node_source(node: &Node, dir: &Path, i: usize) -> PathBuf {
    if node.on_host {
        PathBuf::from(&node.host_path)
    } else {
        dir.join(format!("node{i}"))
    }
}

/// `device_paths`, the nodes the host lacks made under `dir` with the spec's numbers,
/// mode and owner; those an earlier start made are kept. `dir`'s parent is the
/// machine's own (0700): a node made there is no node for the host's users.
pub fn devices(plan: &Plan, dir: &Path) -> Result<Vec<Device>> {
    use std::os::unix::fs::DirBuilderExt;
    let mut wanted = BTreeSet::new();
    for (i, node) in plan.nodes.iter().enumerate() {
        if node.on_host {
            continue;
        }
        if let Some(parent) = dir.parent() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .with_context(|| format!("setting the mode of {}", parent.display()))?;
        }
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = node_source(node, dir, i);
        wanted.insert(path.clone());
        let same = std::fs::symlink_metadata(&path).is_ok_and(|meta| {
            let t = meta.file_type();
            let kind = if t.is_block_device() {
                "b"
            } else if t.is_char_device() {
                "c"
            } else if t.is_fifo() {
                "p"
            } else {
                ""
            };
            kind == node.kind
                && (kind == "p"
                    || meta.rdev() == nix::sys::stat::makedev(node.major as u64, node.minor as u64))
        });
        if !same {
            let _ = std::fs::remove_file(&path);
            make_node(&path, node)?;
        } else {
            set_node_owner(&path, node)?;
        }
    }
    // Nodes of devices the machine no longer takes.
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if !wanted.contains(&entry.path()) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    Ok(device_paths(plan, dir))
}

fn node_kind(kind: &str) -> nix::sys::stat::SFlag {
    use nix::sys::stat::SFlag;
    match kind {
        "b" => SFlag::S_IFBLK,
        "p" => SFlag::S_IFIFO,
        _ => SFlag::S_IFCHR,
    }
}

fn make_node(path: &Path, node: &Node) -> Result<()> {
    use nix::sys::stat::{makedev, mknod, Mode};
    mknod(
        path,
        node_kind(&node.kind),
        Mode::from_bits_truncate(node.file_mode),
        makedev(node.major as u64, node.minor as u64),
    )
    .with_context(|| format!("making the CDI device node {}", path.display()))?;
    set_node_owner(path, node)
}

fn set_node_owner(path: &Path, node: &Node) -> Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(node.file_mode))
        .with_context(|| format!("setting the mode of {}", path.display()))?;
    if node.uid.is_some() || node.gid.is_some() {
        std::os::unix::fs::lchown(path, node.uid, node.gid)
            .with_context(|| format!("setting the owner of {}", path.display()))?;
    }
    Ok(())
}

/// Seconds a hook without a timeout of its own may take.
const HOOK_TIMEOUT: u64 = 60;

/// What the hooks are told of the machine.
pub struct HookMachine<'a> {
    pub name: &'a str,
    /// Its root on the host, mounted.
    pub root: &'a Path,
    /// A directory for the bundle's config.json.
    pub bundle: &'a Path,
    /// The image's environment and the machine's.
    pub env: &'a [String],
    pub command: &'a [String],
    /// Where `devices` made the nodes the host lacks.
    pub nodes: &'a Path,
}

fn cstring(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .with_context(|| format!("{} has a NUL byte", path.display()))
}

/// The OCI bundle the hooks read the root from: config.json naming it, with the
/// mounts, devices and process the machine gets.
fn write_bundle(plan: &Plan, machine: &HookMachine) -> Result<()> {
    std::fs::create_dir_all(machine.bundle)
        .with_context(|| format!("creating {}", machine.bundle.display()))?;
    let config = serde_json::json!({
        "ociVersion": "1.2.0",
        "root": { "path": machine.root, "readonly": false },
        "hostname": machine.name,
        "process": {
            "args": machine.command,
            "env": machine.env.iter().chain(&plan.env).collect::<Vec<_>>(),
            "cwd": "/",
            "user": { "uid": 0, "gid": 0 },
        },
        "mounts": plan.mounts.iter().map(|b| serde_json::json!({
            "destination": b.target,
            "source": b.source,
            "type": "bind",
            "options": if b.read_only { vec!["rbind", "ro"] } else { vec!["rbind"] },
        })).collect::<Vec<_>>(),
        "linux": {
            "devices": plan.nodes.iter().map(|n| serde_json::json!({
                "path": n.path,
                "type": n.kind,
                "major": n.major,
                "minor": n.minor,
                "fileMode": n.file_mode,
                "uid": n.uid.unwrap_or(0),
                "gid": n.gid.unwrap_or(0),
            })).collect::<Vec<_>>(),
        },
    });
    let path = machine.bundle.join("config.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&config)?)
        .with_context(|| format!("writing {}", path.display()))
}

/// The mounts the hooks work in, and the mount points they need in the root.
fn hook_root(plan: &Plan, root: &Path, nodes_dir: &Path) -> Result<crate::nsenter::HookRoot> {
    let dev = crate::backend::resolve_in_root(root, "/dev")?;
    // Mounts below /dev land on the tmpfs the hooks get there, as the machine's land on
    // the /dev systemd-nspawn makes: their mount points are made on it.
    let below_dev = |target: &str| {
        target
            .strip_prefix("/dev/")
            .filter(|_| crate::volume::plain_inside_path(target))
            .map(|relative| dev.join(relative))
    };
    let mut points: Vec<(String, bool)> = Vec::new();
    let mut dev_points = Vec::new();
    for bind in &plan.mounts {
        match below_dev(&bind.target) {
            Some(path) => dev_points.push((cstring(&path)?, bind.source.is_dir())),
            None => points.push((bind.target.clone(), bind.source.is_dir())),
        }
    }
    points.push(("/dev".to_string(), true));
    points.push(("/proc".to_string(), true));
    crate::backend::ensure_mount_points(root, &points)?;
    let mut binds = Vec::new();
    for bind in &plan.mounts {
        let target = match below_dev(&bind.target) {
            Some(path) => path,
            None => crate::backend::resolve_in_root(root, &bind.target)?,
        };
        binds.push((cstring(&bind.source)?, cstring(&target)?, bind.read_only));
    }
    let mut dev_dirs = BTreeSet::new();
    let mut dev_nodes = Vec::new();
    let parents = |path: &Path, dirs: &mut BTreeSet<PathBuf>| {
        let mut parent = path.parent();
        while let Some(dir) = parent.filter(|d| d.starts_with(&dev) && *d != dev) {
            dirs.insert(dir.to_path_buf());
            parent = dir.parent();
        }
    };
    for (i, node) in plan.nodes.iter().enumerate() {
        let Some(path) = below_dev(&node.path) else {
            continue;
        };
        parents(&path, &mut dev_dirs);
        // The machine's own node bound there: making one would need the cgroup's m.
        dev_nodes.push(crate::nsenter::DevNode {
            path: cstring(&path)?,
            source: cstring(&node_source(node, nodes_dir, i))?,
        });
    }
    for (path, _) in &dev_points {
        parents(
            Path::new(std::ffi::OsStr::from_bytes(path.as_bytes())),
            &mut dev_dirs,
        );
    }
    Ok(crate::nsenter::HookRoot {
        root: cstring(root)?,
        binds,
        dev: Some(cstring(&dev)?),
        dev_dirs: dev_dirs.iter().map(|d| cstring(d)).collect::<Result<_>>()?,
        dev_nodes,
        dev_points,
        proc_dir: Some(cstring(&crate::backend::resolve_in_root(root, "/proc")?)?),
        context: crate::nsenter::unit_context(),
    })
}

/// Runs the hooks of `plan`, each with the OCI state of the machine being created on
/// its stdin, in a private mount namespace where the machine's root is mounted as a
/// runtime's createContainer hooks find it: the CDI mounts in place, a /dev of the
/// CDI device nodes, a /proc. What they write to the root stays in the machine; what
/// they mount goes with their namespace.
pub async fn run_hooks(plan: &Plan, machine: &HookMachine<'_>) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    if plan.hooks.is_empty() {
        return Ok(());
    }
    write_bundle(plan, machine)?;
    let root = hook_root(plan, machine.root, machine.nodes)?;
    let state = serde_json::to_vec(&serde_json::json!({
        "ociVersion": "1.2.0",
        "id": machine.name,
        "status": "creating",
        "bundle": machine.bundle,
        "annotations": {},
    }))?;
    for hook in &plan.hooks {
        let mut cmd = tokio::process::Command::new(&hook.path);
        if let Some((argv0, rest)) = hook.args.split_first() {
            cmd.arg0(argv0).args(rest);
        }
        cmd.env_clear()
            .envs(hook.env.iter().filter_map(|v| v.split_once('=')))
            .current_dir(machine.bundle)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        crate::nsenter::in_hook_root(&mut cmd, root.clone());
        let mut child = cmd.spawn().with_context(|| {
            format!(
                "running the {} hook {} of the CDI specs",
                hook.hook_name, hook.path
            )
        })?;
        if let Some(mut stdin) = child.stdin.take() {
            // A hook that does not read its state closes the pipe early.
            let _ = stdin.write_all(&state).await;
        }
        // The start holds the store while hooks run: one without a timeout of its own
        // gets the time other starts wait for the store.
        let seconds = hook
            .timeout
            .filter(|t| *t > 0)
            .map_or(HOOK_TIMEOUT, |t| t as u64);
        let output = match tokio::time::timeout(
            std::time::Duration::from_secs(seconds),
            child.wait_with_output(),
        )
        .await
        {
            Ok(output) => output?,
            Err(_) => bail!(
                "the {} hook {} of the CDI specs did not end within {seconds}s",
                hook.hook_name,
                hook.path
            ),
        };
        for stream in [&output.stdout, &output.stderr] {
            let text = String::from_utf8_lossy(stream);
            if !text.trim().is_empty() {
                eprintln!("{}", text.trim_end());
            }
        }
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let last = stderr.trim().lines().last().unwrap_or("").to_string();
            bail!(
                "the {} hook {} of the CDI specs failed ({}){}",
                hook.hook_name,
                hook.path,
                output.status,
                if last.is_empty() {
                    String::new()
                } else {
                    format!(": {last}")
                }
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn made_nodes_get_rules_systemd_takes() {
        let node = |on_host: bool, permissions: &str| Node {
            path: "/dev/x".into(),
            host_path: "/dev/x".into(),
            kind: "c".into(),
            major: 1,
            minor: 3,
            file_mode: 0o666,
            uid: None,
            gid: None,
            permissions: permissions.into(),
            on_host,
        };
        let plan = Plan {
            nodes: vec![
                node(true, "rw"),
                node(false, "rw"),
                node(false, ""),
                node(false, "rwm"),
            ],
            ..Plan::default()
        };
        let dir = Path::new("/var/lib/nspawn/machines/m/cdi/nodes");
        let devices = device_paths(&plan, dir);
        assert_eq!(devices[0].host, "/dev/x");
        assert_eq!(devices[0].permissions, "rw");
        // A start may have to make it again, inside the unit's cgroup.
        assert_eq!(devices[1].host, dir.join("node1").to_string_lossy());
        assert_eq!(devices[1].permissions, "rwm");
        assert_eq!(devices[2].permissions, "");
        assert_eq!(devices[3].permissions, "rwm");
        // A node outside /dev is allowed by its numbers.
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("node0");
        std::os::unix::fs::symlink("/dev/null", &link).unwrap();
        assert_eq!(
            crate::tuning::device_nodes(&link.to_string_lossy()),
            ["/dev/char/1:3"]
        );
        assert!(crate::tuning::device_nodes(&tmp.path().join("gone").to_string_lossy()).is_empty());
        assert_eq!(crate::tuning::device_nodes("/dev/null"), ["/dev/null"]);
    }

    fn write(dir: &Path, file: &str, text: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(file), text).unwrap();
    }

    #[test]
    fn qualified_names() {
        assert_eq!(
            parse_qualified("nvidia.com/gpu=all"),
            Ok(("nvidia.com", "gpu", "all"))
        );
        assert!(is_qualified("vendor.com/class=dev:1"));
        for (name, error) in [
            ("/dev/null", "unqualified device \"/dev/null\", missing vendor"),
            ("vendor.com/class", "unqualified device \"vendor.com/class\", missing vendor"),
            ("vendor.com/=dev", "unqualified device \"vendor.com/=dev\", missing vendor"),
            ("class=dev", "unqualified device \"class=dev\", missing vendor"),
            (
                "1vendor/class=dev",
                "invalid device \"1vendor/class=dev\": invalid vendor. \"1vendor\", should start with letter",
            ),
            (
                "vendor/cl*ss=dev",
                "invalid device \"vendor/cl*ss=dev\": invalid class. invalid character '*' in name \"cl*ss\"",
            ),
            (
                "vendor/class=dev-",
                "invalid device \"vendor/class=dev-\": invalid name \"dev-\", should end with a letter or digit",
            ),
            (
                "vendor/class=-dev",
                "invalid device \"vendor/class=-dev\": invalid class \"-dev\", should start with a letter or digit",
            ),
        ] {
            assert_eq!(parse_qualified(name).unwrap_err(), error, "{name}");
        }
    }

    const NVIDIA: &str = r#"---
cdiVersion: 0.5.0
kind: nvidia.com/gpu
devices:
- name: "0"
  containerEdits:
    deviceNodes:
    - path: /dev/nvidia0
      hostPath: /dev/null
- name: all
  containerEdits:
    deviceNodes:
    - path: /dev/nvidia0
      hostPath: /dev/null
    - path: /dev/nvidia1
      hostPath: /dev/zero
containerEdits:
  env:
  - NVIDIA_VISIBLE_DEVICES=void
  deviceNodes:
  - path: /dev/nvidiactl
    hostPath: /dev/full
    permissions: rw
  mounts:
  - hostPath: /etc/hostname
    containerPath: /usr/lib/libcuda.so.1
    options: [ro, nosuid, nodev, bind]
  hooks:
  - hookName: createContainer
    path: /usr/bin/nvidia-cdi-hook
    args: [nvidia-cdi-hook, update-ldcache, --folder, /usr/lib]
  - hookName: poststop
    path: /usr/bin/true
"#;

    #[test]
    fn specs_are_read_and_resolved() {
        let dir = tempfile::tempdir().unwrap();
        let (etc, run) = (dir.path().join("etc"), dir.path().join("run"));
        write(&etc, "nvidia.yaml", NVIDIA);
        // Not a spec file: neither .yaml nor .json.
        write(&etc, "notes.yml", "nonsense");
        let registry = Registry::load_from(&[&etc, &run]);
        assert!(registry.errors.is_empty(), "{:?}", registry.errors);
        assert!(registry.has_kind("nvidia.com/gpu"));
        let tuning = Tuning {
            gpus: vec![parse_gpus("all").unwrap()],
            ..Tuning::default()
        };
        let plan = plan_with(&registry, &tuning).unwrap().unwrap();
        assert_eq!(plan.names, ["nvidia.com/gpu=all"]);
        assert_eq!(plan.env, ["NVIDIA_VISIBLE_DEVICES=void"]);
        let nodes: Vec<(&str, &str, &str)> = plan
            .nodes
            .iter()
            .map(|n| {
                (
                    n.path.as_str(),
                    n.host_path.as_str(),
                    n.permissions.as_str(),
                )
            })
            .collect();
        assert_eq!(
            nodes,
            [
                ("/dev/nvidiactl", "/dev/full", "rw"),
                ("/dev/nvidia0", "/dev/null", "rwm"),
                ("/dev/nvidia1", "/dev/zero", "rwm"),
            ]
        );
        assert!(plan.nodes.iter().all(|n| n.on_host && n.kind == "c"));
        assert_eq!(
            plan.mounts,
            [Bind {
                source: "/etc/hostname".into(),
                target: "/usr/lib/libcuda.so.1".into(),
                read_only: true,
            }]
        );
        assert_eq!(plan.hooks.len(), 1);
        assert_eq!(plan.hooks[0].args[1], "update-ldcache");
        assert_eq!(plan.notes.len(), 1, "the poststop hook is not run");
        // A device the specs do not have.
        let two = Tuning {
            gpus: vec![parse_gpus("2").unwrap()],
            ..Tuning::default()
        };
        assert_eq!(
            plan_with(&registry, &two).unwrap_err().to_string(),
            "CDI device injection failed: unresolvable CDI devices nvidia.com/gpu=1"
        );
        // No AMD spec, and no driver for a capability nobody has.
        for (request, error) in [
            (
                "driver=amd",
                "could not select device driver \"amd\" with capabilities: [[gpu]]",
            ),
            (
                "capabilities=tpu",
                "could not select device driver \"\" with capabilities: [[tpu gpu]]",
            ),
        ] {
            let tuning = Tuning {
                gpus: vec![parse_gpus(request).unwrap()],
                ..Tuning::default()
            };
            assert_eq!(
                plan_with(&registry, &tuning).unwrap_err().to_string(),
                error
            );
        }
    }

    #[test]
    fn later_directories_win_and_equal_ones_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let (etc, run) = (dir.path().join("etc"), dir.path().join("run"));
        let spec = |env: &str| {
            format!(
                r#"{{"cdiVersion":"0.3.0","kind":"e2e.test/dev","devices":[{{"name":"one","containerEdits":{{"env":["{env}"]}}}}]}}"#
            )
        };
        write(&etc, "a.json", &spec("FROM=etc"));
        write(&run, "a.json", &spec("FROM=run"));
        let registry = Registry::load_from(&[&etc, &run]);
        let edits = registry.edits(&["e2e.test/dev=one".to_string()]).unwrap();
        assert_eq!(edits.env, ["FROM=run"]);
        write(&run, "b.json", &spec("FROM=run2"));
        let registry = Registry::load_from(&[&etc, &run]);
        assert!(registry.edits(&["e2e.test/dev=one".to_string()]).is_err());
        assert!(registry.errors[0].starts_with("conflicting device \"e2e.test/dev=one\""));
    }

    #[test]
    fn what_the_reference_refuses() {
        for (text, error) in [
            (
                "cdiVersion: 0.3.0\nkind: v.com/c\nfoo: 1\ndevices: []\n",
                "unknown field `foo`",
            ),
            ("cdiVersion: 9.9.9\nkind: v.com/c\n", "invalid version \"9.9.9\""),
            (
                "cdiVersion: 0.3.0\nkind: v.com/c\ndevices:\n- name: \"0\"\n  containerEdits: {env: [A=1]}\n",
                "the spec version must be at least v0.5.0",
            ),
            (
                "cdiVersion: 0.6.0\nkind: v.com/c\ndevices:\n- name: x\n  containerEdits: {}\n",
                "failed add device \"x\": invalid device, empty device edits",
            ),
            (
                "cdiVersion: 0.6.0\nkind: v.com/c\ndevices:\n- name: x\n  containerEdits: {env: [\"=1\"]}\n",
                "invalid device \"x\": invalid container edits: invalid environment variable \"=1\"",
            ),
            (
                "cdiVersion: 0.6.0\nkind: v.com/c\ndevices:\n- name: x\n  containerEdits: {deviceNodes: [{path: /dev/x, permissions: rx}]}\n",
                "device \"/dev/x\": invalid permissions \"rx\"",
            ),
            (
                "cdiVersion: 0.6.0\nkind: v.com/c\ndevices:\n- name: x\n  containerEdits: {hooks: [{hookName: later, path: /x}]}\n",
                "invalid hook name \"later\"",
            ),
            ("cdiVersion: 0.6.0\nkind: v.com/c\ndevices: []\n", "invalid spec, no devices"),
            (
                "cdiVersion: 0.6.0\nkind: c\ndevices: []\n",
                "invalid vendor. empty name",
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            write(dir.path(), "s.yaml", text);
            let registry = Registry::load_from(&[dir.path()]);
            assert_eq!(registry.errors.len(), 1, "{text}");
            assert!(
                registry.errors[0].contains(error),
                "{text}: {}",
                registry.errors[0]
            );
        }
    }

    #[test]
    fn gpu_requests_as_the_reference_reads_them() {
        let all = GpuRequest {
            count: -1,
            ..GpuRequest::default()
        };
        for text in ["all", "-1", "count=all", "count=-1"] {
            assert_eq!(parse_gpus(text).unwrap(), all, "{text}");
        }
        let full = GpuRequest {
            driver: "nvidia".into(),
            count: 1,
            capabilities: vec!["compute".into(), "utility".into()],
            options: [("foo".into(), "bar".into()), ("baz".into(), "qux".into())].into(),
            ..GpuRequest::default()
        };
        for text in [
            r#"driver=nvidia,"capabilities=compute,utility","options=foo=bar,baz=qux""#,
            r#"1,driver=nvidia,"capabilities=compute,utility","options=foo=bar,baz=qux""#,
            r#"count=1,driver=nvidia,"capabilities=compute,utility","options=foo=bar,baz=qux""#,
            r#"driver=nvidia,"capabilities=compute,utility","options=foo=bar,baz=qux",count=1"#,
        ] {
            assert_eq!(parse_gpus(text).unwrap(), full, "{text}");
        }
        assert_eq!(
            parse_gpus(r#""device=0,1""#).unwrap().device_ids,
            ["0", "1"]
        );
        for (text, error) in [
            (
                "count=invalid-integer",
                r#"invalid count (invalid-integer): value must be either "all" or an integer: invalid syntax"#,
            ),
            (
                "all,count=2",
                "gpu request key 'count' can be specified only once",
            ),
            ("size=2", "unexpected key 'size' in 'size=2'"),
            (
                "count=1,device=0",
                "cannot set both Count and DeviceIDs on device request",
            ),
        ] {
            assert_eq!(parse_gpus(text).unwrap_err().to_string(), error, "{text}");
        }
        // Printed back as --gpus takes it.
        for request in [all, full, parse_gpus(r#""device=0,1",driver=cdi"#).unwrap()] {
            assert_eq!(parse_gpus(&request.to_string()).unwrap(), request);
        }
        assert_eq!(parse_gpus("all").unwrap().to_string(), "all");
        assert_eq!(parse_gpus("2").unwrap().to_string(), "count=2");
    }

    #[test]
    fn nodes_take_what_the_host_has() {
        let null = complete(&DeviceNode {
            path: "/dev/x".into(),
            host_path: "/dev/null".into(),
            ..DeviceNode::default()
        })
        .unwrap();
        assert_eq!((null.kind.as_str(), null.major, null.minor), ("c", 1, 3));
        assert!(null.on_host);
        // Fully described, absent on the host: made by nspawn.
        let made = complete(&DeviceNode {
            path: "/dev/y".into(),
            host_path: "/nonexistent/node".into(),
            kind: "c".into(),
            major: 1,
            minor: 5,
            permissions: "none".into(),
            ..DeviceNode::default()
        })
        .unwrap();
        assert!(!made.on_host);
        assert_eq!(made.permissions, "");
        assert!(complete(&DeviceNode {
            path: "/dev/z".into(),
            host_path: "/nonexistent/node".into(),
            ..DeviceNode::default()
        })
        .unwrap_err()
        .to_string()
        .starts_with("failed to stat CDI host device \"/nonexistent/node\""));
        assert!(complete(&DeviceNode {
            path: "/dev/z".into(),
            host_path: "/dev/null".into(),
            kind: "b".into(),
            ..DeviceNode::default()
        })
        .unwrap_err()
        .to_string()
        .contains("host type mismatch (b, c)"));
    }

    #[test]
    fn mount_targets_resolve_inside_the_root() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("usr/lib")).unwrap();
        std::fs::create_dir_all(root.path().join("etc")).unwrap();
        std::os::unix::fs::symlink("/usr/lib", root.path().join("lib64")).unwrap();
        std::os::unix::fs::symlink("/etc/shadow", root.path().join("usr/lib/evil")).unwrap();
        assert_eq!(
            crate::backend::resolve_in_root(root.path(), "/lib64/libcuda.so").unwrap(),
            root.path().join("usr/lib/libcuda.so")
        );
        // A link to a host path lands in the root.
        assert_eq!(
            crate::backend::resolve_in_root(root.path(), "/usr/lib/evil").unwrap(),
            root.path().join("etc/shadow")
        );
    }
}
