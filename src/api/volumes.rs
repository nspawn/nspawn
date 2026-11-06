//! Named volumes: the directories `-v NAME:/path` makes under the state directory, listed
//! with the machines that use them, made ahead of time, and removed once nobody does.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use anyhow::{bail, Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::api::failure::invalid;
use crate::api::images::Removal;
use crate::api::{line, note, require_root, Context, Report};
use crate::backend::is_mountpoint;
use crate::store::{now_unix, unique_suffix, ImageRecord, Store};
use crate::volume::validate_volume_name;

/// One named volume as `volume ls` shows it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VolumeInfo {
    pub name: String,
    pub path: PathBuf,
    /// The machines whose records mount it, sorted.
    pub used_by: Vec<String>,
    /// Unix seconds it was made: as kept when it was made by a create, else the
    /// directory's birth time (its modification time where the file system keeps none).
    pub created: u64,
    /// KEY=VALUE, given when it was made by a create.
    pub labels: BTreeMap<String, String>,
    /// Made by the Docker API without a name: what the API's prune takes without `all`.
    pub anonymous: bool,
}

/// What is kept of a volume made by a create, beside its directory and never inside
/// it, where the machines that mount it could change it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct VolumeMeta {
    created: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    labels: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    anonymous: bool,
}

/// The file of `name` in the volumes' metadata directory: the name alone, so that any
/// name a directory takes fits.
fn meta_path(store: &Store, name: &str) -> PathBuf {
    store.volume_meta_dir().join(name)
}

/// What was kept of `name`. One that cannot be read counts as none: it only adds to
/// what the directory says.
fn read_meta(store: &Store, name: &str) -> Option<VolumeMeta> {
    let bytes = fs::read(meta_path(store, name)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Writes what is kept of `name` through a short temporary name and a rename, so that a
/// crash leaves the old file or the new one, and a long volume name still fits.
fn write_meta(store: &Store, name: &str, meta: &VolumeMeta) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    let dir = store.volume_meta_dir();
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    let path = meta_path(store, name);
    let tmp = dir.join(format!(".tmp-{}", unique_suffix()));
    let written = fs::File::create(&tmp).and_then(|mut file| {
        std::io::Write::write_all(&mut file, &serde_json::to_vec(meta)?)?;
        file.sync_all()
    });
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("writing {}", path.display()));
    }
    fs::rename(&tmp, &path).with_context(|| format!("moving {} into place", path.display()))
}

/// Forgets what was kept of `name`; a volume without any is fine. A start that makes a
/// volume's directory calls it too: what was kept is of a directory removed by hand,
/// another volume.
pub fn forget_meta(store: &Store, name: &str) -> Result<()> {
    if validate_volume_name(name).is_err() {
        return Ok(());
    }
    let path = meta_path(store, name);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
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

/// Every directory under the volumes directory, with the machines that use them. Names
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
        let kept = read_meta(store, &name).unwrap_or_default();
        let created = if kept.created > 0 {
            kept.created
        } else {
            meta.created()
                .or_else(|_| meta.modified())
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0)
        };
        out.push(VolumeInfo {
            used_by: users
                .get(&name)
                .map(|u| u.iter().cloned().collect())
                .unwrap_or_default(),
            path: entry.path(),
            name,
            created,
            labels: kept.labels,
            anonymous: kept.anonymous,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

fn no_volume(name: &str) -> anyhow::Error {
    crate::api::FailureKind::NoSuchVolume.error(format!("no volume named {name}"))
}

/// One volume as `list` has it.
pub fn get(store: &Store, name: &str) -> Result<VolumeInfo> {
    if validate_volume_name(name).is_err() {
        return Err(no_volume(name));
    }
    list(store)?
        .into_iter()
        .find(|v| v.name == name)
        .ok_or_else(|| no_volume(name))
}

/// 64 random hex digits, the name of a volume made without one.
fn random_name() -> Result<String> {
    use std::io::Read as _;
    let mut bytes = [0u8; 32];
    fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .context("reading /dev/urandom")?;
    Ok(hex::encode(bytes))
}

/// Makes a named volume ahead of its first use, the way `start` would: a directory
/// owned by root, mode 0755, with `labels` and the `anonymous` mark kept beside it. An
/// empty name makes one of 64 random hex digits. One that exists already is fine, and
/// stays as it is, labels included.
pub async fn create(
    ctx: &Context,
    name: &str,
    labels: BTreeMap<String, String>,
    anonymous: bool,
) -> Result<VolumeInfo> {
    require_root("volume create")?;
    let store = &ctx.store;
    let name = if name.is_empty() {
        random_name()?
    } else {
        name.to_string()
    };
    let _lock = store.lock().await?;
    let (path, new) = create_in(store, &name, labels, anonymous)?;
    if new {
        crate::api::events::emit(
            "volume",
            "create",
            &name,
            &[("path", &path.to_string_lossy())],
        );
    }
    get(store, &name)
}

/// The volume's directory, and whether it was made now.
fn create_in(
    store: &Store,
    name: &str,
    labels: BTreeMap<String, String>,
    anonymous: bool,
) -> Result<(PathBuf, bool)> {
    validate_volume_name(name).map_err(invalid)?;
    let dir = store.volumes_dir();
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(name);
    match fs::create_dir(&path) {
        Ok(()) => {
            let kept = VolumeMeta {
                created: now_unix(),
                labels,
                anonymous,
            };
            if let Err(e) = write_meta(store, name, &kept) {
                // Empty still: nothing of anyone's goes with it.
                let _ = fs::remove_dir(&path);
                return Err(e);
            }
            Ok((path, true))
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let meta = path
                .symlink_metadata()
                .with_context(|| format!("reading {}", path.display()))?;
            if meta.is_dir() {
                Ok((path, false))
            } else {
                Err(crate::api::FailureKind::Exists.error(format!(
                    "{} exists and is not a directory; it is not a volume",
                    path.display()
                )))
            }
        }
        Err(e) => Err(e).with_context(|| format!("creating {}", path.display())),
    }
}

/// Removes volumes nobody uses. One in use, unknown or not made by nspawn is refused and
/// does not stop the others.
pub async fn remove(ctx: &Context, names: &[String], report: Report<'_>) -> Result<Removal> {
    require_root("volume rm")?;
    let store = &ctx.store;
    // start makes volumes under the same lock, so nothing starts using one meanwhile.
    // Trees of any size are removed off the runtime's workers.
    let _lock = store.lock().await?;
    tokio::task::block_in_place(|| remove_in(store, names, report))
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
                crate::api::events::emit(
                    "volume",
                    "remove",
                    name,
                    &[("path", &store.volumes_dir().join(name).to_string_lossy())],
                );
                removal.removed.push(name.clone());
            }
            Err(e) => removal.refuse(name, e),
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
        return Err(no_volume(name));
    }
    let meta = match path.symlink_metadata() {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(no_volume(name)),
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
        return Err(crate::api::FailureKind::InUse.error(format!(
            "volume {name} is in use by {}; start them with -v none (or other volumes) first",
            machines.join(", ")
        )));
    }
    if is_mountpoint(&path)? {
        bail!("{} is a mount point; unmount it first", path.display());
    }
    crate::store::remove_tree(&path).with_context(|| format!("removing {}", path.display()))?;
    forget_meta(store, name)
}

/// Removes every volume no machine uses. Mount points and directories that are not
/// volume names are left alone.
pub async fn prune(ctx: &Context, report: Report<'_>) -> Result<Vec<String>> {
    require_root("volume prune")?;
    let store = &ctx.store;
    let _lock = store.lock().await?;
    tokio::task::block_in_place(|| prune_in(store, report))
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
        // One that cannot go is said and left; the others still go.
        if let Err(e) = remove_one(store, &volume.name, &users) {
            note(report, format!("warning: {} stays: {e:#}", volume.name));
            continue;
        }
        line(report, format!("removed {}", volume.name));
        crate::api::events::emit(
            "volume",
            "remove",
            &volume.name,
            &[(
                "path",
                &store.volumes_dir().join(&volume.name).to_string_lossy(),
            )],
        );
        removed.push(volume.name);
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::FailureKind;
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
                index_digest: None,
                layers: Vec::new(),
                backend: BackendChoice::Flat,
                created: 1,
                origin: "pull".into(),
                mode: Mode::App,
                pid1: false,
                started: None,
                fixed_addresses: Default::default(),
                built: false,
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
                api_name: None,
                tty: false,
                interactive: false,
                stdin_once: false,
                extra_networks: Vec::new(),
                aliases: BTreeMap::new(),
                no_network: false,
                network_container: None,
                signed_by: None,
                signed_at: None,
                healthcheck: None,
                tuning: Default::default(),
            })
            .unwrap();
    }

    /// A volume made by a create without labels.
    fn make(store: &Store, name: &str) -> PathBuf {
        create_in(store, name, BTreeMap::new(), false).unwrap().0
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
        let none = BTreeMap::new;
        let (path, new) = create_in(&store, "data", none(), false).unwrap();
        assert!(path.is_dir() && new);
        assert_eq!(
            create_in(&store, "data", none(), false).unwrap(),
            (path, false),
            "again is fine"
        );
        for bad in ["", "..", ".x", "a/b", "a b"] {
            let e = create_in(&store, bad, none(), false).unwrap_err();
            assert_eq!(crate::api::kind_of(&e), Some(FailureKind::Invalid), "{bad}");
        }
        fs::write(store.volumes_dir().join("afile"), "x").unwrap();
        assert!(create_in(&store, "afile", none(), false).is_err());
        assert!(
            !meta_path(&store, "afile").exists(),
            "nothing is kept of what is no volume"
        );
    }

    #[test]
    fn only_unused_volumes_nspawn_made_are_removed() {
        let (_tmp, store) = store();
        make(&store, "used");
        make(&store, "free");
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
            make(&store, name);
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
        make(&store, "data");
        fs::write(store.images_dir().join("broken.json"), "{").unwrap();
        assert!(remove_in(&store, &["data".to_string()], &quiet).is_err());
        assert!(prune_in(&store, &quiet).is_err());
        assert!(store.volumes_dir().join("data").is_dir());
    }

    #[test]
    fn labels_and_the_anonymous_mark_are_kept_beside_the_volume() {
        let (_tmp, store) = store();
        let labels: BTreeMap<String, String> = [("team".to_string(), "a".to_string())].into();
        let (path, _) = create_in(&store, "data", labels.clone(), true).unwrap();
        assert_eq!(
            fs::read_dir(&path).unwrap().count(),
            0,
            "nothing inside what a machine mounts"
        );
        assert!(meta_path(&store, "data").is_file());
        let listed = get(&store, "data").unwrap();
        assert_eq!(listed.labels, labels);
        assert!(listed.anonymous);
        assert!(listed.created > 0);
        // One that exists is returned as it is.
        create_in(&store, "data", BTreeMap::new(), false).unwrap();
        let again = get(&store, "data").unwrap();
        assert_eq!((again.labels, again.anonymous), (labels, true));
        // Made by a start: nothing kept, and a name it has is not anonymous.
        fs::create_dir(store.volumes_dir().join("plain")).unwrap();
        let plain = get(&store, "plain").unwrap();
        assert!(plain.labels.is_empty() && !plain.anonymous && plain.created > 0);
        let text = fs::read_to_string(meta_path(&store, "data")).unwrap();
        let kept: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(kept["labels"]["team"], "a");
        assert_eq!(kept["anonymous"], true);
        create_in(&store, "bare", BTreeMap::new(), false).unwrap();
        let bare = fs::read_to_string(meta_path(&store, "bare")).unwrap();
        assert!(
            !bare.contains("labels") && !bare.contains("anonymous"),
            "{bare}"
        );
    }

    #[test]
    fn what_was_kept_of_a_volume_removed_by_hand_is_not_another_s() {
        let (_tmp, store) = store();
        let labels: BTreeMap<String, String> = [("k".to_string(), "v".to_string())].into();
        let (path, _) = create_in(&store, "data", labels, true).unwrap();
        fs::remove_dir(&path).unwrap();
        assert!(get(&store, "data").is_err());
        // A create of the name keeps what it is given.
        let labels: BTreeMap<String, String> = [("n".to_string(), "2".to_string())].into();
        create_in(&store, "data", labels.clone(), false).unwrap();
        let again = get(&store, "data").unwrap();
        assert_eq!((again.labels, again.anonymous), (labels, false));
        // A start that makes it again forgets it.
        fs::remove_dir(&path).unwrap();
        fs::create_dir(&path).unwrap();
        forget_meta(&store, "data").unwrap();
        let made = get(&store, "data").unwrap();
        assert!(made.labels.is_empty() && !made.anonymous, "{made:?}");
        forget_meta(&store, "data").unwrap();
    }

    #[test]
    fn removal_and_prune_forget_what_was_kept() {
        let (_tmp, store) = store();
        make(&store, "gone");
        make(&store, "pruned");
        make(&store, "used");
        machine(&store, "web", &["used:/srv"]);
        let removal = remove_in(&store, &["gone".to_string()], &quiet).unwrap();
        assert_eq!(removal.removed, ["gone"]);
        assert!(!meta_path(&store, "gone").exists());
        assert_eq!(prune_in(&store, &quiet).unwrap(), ["pruned"]);
        assert!(!meta_path(&store, "pruned").exists());
        assert!(
            meta_path(&store, "used").exists(),
            "a volume in use keeps it"
        );
    }

    #[test]
    fn a_volume_is_looked_up_by_its_name() {
        let (_tmp, store) = store();
        make(&store, "data");
        assert_eq!(get(&store, "data").unwrap().name, "data");
        for missing in ["nope", "bad name", "../x", ".hidden"] {
            let e = get(&store, missing).unwrap_err();
            assert_eq!(
                crate::api::kind_of(&e),
                Some(FailureKind::NoSuchVolume),
                "{missing}"
            );
            assert_eq!(format!("{e:#}"), format!("no volume named {missing}"));
        }
    }

    #[test]
    fn a_volume_made_without_a_name_gets_64_hex_digits() {
        let one = random_name().unwrap();
        assert_eq!(one.len(), 64);
        assert!(one
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_ne!(one, random_name().unwrap());
        assert!(validate_volume_name(&one).is_ok());
    }
}
