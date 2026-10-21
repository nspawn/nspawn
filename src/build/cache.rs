//! The build cache: a layer each step made, by a key of the step and everything it
//! depends on, and the base images builds took from registries. What it names stays in
//! the store until `builder prune` lets it go.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A step that made a layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// The gzip blob, as the store names layers.
    pub layer: String,
    pub diff_id: String,
    pub size: u64,
    /// When the step ran: the history entry keeps it, so that a build taken whole from
    /// the cache gives the same image.
    pub created: String,
    /// Unix seconds of its last use.
    #[serde(default)]
    pub used: u64,
    /// The stage it was made in, for --no-cache-filter and the listing.
    #[serde(default)]
    pub description: String,
    /// The stage as the step left it. A build that takes the entry goes on from there,
    /// though a step before it ran again since and made something else: keys follow
    /// what the steps are, as BuildKit's do, and an entry without one is taken by no
    /// build.
    #[serde(default)]
    pub stage: Option<Stage>,
}

/// A stage's layers, bottom first and the step's own last, and its history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stage {
    pub layers: Vec<StageLayer>,
    pub history: Vec<crate::build::image::History>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageLayer {
    pub digest: String,
    pub media_type: String,
    pub size: u64,
    pub diff_id: String,
}

/// A base image a build pulled: its config and layers, kept with the cache.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Base {
    pub reference: String,
    pub manifest_digest: String,
    pub config: String,
    pub layers: Vec<String>,
    #[serde(default)]
    pub used: u64,
}

/// The key of a step: the parts it depends on, each length-prefixed so that no two
/// lists of parts give the same bytes.
pub fn key(parts: &[&[u8]]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update((part.len() as u64).to_le_bytes());
        hash.update(part);
    }
    format!("sha256:{}", hex::encode(hash.finalize()))
}

#[derive(Debug, Clone)]
pub struct Cache {
    dir: PathBuf,
}

fn file_name(key: &str) -> String {
    key.replacen(':', "-", 1)
}

impl Cache {
    pub fn new(dir: &Path) -> Cache {
        Cache {
            dir: dir.to_path_buf(),
        }
    }

    fn entries_dir(&self) -> PathBuf {
        self.dir.join("entries")
    }

    fn bases_dir(&self) -> PathBuf {
        self.dir.join("bases")
    }

    /// The entry of `key`, its use noted.
    pub fn get(&self, key: &str) -> Option<Entry> {
        let path = self.entries_dir().join(format!("{}.json", file_name(key)));
        let mut entry: Entry = serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
        entry.used = crate::store::now_unix();
        let _ = self.write(&path, &entry);
        Some(entry)
    }

    pub fn put(&self, key: &str, entry: &Entry) -> Result<()> {
        let dir = self.entries_dir();
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        self.write(&dir.join(format!("{}.json", file_name(key))), entry)
    }

    pub fn put_base(&self, base: &Base) -> Result<()> {
        let dir = self.bases_dir();
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        self.write(
            &dir.join(format!("{}.json", file_name(&base.manifest_digest))),
            base,
        )
    }

    fn write<T: Serialize>(&self, path: &Path, value: &T) -> Result<()> {
        // Two builds may write one entry at once: each through a file of its own.
        let tmp = path.with_extension(format!("json.tmp-{}", crate::store::unique_suffix()));
        std::fs::write(&tmp, serde_json::to_vec(value)?)
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
    }

    /// Every entry, with its file.
    pub fn entries(&self) -> Vec<(PathBuf, Entry)> {
        read_all(&self.entries_dir())
    }

    pub fn bases(&self) -> Vec<(PathBuf, Base)> {
        read_all(&self.bases_dir())
    }

    /// The blobs the cache keeps: its layers and those of the stages they were made in,
    /// and the configs and layers of its bases.
    pub fn blobs(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (_, entry) in self.entries() {
            out.push(entry.layer);
            out.extend(
                entry
                    .stage
                    .into_iter()
                    .flat_map(|s| s.layers)
                    .map(|l| l.digest),
            );
        }
        for (_, base) in self.bases() {
            out.push(base.config);
            out.extend(base.layers);
        }
        out
    }
}

/// A row of `builder du`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Usage {
    /// The entry's key, or the cache mount's or base image's name.
    pub id: String,
    /// layer, mount or base.
    pub kind: &'static str,
    pub description: String,
    pub size: u64,
    pub used: u64,
    /// Whether an image of the store takes the blob too.
    pub shared: bool,
}

/// What the cache holds, most recently used first.
pub fn usage(store: &crate::store::Store) -> Result<Vec<Usage>> {
    let cache = Cache::new(&store.build_cache_dir());
    let in_images: std::collections::HashSet<String> = store
        .list_images()?
        .into_iter()
        .flat_map(|r| r.layers)
        .collect();
    let blob_size = |d: &str| std::fs::metadata(store.blob_path(d)).map_or(0, |m| m.len());
    let mut out = Vec::new();
    for (path, entry) in cache.entries() {
        out.push(Usage {
            id: path
                .file_stem()
                .map(|s| s.to_string_lossy().replacen('-', ":", 1))
                .unwrap_or_default(),
            kind: "layer",
            description: entry.description.clone(),
            size: blob_size(&entry.layer),
            used: entry.used,
            shared: in_images.contains(&entry.layer),
        });
    }
    for (_, base) in cache.bases() {
        let shared = base.layers.iter().all(|l| in_images.contains(l));
        out.push(Usage {
            id: base.manifest_digest.clone(),
            kind: "base",
            description: base.reference.clone(),
            size: base.layers.iter().map(|l| blob_size(l)).sum::<u64>() + blob_size(&base.config),
            used: base.used,
            shared,
        });
    }
    if let Ok(mounts) = std::fs::read_dir(cache.dir.join("mounts")) {
        // A mount being seeded (.new-*) is no cache yet.
        for mount in mounts
            .flatten()
            .filter(|m| !m.file_name().to_string_lossy().starts_with('.'))
        {
            let meta = mount.metadata().ok();
            out.push(Usage {
                id: mount.file_name().to_string_lossy().into_owned(),
                kind: "mount",
                description: "RUN --mount=type=cache".to_string(),
                size: crate::api::disk::tree_size(&mount.path(), &mut Default::default()),
                used: meta.and_then(|m| m.modified().ok()).map_or(0, |t| {
                    t.duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| d.as_secs())
                }),
                shared: false,
            });
        }
    }
    out.sort_by(|a, b| b.used.cmp(&a.used).then(a.id.cmp(&b.id)));
    Ok(out)
}

/// What `builder prune` takes, as BuildKit's prune: without `all`, what no image takes
/// too; only what was last used before `until`; and with `keep_storage`, the least
/// recently used first, until what the cache holds of its own fits in that many bytes.
/// `kept` rows are never taken (a cache mount a build has bound, say), and still count
/// against `keep_storage`: they stay on disk.
pub fn prunable(
    rows: &[Usage],
    all: bool,
    until: Option<u64>,
    keep_storage: Option<u64>,
    kept: &dyn Fn(&Usage) -> bool,
) -> Vec<Usage> {
    let eligible =
        |row: &&Usage| (all || !row.shared) && until.is_none_or(|t| row.used < t) && !kept(row);
    let Some(limit) = keep_storage else {
        return rows.iter().filter(eligible).cloned().collect();
    };
    // What images take too is theirs: it counts against nothing.
    let mut total: u64 = rows.iter().filter(|r| !r.shared).map(|r| r.size).sum();
    let mut candidates: Vec<&Usage> = rows.iter().filter(eligible).collect();
    candidates.sort_by(|a, b| a.used.cmp(&b.used).then(a.id.cmp(&b.id)));
    let mut out = Vec::new();
    for row in candidates {
        if total <= limit {
            break;
        }
        if !row.shared {
            total -= row.size;
        }
        out.push(row.clone());
    }
    out
}

/// Removes `rows` from the cache; the store's gc then takes the blobs and layers nothing
/// else keeps. Cache mounts are moved into `trash`, for the caller to delete once it
/// holds the store lock no more.
pub fn remove(store: &crate::store::Store, rows: &[Usage], trash: &Path) -> Result<()> {
    let cache = Cache::new(&store.build_cache_dir());
    for row in rows {
        let path = match row.kind {
            "layer" => cache
                .entries_dir()
                .join(format!("{}.json", file_name(&row.id))),
            "base" => cache
                .bases_dir()
                .join(format!("{}.json", file_name(&row.id))),
            _ => {
                let dir = cache.dir.join("mounts").join(&row.id);
                std::fs::create_dir_all(trash)
                    .with_context(|| format!("creating {}", trash.display()))?;
                match std::fs::rename(&dir, trash.join(&row.id)) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e).with_context(|| format!("removing {}", dir.display())),
                }
                continue;
            }
        };
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {}", path.display())),
        }
    }
    Ok(())
}

fn read_all<T: serde::de::DeserializeOwned>(dir: &Path) -> Vec<(PathBuf, T)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(PathBuf, T)> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .filter_map(|p| {
            let value = serde_json::from_slice(&std::fs::read(&p).ok()?).ok()?;
            Some((p, value))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prune_takes_what_the_options_say() {
        let row = |id: &str, size, used, shared| Usage {
            id: id.into(),
            kind: "layer",
            description: String::new(),
            size,
            used,
            shared,
        };
        // Most recently used first, as `usage` sorts them.
        let rows = [
            row("a", 10, 30, false),
            row("b", 10, 20, true),
            row("c", 10, 10, false),
        ];
        let ids = |v: Vec<Usage>| v.into_iter().map(|r| r.id).collect::<Vec<_>>();
        let none = |_: &Usage| false;
        assert_eq!(ids(prunable(&rows, false, None, None, &none)), ["a", "c"]);
        assert_eq!(
            ids(prunable(&rows, true, None, None, &none)),
            ["a", "b", "c"]
        );
        assert_eq!(
            ids(prunable(&rows, true, Some(25), None, &none)),
            ["b", "c"]
        );
        // 20 bytes of the cache's own: within 25 nothing goes; within 15 the least
        // recently used goes first.
        assert!(prunable(&rows, true, None, Some(25), &none).is_empty());
        assert_eq!(ids(prunable(&rows, true, None, Some(15), &none)), ["c"]);
        assert_eq!(
            ids(prunable(&rows, true, None, Some(5), &none)),
            ["c", "b", "a"]
        );
        assert_eq!(
            ids(prunable(&rows, false, None, Some(5), &none)),
            ["c", "a"]
        );
        // An old mount a build holds stays, and the newer rows go in its place.
        let mut held = rows.to_vec();
        held.push(Usage {
            kind: "mount",
            ..row("m", 30, 1, false)
        });
        let building = |r: &Usage| r.kind == "mount";
        assert_eq!(
            ids(prunable(&held, false, None, Some(25), &building)),
            ["c", "a"]
        );
    }

    #[test]
    fn the_layers_a_step_was_made_on_stay_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path());
        let layer = |d: &str| StageLayer {
            digest: d.into(),
            media_type: String::new(),
            size: 1,
            diff_id: format!("diff-{d}"),
        };
        let entry = |own: &str, stage: Option<Vec<StageLayer>>| Entry {
            layer: own.into(),
            diff_id: format!("diff-{own}"),
            size: 1,
            created: String::new(),
            used: 0,
            description: String::new(),
            stage: stage.map(|layers| Stage {
                layers,
                history: Vec::new(),
            }),
        };
        // The step below ran again and its entry names another layer now: the one this
        // step was made on stays in the store for it.
        cache
            .put(
                "k2",
                &entry("l2", Some(vec![layer("base"), layer("l1"), layer("l2")])),
            )
            .unwrap();
        cache
            .put("k1", &entry("l1b", Some(vec![layer("base"), layer("l1b")])))
            .unwrap();
        let mut blobs = cache.blobs();
        blobs.sort();
        blobs.dedup();
        assert_eq!(blobs, ["base", "l1", "l1b", "l2"]);
        // An entry without a stage reads, and is taken by no build.
        std::fs::write(
            dir.path().join("entries").join("sha256-old.json"),
            r#"{"layer":"l0","diff_id":"d","size":1,"created":"x"}"#,
        )
        .unwrap();
        assert_eq!(cache.get("sha256:old").unwrap().stage, None);
    }

    #[test]
    fn keys_tell_lists_of_parts_apart() {
        assert_ne!(key(&[b"ab", b"c"]), key(&[b"a", b"bc"]));
        assert_eq!(key(&[b"x"]), key(&[b"x"]));
    }

    #[test]
    fn entries_and_bases_name_their_blobs() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path());
        assert!(cache.get("sha256:k").is_none());
        let entry = Entry {
            layer: "sha256:l".into(),
            diff_id: "sha256:d".into(),
            size: 3,
            created: "2026-10-18T00:00:00.000000000Z".into(),
            used: 0,
            description: "RUN true".into(),
            stage: None,
        };
        cache.put("sha256:k", &entry).unwrap();
        let got = cache.get("sha256:k").unwrap();
        assert_eq!(got.layer, "sha256:l");
        assert!(got.used > 0);
        cache
            .put_base(&Base {
                reference: "busybox".into(),
                manifest_digest: "sha256:m".into(),
                config: "sha256:c".into(),
                layers: vec!["sha256:b1".into()],
                used: 0,
            })
            .unwrap();
        let mut blobs = cache.blobs();
        blobs.sort();
        assert_eq!(blobs, ["sha256:b1", "sha256:c", "sha256:l"]);
    }
}
