//! What an OCI image config says about running the image, and whether the image boots an
//! init system (a "machine") or runs a single program (an "app", the docker case).

use std::collections::BTreeMap;
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

/// What the user may ask for; `Auto` leaves the detection to the image's contents.
#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum ModeChoice {
    /// Boot when the image has an init system and no other entrypoint, app otherwise.
    Auto,
    /// Boot the image's init system (systemd machines).
    Boot,
    /// Run the image's entrypoint under nspawn's stub init (docker-style images).
    App,
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
    /// The image's own labels (LABEL in a Containerfile, OciLabels= in mkosi).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    /// HEALTHCHECK of a Containerfile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub healthcheck: Option<crate::health::Healthcheck>,
}

impl RunSpec {
    pub fn from_config(bytes: &[u8]) -> Result<Self> {
        let file: ConfigFile =
            serde_json::from_slice(bytes).context("parsing the OCI image config")?;
        let Some(config) = file.config else {
            return Ok(RunSpec::default());
        };
        // A docker extension the typed config leaves out.
        let raw: serde_json::Value =
            serde_json::from_slice(bytes).context("parsing the OCI image config")?;
        Ok(RunSpec {
            healthcheck: crate::health::Healthcheck::from_config(&raw),
            entrypoint: config.entrypoint.unwrap_or_default(),
            cmd: config.cmd.unwrap_or_default(),
            command: Vec::new(),
            env: config.env.unwrap_or_default(),
            working_dir: config.working_dir.filter(|d| !d.is_empty()),
            user: config.user.filter(|u| !u.is_empty()),
            stop_signal: config.stop_signal.filter(|s| !s.is_empty()),
            labels: config.labels.unwrap_or_default().into_iter().collect(),
        })
    }

    /// The image's own entrypoint.
    pub fn entrypoint(&self) -> &[String] {
        &self.entrypoint
    }

    /// The image's own cmd, or `command` in a config that has only that.
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
/// one flat root); the topmost layer that says something decides, with overlay's rules:
/// a whiteout (character device 0:0), a file where a directory is expected or an opaque
/// directory hides the lower layers, and a symlink on the way counts as "not this layer"
/// (it may point anywhere, the host included).
pub fn has_init(trees: &[PathBuf]) -> bool {
    INIT_PATHS.iter().any(|p| {
        for tree in trees.iter().rev() {
            match lookup(tree, Path::new(p)) {
                Lookup::Present => return true,
                Lookup::Hidden => return false,
                Lookup::Absent => {}
            }
        }
        false
    })
}

enum Lookup {
    Present,
    Hidden,
    Absent,
}

fn lookup(tree: &Path, relative: &Path) -> Lookup {
    let components: Vec<_> = relative.components().collect();
    let mut path = tree.to_path_buf();
    for (i, component) in components.iter().enumerate() {
        path.push(component);
        let Ok(meta) = path.symlink_metadata() else {
            return Lookup::Absent;
        };
        let kind = meta.file_type();
        if kind.is_char_device() && meta.rdev() == 0 {
            return Lookup::Hidden;
        }
        // The init itself may be a symlink (sbin/init -> ../lib/systemd/systemd); a
        // symlink on the way is another matter.
        if i + 1 == components.len() {
            return Lookup::Present;
        }
        if kind.is_symlink() {
            return Lookup::Absent;
        }
        if !kind.is_dir() {
            return Lookup::Hidden;
        }
        if xattr::get(&path, "trusted.overlay.opaque")
            .ok()
            .flatten()
            .is_some_and(|v| v == b"y")
        {
            // The lower layers' contents of this directory are hidden: only this layer
            // can still provide the rest of the path.
            let rest: PathBuf = components[i + 1..].iter().collect();
            return match lookup(&path, &rest) {
                Lookup::Present => Lookup::Present,
                _ => Lookup::Hidden,
            };
        }
    }
    Lookup::Absent
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
            "Env":["PATH=/usr/bin","NGINX_VERSION=1.27"],"WorkingDir":"/srv","User":"nginx","StopSignal":"SIGQUIT",
            "Labels":{"org.opencontainers.image.title":"nginx","maintainer":"someone"},
            "Healthcheck":{"Test":["CMD-SHELL","curl -f http://localhost/ || exit 1"],"Interval":30000000000,"Timeout":3000000000,"Retries":3}}}"#;
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
        let hc = spec.healthcheck.as_ref().unwrap();
        assert_eq!(hc.test[1], "curl -f http://localhost/ || exit 1");
        assert_eq!(
            (hc.interval, hc.timeout, hc.retries),
            (30_000_000, 3_000_000, 3)
        );
        assert_eq!(
            spec.labels,
            BTreeMap::from([
                ("maintainer".to_string(), "someone".to_string()),
                (
                    "org.opencontainers.image.title".to_string(),
                    "nginx".to_string()
                ),
            ])
        );
        let round: RunSpec = serde_json::from_str(&serde_json::to_string(&spec).unwrap()).unwrap();
        assert_eq!(round, spec, "labels survive the record");

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
        assert!(has_init(&[base.clone(), top.clone()]));
        // A base layer with init and a top layer that says nothing: still an init.
        let quiet = tmp.path().join("quiet");
        std::fs::create_dir_all(&quiet).unwrap();
        assert!(has_init(&[top.clone(), quiet.clone()]));
        // A top layer that turns the directory into a file hides the lower init.
        let flattened = tmp.path().join("flattened");
        std::fs::create_dir_all(flattened.join("usr/lib")).unwrap();
        std::fs::write(flattened.join("usr/lib/systemd"), "not a dir").unwrap();
        assert!(!has_init(&[top.clone(), flattened]));
        // An absolute symlink must not make the host's own init count.
        let hostlink = tmp.path().join("hostlink");
        std::fs::create_dir_all(&hostlink).unwrap();
        std::os::unix::fs::symlink("/usr/lib", hostlink.join("lib")).unwrap();
        std::os::unix::fs::symlink("/usr/sbin", hostlink.join("sbin")).unwrap();
        std::os::unix::fs::symlink("/usr", hostlink.join("usr")).unwrap();
        assert!(!has_init(&[hostlink]));
    }
}
