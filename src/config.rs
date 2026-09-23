//! Configuration: defaults, /etc/nspawn/nspawn.toml, environment and flags.

use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::backend::BackendChoice;
use crate::bridge::Subnet;

pub const DEFAULT_REGISTRY: &str = "hub.nspawn.org";
pub const DEFAULT_CONFIG_PATH: &str = "/etc/nspawn/nspawn.toml";
pub const DEFAULT_MACHINES_DIR: &str = "/var/lib/machines";
/// Layers, records and per-machine writable directories. Kept outside /var/lib/machines on
/// purpose: machined would list a hidden directory there as an image and `machinectl clean`
/// would delete it.
pub const DEFAULT_STATE_DIR: &str = "/var/lib/nspawn";
pub const DEFAULT_BRIDGE: &str = "nspawn0";
pub const DEFAULT_SUBNET: &str = "10.99.0.0/24";

/// What the configuration file may contain. Every field is optional.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    pub registry: Option<String>,
    pub ca_cert: Option<PathBuf>,
    pub backend: Option<BackendChoice>,
    pub machines_dir: Option<PathBuf>,
    pub state_dir: Option<PathBuf>,
    /// Name of the bridge the machines join.
    pub bridge: Option<String>,
    /// IPv4 subnet of the bridge; the first address is the bridge's own.
    pub subnet: Option<String>,
    /// DNS servers handed to the machines (default: the host's upstream servers).
    pub dns: Option<Vec<IpAddr>>,
}

/// Effective configuration after merging file, environment and command line.
#[derive(Debug, Clone)]
pub struct Config {
    pub registry: String,
    /// Whether the registry was given (flag, environment or file) rather than defaulted.
    pub registry_set: bool,
    pub ca_cert: Option<PathBuf>,
    pub backend: BackendChoice,
    pub machines_dir: PathBuf,
    pub state_dir: PathBuf,
    pub bridge: String,
    pub subnet: Subnet,
    pub dns: Vec<IpAddr>,
    /// The file given with --config, so that unit hooks can use the same one.
    pub config_path: Option<PathBuf>,
}

impl Config {
    /// `path` is an explicit file (must exist); otherwise the default path is read if present.
    pub fn load(
        path: Option<&Path>,
        registry: Option<String>,
        ca_cert: Option<PathBuf>,
    ) -> Result<Self> {
        let file = match path {
            Some(p) => Self::read_file(p)?,
            None => {
                let default = Path::new(DEFAULT_CONFIG_PATH);
                if default.exists() {
                    Self::read_file(default)?
                } else {
                    FileConfig::default()
                }
            }
        };
        let mut config = Self::merge(file, registry, ca_cert)?;
        // The unit hooks run from /, so the file must be named by an absolute path.
        config.config_path = match path {
            Some(p) => {
                Some(fs::canonicalize(p).with_context(|| format!("resolving {}", p.display()))?)
            }
            None => None,
        };
        Ok(config)
    }

    fn read_file(path: &Path) -> Result<FileConfig> {
        let text =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    /// Command line values win over the file, the file over the defaults.
    pub fn merge(
        file: FileConfig,
        registry: Option<String>,
        ca_cert: Option<PathBuf>,
    ) -> Result<Self> {
        let subnet = file
            .subnet
            .as_deref()
            .unwrap_or(DEFAULT_SUBNET)
            .parse()
            .context("subnet in the configuration")?;
        let dns = file.dns.unwrap_or_default();
        if let Some(bad) = dns.iter().find(|a| !a.is_ipv4() || a.is_loopback()) {
            anyhow::bail!(
                "dns server {bad}: only IPv4 servers reachable from the bridge can serve the machines (no loopback, no IPv6)"
            );
        }
        let machines_dir = file
            .machines_dir
            .unwrap_or_else(|| PathBuf::from(DEFAULT_MACHINES_DIR));
        let state_dir = file
            .state_dir
            .unwrap_or_else(|| PathBuf::from(DEFAULT_STATE_DIR));
        for (key, dir) in [("machines_dir", &machines_dir), ("state_dir", &state_dir)] {
            if !dir.is_absolute() {
                anyhow::bail!(
                    "{key} must be an absolute path (unit files refer to it): {}",
                    dir.display()
                );
            }
        }
        let bridge = file.bridge.unwrap_or_else(|| DEFAULT_BRIDGE.to_string());
        if bridge.is_empty()
            || bridge.len() > 15
            || !bridge
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            anyhow::bail!("bridge name {bridge:?}: 1 to 15 letters, digits, - or _");
        }
        let registry_set = registry.is_some() || file.registry.is_some();
        Ok(Config {
            registry: registry
                .or(file.registry)
                .unwrap_or_else(|| DEFAULT_REGISTRY.to_string()),
            registry_set,
            ca_cert: ca_cert.or(file.ca_cert),
            backend: file.backend.unwrap_or(BackendChoice::Auto),
            machines_dir,
            state_dir,
            bridge,
            subnet,
            dns,
            config_path: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_nothing_is_set() {
        let c = Config::merge(FileConfig::default(), None, None).unwrap();
        assert_eq!(c.registry, DEFAULT_REGISTRY);
        assert!(!c.registry_set);
        assert_eq!(c.bridge, "nspawn0");
        assert_eq!(c.subnet.to_string(), "10.99.0.0/24");
        assert!(c.dns.is_empty());
        assert_eq!(c.backend, BackendChoice::Auto);
        assert_eq!(c.machines_dir, PathBuf::from(DEFAULT_MACHINES_DIR));
        assert!(c.ca_cert.is_none());
        assert_eq!(c.state_dir, PathBuf::from(DEFAULT_STATE_DIR));
    }

    #[test]
    fn command_line_wins_over_file() {
        let file: FileConfig = toml::from_str(
            "registry = \"file.example\"\nca_cert = \"/a.pem\"\nbackend = \"overlay\"\nmachines_dir = \"/srv/m\"\nstate_dir = \"/srv/s\"\nbridge = \"br-lab\"\nsubnet = \"172.30.5.0/24\"\ndns = [\"10.0.0.53\"]\n",
        )
        .unwrap();
        let c = Config::merge(file, Some("cli.example".into()), None).unwrap();
        assert_eq!(c.bridge, "br-lab");
        assert_eq!(c.subnet.to_string(), "172.30.5.0/24");
        assert_eq!(c.dns, vec!["10.0.0.53".parse::<IpAddr>().unwrap()]);
        assert_eq!(c.registry, "cli.example");
        assert!(c.registry_set);
        assert_eq!(c.ca_cert, Some(PathBuf::from("/a.pem")));
        assert_eq!(c.backend, BackendChoice::Overlay);
        assert_eq!(c.machines_dir, PathBuf::from("/srv/m"));
        assert_eq!(c.state_dir, PathBuf::from("/srv/s"));
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(toml::from_str::<FileConfig>("registri = \"x\"\n").is_err());
        let bad: FileConfig = toml::from_str("subnet = \"10.0.0.0/33\"\n").unwrap();
        assert!(Config::merge(bad, None, None).is_err());
        let long: FileConfig = toml::from_str("bridge = \"nspawn-machines0\"\n").unwrap();
        assert!(Config::merge(long, None, None).is_err());
        for bad in [
            "dns = [\"127.0.0.1\"]\n",
            "dns = [\"2001:db8::53\"]\n",
            "state_dir = \"relative/dir\"\n",
        ] {
            let file: FileConfig = toml::from_str(bad).unwrap();
            assert!(Config::merge(file, None, None).is_err(), "{bad}");
        }
    }

    #[test]
    fn config_path_is_kept_absolute() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nspawn.toml");
        fs::write(&path, "registry = \"x.example\"\n").unwrap();
        let config = Config::load(Some(&path), None, None).unwrap();
        assert!(config.config_path.as_ref().unwrap().is_absolute());
        assert_eq!(config.registry, "x.example");
        assert!(config.registry_set, "a file's registry counts as given");
        let relative = Path::new("nspawn.toml");
        assert!(
            Config::load(Some(relative), None, None).is_err(),
            "no such file here"
        );
    }
}
