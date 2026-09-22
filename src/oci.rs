//! What an OCI image config says about running the image, and whether the image boots an
//! init system (a "machine") or runs a single program (an "app", the docker case).

use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use oci_client::config::ConfigFile;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// The image contains systemd (or another init) and is booted with --boot.
    Boot,
    /// The image's entrypoint runs as PID 2 under nspawn's stub init.
    App,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::Boot => "boot",
            Mode::App => "app",
        }
    }
}

/// The parts of the OCI config that matter at run time.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSpec {
    #[serde(default)]
    pub entrypoint: Vec<String>,
    #[serde(default)]
    pub cmd: Vec<String>,
    /// Older records kept entrypoint and cmd joined here; read as the cmd.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub working_dir: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub stop_signal: Option<String>,
}

impl RunSpec {
    pub fn from_config(bytes: &[u8]) -> Result<Self> {
        let file: ConfigFile =
            serde_json::from_slice(bytes).context("parsing the OCI image config")?;
        let Some(config) = file.config else {
            return Ok(RunSpec::default());
        };
        Ok(RunSpec {
            entrypoint: config.entrypoint.unwrap_or_default(),
            cmd: config.cmd.unwrap_or_default(),
            command: Vec::new(),
            env: config.env.unwrap_or_default(),
            working_dir: config.working_dir.filter(|d| !d.is_empty()),
            user: config.user.filter(|u| !u.is_empty()),
            stop_signal: config.stop_signal.filter(|s| !s.is_empty()),
        })
    }

    /// The image's own entrypoint.
    pub fn entrypoint(&self) -> &[String] {
        &self.entrypoint
    }

    /// The image's own cmd (older records: the joined command).
    pub fn cmd(&self) -> &[String] {
        if self.entrypoint.is_empty() && self.cmd.is_empty() {
            &self.command
        } else {
            &self.cmd
        }
    }

    /// Entrypoint followed by cmd: what runs without overrides.
    pub fn argv(&self) -> Vec<String> {
        let mut argv = self.entrypoint().to_vec();
        argv.extend_from_slice(self.cmd());
        argv
    }
}

const INIT_PATHS: [&str; 4] = [
    "usr/lib/systemd/systemd",
    "lib/systemd/systemd",
    "sbin/init",
    "usr/sbin/init",
];

/// Whether the image ships an init program. `trees` are the layers from the base up (or
/// one flat root); the topmost layer that mentions a path decides, and a whiteout there
/// (a character device 0:0) means the path was deleted.
pub fn has_init(trees: &[PathBuf]) -> bool {
    INIT_PATHS.iter().any(|p| {
        trees
            .iter()
            .rev()
            .find_map(|tree| tree.join(p).symlink_metadata().ok())
            .map(|meta| !(meta.file_type().is_char_device() && meta.rdev() == 0))
            .unwrap_or(false)
    })
}

/// Boot when there is an init and the image does not ask for something else to run.
pub fn detect_mode(init_present: bool, command: &[String]) -> Mode {
    if !init_present {
        return Mode::App;
    }
    match command.first() {
        None => Mode::Boot,
        Some(argv0) => {
            let base = Path::new(argv0)
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or(argv0);
            if base == "init" || base == "systemd" {
                Mode::Boot
            } else {
                Mode::App
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_run_spec_from_config() {
        let json = br#"{"architecture":"amd64","os":"linux","rootfs":{"type":"layers","diff_ids":[]},
            "config":{"Entrypoint":["/docker-entrypoint.sh"],"Cmd":["nginx","-g","daemon off;"],
            "Env":["PATH=/usr/bin","NGINX_VERSION=1.27"],"WorkingDir":"/srv","User":"nginx","StopSignal":"SIGQUIT"}}"#;
        let spec = RunSpec::from_config(json).unwrap();
        assert_eq!(spec.entrypoint(), ["/docker-entrypoint.sh"]);
        assert_eq!(spec.cmd(), ["nginx", "-g", "daemon off;"]);
        assert_eq!(
            spec.argv(),
            vec!["/docker-entrypoint.sh", "nginx", "-g", "daemon off;"]
        );
        let legacy: RunSpec = serde_json::from_str(r#"{"command":["sh","-c","x"]}"#).unwrap();
        assert!(legacy.entrypoint().is_empty());
        assert_eq!(legacy.cmd(), ["sh", "-c", "x"]);
        assert_eq!(legacy.argv(), ["sh", "-c", "x"]);
        assert_eq!(spec.env, vec!["PATH=/usr/bin", "NGINX_VERSION=1.27"]);
        assert_eq!(spec.working_dir.as_deref(), Some("/srv"));
        assert_eq!(spec.user.as_deref(), Some("nginx"));
        assert_eq!(spec.stop_signal.as_deref(), Some("SIGQUIT"));

        let minimal =
            br#"{"architecture":"amd64","os":"linux","rootfs":{"type":"layers","diff_ids":[]}}"#;
        assert_eq!(RunSpec::from_config(minimal).unwrap(), RunSpec::default());
        assert!(RunSpec::from_config(b"nope").is_err());
    }

    #[test]
    fn mode_detection() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(detect_mode(true, &s(&["/sbin/init"])), Mode::Boot);
        assert_eq!(
            detect_mode(true, &s(&["/usr/lib/systemd/systemd"])),
            Mode::Boot
        );
        assert_eq!(detect_mode(true, &[]), Mode::Boot);
        assert_eq!(
            detect_mode(true, &s(&["nginx", "-g", "daemon off;"])),
            Mode::App
        );
        assert_eq!(detect_mode(false, &s(&["/sbin/init"])), Mode::App);
        assert_eq!(detect_mode(false, &[]), Mode::App);
    }

    #[test]
    fn init_detection_in_trees() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("base");
        let top = tmp.path().join("top");
        std::fs::create_dir_all(top.join("usr/lib/systemd")).unwrap();
        std::fs::create_dir_all(&base).unwrap();
        assert!(!has_init(&[base.clone(), top.clone()]));
        std::os::unix::fs::symlink("systemd", top.join("usr/lib/systemd/systemd")).unwrap();
        assert!(has_init(&[base, top]));
    }
}
