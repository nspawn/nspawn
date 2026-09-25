//! How a pulled image is laid out on this host: native mstack, overlayfs mount unit or a
//! flat directory.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::store::{extract_layer, Ownership, Store, WhiteoutMode, FOREIGN_UID_BASE};
use crate::systemd::Systemd;
use crate::unitname;

pub const UNIT_DIR: &str = "/etc/systemd/system";
/// Managed user namespaces need systemd-nsresourced (UID range delegation) and
/// systemd-mountfsd (the actual mounts); both are socket activated and shipped disabled.
pub const MANAGED_NS_SOCKETS: [&str; 2] = ["systemd-nsresourced.socket", "systemd-mountfsd.socket"];
const MANAGED_NS_UNIT_FILES: [&str; 2] = [
    "/usr/lib/systemd/system/systemd-nsresourced.socket",
    "/usr/lib/systemd/system/systemd-mountfsd.socket",
];

/// What the user may ask for; `Backend::choose` turns it into a `Backend`.
#[derive(
    Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum, serde::Deserialize, serde::Serialize,
)]
#[serde(rename_all = "lowercase")]
pub enum BackendChoice {
    /// mstack when the host supports it, otherwise an overlay mount, otherwise a flat copy.
    Auto,
    /// Shared layers plus an overlayfs mount unit per machine (any systemd with overlayfs).
    Overlay,
    /// Extract the layers into a plain directory (no sharing, maximum compatibility).
    Flat,
    /// Native systemd.mstack directory (systemd 261 or newer with nsresourced).
    Mstack,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Overlay,
    Flat,
    Mstack,
}

impl Backend {
    /// mstack images are mounted by systemd-mountfsd for a managed user namespace, which
    /// maps the foreign UID range; the other backends keep the image's own IDs.
    pub fn ownership(self) -> Ownership {
        match self {
            Backend::Mstack => Ownership::Foreign,
            Backend::Overlay | Backend::Flat => Ownership::Root,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Backend::Overlay => "overlay",
            Backend::Flat => "flat",
            Backend::Mstack => "mstack",
        }
    }

    pub fn as_choice(self) -> BackendChoice {
        match self {
            Backend::Overlay => BackendChoice::Overlay,
            Backend::Flat => BackendChoice::Flat,
            Backend::Mstack => BackendChoice::Mstack,
        }
    }

    pub async fn choose(choice: BackendChoice, sd: &Systemd) -> Result<Backend> {
        match choice {
            BackendChoice::Overlay => Ok(Backend::Overlay),
            BackendChoice::Flat => Ok(Backend::Flat),
            BackendChoice::Mstack => {
                if !mstack_supported(sd).await {
                    bail!(
                        "mstack images need systemd 261 or newer with systemd-nsresourced running"
                    )
                }
                Ok(Backend::Mstack)
            }
            // mstack is experimental and only ever chosen by name.
            BackendChoice::Auto => {
                if overlay_supported() {
                    Ok(Backend::Overlay)
                } else {
                    Ok(Backend::Flat)
                }
            }
        }
    }
}

/// What every pull, build or create with the mstack backend says.
pub const MSTACK_EXPERIMENTAL: &str = "note: the mstack backend is experimental: it boots the machine in a managed user namespace through systemd-nsresourced and systemd-mountfsd, which are still settling; overlay is the default";

pub fn systemd_major(version: &str) -> Option<u32> {
    version
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .ok()
}

pub fn overlay_supported() -> bool {
    fs::read_to_string("/proc/filesystems")
        .map(|s| {
            s.lines()
                .any(|l| l.split_whitespace().last() == Some("overlay"))
        })
        .unwrap_or(false)
}

/// mstack images boot with managed user namespaces; the sockets are started on demand, so
/// the units only have to be installed.
async fn mstack_supported(sd: &Systemd) -> bool {
    let new_enough = sd
        .version()
        .await
        .ok()
        .and_then(|v| systemd_major(&v))
        .is_some_and(|m| m >= 261);
    new_enough && MANAGED_NS_UNIT_FILES.iter().all(|f| Path::new(f).exists())
}

/// A layer as downloaded: blob on disk plus its descriptor data.
pub struct Layer {
    pub digest: String,
    pub media_type: String,
    pub blob: PathBuf,
}

pub struct Assembler<'a> {
    pub store: &'a Store,
    pub sd: &'a Systemd,
}

impl Assembler<'_> {
    pub fn machine_dir(&self, name: &str) -> PathBuf {
        self.store.machines_dir.join(name)
    }

    pub fn mstack_dir(&self, name: &str) -> PathBuf {
        self.store.machines_dir.join(format!("{name}.mstack"))
    }

    /// Makes the image `name` available to machinectl. Blobs stay in the store so that the
    /// image can be pushed later. Returns the directory trees that make up the root: the
    /// shared layers, or the flat directory.
    pub async fn assemble(
        &self,
        backend: Backend,
        name: &str,
        layers: &[Layer],
    ) -> Result<Vec<PathBuf>> {
        match backend {
            // Extraction is long and synchronous: it runs off the runtime's workers, so
            // that the service keeps answering the bus meanwhile.
            Backend::Flat => {
                let dir = self.machine_dir(name);
                fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
                for layer in layers {
                    let (blob, media_type, digest, target) = (
                        layer.blob.clone(),
                        layer.media_type.clone(),
                        layer.digest.clone(),
                        dir.clone(),
                    );
                    tokio::task::spawn_blocking(move || {
                        extract_layer(
                            &blob,
                            &media_type,
                            &target,
                            WhiteoutMode::Apply,
                            Ownership::Root,
                        )
                        .with_context(|| format!("extracting layer {digest}"))
                    })
                    .await
                    .context("extracting a layer")??;
                }
                Ok(vec![dir])
            }
            Backend::Overlay | Backend::Mstack => {
                let mut dirs = Vec::new();
                for layer in layers {
                    let store = self.store.clone();
                    let (digest, media_type, blob, ownership) = (
                        layer.digest.clone(),
                        layer.media_type.clone(),
                        layer.blob.clone(),
                        backend.ownership(),
                    );
                    dirs.push(
                        tokio::task::spawn_blocking(move || {
                            store.import_layer(&digest, &media_type, &blob, ownership)
                        })
                        .await
                        .context("extracting a layer")??,
                    );
                }
                if backend == Backend::Overlay {
                    self.write_overlay(name, &dirs).await?;
                } else {
                    write_mstack(&self.mstack_dir(name), &dirs)?;
                }
                Ok(dirs)
            }
        }
    }

    async fn write_overlay(&self, name: &str, layer_dirs: &[PathBuf]) -> Result<()> {
        let mountpoint = self.machine_dir(name);
        fs::create_dir_all(&mountpoint)
            .with_context(|| format!("creating {}", mountpoint.display()))?;
        let private = self.store.machines_private_dir().join(name);
        let upper = private.join("upper");
        let work = private.join("work");
        fs::create_dir_all(&upper).with_context(|| format!("creating {}", upper.display()))?;
        fs::create_dir_all(&work).with_context(|| format!("creating {}", work.display()))?;
        // What the machine writes, setuid programs among it, is nobody else's to reach.
        crate::store::restrict(&private, crate::store::PRIVATE)?;
        let mp = mountpoint.to_string_lossy().to_string();
        let unit = unitname::mount_unit_for(&mp);
        let text = overlay_unit_text(name, &mp, layer_dirs, &upper, &work);
        let unit_path = Path::new(UNIT_DIR).join(&unit);
        fs::write(&unit_path, text).with_context(|| format!("writing {}", unit_path.display()))?;
        let dropin_dir = dropin_dir(name);
        fs::create_dir_all(&dropin_dir)
            .with_context(|| format!("creating {}", dropin_dir.display()))?;
        let dropin = dropin_dir.join("nspawn-overlay.conf");
        fs::write(
            &dropin,
            format!("# Generated by nspawn; do not edit.\n[Unit]\nRequiresMountsFor={mp}\n"),
        )
        .with_context(|| format!("writing {}", dropin.display()))?;
        self.sd.reload().await
    }

    /// Removes everything `assemble` created for `name`.
    pub async fn remove(&self, name: &str, backend: BackendChoice) -> Result<()> {
        match backend {
            BackendChoice::Overlay => {
                let mountpoint = self.machine_dir(name);
                let mp = mountpoint.to_string_lossy().to_string();
                let unit = unitname::mount_unit_for(&mp);
                let _ = self.sd.stop_unit(&unit).await;
                if is_mountpoint(&mountpoint)? {
                    bail!(
                        "{} is still mounted; stop the machine first",
                        mountpoint.display()
                    );
                }
                let _ = fs::remove_file(Path::new(UNIT_DIR).join(&unit));
                let _ = fs::remove_dir_all(self.store.machines_private_dir().join(name));
                remove_dir_if_exists(&mountpoint)?;
            }
            BackendChoice::Flat => remove_dir_if_exists(&self.machine_dir(name))?,
            BackendChoice::Mstack => remove_dir_if_exists(&self.mstack_dir(name))?,
            BackendChoice::Auto => bail!("image record for {name} has no concrete backend"),
        }
        // Every backend gets the unit hooks, overlay also a mount dependency.
        remove_dropins(name);
        self.sd.reload().await?;
        crate::settings::remove(name);
        Ok(())
    }

    /// Best-effort removal of whatever any backend may have left for `name` when no
    /// record says which one made it: the overlay path also covers a flat directory
    /// (same mount point) and refuses while something is still mounted there.
    pub async fn remove_leftovers(&self, name: &str) -> Result<()> {
        self.remove(name, BackendChoice::Overlay).await?;
        remove_dir_if_exists(&self.mstack_dir(name))
    }
}

/// Mounts an overlay machine's root ahead of its unit, which RequiresMountsFor= would
/// do itself: `start` runs `prepare` before the unit, and what prepare looks at and
/// makes in the root has to be there by then. Nothing to do once it is mounted.
pub async fn mount_root(sd: &Systemd, store: &Store, name: &str) -> Result<()> {
    let mp = store.machines_dir.join(name).to_string_lossy().to_string();
    sd.start_unit(&unitname::mount_unit_for(&mp)).await
}

/// Where a mount point has to be made before the machine's root is mounted read-only
/// (systemd-nspawn cannot make it then): the root itself for overlay and flat, since
/// the overlay is mounted before the unit's hooks run and a file made behind its back
/// in the upper layer is not seen through it; the writable layer of an mstack tree,
/// which systemd-nspawn merges itself at start.
pub fn mount_point_root(store: &Store, name: &str, backend: BackendChoice) -> Result<PathBuf> {
    let root = store.machines_dir.join(name);
    match backend {
        BackendChoice::Overlay => {
            if !is_mountpoint(&root)? {
                bail!("{} is not mounted", root.display());
            }
            Ok(root)
        }
        BackendChoice::Mstack => Ok(store.machines_dir.join(format!("{name}.mstack")).join("rw")),
        BackendChoice::Flat | BackendChoice::Auto => Ok(root),
    }
}

/// Whether `relative` is in the root systemd-nspawn gives the machine, and `test` holds
/// for what is there (a symlink is looked at, not followed: its target is the image's,
/// not the host's): the assembled root, or any layer of an mstack tree, whose layers
/// systemd-nspawn merges itself at start.
pub fn root_has(
    store: &Store,
    name: &str,
    backend: BackendChoice,
    relative: &str,
    test: impl Fn(&fs::Metadata) -> bool,
) -> bool {
    root_path(store, name, backend, relative)
        .and_then(|path| path.symlink_metadata().ok())
        .is_some_and(|m| test(&m))
}

/// Where `relative` is on the host, for what looks into the machine's root ahead of
/// its start: the assembled root, or the topmost layer of an mstack tree that has it.
pub fn root_path(
    store: &Store,
    name: &str,
    backend: BackendChoice,
    relative: &str,
) -> Option<PathBuf> {
    match backend {
        BackendChoice::Mstack => {
            let dir = store.machines_dir.join(format!("{name}.mstack"));
            let mut layers: Vec<PathBuf> = fs::read_dir(dir)
                .ok()?
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("layer@"))
                })
                .collect();
            layers.sort();
            layers
                .into_iter()
                .rev()
                .map(|layer| layer.join(relative))
                .find(|path| path.symlink_metadata().is_ok())
        }
        BackendChoice::Overlay | BackendChoice::Flat | BackendChoice::Auto => {
            let path = store.machines_dir.join(name).join(relative);
            path.symlink_metadata().is_ok().then_some(path)
        }
    }
}

/// Where `relative` is in the image itself, ahead of any run: the topmost layer that
/// has it, unless a layer above whites it out. The assembled root would show what
/// earlier runs did to it, such as a copy chowned into a picked user namespace. A
/// tree without layers of its own (flat) is looked at as it is.
pub fn image_path(
    store: &Store,
    layers: &[String],
    backend: BackendChoice,
    relative: &str,
) -> Option<PathBuf> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let ownership = match backend {
        BackendChoice::Mstack => Ownership::Foreign,
        BackendChoice::Overlay | BackendChoice::Flat | BackendChoice::Auto => Ownership::Root,
    };
    let dirs: Vec<PathBuf> = layers
        .iter()
        .rev()
        .map(|digest| store.layer_dir(digest, ownership))
        .filter(|dir| dir.is_dir())
        .collect();
    if dirs.is_empty() {
        return None;
    }
    for dir in dirs {
        let path = dir.join(relative);
        let Ok(meta) = path.symlink_metadata() else {
            continue;
        };
        // An overlayfs whiteout: a character device 0:0.
        if meta.file_type().is_char_device() && meta.rdev() == 0 {
            return None;
        }
        return Some(path);
    }
    None
}

/// Whether a mount at `target` inside the machine would land on /run, which is a tmpfs
/// of every machine already and holds what systemd-nspawn keeps there: through the
/// image's own symlink, /var/run say, as much as by name.
pub fn lands_on_run(store: &Store, name: &str, backend: BackendChoice, target: &str) -> bool {
    let link = root_path(store, name, backend, target.trim_start_matches('/'))
        .and_then(|path| fs::read_link(path).ok());
    resolves_to_run(target, link.as_deref())
}

fn resolves_to_run(target: &str, link: Option<&Path>) -> bool {
    let is_run = |path: &Path| path == Path::new("/run") || path.starts_with("/run/");
    if is_run(Path::new(target)) {
        return true;
    }
    let Some(link) = link else {
        return false;
    };
    let resolved = if link.is_absolute() {
        link.to_path_buf()
    } else {
        Path::new(target)
            .parent()
            .unwrap_or(Path::new("/"))
            .join(link)
    };
    // The lexical form: a/../b is b.
    let mut clean = PathBuf::from("/");
    for part in resolved.components() {
        match part {
            std::path::Component::ParentDir => {
                clean.pop();
            }
            std::path::Component::Normal(c) => clean.push(c),
            _ => {}
        }
    }
    is_run(&clean)
}

/// Makes the mount points of a read-only machine ahead of its start, below `root`: a
/// directory, or a file when `dir` is false. What exists already stays as it is.
pub fn ensure_mount_points(root: &Path, targets: &[(String, bool)]) -> Result<()> {
    for (target, dir) in targets {
        if target.split('/').any(|c| c == "..") || !target.starts_with('/') {
            bail!("{target}: a mount point is an absolute path inside the machine, without ..");
        }
        let path = root.join(target.trim_start_matches('/'));
        if path.symlink_metadata().is_ok() {
            continue;
        }
        if *dir {
            fs::create_dir_all(&path).with_context(|| format!("creating {}", path.display()))?;
        } else {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            fs::File::create(&path).with_context(|| format!("creating {}", path.display()))?;
        }
    }
    Ok(())
}

pub fn dropin_dir(name: &str) -> PathBuf {
    Path::new(UNIT_DIR).join(format!("systemd-nspawn@{name}.service.d"))
}

/// Drop-in files nspawn writes for a machine's unit.
pub const DROPINS: [&str; 2] = ["nspawn-overlay.conf", "nspawn-hooks.conf"];

/// Removes nspawn's own drop-ins, and the directory when nothing else is left in it, so
/// that an administrator's drop-ins survive.
pub fn remove_dropins(name: &str) {
    let dir = dropin_dir(name);
    for file in DROPINS {
        let _ = fs::remove_file(dir.join(file));
    }
    let _ = fs::remove_dir(&dir);
}

fn remove_dir_if_exists(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

/// Text of the mount unit. overlayfs lists the topmost layer first in lowerdir=.
///
/// `metacopy=on` matters with user namespaces: no released kernel lets an overlayfs
/// mount be idmapped, so nspawn shifts the tree's ownership with a recursive chown
/// instead, and without metacopy every chown copies the whole file into the upper
/// directory. With it only the inode is copied and the layers stay shared.
pub fn overlay_unit_text(
    name: &str,
    mountpoint: &str,
    layer_dirs: &[PathBuf],
    upper: &Path,
    work: &Path,
) -> String {
    // One lowerdir+= per layer, top layer first: util-linux 2.41 mounts through the
    // new mount API, whose lowerdir= takes no colon-separated list beyond two layers
    // (EINVAL), while the kernel takes lowerdir+= since 6.5 either way.
    let lower: Vec<String> = layer_dirs
        .iter()
        .rev()
        .map(|d| format!("lowerdir+={}", d.to_string_lossy()))
        .collect();
    format!(
        "# Generated by nspawn; do not edit.\n\
         [Unit]\n\
         Description=Root file system of nspawn machine {name}\n\
         \n\
         [Mount]\n\
         What=overlay\n\
         Where={mountpoint}\n\
         Type=overlay\n\
         Options={},upperdir={},workdir={},metacopy=on\n",
        lower.join(","),
        upper.display(),
        work.display()
    )
}

/// Writes a systemd.mstack(7) directory: layer@N symlinks to the shared layers plus rw/.
pub fn write_mstack(dir: &Path, layer_dirs: &[PathBuf]) -> Result<()> {
    if dir.exists() {
        fs::remove_dir_all(dir).with_context(|| format!("removing {}", dir.display()))?;
    }
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    for (i, layer) in layer_dirs.iter().enumerate() {
        if !layer.is_absolute() {
            bail!("layer directory {} is not absolute", layer.display());
        }
        let link = dir.join(format!("layer@{i}"));
        symlink(layer, &link).with_context(|| format!("creating {}", link.display()))?;
    }
    let rw = dir.join("rw");
    fs::create_dir_all(&rw).with_context(|| format!("creating {}", rw.display()))?;
    // The writable layer must belong to the foreign range too, or mountfsd maps it as
    // identity and the container's root cannot write to it.
    if nix::unistd::geteuid().is_root() {
        std::os::unix::fs::chown(&rw, Some(FOREIGN_UID_BASE), Some(FOREIGN_UID_BASE))
            .with_context(|| format!("shifting ownership of {}", rw.display()))?;
    }
    Ok(())
}

pub fn is_mountpoint(path: &Path) -> Result<bool> {
    let mounts = fs::read_to_string("/proc/self/mounts").context("reading /proc/self/mounts")?;
    let wanted = path.to_string_lossy().replace(' ', "\\040");
    Ok(mounts
        .lines()
        .any(|l| l.split_whitespace().nth(1) == Some(wanted.as_str())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_root_is_looked_at_where_nspawn_will_see_it() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(&tmp.path().join("machines"), &tmp.path().join("state"));
        let flat = store.machines_dir.join("flat");
        fs::create_dir_all(flat.join("usr/bin")).unwrap();
        fs::write(flat.join("usr/bin/getent"), "").unwrap();
        fs::write(flat.join("bin-sh"), "#!/bin/sh").unwrap();
        let program = |m: &fs::Metadata| !m.is_file() || m.len() > 0;
        // An absolute symlink of the image is not followed on the host.
        std::os::unix::fs::symlink("/bin/busybox-of-the-image", flat.join("sh-link")).unwrap();
        assert!(root_has(
            &store,
            "flat",
            BackendChoice::Flat,
            "sh-link",
            program
        ));
        assert_eq!(
            root_path(&store, "flat", BackendChoice::Flat, "sh-link"),
            Some(flat.join("sh-link"))
        );
        assert_eq!(root_path(&store, "flat", BackendChoice::Flat, "nope"), None);
        assert!(root_has(
            &store,
            "flat",
            BackendChoice::Flat,
            "usr/bin",
            program
        ));
        assert!(root_has(
            &store,
            "flat",
            BackendChoice::Flat,
            "bin-sh",
            program
        ));
        // A mount point made ahead of an earlier start is an empty file, not a program.
        assert!(!root_has(
            &store,
            "flat",
            BackendChoice::Flat,
            "usr/bin/getent",
            program
        ));
        assert!(root_has(
            &store,
            "flat",
            BackendChoice::Flat,
            "usr/bin/getent",
            |_| true
        ));
        assert!(!root_has(
            &store,
            "flat",
            BackendChoice::Flat,
            "bin/sh",
            |_| true
        ));
        let mstack = store.machines_dir.join("m.mstack");
        fs::create_dir_all(mstack.join("layer@0/bin")).unwrap();
        fs::write(mstack.join("layer@0/bin/sh"), "#!/bin/sh").unwrap();
        fs::create_dir_all(mstack.join("rw")).unwrap();
        assert!(root_has(
            &store,
            "m",
            BackendChoice::Mstack,
            "bin/sh",
            program
        ));
        assert!(!root_has(
            &store,
            "m",
            BackendChoice::Mstack,
            "bin/getent",
            program
        ));
        assert!(!root_has(
            &store,
            "none",
            BackendChoice::Mstack,
            "bin/sh",
            program
        ));
    }

    #[test]
    fn the_topmost_mstack_layer_with_the_path_is_the_one_seen() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(&tmp.path().join("machines"), &tmp.path().join("state"));
        let mstack = store.machines_dir.join("m.mstack");
        for layer in ["layer@0", "layer@1"] {
            fs::create_dir_all(mstack.join(layer).join("bin")).unwrap();
            fs::write(mstack.join(layer).join("bin/sh"), layer).unwrap();
        }
        fs::create_dir_all(mstack.join("rw")).unwrap();
        assert_eq!(
            root_path(&store, "m", BackendChoice::Mstack, "bin/sh"),
            Some(mstack.join("layer@1/bin/sh"))
        );
        assert_eq!(
            root_path(&store, "m", BackendChoice::Mstack, "bin/nope"),
            None
        );
    }

    #[test]
    fn the_image_is_looked_at_layer_by_layer_from_the_top() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(&tmp.path().join("machines"), &tmp.path().join("state"));
        let layers = vec!["sha256:base".to_string(), "sha256:top".to_string()];
        let base = store.layer_dir("sha256:base", Ownership::Root);
        let top = store.layer_dir("sha256:top", Ownership::Root);
        fs::create_dir_all(base.join("data")).unwrap();
        fs::create_dir_all(top.join("etc")).unwrap();
        assert_eq!(
            image_path(&store, &layers, BackendChoice::Overlay, "data"),
            Some(base.join("data"))
        );
        fs::create_dir_all(top.join("data")).unwrap();
        assert_eq!(
            image_path(&store, &layers, BackendChoice::Overlay, "data"),
            Some(top.join("data"))
        );
        assert_eq!(
            image_path(&store, &layers, BackendChoice::Overlay, "nope"),
            None
        );
        // No layers kept: nothing to say.
        assert_eq!(
            image_path(
                &store,
                &["sha256:gone".to_string()],
                BackendChoice::Flat,
                "data"
            ),
            None
        );
    }

    #[test]
    fn a_tmpfs_on_run_is_told_apart() {
        assert!(resolves_to_run("/run", None));
        assert!(resolves_to_run("/run/lock", None));
        assert!(!resolves_to_run("/tmp", None));
        assert!(!resolves_to_run("/runtime", None));
        // Debian's /var/run -> /run, and a relative form of it.
        assert!(resolves_to_run("/var/run", Some(Path::new("/run"))));
        assert!(resolves_to_run("/var/run", Some(Path::new("../run"))));
        assert!(!resolves_to_run("/var/run", Some(Path::new("../lib/run"))));
        assert!(!resolves_to_run("/var/tmp", Some(Path::new("/tmp"))));
    }

    #[test]
    fn mount_points_are_made_below_the_root() {
        let tmp = tempfile::tempdir().unwrap();
        let layer = tmp.path();
        fs::create_dir_all(layer.join("etc")).unwrap();
        fs::write(layer.join("etc/hosts"), "x").unwrap();
        ensure_mount_points(
            layer,
            &[
                ("/scratch".to_string(), true),
                ("/etc/hosts".to_string(), false),
                ("/dev/nullo".to_string(), false),
                ("/a/b/c".to_string(), true),
            ],
        )
        .unwrap();
        assert!(layer.join("scratch").is_dir());
        assert!(layer.join("dev/nullo").is_file());
        assert!(layer.join("a/b/c").is_dir());
        assert_eq!(fs::read_to_string(layer.join("etc/hosts")).unwrap(), "x");
        assert!(ensure_mount_points(layer, &[("/../x".to_string(), true)]).is_err());
        assert!(ensure_mount_points(layer, &[("x".to_string(), true)]).is_err());
    }

    #[test]
    fn parses_systemd_versions() {
        assert_eq!(systemd_major("261.3-1-arch"), Some(261));
        assert_eq!(systemd_major("259 (259.7-1.fc44)"), Some(259));
        assert_eq!(systemd_major("v255"), None);
    }

    #[test]
    fn overlay_unit_lists_top_layer_first() {
        let text = overlay_unit_text(
            "m",
            "/var/lib/machines/m",
            &[PathBuf::from("/l/base"), PathBuf::from("/l/top")],
            Path::new("/u"),
            Path::new("/w"),
        );
        assert!(text.contains("Where=/var/lib/machines/m\n"));
        assert!(text.contains(
            "Options=lowerdir+=/l/top,lowerdir+=/l/base,upperdir=/u,workdir=/w,metacopy=on\n"
        ));
        assert!(text.contains("Type=overlay\n"));
    }

    #[test]
    fn mstack_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("m.mstack");
        let layers = [
            PathBuf::from("/var/lib/nspawn/layers/sha256-aaa"),
            PathBuf::from("/var/lib/nspawn/layers/sha256-bbb"),
        ];
        write_mstack(&dir, &layers).unwrap();
        assert_eq!(fs::read_link(dir.join("layer@0")).unwrap(), layers[0]);
        assert_eq!(fs::read_link(dir.join("layer@1")).unwrap(), layers[1]);
        assert!(dir.join("rw").is_dir());
        assert!(write_mstack(&dir, &[PathBuf::from("relative")]).is_err());
    }

    #[test]
    fn root_is_a_mountpoint() {
        assert!(is_mountpoint(Path::new("/")).unwrap());
        assert!(!is_mountpoint(Path::new("/definitely/not/mounted")).unwrap());
    }
}
