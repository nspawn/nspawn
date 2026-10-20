//! What the store holds by kind, as system df counts it, and the image prune:
//! records are both images and machines, so an image here is a record pulled or built
//! and never started, and a machine is anything else.

use std::collections::{BTreeMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::Result;

use crate::api::{line, note, Context, Report};
use crate::store::{ImageRecord, Ownership, Store};

/// A record pulled or built and never run as a machine. Records of releases before the
/// first start was kept say nothing of it: an address, options only start gives, or a
/// writable layer with something in it say it ran.
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
        || record.tuning != crate::tuning::Tuning::default();
    if given {
        return false;
    }
    let upper = store
        .machines_private_dir()
        .join(&record.name)
        .join("upper");
    std::fs::read_dir(upper).map_or(true, |mut d| d.next().is_none())
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
    let mut sizes = BTreeMap::new();
    let mut usage = Usage::default();

    // An image is a manifest: what records of it share, active while a machine of it
    // exists. A layer counts once, and is reclaimable when no machine uses it.
    let mut by_manifest: BTreeMap<&str, Vec<&ImageRecord>> = BTreeMap::new();
    for r in &records {
        by_manifest
            .entry(r.manifest_digest.as_str())
            .or_default()
            .push(r);
    }
    let machine_layers: HashSet<&str> = machines
        .iter()
        .flat_map(|r| r.layers.iter().map(String::as_str))
        .collect();
    let mut counted = HashSet::new();
    for (digest, group) in &by_manifest {
        let containers = group.iter().filter(|r| !image_only(store, r)).count();
        let size: u64 = group[0]
            .layers
            .iter()
            .map(|l| layer_size(store, l, &mut sizes))
            .sum();
        usage.images.total += 1;
        if containers > 0 {
            usage.images.active += 1;
        }
        for layer in &group[0].layers {
            if counted.insert(layer.as_str()) {
                let s = layer_size(store, layer, &mut sizes);
                usage.images.size += s;
                if !machine_layers.contains(layer.as_str()) {
                    usage.images.reclaimable += s;
                }
            }
        }
        let shown = group
            .iter()
            .find(|r| image_only(store, r))
            .unwrap_or(&group[0]);
        usage.image_rows.push(ImageRow {
            name: shown.name.clone(),
            reference: shown.reference.clone(),
            created: shown.created,
            size,
            containers,
        });
        let _ = digest;
    }

    for r in &machines {
        let mut seen = HashSet::new();
        let size = tree_size(
            &store.machines_private_dir().join(&r.name).join("upper"),
            &mut seen,
        );
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
    // What each takes with it: its layers and blobs that nothing staying, nor the build
    // cache, refers to; counted before they go.
    let staying: HashSet<String> = records
        .iter()
        .filter(|r| !names.contains(&r.name))
        .flat_map(|r| r.layers.iter().cloned())
        .chain(crate::build::cache::Cache::new(&store.build_cache_dir()).blobs())
        .collect();
    let mut sizes = BTreeMap::new();
    let mut freed = HashSet::new();
    let mut own: BTreeMap<String, u64> = BTreeMap::new();
    for r in records.iter().filter(|r| names.contains(&r.name)) {
        let mut size = 0;
        for layer in &r.layers {
            if !staying.contains(layer) && freed.insert(layer.clone()) {
                size += layer_size(store, layer, &mut sizes);
                size += std::fs::metadata(store.blob_path(layer)).map_or(0, |m| m.len());
            }
        }
        own.insert(r.name.clone(), size);
    }
    let removal = crate::api::images::remove_machines(ctx, &names, false, report).await?;
    for name in &removal.removed {
        line(report, format!("deleted: {name}"));
    }
    // One that is still taken (a machine depends on it, say) stays; the rest go.
    if let Some(e) = removal.error() {
        note(report, format!("warning: {e}"));
    }
    let reclaimed = removal.removed.iter().filter_map(|n| own.get(n)).sum();
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
    let _lock = store.lock().await?;
    let rows = crate::build::cache::usage(store)?;
    // A build running now may have a cache mount bound in a step: those wait for it.
    let building = std::fs::read_dir(store.builds_dir()).is_ok_and(|mut d| {
        d.any(|e| e.is_ok_and(|e| e.file_name().to_string_lossy().starts_with("df-")))
    });
    let gone: Vec<_> = crate::build::cache::prunable(&rows, all, until, keep_storage)
        .into_iter()
        .filter(|r| !(building && r.kind == "mount"))
        .collect();
    crate::build::cache::remove(store, &gone)?;
    store.gc_layers()?;
    store.gc_blobs()?;
    for row in &gone {
        line(report, format!("deleted: {}", row.id));
    }
    Ok(Pruned {
        reclaimed: gone.iter().map(|r| r.size).sum(),
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
