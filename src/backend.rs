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
                if mstack_supported(sd).await {
                    Ok(Backend::Mstack)
                } else {
                    bail!(
                        "mstack images need systemd 261 or newer with systemd-nsresourced running"
                    )
                }
            }
            BackendChoice::Auto => {
                if mstack_supported(sd).await {
                    Ok(Backend::Mstack)
                } else if overlay_supported() {
                    Ok(Backend::Overlay)
                } else {
                    Ok(Backend::Flat)
                }
            }
        }
    }
}

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
    let lower: Vec<String> = layer_dirs
        .iter()
        .rev()
        .map(|d| d.to_string_lossy().to_string())
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
         Options=lowerdir={},upperdir={},workdir={},metacopy=on\n",
        lower.join(":"),
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
        assert!(
            text.contains("Options=lowerdir=/l/top:/l/base,upperdir=/u,workdir=/w,metacopy=on\n")
        );
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
