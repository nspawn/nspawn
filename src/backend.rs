//! How a pulled image is laid out on this host: native mstack, overlayfs mount unit or a
//! flat directory.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::cli::BackendChoice;
use crate::store::{extract_layer, Store, WhiteoutMode};
use crate::systemd::Systemd;
use crate::unitname;

pub const UNIT_DIR: &str = "/etc/systemd/system";
const NSRESOURCED_SOCKET: &str = "/run/systemd/io.systemd.NamespaceResource";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Overlay,
    Flat,
    Mstack,
}

impl Backend {
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

async fn mstack_supported(sd: &Systemd) -> bool {
    let new_enough = sd
        .version()
        .await
        .ok()
        .and_then(|v| systemd_major(&v))
        .is_some_and(|m| m >= 261);
    new_enough && Path::new(NSRESOURCED_SOCKET).exists()
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

    /// Makes the image `name` available to machinectl. Blobs are consumed.
    pub async fn assemble(&self, backend: Backend, name: &str, layers: &[Layer]) -> Result<()> {
        match backend {
            Backend::Flat => {
                let dir = self.machine_dir(name);
                fs::create_dir_all(&dir)?;
                for layer in layers {
                    extract_layer(&layer.blob, &layer.media_type, &dir, WhiteoutMode::Apply)
                        .with_context(|| format!("extracting layer {}", layer.digest))?;
                }
            }
            Backend::Overlay | Backend::Mstack => {
                let mut dirs = Vec::new();
                for layer in layers {
                    dirs.push(self.store.import_layer(
                        &layer.digest,
                        &layer.media_type,
                        &layer.blob,
                    )?);
                }
                if backend == Backend::Overlay {
                    self.write_overlay(name, &dirs).await?;
                } else {
                    write_mstack(&self.mstack_dir(name), &dirs)?;
                }
            }
        }
        for layer in layers {
            let _ = fs::remove_file(&layer.blob);
        }
        Ok(())
    }

    async fn write_overlay(&self, name: &str, layer_dirs: &[PathBuf]) -> Result<()> {
        let mountpoint = self.machine_dir(name);
        fs::create_dir_all(&mountpoint)?;
        let private = self.store.machines_private_dir().join(name);
        let upper = private.join("upper");
        let work = private.join("work");
        fs::create_dir_all(&upper)?;
        fs::create_dir_all(&work)?;
        let mp = mountpoint.to_string_lossy().to_string();
        let unit = unitname::mount_unit_for(&mp);
        let text = overlay_unit_text(name, &mp, layer_dirs, &upper, &work);
        let unit_path = Path::new(UNIT_DIR).join(&unit);
        fs::write(&unit_path, text).with_context(|| format!("writing {}", unit_path.display()))?;
        let dropin_dir = dropin_dir(name);
        fs::create_dir_all(&dropin_dir)?;
        fs::write(
            dropin_dir.join("nspawn-overlay.conf"),
            format!("# Generated by nspawn; do not edit.\n[Unit]\nRequiresMountsFor={mp}\n"),
        )?;
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
                let _ = fs::remove_dir_all(dropin_dir(name));
                self.sd.reload().await?;
                let _ = fs::remove_dir_all(self.store.machines_private_dir().join(name));
                remove_dir_if_exists(&mountpoint)?;
            }
            BackendChoice::Flat => remove_dir_if_exists(&self.machine_dir(name))?,
            BackendChoice::Mstack => remove_dir_if_exists(&self.mstack_dir(name))?,
            BackendChoice::Auto => bail!("image record for {name} has no concrete backend"),
        }
        Ok(())
    }
}

fn dropin_dir(name: &str) -> PathBuf {
    Path::new(UNIT_DIR).join(format!("systemd-nspawn@{name}.service.d"))
}

fn remove_dir_if_exists(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

/// Text of the mount unit. overlayfs lists the topmost layer first in lowerdir=.
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
         Options=lowerdir={},upperdir={},workdir={}\n",
        lower.join(":"),
        upper.display(),
        work.display()
    )
}

/// Writes a systemd.mstack(7) directory: layer@N symlinks to the shared layers plus rw/.
pub fn write_mstack(dir: &Path, layer_dirs: &[PathBuf]) -> Result<()> {
    if dir.exists() {
        fs::remove_dir_all(dir)?;
    }
    fs::create_dir_all(dir)?;
    for (i, layer) in layer_dirs.iter().enumerate() {
        if !layer.is_absolute() {
            bail!("layer directory {} is not absolute", layer.display());
        }
        symlink(layer, dir.join(format!("layer@{i}")))?;
    }
    fs::create_dir_all(dir.join("rw"))?;
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
        assert!(text.contains("Options=lowerdir=/l/top:/l/base,upperdir=/u,workdir=/w\n"));
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
