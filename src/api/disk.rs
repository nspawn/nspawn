//! What the store holds by kind, as system df counts it, and the image prune:
//! records are both images and machines, so an image here is a record pulled or built
//! and never started, and a machine is anything else.

use std::collections::{BTreeMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::Result;

use crate::api::{line, note, Context, Report};
use crate::backend::BackendChoice;
use crate::store::{ImageRecord, Ownership, Store};

/// A record pulled or built and never run as a machine. Records of releases that did not
/// keep the first start (written again by one after a downgrade too) say nothing of it:
/// an address, options only start gives (a network among them), or a writable layer with
/// something in it say it ran, whatever else does. A flat one writes into the image's own
/// tree: made since the first starts are kept, its record says; before, it is taken for a
/// machine.
pub fn image_only(store: &Store, record: &ImageRecord) -> bool {
    if record.origin == "create" || record.started.is_some() || record.address.is_some() {
        return false;
    }
    let given = !record.env.is_empty()
        || !record.volumes.is_empty()
        || !record.ports.is_empty()
        || !record.labels.is_empty()
        || record.entrypoint.is_some()
        || record.cmd.is_some()
        || record.healthcheck.is_some()
        || record.restart != crate::policy::Restart::default()
        || record.limits != crate::policy::Limits::default()
        || record.tuning != crate::tuning::Tuning::default()
        || record.network != crate::settings::Network::Bridge
        || record.no_network
        || record.network_name.is_some()
        || record.network_container.is_some()
        || !record.extra_networks.is_empty()
        || !record.aliases.is_empty()
        || !record.fixed_addresses.is_empty();
    if given {
        return false;
    }
    let writable = match record.backend {
        BackendChoice::Overlay => store
            .machines_private_dir()
            .join(&record.name)
            .join("upper"),
        BackendChoice::Mstack => store
            .machines_dir
            .join(format!("{}.mstack", record.name))
            .join("rw"),
        BackendChoice::Flat | BackendChoice::Auto => {
            return store
                .starts_kept_since()
                .is_some_and(|since| record.created >= since)
        }
    };
    std::fs::read_dir(writable).map_or(true, |mut d| d.next().is_none())
}

/// Bytes of the regular files under `path`, each file once however many links it has.
pub fn tree_size(path: &Path, seen: &mut HashSet<(u64, u64)>) -> u64 {
    let Ok(meta) = path.symlink_metadata() else {
        return 0;
    };
    if meta.is_file() {
        return if seen.insert((meta.dev(), meta.ino())) {
            meta.len()
        } else {
            0
        };
    }
    if !meta.is_dir() {
        return 0;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries.flatten().map(|e| tree_size(&e.path(), seen)).sum()
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Kind {
    pub total: usize,
    pub active: usize,
    pub size: u64,
    pub reclaimable: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRow {
    pub name: String,
    pub reference: String,
    pub created: u64,
    pub size: u64,
    /// Machines made from it.
    pub containers: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineRow {
    pub name: String,
    pub reference: String,
    /// Its writable layer.
    pub size: u64,
    pub running: bool,
    /// Made from an image by create or run. A pulled or built record that ran is the
    /// image as well, which a prune of machines leaves alone.
    pub made: bool,
}

impl MachineRow {
    /// What system prune removes.
    pub fn prunable(&self) -> bool {
        self.made && !self.running
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeRow {
    pub name: String,
    pub links: usize,
    pub size: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Usage {
    pub images: Kind,
    pub containers: Kind,
    pub volumes: Kind,
    pub build_cache: Kind,
    pub image_rows: Vec<ImageRow>,
    pub machine_rows: Vec<MachineRow>,
    pub volume_rows: Vec<VolumeRow>,
}

fn layer_size(store: &Store, digest: &str, sizes: &mut BTreeMap<String, u64>) -> u64 {
    if let Some(size) = sizes.get(digest) {
        return *size;
    }
    let mut seen = HashSet::new();
    let size = [Ownership::Root, Ownership::Foreign]
        .iter()
        .map(|o| tree_size(&store.layer_dir(digest, *o), &mut seen))
        .sum();
    sizes.insert(digest.to_string(), size);
    size
}

/// What a machine wrote, where its backend keeps it: the overlay's upper layer, the
/// mstack's rw tree, a flat machine's whole tree, which is its image as well.
fn writable_size(store: &Store, record: &ImageRecord) -> u64 {
    let path = match record.backend {
        BackendChoice::Overlay => store
            .machines_private_dir()
            .join(&record.name)
            .join("upper"),
        BackendChoice::Mstack => store
            .machines_dir
            .join(format!("{}.mstack", record.name))
            .join("rw"),
        BackendChoice::Flat | BackendChoice::Auto => store.machines_dir.join(&record.name),
    };
    tree_size(&path, &mut HashSet::new())
}

pub async fn usage(ctx: &Context) -> Result<Usage> {
    let store = &ctx.store;
    let sd = ctx.sd().await?;
    let running: HashSet<String> = sd
        .list_machines()
        .await?
        .into_iter()
        .map(|m| m.name)
        .collect();
    let records = store.list_images()?;
    let (images, machines): (Vec<&ImageRecord>, Vec<&ImageRecord>) =
        records.iter().partition(|r| image_only(store, r));
    let mut usage = Usage::default();
    (usage.images, usage.image_rows) = images_usage(store, &records, &machines);

    for r in &machines {
        let size = writable_size(store, r);
        let row = MachineRow {
            name: r.name.clone(),
            reference: r.reference.clone(),
            size,
            running: running.contains(&r.name),
            made: r.origin == "create",
        };
        usage.containers.total += 1;
        usage.containers.size += size;
        if row.running {
            usage.containers.active += 1;
        }
        if row.prunable() {
            usage.containers.reclaimable += size;
        }
        usage.machine_rows.push(row);
    }
    let _ = images;

    for row in crate::build::cache::usage(store)? {
        usage.build_cache.total += 1;
        usage.build_cache.size += row.size;
        if row.shared {
            usage.build_cache.active += 1;
        } else {
            usage.build_cache.reclaimable += row.size;
        }
    }
    for v in crate::api::volumes::list(store)? {
        let mut seen = HashSet::new();
        let size = tree_size(&v.path, &mut seen);
        usage.volumes.total += 1;
        usage.volumes.size += size;
        if v.used_by.is_empty() {
            usage.volumes.reclaimable += size;
        } else {
            usage.volumes.active += 1;
        }
        usage.volume_rows.push(VolumeRow {
            name: v.name,
            links: v.used_by.len(),
            size,
        });
    }
    Ok(usage)
}

/// The images of `records` as df counts them: an image is a manifest, what records of it
/// share, active while a machine of it exists. A layer counts once, and is reclaimable
/// when no machine uses it.
fn images_usage(
    store: &Store,
    records: &[ImageRecord],
    machines: &[&ImageRecord],
) -> (Kind, Vec<ImageRow>) {
    let mut sizes = BTreeMap::new();
    let mut kind = Kind::default();
    let mut rows = Vec::new();
    let mut by_manifest: BTreeMap<&str, Vec<&ImageRecord>> = BTreeMap::new();
    for r in records {
        by_manifest
            .entry(r.manifest_digest.as_str())
            .or_default()
            .push(r);
    }
    let flat = |r: &ImageRecord| matches!(r.backend, BackendChoice::Flat | BackendChoice::Auto);
    // A flat machine is a tree of its own: the layers under layers/ are nothing to it.
    let machine_layers: HashSet<&str> = machines
        .iter()
        .filter(|r| !flat(r))
        .flat_map(|r| r.layers.iter().map(String::as_str))
        .collect();
    let mut counted = HashSet::new();
    for group in by_manifest.values() {
        let containers = group.iter().filter(|r| !image_only(store, r)).count();
        kind.total += 1;
        if containers > 0 {
            kind.active += 1;
        }
        // The layers under layers/ serve every record of the group that is not flat, each
        // counted once; a flat image is a tree of its own, its layers kept nowhere else.
        let mut size = 0;
        if let Some(layered) = group.iter().find(|r| !flat(r)) {
            for layer in &layered.layers {
                let s = layer_size(store, layer, &mut sizes);
                size += s;
                if counted.insert(layer.as_str()) {
                    kind.size += s;
                    if !machine_layers.contains(layer.as_str()) {
                        kind.reclaimable += s;
                    }
                }
            }
        }
        for r in group.iter().filter(|r| flat(r) && image_only(store, r)) {
            let s = tree_size(&store.machines_dir.join(&r.name), &mut HashSet::new());
            size += s;
            kind.size += s;
            kind.reclaimable += s;
        }
        let shown = group
            .iter()
            .find(|r| image_only(store, r))
            .unwrap_or(&group[0]);
        rows.push(ImageRow {
            name: shown.name.clone(),
            reference: shown.reference.clone(),
            created: shown.created,
            size,
            containers,
        });
    }
    (kind, rows)
}

/// The images `images prune` takes: with `all`, every image no machine was made from
/// (none shares its manifest); without, the dangling ones, which nspawn's store never
/// has, as a tag pulled or built again replaces its record.
pub fn prunable(store: &Store, records: &[ImageRecord], all: bool) -> Vec<String> {
    if !all {
        return Vec::new();
    }
    records
        .iter()
        .filter(|r| image_only(store, r))
        .filter(|r| {
            !records.iter().any(|m| {
                m.name != r.name && m.manifest_digest == r.manifest_digest && !image_only(store, m)
            })
        })
        .map(|r| r.name.clone())
        .collect()
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pruned {
    pub removed: Vec<String>,
    pub reclaimed: u64,
}

/// images prune: removes what `prunable` names, matching `labels` (KEY or KEY=VALUE, all
/// of them) and made before `until` (unix seconds) when given.
pub async fn prune_images(
    ctx: &Context,
    all: bool,
    labels: &[String],
    until: Option<u64>,
    report: Report<'_>,
) -> Result<Pruned> {
    let store = &ctx.store;
    let records = store.list_images()?;
    let names: Vec<String> = prunable(store, &records, all)
        .into_iter()
        .filter(|name| {
            let Some(r) = records.iter().find(|r| &r.name == name) else {
                return false;
            };
            let have = r.effective_labels();
            labels.iter().all(|l| match l.split_once('=') {
                Some((k, v)) => have.get(k).is_some_and(|h| h == v),
                None => have.contains_key(l),
            }) && until.is_none_or(|t| r.created < t)
        })
        .collect();
    if names.is_empty() {
        return Ok(Pruned::default());
    }
    // What each layer of them takes, its tree and its blob, measured before anything
    // goes: what went is known only after the removals, some of which may be refused.
    let mut sizes = BTreeMap::new();
    let mut layer_bytes: BTreeMap<String, u64> = BTreeMap::new();
    for r in records.iter().filter(|r| names.contains(&r.name)) {
        for layer in &r.layers {
            if !layer_bytes.contains_key(layer) {
                let size = layer_size(store, layer, &mut sizes)
                    + std::fs::metadata(store.blob_path(layer)).map_or(0, |m| m.len());
                layer_bytes.insert(layer.clone(), size);
            }
        }
    }
    // Chosen without the lock: one that a machine was made from or that ran since, and
    // so is no image only any more, stays.
    let still = move |store: &Store, records: &[ImageRecord], name: &str| {
        (!prunable(store, records, all).iter().any(|n| n == name))
            .then(|| format!("{name} is in use by now"))
    };
    let removal =
        crate::api::images::remove_machines_if(ctx, &names, false, report, &still).await?;
    for name in &removal.removed {
        line(report, format!("deleted: {name}"));
    }
    // One that is still taken (a machine depends on it, say) stays; the rest go.
    if let Some(e) = removal.error() {
        note(report, format!("warning: {e}"));
    }
    // What went: the layers no record left, nor the build cache, refers to.
    let staying: HashSet<String> = store
        .list_images()
        .unwrap_or_default()
        .iter()
        .flat_map(|r| r.layers.iter().cloned())
        .chain(crate::build::cache::Cache::new(&store.build_cache_dir()).blobs())
        .collect();
    let reclaimed = layer_bytes
        .iter()
        .filter(|(layer, _)| !staying.contains(*layer))
        .map(|(_, size)| size)
        .sum();
    Ok(Pruned {
        removed: removal.removed,
        reclaimed,
    })
}

/// builder prune: the cache rows `prunable` selects removed, the blobs and layers
/// nothing keeps any more with them; what it freed.
pub async fn prune_build_cache(
    ctx: &Context,
    all: bool,
    until: Option<u64>,
    keep_storage: Option<u64>,
    report: Report<'_>,
) -> Result<Pruned> {
    let store = &ctx.store;
    // Measured before the lock, which machine starts wait for: the sizes only decide
    // what goes, and a cache mount in use waits for its build below.
    let rows = crate::build::cache::usage(store)?;
    let trash = store
        .build_cache_dir()
        .join(format!(".trash-{}", crate::store::unique_suffix()));
    let (gone, removed, tidy) = {
        let _lock = store.lock().await?;
        // A build running now may have a cache mount bound in a step: those wait for it.
        // A Dockerfile build's directory, by its marker: an mkosi build of a name that
        // starts with df- is none.
        let building = std::fs::read_dir(store.builds_dir()).is_ok_and(|mut d| {
            d.any(|e| {
                e.is_ok_and(|e| {
                    e.file_name().to_string_lossy().starts_with("df-")
                        && e.path().join(crate::build::exec::MARKER).exists()
                })
            })
        });
        let held = |r: &crate::build::cache::Usage| building && r.kind == "mount";
        let gone = crate::build::cache::prunable(&rows, all, until, keep_storage, &held);
        let removed = crate::build::cache::remove(store, &gone, &trash);
        // What nothing keeps any more goes too, off the runtime's workers; trouble with
        // another image's files does not undo what was pruned.
        let tidy = match &removed {
            Ok(()) => tokio::task::block_in_place(|| {
                store.gc_layers().and_then(|_| store.gc_blobs()).map(|_| ())
            }),
            Err(_) => Ok(()),
        };
        (gone, removed, tidy)
    };
    // The mounts set aside go whatever happened.
    let _ = tokio::task::spawn_blocking(move || crate::store::remove_tree(trash)).await;
    removed?;
    for row in &gone {
        line(report, format!("deleted: {}", row.id));
    }
    if let Err(e) = tidy {
        note(report, format!("warning: {e:#}"));
    }
    // What images take too stays on disk.
    Ok(Pruned {
        reclaimed: gone.iter().filter(|r| !r.shared).map(|r| r.size).sum(),
        removed: gone.into_iter().map(|r| r.id).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(json: serde_json::Value) -> ImageRecord {
        let mut base = serde_json::json!({
            "name": "x", "reference": "r", "manifest_digest": "sha256:a", "layers": ["sha256:l1"],
            "backend": "overlay", "created": 1, "origin": "pull",
        });
        for (k, v) in json.as_object().unwrap() {
            base[k] = v.clone();
        }
        serde_json::from_value(base).unwrap()
    }

    #[test]
    fn an_image_is_sized_by_its_own_backend_whatever_its_machines() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(&dir.path().join("m"), &dir.path().join("s"));
        let layer = format!("sha256:{}", "c".repeat(64));
        let layer_dir = store.layer_dir(&layer, Ownership::Root);
        std::fs::create_dir_all(&layer_dir).unwrap();
        std::fs::write(layer_dir.join("f"), vec![0u8; 4096]).unwrap();
        // A flat machine sorts first, made from an overlay image of the same manifest.
        let machine = record(serde_json::json!({
            "name": "abc", "backend": "flat", "origin": "create", "layers": [layer.clone()],
        }));
        let image = record(serde_json::json!({
            "name": "nginx", "backend": "overlay", "layers": [layer.clone()],
        }));
        let records = vec![machine.clone(), image];
        let (kind, rows) = images_usage(&store, &records, &[&machine]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "nginx");
        assert!(rows[0].size >= 4096, "{rows:?}");
        assert_eq!((kind.total, kind.active), (1, 1));
        assert!(kind.size >= 4096 && kind.reclaimable >= 4096, "{kind:?}");
    }

    #[test]
    fn images_and_machines() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(&dir.path().join("m"), &dir.path().join("s"));
        let image = record(serde_json::json!({ "name": "img" }));
        assert!(image_only(&store, &image));
        assert!(!image_only(
            &store,
            &record(serde_json::json!({ "origin": "create" }))
        ));
        assert!(!image_only(
            &store,
            &record(serde_json::json!({ "started": 5 }))
        ));
        assert!(!image_only(
            &store,
            &record(serde_json::json!({ "address": "10.99.0.2" }))
        ));
        // An older record that wrote into its writable layer ran.
        let ran = record(serde_json::json!({ "name": "ran" }));
        let upper = store.machines_private_dir().join("ran").join("upper");
        std::fs::create_dir_all(&upper).unwrap();
        assert!(image_only(&store, &ran));
        std::fs::write(upper.join("f"), "x").unwrap();
        assert!(!image_only(&store, &ran));
        // Options only start gives say a machine.
        assert!(!image_only(
            &store,
            &record(serde_json::json!({ "env": ["A=b"] }))
        ));
        assert!(!image_only(
            &store,
            &record(serde_json::json!({ "restart": "always" }))
        ));
        // Flat and mstack records of older releases: a flat one is a machine, an mstack
        // one ran when its writable layer has something.
        let flat = record(serde_json::json!({ "name": "f", "backend": "flat" }));
        assert!(!image_only(&store, &flat));
        let mstack = record(serde_json::json!({ "name": "s", "backend": "mstack" }));
        let rw = store.machines_dir.join("s.mstack/rw");
        std::fs::create_dir_all(&rw).unwrap();
        assert!(image_only(&store, &mstack));
        std::fs::write(rw.join("f"), "x").unwrap();
        assert!(!image_only(&store, &mstack));
        // Records made once first starts are kept say it by lacking one.
        std::fs::create_dir_all(dir.path().join("s")).unwrap();
        store.keep_starts().unwrap();
        let since = store.starts_kept_since().unwrap();
        let new = |backend: &str| {
            record(serde_json::json!({ "name": "n", "backend": backend, "created": since }))
        };
        assert!(image_only(&store, &new("flat")));
        assert!(!image_only(
            &store,
            &record(serde_json::json!({ "name": "s", "backend": "mstack" }))
        ));
        store.keep_starts().unwrap();
        assert_eq!(store.starts_kept_since(), Some(since));
        // One an older release wrote again after it ran, its first start lost: what it
        // wrote says so, made since or not.
        let rewritten = record(serde_json::json!({ "name": "ran", "created": since }));
        assert!(!image_only(&store, &rewritten));
        // A network chosen at a start of an older release is one of start's options.
        for network in [
            serde_json::json!({ "network": "host" }),
            serde_json::json!({ "no_network": true }),
            serde_json::json!({ "network_container": "db" }),
            serde_json::json!({ "network_name": "front" }),
        ] {
            assert!(!image_only(&store, &record(network.clone())), "{network}");
        }
    }

    #[test]
    fn a_machine_is_measured_where_its_backend_writes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(&dir.path().join("m"), &dir.path().join("s"));
        let write = |path: std::path::PathBuf, bytes: usize| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, vec![b'x'; bytes]).unwrap();
        };
        write(store.machines_private_dir().join("o/upper/f"), 10);
        write(store.machines_dir.join("s.mstack/rw/f"), 20);
        write(store.machines_dir.join("f/etc/f"), 30);
        let of = |name: &str, backend: &str| {
            writable_size(
                &store,
                &record(serde_json::json!({ "name": name, "backend": backend })),
            )
        };
        assert_eq!(of("o", "overlay"), 10);
        assert_eq!(of("s", "mstack"), 20);
        assert_eq!(of("f", "flat"), 30);
    }

    #[test]
    fn prune_takes_images_nothing_was_made_from() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(&dir.path().join("m"), &dir.path().join("s"));
        let used = record(serde_json::json!({ "name": "used", "manifest_digest": "sha256:u" }));
        let machine = record(
            serde_json::json!({ "name": "web", "manifest_digest": "sha256:u", "origin": "create" }),
        );
        let lone = record(serde_json::json!({ "name": "lone", "manifest_digest": "sha256:l" }));
        let records = vec![used, machine, lone];
        assert!(
            prunable(&store, &records, false).is_empty(),
            "nothing dangles"
        );
        assert_eq!(prunable(&store, &records, true), ["lone"]);
    }

    #[test]
    fn a_prune_of_machines_leaves_pulled_records_alone() {
        let row = |made, running| MachineRow {
            name: "m".into(),
            reference: "r".into(),
            size: 1,
            running,
            made,
        };
        assert!(row(true, false).prunable());
        assert!(!row(true, true).prunable());
        // A pulled image that was started is still the image.
        assert!(!row(false, false).prunable());
    }

    #[test]
    fn sizes_count_a_file_once() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), "12345").unwrap();
        std::fs::hard_link(dir.path().join("a"), dir.path().join("b")).unwrap();
        std::fs::create_dir(dir.path().join("d")).unwrap();
        std::fs::write(dir.path().join("d/c"), "123").unwrap();
        std::os::unix::fs::symlink("a", dir.path().join("s")).unwrap();
        assert_eq!(tree_size(dir.path(), &mut HashSet::new()), 8);
    }
}
