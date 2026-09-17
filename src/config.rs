//! Configuration: defaults, /etc/nspawn/nspawn.toml, environment and flags.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::cli::BackendChoice;

pub const DEFAULT_REGISTRY: &str = "hub.nspawn.org";
pub const DEFAULT_CONFIG_PATH: &str = "/etc/nspawn/nspawn.toml";
pub const DEFAULT_MACHINES_DIR: &str = "/var/lib/machines";
/// Layers, records and per-machine writable directories. Kept outside /var/lib/machines on
/// purpose: machined would list a hidden directory there as an image and `machinectl clean`
/// would delete it.
pub const DEFAULT_STATE_DIR: &str = "/var/lib/nspawn";

/// What the configuration file may contain. Every field is optional.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    pub registry: Option<String>,
    pub ca_cert: Option<PathBuf>,
    pub backend: Option<BackendChoice>,
    pub machines_dir: Option<PathBuf>,
    pub state_dir: Option<PathBuf>,
}

/// Effective configuration after merging file, environment and command line.
#[derive(Debug, Clone)]
pub struct Config {
    pub registry: String,
    pub ca_cert: Option<PathBuf>,
    pub backend: BackendChoice,
    pub machines_dir: PathBuf,
    pub state_dir: PathBuf,
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
        Ok(Self::merge(file, registry, ca_cert))
    }

    fn read_file(path: &Path) -> Result<FileConfig> {
        let text =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    /// Command line values win over the file, the file over the defaults.
    pub fn merge(file: FileConfig, registry: Option<String>, ca_cert: Option<PathBuf>) -> Self {
        Config {
            registry: registry
                .or(file.registry)
                .unwrap_or_else(|| DEFAULT_REGISTRY.to_string()),
            ca_cert: ca_cert.or(file.ca_cert),
            backend: file.backend.unwrap_or(BackendChoice::Auto),
            machines_dir: file
                .machines_dir
                .unwrap_or_else(|| PathBuf::from(DEFAULT_MACHINES_DIR)),
            state_dir: file
                .state_dir
                .unwrap_or_else(|| PathBuf::from(DEFAULT_STATE_DIR)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_nothing_is_set() {
        let c = Config::merge(FileConfig::default(), None, None);
        assert_eq!(c.registry, DEFAULT_REGISTRY);
        assert_eq!(c.backend, BackendChoice::Auto);
        assert_eq!(c.machines_dir, PathBuf::from(DEFAULT_MACHINES_DIR));
        assert!(c.ca_cert.is_none());
        assert_eq!(c.state_dir, PathBuf::from(DEFAULT_STATE_DIR));
    }

    #[test]
    fn command_line_wins_over_file() {
        let file: FileConfig = toml::from_str(
            "registry = \"file.example\"\nca_cert = \"/a.pem\"\nbackend = \"overlay\"\nmachines_dir = \"/srv/m\"\nstate_dir = \"/srv/s\"\n",
        )
        .unwrap();
        let c = Config::merge(file, Some("cli.example".into()), None);
        assert_eq!(c.registry, "cli.example");
        assert_eq!(c.ca_cert, Some(PathBuf::from("/a.pem")));
        assert_eq!(c.backend, BackendChoice::Overlay);
        assert_eq!(c.machines_dir, PathBuf::from("/srv/m"));
        assert_eq!(c.state_dir, PathBuf::from("/srv/s"));
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(toml::from_str::<FileConfig>("registri = \"x\"\n").is_err());
    }
}
