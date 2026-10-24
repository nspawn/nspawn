//! How a RUN step runs: systemd-nspawn over the step's root, without a user namespace
//! (what it writes keeps its real owners), with the capabilities and system call filter
//! of an app machine, the program as PID 1; and what the run leaves in the root that is
//! systemd-nspawn's rather than the step's.

use std::collections::BTreeMap;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Where a RUN step's network comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Network {
    /// A namespace on the default bridge, prepared for the build.
    Namespace(String),
    None,
    Host,
}

/// A mount of RUN --mount, ready for the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mount {
    Bind {
        source: PathBuf,
        target: String,
        read_only: bool,
    },
    Tmpfs {
        target: String,
        options: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// The step's merged root.
    pub root: PathBuf,
    /// Names the scope systemd-nspawn makes for the payload.
    pub machine: String,
    pub argv: Vec<String>,
    /// KEY=VALUE, the image's first, later ones winning.
    pub env: Vec<String>,
    /// Variables whose values (secrets) systemd-nspawn takes from its own environment,
    /// out of its command line, which anybody on the host can read.
    pub secret_env: Vec<(String, Vec<u8>)>,
    pub workdir: String,
    /// A user of the root, by name or number; None for root.
    pub user: Option<String>,
    pub network: Network,
    /// Bound read-only at /etc/hosts and /etc/resolv.conf.
    pub hosts: Option<PathBuf>,
    pub resolv: Option<PathBuf>,
    pub mounts: Vec<Mount>,
    pub shm_size: Option<u64>,
    /// Resource name (nofile, ...) to (soft, hard).
    pub ulimits: BTreeMap<String, (u64, u64)>,
}

/// The capabilities an app machine without a user namespace drops of systemd-nspawn's
/// set, and its system call filter, as command line options.
fn confinement() -> Vec<String> {
    let tuning = crate::tuning::Tuning::default();
    tuning
        .exec_lines(true)
        .into_iter()
        .filter_map(|line| {
            if let Some(caps) = line.strip_prefix("DropCapability=") {
                Some(format!("--drop-capability={}", caps.replace(' ', ",")))
            } else {
                line.strip_prefix("SystemCallFilter=")
                    .map(|filter| format!("--system-call-filter={filter}"))
            }
        })
        .collect()
}

/// The systemd-nspawn command line of a RUN step.
pub fn argv(step: &Step) -> Vec<String> {
    let mut out = vec![
        "systemd-nspawn".to_string(),
        "--quiet".to_string(),
        "--register=no".to_string(),
        "--settings=no".to_string(),
        format!("--machine={}", step.machine),
        // The executor's hostname, which the step's /etc/hosts gives 127.0.0.1.
        "--hostname=buildkitsandbox".to_string(),
        format!("--directory={}", step.root.display()),
        "--private-users=no".to_string(),
        "--console=pipe".to_string(),
        "--link-journal=no".to_string(),
        "--timezone=off".to_string(),
        "--resolv-conf=off".to_string(),
        format!("--chdir={}", step.workdir),
    ];
    out.extend(confinement());
    // An app machine may reboot itself, a step not: reboot(2) fails, as under
    // containerd's default capabilities, instead of starting it again or ending it.
    out.push("--drop-capability=CAP_SYS_BOOT".to_string());
    match &step.network {
        Network::Namespace(name) => out.push(format!(
            "--network-namespace-path={}",
            crate::bridge::netns_path(name)
        )),
        Network::None => out.push("--private-network".to_string()),
        Network::Host => {}
    }
    if let Some(user) = &step.user {
        out.push(format!("--user={user}"));
    }
    // systemd-nspawn's notify socket is no step's business, as it is no app's: empty,
    // which its clients take for none, unless the step's own variables say otherwise.
    out.push("--setenv=NOTIFY_SOCKET=".to_string());
    for var in &step.env {
        out.push(format!("--setenv={var}"));
    }
    for (var, _) in &step.secret_env {
        out.push(format!("--setenv={var}"));
    }
    for (file, target) in [
        (&step.hosts, "/etc/hosts"),
        (&step.resolv, "/etc/resolv.conf"),
    ] {
        if let Some(file) = file {
            out.push(format!("--bind-ro={}:{target}", file.display()));
        }
    }
    // A context file or a target may have a colon, where systemd-nspawn splits.
    let path = crate::settings::bind_path;
    for mount in &step.mounts {
        match mount {
            Mount::Bind {
                source,
                target,
                read_only,
            } => out.push(format!(
                "--{}={}:{}",
                if *read_only { "bind-ro" } else { "bind" },
                path(&source.to_string_lossy()),
                path(target)
            )),
            Mount::Tmpfs { target, options } => out.push(if options.is_empty() {
                format!("--tmpfs={}", path(target))
            } else {
                format!("--tmpfs={}:{options}", path(target))
            }),
        }
    }
    if let Some(size) = step.shm_size {
        out.push(format!("--tmpfs=/dev/shm:size={size}"));
    }
    for (name, (soft, hard)) in &step.ulimits {
        let v = |n: &u64| {
            if *n == u64::MAX {
                "infinity".to_string()
            } else {
                n.to_string()
            }
        };
        out.push(format!(
            "--rlimit=RLIMIT_{}={}:{}",
            name.to_ascii_uppercase(),
            v(soft),
            v(hard)
        ));
    }
    out.push("--".to_string());
    out.extend(step.argv.iter().cloned());
    out
}

/// What systemd-nspawn makes in a root to mount on: removed after the run when the step
/// did not make it, its parent's times put back, as the reference cleans the stubs of
/// its mounts. /proc, /sys and /dev stay, as a runtime's own mounts leave them.
pub const STUBS: [&str; 10] = [
    "/run",
    "/etc/hosts",
    "/etc/resolv.conf",
    "/etc/machine-id",
    "/etc/localtime",
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib64",
];

/// The mount points a runtime makes itself and leaves in the layer: what systemd-nspawn
/// makes of them gets their mode.
const KEPT: [&str; 3] = ["/proc", "/sys", "/dev"];

/// A directory's access and modification times, seconds and nanoseconds.
type Times = (i64, i64, i64, i64);

/// The stubs a run may leave, looked at before it.
#[derive(Debug, Default)]
pub struct Stubs {
    root: PathBuf,
    /// Paths missing before the run, inside the root and on the host, with their
    /// parent's times then.
    missing: Vec<(String, PathBuf, Option<Times>)>,
    /// Of KEPT, the ones missing before the run.
    made: Vec<(String, PathBuf)>,
}

impl Stubs {
    /// What of `paths` (inside the root) is missing now. `extra` are the targets of the
    /// step's own mounts.
    pub fn before(root: &Path, extra: &[String]) -> Stubs {
        let mut missing = Vec::new();
        for path in STUBS
            .iter()
            .map(|s| s.to_string())
            .chain(extra.iter().cloned())
        {
            // The path and, where its directories are missing too, each of them from the
            // first one missing: systemd-nspawn makes them all for the mount, and the
            // executor takes them all away after.
            let mut chain = Vec::new();
            let mut wanted = Path::new(path.trim_start_matches('/'));
            let host = loop {
                if let Ok(host) = crate::backend::resolve_in_root(root, &wanted.to_string_lossy()) {
                    break Some(host);
                }
                chain.push(wanted.to_path_buf());
                match wanted.parent() {
                    Some(parent) if !parent.as_os_str().is_empty() => wanted = parent,
                    _ => break None,
                }
            };
            let Some(mut host) = host else {
                continue;
            };
            let mut first = true;
            if !chain.is_empty() {
                // The deepest prefix that resolves is there; what comes below it is made.
                if host.symlink_metadata().is_err() {
                    chain.push(wanted.to_path_buf());
                } else {
                    first = false;
                }
            } else if host.symlink_metadata().is_ok() {
                continue;
            } else {
                chain.push(wanted.to_path_buf());
            }
            chain.reverse();
            let base = if first {
                host.parent().map(Path::to_path_buf).unwrap_or_default()
            } else {
                host.clone()
            };
            let top = if first { wanted.parent() } else { Some(wanted) };
            for (i, inside) in chain.iter().enumerate() {
                let below = top.map_or(inside.as_path(), |t| {
                    inside.strip_prefix(t).unwrap_or(inside)
                });
                host = base.join(below);
                // The times of the parent that was there go back once all of it is gone.
                let times = (i == 0)
                    .then(|| host.parent().and_then(|p| p.metadata().ok()))
                    .flatten()
                    .map(|m| (m.atime(), m.atime_nsec(), m.mtime(), m.mtime_nsec()));
                missing.push((format!("/{}", inside.display()), host.clone(), times));
            }
        }
        let made = KEPT
            .iter()
            .filter_map(|p| {
                Some((
                    p.to_string(),
                    crate::backend::resolve_in_root(root, p).ok()?,
                ))
            })
            .filter(|(_, p)| p.symlink_metadata().is_err())
            .collect();
        Stubs {
            root: root.to_path_buf(),
            missing,
            made,
        }
    }

    /// Whether `host` is still where `inside` leads in the root: the run may have made a
    /// directory on the way a link, which would take a removal out of the root.
    fn still(&self, inside: &str, host: &Path) -> bool {
        crate::backend::chase(&self.root, Path::new(inside.trim_start_matches('/')))
            .is_some_and(|p| p == host)
    }

    /// Removes what is there now of what was missing, when it is an empty file or an
    /// empty directory, and puts the parents' times back.
    pub fn clean(&self) {
        for (inside, path) in &self.made {
            if self.still(inside, path) && path.symlink_metadata().is_ok_and(|m| m.is_dir()) {
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
            }
        }
        for (inside, path, times) in self.missing.iter().rev() {
            if !self.still(inside, path) {
                continue;
            }
            let Ok(meta) = path.symlink_metadata() else {
                continue;
            };
            // A link into /usr where the image had nothing is systemd-nspawn's.
            let nspawn_link = meta.file_type().is_symlink()
                && std::fs::read_link(path).is_ok_and(|t| t.starts_with("usr/"));
            let removed = if meta.is_dir() {
                std::fs::remove_dir(path).is_ok()
            } else if (meta.is_file() && meta.len() == 0) || nspawn_link {
                std::fs::remove_file(path).is_ok()
            } else {
                false
            };
            if let (true, Some((at, atn, mt, mtn)), Some(parent)) = (removed, times, path.parent())
            {
                let ts = |s: i64, n: i64| nix::sys::time::TimeSpec::new(s, n);
                let _ = nix::sys::stat::utimensat(
                    nix::fcntl::AT_FDCWD,
                    parent,
                    &ts(*at, *atn),
                    &ts(*mt, *mtn),
                    nix::sys::stat::UtimensatFlags::NoFollowSymlink,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn mount_points_and_the_directories_made_for_them_go_after_the_step() {
        use std::os::unix::fs::MetadataExt;
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("srv")).unwrap();
        let srv_mtime = std::fs::metadata(root.path().join("srv")).unwrap().mtime();
        let stubs = Stubs::before(
            root.path(),
            &["/cache/go/mod".to_string(), "/srv/x/y".to_string()],
        );
        // What systemd-nspawn makes for the mounts, and what the step writes itself.
        std::fs::create_dir_all(root.path().join("cache/go/mod")).unwrap();
        std::fs::create_dir_all(root.path().join("srv/x/y")).unwrap();
        std::fs::create_dir_all(root.path().join("etc")).unwrap();
        std::fs::write(root.path().join("etc/hosts"), "").unwrap();
        std::fs::write(root.path().join("etc/kept"), "written by the step").unwrap();
        stubs.clean();
        assert!(
            !root.path().join("cache").exists(),
            "every directory made for the mount goes"
        );
        assert!(!root.path().join("srv/x").exists());
        assert!(root.path().join("srv").is_dir());
        assert_eq!(
            std::fs::metadata(root.path().join("srv")).unwrap().mtime(),
            srv_mtime
        );
        // A directory the step wrote in stays, the stub in it does not.
        assert!(root.path().join("etc/kept").exists());
        assert!(!root.path().join("etc/hosts").exists());
    }

    use super::*;

    #[test]
    fn the_command_line_of_a_step() {
        let step = Step {
            root: "/b/s/merged".into(),
            machine: "nspawn-build-1-2".into(),
            argv: vec!["/bin/sh".into(), "-c".into(), "echo hi".into()],
            env: vec!["PATH=/bin".into(), "A=b c".into()],
            secret_env: vec![("TOKEN".into(), b"s3cret".to_vec())],
            workdir: "/app".into(),
            user: Some("1000:1000".into()),
            network: Network::None,
            hosts: Some("/b/hosts".into()),
            resolv: None,
            mounts: vec![
                Mount::Bind {
                    source: "/c/x".into(),
                    target: "/cache".into(),
                    read_only: false,
                },
                Mount::Tmpfs {
                    target: "/scratch".into(),
                    options: "size=1m".into(),
                },
                Mount::Bind {
                    source: "/b/context/a:b".into(),
                    target: "/x:rbind".into(),
                    read_only: true,
                },
            ],
            shm_size: Some(1 << 20),
            ulimits: [("nofile".to_string(), (64, 128))].into(),
        };
        let argv = argv(&step);
        for wanted in [
            "--directory=/b/s/merged",
            "--private-users=no",
            "--private-network",
            "--user=1000:1000",
            "--setenv=A=b c",
            "--chdir=/app",
            "--bind-ro=/b/hosts:/etc/hosts",
            "--bind=/c/x:/cache",
            "--tmpfs=/scratch:size=1m",
            "--bind-ro=/b/context/a\\:b:/x\\:rbind",
            "--tmpfs=/dev/shm:size=1048576",
            "--rlimit=RLIMIT_NOFILE=64:128",
            "--system-call-filter=~kexec_load kexec_file_load",
            "--hostname=buildkitsandbox",
            "--drop-capability=CAP_SYS_BOOT",
        ] {
            assert!(argv.iter().any(|a| a == wanted), "{wanted} in {argv:?}");
        }
        assert!(argv.iter().any(|a| a.starts_with("--drop-capability=")));
        assert!(argv.iter().any(|a| a == "--setenv=TOKEN"));
        assert!(
            !argv.iter().any(|a| a.contains("s3cret")),
            "a secret in the command line"
        );
        let dash = argv.iter().position(|a| a == "--").unwrap();
        assert_eq!(argv[dash + 1..], ["/bin/sh", "-c", "echo hi"]);
    }

    #[test]
    fn stubs_go_and_their_parents_keep_their_times() {
        let root = tempfile::tempdir().unwrap();
        let etc = root.path().join("etc");
        std::fs::create_dir(&etc).unwrap();
        std::fs::write(etc.join("resolv.conf"), "kept").unwrap();
        let old = nix::sys::time::TimeSpec::new(1_000_000, 0);
        nix::sys::stat::utimensat(
            nix::fcntl::AT_FDCWD,
            &etc,
            &old,
            &old,
            nix::sys::stat::UtimensatFlags::NoFollowSymlink,
        )
        .unwrap();
        let stubs = Stubs::before(root.path(), &["/cache".to_string()]);
        // The run: stubs made, one of them written to, the step's own mount point.
        std::fs::write(etc.join("hosts"), "").unwrap();
        std::fs::create_dir(root.path().join("run")).unwrap();
        std::fs::create_dir(root.path().join("usr")).unwrap();
        std::fs::write(root.path().join("usr/made"), "x").unwrap();
        std::fs::create_dir(root.path().join("cache")).unwrap();
        stubs.clean();
        assert!(!etc.join("hosts").exists());
        assert!(!root.path().join("run").exists());
        assert!(!root.path().join("cache").exists());
        assert!(
            root.path().join("usr/made").exists(),
            "what the step made stays"
        );
        assert_eq!(
            std::fs::read_to_string(etc.join("resolv.conf")).unwrap(),
            "kept"
        );
        assert_eq!(etc.metadata().unwrap().mtime(), 1_000_000);
    }

    #[test]
    fn a_stub_behind_a_link_the_run_made_is_left_alone() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("etc")).unwrap();
        let stubs = Stubs::before(root.path(), &[]);
        // The run swaps /etc for a link to a directory of the host with an empty
        // hosts file in it.
        std::fs::write(outside.path().join("hosts"), "").unwrap();
        std::fs::remove_dir(root.path().join("etc")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("etc")).unwrap();
        stubs.clean();
        assert!(
            outside.path().join("hosts").exists(),
            "a file outside the root was removed"
        );
    }
}
