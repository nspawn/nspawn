//! Named volumes: the directories `-v NAME:/path` makes under the state directory, listed
//! with the machines that use them, made ahead of time, and removed once nobody does.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use anyhow::{bail, Context as _, Result};

use crate::api::images::Removal;
use crate::api::{line, note, require_root, Context, Report};
use crate::backend::is_mountpoint;
use crate::store::{ImageRecord, Store};
use crate::volume::validate_volume_name;

/// One named volume as `volume ls` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeInfo {
    pub name: String,
    pub path: PathBuf,
    /// The machines whose records mount it, sorted.
    pub used_by: Vec<String>,
    /// Unix seconds the directory was made (its modification time where the file system
    /// keeps no birth time).
    pub created: u64,
}

/// Which machines mount each named volume.
fn users(records: &[ImageRecord]) -> BTreeMap<String, BTreeSet<String>> {
    let mut users: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for record in records {
        for volume in record.volumes.iter().filter(|v| v.is_named()) {
            users
                .entry(volume.source.clone())
                .or_default()
                .insert(record.name.clone());
        }
    }
    users
}

/// Every directory under the volumes directory, with the machines that use it. Names
/// starting with a dot, plain files and symlinks are not volumes and are left out.
pub fn list(store: &Store) -> Result<Vec<VolumeInfo>> {
    let dir = store.volumes_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    let users = users(&store.list_images()?);
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let Ok(meta) = entry.path().symlink_metadata() else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        let created = meta
            .created()
            .or_else(|_| meta.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        out.push(VolumeInfo {
            used_by: users
                .get(&name)
                .map(|u| u.iter().cloned().collect())
                .unwrap_or_default(),
            path: entry.path(),
            name,
            created,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Makes a named volume ahead of its first use, the way `start` would: a directory
/// owned by root, mode 0755. One that exists already is fine, like docker volume create.
pub async fn create(ctx: &Context, name: &str) -> Result<PathBuf> {
    require_root("volume create")?;
    let store = &ctx.store;
    let _lock = store.lock().await?;
    let new = !store.volumes_dir().join(name).is_dir();
    let path = create_in(store, name)?;
    if new {
        crate::api::events::emit("volume", "create", name, &[]);
    }
    Ok(path)
}

fn create_in(store: &Store, name: &str) -> Result<PathBuf> {
    validate_volume_name(name)?;
    let dir = store.volumes_dir();
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(name);
    match fs::create_dir(&path) {
        Ok(()) => Ok(path),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let meta = path
                .symlink_metadata()
                .with_context(|| format!("reading {}", path.display()))?;
            if meta.is_dir() {
                Ok(path)
            } else {
                bail!(
                    "{} exists and is not a directory; it is not a volume",
                    path.display()
                )
            }
        }
        Err(e) => Err(e).with_context(|| format!("creating {}", path.display())),
    }
}

/// Removes volumes nobody uses. One in use, unknown or not made by nspawn is refused and
/// does not stop the others, like docker volume rm.
pub async fn remove(ctx: &Context, names: &[String], report: Report<'_>) -> Result<Removal> {
    require_root("volume rm")?;
    let store = &ctx.store;
    // start makes volumes under the same lock, so nothing starts using one meanwhile.
    let _lock = store.lock().await?;
    remove_in(store, names, report)
}

fn remove_in(store: &Store, names: &[String], report: Report<'_>) -> Result<Removal> {
    // The strict reader: a record that cannot be read must not make its volume look
    // unused. A running machine's volumes are always in its record, since the record only
    // changes through start and create, both refused while it runs.
    let users = users(&store.list_images_strict()?);
    let mut removal = Removal::default();
    for name in names {
        let outcome = remove_one(store, name, &users);
        match outcome {
            Ok(()) => {
                line(report, format!("removed {name}"));
                crate::api::events::emit("volume", "remove", name, &[]);
                removal.removed.push(name.clone());
            }
            Err(e) => removal.failed.push((name.clone(), format!("{e:#}"))),
        }
    }
    Ok(removal)
}

fn remove_one(store: &Store, name: &str, users: &BTreeMap<String, BTreeSet<String>>) -> Result<()> {
    let path = store.volumes_dir().join(name);
    if validate_volume_name(name).is_err() {
        if path.symlink_metadata().is_ok() && !name.contains('/') && name != ".." {
            bail!(
                "{name} is not a volume name; remove {} by hand if it has to go",
                path.display()
            );
        }
        bail!("no volume named {name}");
    }
    let meta = match path.symlink_metadata() {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!("no volume named {name}"),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    if !meta.is_dir() {
        bail!(
            "{} is not a directory nspawn made; it is not a volume",
            path.display()
        );
    }
    if let Some(machines) = users.get(name) {
        let machines: Vec<&str> = machines.iter().map(String::as_str).collect();
        bail!(
            "volume {name} is in use by {}; start them with -v none (or other volumes) first",
            machines.join(", ")
        );
    }
    if is_mountpoint(&path)? {
        bail!("{} is a mount point; unmount it first", path.display());
    }
    fs::remove_dir_all(&path).with_context(|| format!("removing {}", path.display()))
}

/// Removes every volume no machine uses. Mount points and directories that are not
/// volume names are left alone.
pub async fn prune(ctx: &Context, report: Report<'_>) -> Result<Vec<String>> {
    require_root("volume prune")?;
    let store = &ctx.store;
    let _lock = store.lock().await?;
    prune_in(store, report)
}

fn prune_in(store: &Store, report: Report<'_>) -> Result<Vec<String>> {
    let users = users(&store.list_images_strict()?);
    let mut removed = Vec::new();
    for volume in list(store)? {
        if !volume.used_by.is_empty() || validate_volume_name(&volume.name).is_err() {
            continue;
        }
        if is_mountpoint(&volume.path)? {
            note(
                report,
                format!(
                    "note: {} is a mount point; left alone",
                    volume.path.display()
                ),
            );
            continue;
        }
        remove_one(store, &volume.name, &users)?;
        line(report, format!("removed {}", volume.name));
        crate::api::events::emit("volume", "remove", &volume.name, &[]);
        removed.push(volume.name);
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::BackendChoice;
    use crate::oci::{Mode, RunSpec};
    use crate::settings::Network;
    use crate::volume::Volume;

    fn quiet(_: crate::api::Event) {}

    fn store() -> (tempfile::TempDir, Store) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(&tmp.path().join("machines"), &tmp.path().join("state"));
        store.init().unwrap();
        (tmp, store)
    }

    fn machine(store: &Store, name: &str, volumes: &[&str]) {
        store
            .record_image(&ImageRecord {
                name: name.into(),
                reference: "hub.example/img:1".into(),
                manifest_digest: "sha256:m".into(),
                layers: Vec::new(),
                backend: BackendChoice::Flat,
                created: 1,
                origin: "pull".into(),
                mode: Mode::App,
                run: RunSpec::default(),
                network: Network::Host,
                network_name: None,
                address: None,
                ports: Vec::new(),
                entrypoint: None,
                cmd: None,
                env: Vec::new(),
                volumes: volumes
                    .iter()
                    .map(|v| v.parse::<Volume>().unwrap())
                    .collect(),
                labels: BTreeMap::new(),
                restart: Default::default(),
                limits: Default::default(),
                remove_on_exit: false,
            })
            .unwrap();
    }

    #[test]
    fn the_store_has_a_volumes_directory() {
        let (_tmp, store) = store();
        assert!(store.volumes_dir().is_dir());
    }

    #[test]
    fn volumes_are_listed_with_the_machines_that_use_them() {
        let (_tmp, store) = store();
        for name in ["data", "cache", ".hidden"] {
            fs::create_dir(store.volumes_dir().join(name)).unwrap();
        }
        fs::write(store.volumes_dir().join("afile"), "x").unwrap();
        std::os::unix::fs::symlink("/etc", store.volumes_dir().join("alink")).unwrap();
        machine(&store, "web", &["data:/srv", "/host:/host"]);
        machine(&store, "api", &["data:/data", "cache:/cache:ro"]);
        let listed = list(&store).unwrap();
        let names: Vec<&str> = listed.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, ["cache", "data"]);
        assert_eq!(listed[0].used_by, ["api"]);
        assert_eq!(listed[1].used_by, ["api", "web"]);
        assert_eq!(listed[1].path, store.volumes_dir().join("data"));
        assert!(listed[1].created > 0);
    }

    #[test]
    fn a_volume_is_created_once_and_only_under_a_good_name() {
        let (_tmp, store) = store();
        let path = create_in(&store, "data").unwrap();
        assert!(path.is_dir());
        assert_eq!(create_in(&store, "data").unwrap(), path, "again is fine");
        for bad in ["", "..", ".x", "a/b", "a b"] {
            assert!(create_in(&store, bad).is_err(), "{bad}");
        }
        fs::write(store.volumes_dir().join("afile"), "x").unwrap();
        assert!(create_in(&store, "afile").is_err());
    }

    #[test]
    fn only_unused_volumes_nspawn_made_are_removed() {
        let (_tmp, store) = store();
        create_in(&store, "used").unwrap();
        create_in(&store, "free").unwrap();
        fs::write(store.volumes_dir().join("free/file"), "x").unwrap();
        fs::write(store.volumes_dir().join("afile"), "x").unwrap();
        std::os::unix::fs::symlink("/etc", store.volumes_dir().join("alink")).unwrap();
        fs::create_dir(store.volumes_dir().join("bad name")).unwrap();
        machine(&store, "web", &["used:/srv"]);
        let names: Vec<String> = ["free", "used", "nope", "afile", "alink", "bad name", "../x"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let removal = remove_in(&store, &names, &quiet).unwrap();
        assert_eq!(removal.removed, ["free"]);
        assert!(!store.volumes_dir().join("free").exists());
        let why: BTreeMap<&str, &str> = removal
            .failed
            .iter()
            .map(|(n, w)| (n.as_str(), w.as_str()))
            .collect();
        assert!(why["used"].contains("in use by web"), "{}", why["used"]);
        assert!(why["nope"].contains("no volume named nope"));
        assert!(why["afile"].contains("not a directory"));
        assert!(why["alink"].contains("not a directory"));
        assert!(why["bad name"].contains("by hand"));
        assert!(why["../x"].contains("no volume named"));
        assert!(store.volumes_dir().join("used").is_dir());
        assert!(
            PathBuf::from("/etc").is_dir(),
            "a symlink is never followed"
        );
    }

    #[test]
    fn prune_removes_what_nobody_uses() {
        let (_tmp, store) = store();
        for name in ["used", "free1", "free2"] {
            create_in(&store, name).unwrap();
        }
        fs::create_dir(store.volumes_dir().join("bad name")).unwrap();
        machine(&store, "web", &["used:/srv"]);
        let removed = prune_in(&store, &quiet).unwrap();
        assert_eq!(removed, ["free1", "free2"]);
        assert!(store.volumes_dir().join("used").is_dir());
        assert!(store.volumes_dir().join("bad name").is_dir());
    }

    #[test]
    fn a_broken_record_stops_removal_rather_than_hide_a_user() {
        let (_tmp, store) = store();
        create_in(&store, "data").unwrap();
        fs::write(store.images_dir().join("broken.json"), "{").unwrap();
        assert!(remove_in(&store, &["data".to_string()], &quiet).is_err());
        assert!(prune_in(&store, &quiet).is_err());
        assert!(store.volumes_dir().join("data").is_dir());
    }
}
