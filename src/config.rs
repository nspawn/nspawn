//! Configuration: defaults, /etc/nspawn/nspawn.toml, environment and flags.

use std::collections::BTreeMap;
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
pub const DEFAULT_NETWORK_POOL: &str = "10.99.0.0/16";

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
    /// Where `network create` takes the /24 of a network given no --subnet.
    pub network_pool: Option<String>,
    /// DNS servers handed to the machines (default: the host's upstream servers).
    pub dns: Option<Vec<IpAddr>>,
    /// What the images of a registry must carry, by registry (host or host:port).
    #[serde(default)]
    pub registries: BTreeMap<String, RegistryFile>,
}

/// What the file says of a registry's signatures. Every field is optional.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RegistryFile {
    /// false: nothing is checked for this registry (the hub's built-in policy included).
    pub verify: Option<bool>,
    /// A cosign public key (PEM) the images may be signed with; `keys` for several.
    pub key: Option<PathBuf>,
    #[serde(default)]
    pub keys: Vec<PathBuf>,
    /// A keyless signer: the certificate's identity and the issuer of its token.
    pub identity: Option<String>,
    pub issuer: Option<String>,
    /// Default true: no verifying signature fails the pull; false: a note, and it goes on.
    pub required: Option<bool>,
    /// Default true: the transparency log entry of a signature must verify.
    pub rekor: Option<bool>,
    /// The trusted root of another Sigstore deployment (trusted_root.json).
    pub trusted_root: Option<PathBuf>,
}

/// The signature policy of a registry, as configured; the keys are read when it applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryPolicy {
    pub verify: bool,
    pub keys: Vec<PathBuf>,
    /// (identity, issuer)
    pub identity: Option<(String, String)>,
    pub required: bool,
    pub rekor: bool,
    pub trusted_root: Option<PathBuf>,
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
    pub network_pool: Subnet,
    pub dns: Vec<IpAddr>,
    /// Signature policies by registry, the name as `auth::canonical` spells it.
    pub registries: BTreeMap<String, RegistryPolicy>,
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
                match fs::read_to_string(default) {
                    Ok(text) => toml::from_str(&text)
                        .with_context(|| format!("parsing {}", default.display()))?,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => FileConfig::default(),
                    // Closed to a user who drives the service: the service reads it as
                    // root, and the command line goes by the defaults.
                    Err(e)
                        if e.kind() == std::io::ErrorKind::PermissionDenied
                            && !nix::unistd::geteuid().is_root() =>
                    {
                        FileConfig::default()
                    }
                    Err(e) => {
                        return Err(e).with_context(|| format!("reading {}", default.display()))
                    }
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

    /// What the hooks that clean up after a machine go by when the file no longer loads:
    /// each of its settings that is still valid, the directories above all, which say
    /// where the machine's record and tree are, and the defaults for the rest.
    pub fn load_leniently(
        path: Option<&Path>,
        registry: Option<String>,
        ca_cert: Option<PathBuf>,
    ) -> Self {
        let text = fs::read_to_string(path.unwrap_or_else(|| Path::new(DEFAULT_CONFIG_PATH)));
        let table: toml::Table = text
            .ok()
            .and_then(|text| toml::from_str(&text).ok())
            .unwrap_or_default();
        // Each key on its own: an unknown one, or a value of the wrong type, takes no
        // other with it.
        let valid: toml::Table = table
            .into_iter()
            .filter(|(key, value)| {
                toml::Table::from_iter([(key.clone(), value.clone())])
                    .try_into::<FileConfig>()
                    .is_ok()
            })
            .collect();
        let file: FileConfig = valid.try_into().unwrap_or_default();
        let dirs = FileConfig {
            machines_dir: file.machines_dir.clone(),
            state_dir: file.state_dir.clone(),
            ..Default::default()
        };
        let store = FileConfig {
            state_dir: file.state_dir.clone(),
            ..Default::default()
        };
        let mut config = [file, dirs, store, FileConfig::default()]
            .into_iter()
            .find_map(|file| Self::merge(file, registry.clone(), ca_cert.clone()).ok())
            .unwrap_or_else(|| {
                Self::merge(FileConfig::default(), None, None).expect("the defaults are valid")
            });
        // Passed on to the hooks it starts, even when it is gone: the default file may
        // name another store.
        config.config_path = path.map(|p| fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()));
        config
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
        let network_pool = file
            .network_pool
            .as_deref()
            .unwrap_or(DEFAULT_NETWORK_POOL)
            .parse()
            .context("network_pool in the configuration")?;
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
        // systemd-nspawn@.service finds a machine's tree by its name in machined's
        // image directories, and nowhere else.
        if !MACHINED_DIRS.iter().any(|d| machines_dir == Path::new(d)) {
            anyhow::bail!(
                "machines_dir {}: systemd-nspawn finds machines in {} only",
                machines_dir.display(),
                MACHINED_DIRS.join(", ")
            );
        }
        // Inside a directory machined lists images from, the store is an image to it (a
        // hidden one if its name starts with a dot), which machinectl clean or remove
        // deletes; holding one, the store's own directories are there.
        if let Some(dir) = MACHINED_SEARCH
            .iter()
            .map(Path::new)
            .find(|dir| state_dir.starts_with(dir) || dir.starts_with(&state_dir))
        {
            anyhow::bail!(
                "state_dir {} and {}, where machined lists images, must not be one inside the other: machinectl clean or remove would delete what nspawn keeps",
                state_dir.display(),
                dir.display()
            );
        }
        // The service reads it from its own working directory, /.
        if let Some(ca) = file.ca_cert.as_ref().filter(|c| !c.is_absolute()) {
            anyhow::bail!(
                "ca_cert in the configuration must be an absolute path: {}",
                ca.display()
            );
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
        let registries = registry_policies(file.registries)?;
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
            network_pool,
            dns,
            registries,
            config_path: None,
        })
    }
}

/// machined's image search path, where systemd-nspawn@.service looks for a machine.
const MACHINED_DIRS: [&str; 5] = [
    "/var/lib/machines",
    "/etc/machines",
    "/run/machines",
    "/usr/local/lib/machines",
    "/usr/lib/machines",
];

/// Every directory machined lists images from: its old /var/lib/container too, which
/// nspawn does not put machines in.
const MACHINED_SEARCH: [&str; 6] = [
    "/var/lib/machines",
    "/etc/machines",
    "/run/machines",
    "/var/lib/container",
    "/usr/local/lib/machines",
    "/usr/lib/machines",
];

/// The `[registries."host"]` tables, checked without reading a file: the command line
/// loads the configuration too, and must not fail on a key only the service may read.
fn registry_policies(
    tables: BTreeMap<String, RegistryFile>,
) -> Result<BTreeMap<String, RegistryPolicy>> {
    let mut policies = BTreeMap::new();
    for (name, table) in tables {
        if name.is_empty() || name.contains('/') || name.contains(char::is_whitespace) {
            anyhow::bail!("registries.{name:?}: a registry is a host, or host:port");
        }
        let registry = crate::auth::canonical(&name);
        let mut keys = table.keys;
        if let Some(key) = table.key {
            keys.insert(0, key);
        }
        let identity = match (table.identity, table.issuer) {
            (Some(identity), Some(issuer)) => Some((identity, issuer)),
            (None, None) => None,
            _ => anyhow::bail!(
                "registries.{name:?}: identity and issuer go together (the certificate's identity and the issuer of its token)"
            ),
        };
        let verify = table.verify.unwrap_or(true);
        if verify && keys.is_empty() && identity.is_none() {
            anyhow::bail!(
                "registries.{name:?}: name a key (key or keys), an identity with its issuer, or set verify = false"
            );
        }
        for path in keys.iter().chain(table.trusted_root.iter()) {
            if !path.is_absolute() {
                anyhow::bail!(
                    "registries.{name:?}: {} must be an absolute path",
                    path.display()
                );
            }
        }
        if policies
            .insert(
                registry.clone(),
                RegistryPolicy {
                    verify,
                    keys,
                    identity,
                    required: table.required.unwrap_or(true),
                    rekor: table.rekor.unwrap_or(true),
                    trusted_root: table.trusted_root,
                },
            )
            .is_some()
        {
            anyhow::bail!("registries.{name:?}: {registry} is configured twice");
        }
    }
    Ok(policies)
}

impl Config {
    /// The default network, the one of `bridge` and `subnet`.
    pub fn default_network(&self) -> crate::bridge::NetSpec {
        crate::bridge::NetSpec {
            name: crate::bridge::DEFAULT_NETWORK.to_string(),
            interface: self.bridge.clone(),
            subnet: self.subnet,
            internal: false,
            created: 0,
            labels: std::collections::BTreeMap::new(),
        }
    }
}

#[cfg(test)]
mod registry_tests {
    use super::*;

    #[test]
    fn a_registry_table_names_what_its_images_must_carry() {
        let file: FileConfig = toml::from_str(
            r#"
[registries."registry.example.com"]
key = "/etc/nspawn/keys/example.pub"
keys = ["/etc/nspawn/keys/other.pub"]
identity = "https://github.com/org/repo/.github/workflows/x.yml@refs/heads/main"
issuer = "https://token.actions.githubusercontent.com"
rekor = false
trusted_root = "/etc/nspawn/trusted_root.json"

[registries."Index.Docker.IO"]
verify = false

[registries."hub.nspawn.test:8443"]
keys = ["/etc/nspawn/e2e.pub"]
required = false
"#,
        )
        .unwrap();
        let c = Config::merge(file, None, None).unwrap();
        let example = &c.registries["registry.example.com"];
        assert_eq!(
            example,
            &RegistryPolicy {
                verify: true,
                keys: vec![
                    PathBuf::from("/etc/nspawn/keys/example.pub"),
                    PathBuf::from("/etc/nspawn/keys/other.pub")
                ],
                identity: Some((
                    "https://github.com/org/repo/.github/workflows/x.yml@refs/heads/main".into(),
                    "https://token.actions.githubusercontent.com".into()
                )),
                required: true,
                rekor: false,
                trusted_root: Some(PathBuf::from("/etc/nspawn/trusted_root.json")),
            }
        );
        assert!(
            !c.registries["docker.io"].verify,
            "the name as auth spells it"
        );
        let test = &c.registries["hub.nspawn.test:8443"];
        assert!(test.verify && !test.required && test.rekor);
        assert!(!c.registries.contains_key("hub.nspawn.org"));
        assert!(Config::merge(FileConfig::default(), None, None)
            .unwrap()
            .registries
            .is_empty());
    }

    #[test]
    fn a_table_that_names_nothing_or_names_it_wrong_is_refused() {
        for bad in [
            "[registries.\"x.example\"]\nkey = \"/k.pub\"\nkeyx = 1\n",
            "[registries.\"x.example\"]\nkey = \"/k.pub\"\nidentity = \"me\"\n",
            "[registries.\"x.example\"]\nkey = \"/k.pub\"\nissuer = \"https://x\"\n",
            "[registries.\"x.example\"]\nrequired = false\n",
            "[registries.\"x.example\"]\nkey = \"keys/k.pub\"\n",
            "[registries.\"x.example\"]\nkey = \"/k.pub\"\ntrusted_root = \"root.json\"\n",
            "[registries.\"x.example/repo\"]\nkey = \"/k.pub\"\n",
            "[registries.\"docker.io\"]\nverify = false\n[registries.\"index.docker.io\"]\nverify = false\n",
        ] {
            let outcome = toml::from_str::<FileConfig>(bad)
                .map_err(|e| anyhow::anyhow!(e))
                .and_then(|file| Config::merge(file, None, None));
            assert!(outcome.is_err(), "{bad}");
        }
        let off: FileConfig =
            toml::from_str("[registries.\"x.example\"]\nverify = false\n").unwrap();
        assert!(!Config::merge(off, None, None).unwrap().registries["x.example"].verify);
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
            "registry = \"file.example\"\nca_cert = \"/a.pem\"\nbackend = \"overlay\"\nmachines_dir = \"/usr/local/lib/machines\"\nstate_dir = \"/srv/s\"\nbridge = \"br-lab\"\nsubnet = \"172.30.5.0/24\"\ndns = [\"10.0.0.53\"]\n",
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
        assert_eq!(c.machines_dir, PathBuf::from("/usr/local/lib/machines"));
        assert_eq!(c.state_dir, PathBuf::from("/srv/s"));
    }

    #[test]
    fn directories_the_rest_cannot_work_with_are_refused() {
        let merged = |text: &str| Config::merge(toml::from_str(text).unwrap(), None, None);
        let err = merged("machines_dir = \"/srv/m\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("finds machines in"), "{err}");
        // Wherever machined looks for images, whichever machines_dir nspawn uses.
        for text in [
            "state_dir = \"/var/lib/machines/.nspawn\"\n",
            "state_dir = \"/etc/machines/.nspawn\"\n",
            "state_dir = \"/run/machines/nspawn\"\n",
            "state_dir = \"/var/lib/container/.nspawn\"\n",
            "machines_dir = \"/etc/machines\"\nstate_dir = \"/var/lib/machines/.nspawn\"\n",
            "state_dir = \"/var/lib\"\n",
            "state_dir = \"/run\"\n",
        ] {
            let err = merged(text).unwrap_err().to_string();
            assert!(err.contains("one inside the other"), "{text}: {err}");
        }
        for text in [
            "state_dir = \"/srv/nspawn\"\n",
            "state_dir = \"/var/lib/nspawn\"\n",
        ] {
            assert!(merged(text).is_ok(), "{text}");
        }
        let err = merged("ca_cert = \"hub-ca.pem\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("absolute"), "{err}");
    }

    #[test]
    fn a_broken_file_still_gives_the_cleanup_its_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nspawn.toml");
        let lenient = |text: &str| {
            fs::write(&path, text).unwrap();
            assert!(Config::load(Some(&path), None, None).is_err(), "{text}");
            Config::load_leniently(Some(&path), None, None)
        };
        // A key this release does not know: the rest of the file stands.
        let c = lenient("state_dir = \"/srv/nspawn\"\nnew_key = 1\nbridge = \"br0\"\n");
        assert_eq!(c.state_dir, Path::new("/srv/nspawn"));
        assert_eq!(c.bridge, "br0");
        // A value merge refuses: the directories stand.
        let c = lenient("state_dir = \"/srv/nspawn\"\ndns = [\"127.0.0.1\"]\nbridge = \"br0\"\n");
        assert_eq!(c.state_dir, Path::new("/srv/nspawn"));
        assert_eq!(c.bridge, DEFAULT_BRIDGE);
        // A machines_dir machined does not search: the state_dir alone.
        let c = lenient("state_dir = \"/srv/nspawn\"\nmachines_dir = \"/srv/m\"\n");
        assert_eq!(c.state_dir, Path::new("/srv/nspawn"));
        assert_eq!(c.machines_dir, Path::new(DEFAULT_MACHINES_DIR));
        // A file that does not parse, or is gone: the defaults, and its name kept for the
        // hooks it starts.
        let c = lenient("state_dir = \"/srv/nspawn\n");
        assert_eq!(c.state_dir, Path::new(DEFAULT_STATE_DIR));
        let gone = dir.path().join("gone.toml");
        let c = Config::load_leniently(Some(&gone), None, None);
        assert_eq!(c.config_path.as_deref(), Some(gone.as_path()));
        assert_eq!(c.state_dir, Path::new(DEFAULT_STATE_DIR));
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
