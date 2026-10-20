//! What an OCI image config says about running the image, and whether the image boots an
//! init system (a "machine") or runs a single program (an "app").

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
    /// The image's entrypoint runs as a single program: under the stub init of
    /// systemd-nspawn, or as PID 1 when the image or --init=false asks for it.
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
    /// Run the image's entrypoint as a single program (docker-style images).
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
    /// VOLUME of a Containerfile: paths the image expects a mount over.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<String>,
    /// EXPOSE of a Containerfile, PORT/PROTOCOL: what the program listens on, which tools
    /// reading the Docker API take as its port when nothing else names one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exposed_ports: Vec<String>,
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
        let mut volumes: Vec<String> = raw
            .get("config")
            .and_then(|c| c.get("Volumes"))
            .and_then(serde_json::Value::as_object)
            .map(|v| v.keys().cloned().collect())
            .unwrap_or_default();
        volumes.sort();
        let spec = RunSpec {
            healthcheck: crate::health::Healthcheck::from_config(&raw),
            volumes,
            exposed_ports: exposed_ports(&raw),
            entrypoint: config.entrypoint.unwrap_or_default(),
            cmd: config.cmd.unwrap_or_default(),
            command: Vec::new(),
            env: config.env.unwrap_or_default(),
            working_dir: config.working_dir.filter(|d| !d.is_empty()),
            user: config.user.filter(|u| !u.is_empty()),
            stop_signal: config.stop_signal.filter(|s| !s.is_empty()),
            labels: config.labels.unwrap_or_default().into_iter().collect(),
        };
        spec.check_plain()?;
        Ok(spec)
    }

    /// Its exposed ports, read from the config blob of the image's manifest: a record
    /// written before they were kept has none of its own.
    pub fn exposed_ports_of(config_blob: &[u8]) -> Vec<String> {
        serde_json::from_slice(config_blob)
            .map(|raw| exposed_ports(&raw))
            .unwrap_or_default()
    }

    /// Refuses a config whose values carry control characters: they go into the
    /// machine's settings file, which is line based, and an image does not get to
    /// write directives there (a newline in WorkingDir would be one). A tab in what
    /// the file quotes (the program's arguments, the environment) is a character like
    /// any other.
    pub fn check_plain(&self) -> Result<()> {
        let fields = [
            ("Entrypoint", &self.entrypoint[..]),
            ("Cmd", &self.cmd[..]),
            ("Env", &self.env[..]),
        ];
        for (field, values) in fields {
            for value in values {
                plain(field, value, true)?;
            }
        }
        for (field, value) in [
            ("WorkingDir", &self.working_dir),
            ("User", &self.user),
            ("StopSignal", &self.stop_signal),
        ] {
            if let Some(value) = value {
                plain(field, value, false)?;
            }
        }
        Ok(())
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

fn plain(field: &str, value: &str, quoted: bool) -> Result<()> {
    if value
        .chars()
        .any(|c| c.is_control() && !(quoted && c == '\t'))
    {
        anyhow::bail!(
            "the image config's {field} holds a control character ({value:?}); refusing the image"
        );
    }
    Ok(())
}

/// The keys of the config's ExposedPorts as PORT/PROTOCOL, sorted by port; anything
/// else is left out.
fn exposed_ports(raw: &serde_json::Value) -> Vec<String> {
    let mut ports: Vec<(u16, String)> = raw
        .get("config")
        .and_then(|c| c.get("ExposedPorts"))
        .and_then(serde_json::Value::as_object)
        .map(|ports| {
            ports
                .keys()
                .filter_map(|key| {
                    let (port, protocol) = key.split_once('/').unwrap_or((key, "tcp"));
                    let port: u16 = port.parse().ok().filter(|p| *p > 0)?;
                    let protocol = protocol.to_ascii_lowercase();
                    matches!(protocol.as_str(), "tcp" | "udp" | "sctp")
                        .then(|| (port, format!("{port}/{protocol}")))
                })
                .collect()
        })
        .unwrap_or_default();
    ports.sort();
    ports.dedup();
    ports.into_iter().map(|(_, p)| p).collect()
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
    INIT_PATHS.iter().any(|p| present(trees, p))
}

/// What s6-overlay installs (v3, then v2): its /init refuses to run unless it is PID 1.
const PID1_MARKERS: [&str; 4] = [
    "package/admin/s6-overlay",
    "command/s6-overlay-suexec",
    "etc/s6-overlay",
    "etc/s6/init/init-stage1",
];

/// Whether the image's program must be PID 1 of the machine rather than the stub init's
/// child, as its supervisor insists on reaping and shutting down the machine itself.
pub fn needs_pid1(trees: &[PathBuf]) -> bool {
    PID1_MARKERS.iter().any(|p| present(trees, p))
}

/// Whether `relative` exists in the merged view of `trees`, base first.
fn present(trees: &[PathBuf], relative: &str) -> bool {
    for tree in trees.iter().rev() {
        match lookup(tree, Path::new(relative)) {
            Lookup::Present => return true,
            Lookup::Hidden => return false,
            Lookup::Absent => {}
        }
    }
    false
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

/// Boot when there is an init and the image does not ask for something else to run: a
/// command naming the init where systemd-nspawn looks for it, or by its bare name, or
/// systemd wherever it is. Any other program, an `/init` of the image's own among them,
/// runs as an app.
pub fn detect_mode(init_present: bool, command: &[String]) -> Mode {
    if !init_present {
        return Mode::App;
    }
    match command.first() {
        None => Mode::Boot,
        Some(argv0) => {
            let init = match argv0.strip_prefix('/') {
                Some(path) => {
                    INIT_PATHS.contains(&path) || path.rsplit('/').next() == Some("systemd")
                }
                None => argv0 == "init" || argv0 == "systemd",
            };
            if init {
                Mode::Boot
            } else {
                Mode::App
            }
        }
    }
}

/// This host's architecture as image platforms name it (GOARCH).
pub fn host_architecture() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        "powerpc64" => "ppc64le",
        "loongarch64" => "loong64",
        other => other,
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
            "Healthcheck":{"Test":["CMD-SHELL","curl -f http://localhost/ || exit 1"],"Interval":30000000000,"Timeout":3000000000,"Retries":3},
            "Volumes":{"/var/lib/data":{},"/cache":{}},
            "ExposedPorts":{"443/tcp":{},"80":{},"53/UDP":{},"x/tcp":{},"0/tcp":{},"9/icmp":{}}}}"#;
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
        assert_eq!(spec.volumes, ["/cache", "/var/lib/data"]);
        // A bare port is tcp; ports that are not ones are left out.
        assert_eq!(spec.exposed_ports, ["53/udp", "80/tcp", "443/tcp"]);
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
    fn a_config_that_would_write_settings_directives_is_refused() {
        // A newline in any of these would end the line of the settings file and start a
        // directive of the image's choosing.
        for (field, value) in [
            ("WorkingDir", r#""/srv\n[Files]\nBind=/:/host""#),
            ("User", r#""nginx\nPrivateUsers=no""#),
            ("StopSignal", r#""SIGTERM\nCapability=all""#),
            ("Entrypoint", r#"["/bin/sh\nBind=/:/host"]"#),
            ("Cmd", r#"["a", "b\n"]"#),
            ("Env", r#"["A=1\nBind=/:/x"]"#),
        ] {
            let json = format!(
                r#"{{"architecture":"amd64","os":"linux","rootfs":{{"type":"layers","diff_ids":[]}},"config":{{"{field}":{value}}}}}"#
            );
            let err = RunSpec::from_config(json.as_bytes()).unwrap_err();
            assert!(
                err.to_string().contains(field) && err.to_string().contains("control"),
                "{field}: {err}"
            );
        }
        // A tab in what the settings file quotes is kept, as postgres:18-alpine has one
        // in its Env; elsewhere it is refused like the rest.
        let tab = br#"{"architecture":"amd64","os":"linux","rootfs":{"type":"layers","diff_ids":[]},"config":{"Cmd":["a\tb"],"Env":["DEPS=llvm21-dev \tclang21"]}}"#;
        let spec = RunSpec::from_config(tab).unwrap();
        assert_eq!(spec.env, ["DEPS=llvm21-dev \tclang21"]);
        let tab = br#"{"architecture":"amd64","os":"linux","rootfs":{"type":"layers","diff_ids":[]},"config":{"WorkingDir":"/a\tb"}}"#;
        assert!(RunSpec::from_config(tab).is_err());
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
        for init in [
            "/usr/sbin/init",
            "/lib/systemd/systemd",
            "init",
            "systemd",
            "/bin/systemd",
            "/usr/bin/systemd",
        ] {
            assert_eq!(detect_mode(true, &s(&[init])), Mode::Boot, "{init}");
        }
        // s6-overlay's /init, in an Alpine base whose busybox gives /sbin/init too, is the
        // image's program, not systemd-nspawn's init to boot.
        for program in ["/init", "/bin/init", "/usr/bin/tini", "./init", "sbin/init"] {
            assert_eq!(detect_mode(true, &s(&[program])), Mode::App, "{program}");
        }
    }

    #[test]
    fn s6_overlay_wants_pid1() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("base");
        let top = tmp.path().join("top");
        std::fs::create_dir_all(base.join("package/admin/s6-overlay-3.2.0.2")).unwrap();
        std::fs::create_dir_all(&top).unwrap();
        assert!(!needs_pid1(&[base.clone(), top.clone()]));
        std::os::unix::fs::symlink("s6-overlay-3.2.0.2", base.join("package/admin/s6-overlay"))
            .unwrap();
        assert!(needs_pid1(&[base.clone(), top.clone()]));
        // v2 kept its stages under /etc/s6/init; a supervisor tree of /etc/s6 alone (as
        // gitea's) runs anywhere.
        let v2 = tmp.path().join("v2");
        std::fs::create_dir_all(v2.join("etc/s6/gitea")).unwrap();
        assert!(!needs_pid1(std::slice::from_ref(&v2)));
        std::fs::create_dir_all(v2.join("etc/s6/init")).unwrap();
        std::fs::write(v2.join("etc/s6/init/init-stage1"), "").unwrap();
        assert!(needs_pid1(&[v2]));
        // A layer that removes the overlay's directories takes the requirement away.
        let removed = tmp.path().join("removed");
        std::fs::create_dir_all(removed.join("package")).unwrap();
        std::fs::write(removed.join("package/admin"), "").unwrap();
        assert!(!needs_pid1(&[base, removed]));
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
        assert!(has_init(&[base, top.clone()]));
        // A base layer with init and a top layer that says nothing: still an init.
        let quiet = tmp.path().join("quiet");
        std::fs::create_dir_all(&quiet).unwrap();
        assert!(has_init(&[top.clone(), quiet]));
        // A top layer that turns the directory into a file hides the lower init.
        let flattened = tmp.path().join("flattened");
        std::fs::create_dir_all(flattened.join("usr/lib")).unwrap();
        std::fs::write(flattened.join("usr/lib/systemd"), "not a dir").unwrap();
        assert!(!has_init(&[top, flattened]));
        // An absolute symlink must not make the host's own init count.
        let hostlink = tmp.path().join("hostlink");
        std::fs::create_dir_all(&hostlink).unwrap();
        std::os::unix::fs::symlink("/usr/lib", hostlink.join("lib")).unwrap();
        std::os::unix::fs::symlink("/usr/sbin", hostlink.join("sbin")).unwrap();
        std::os::unix::fs::symlink("/usr", hostlink.join("usr")).unwrap();
        assert!(!has_init(&[hostlink]));
    }
}
