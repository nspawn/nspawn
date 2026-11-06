//! Images as the Docker API shows them: one for each image config, which is what the
//! engine keys its images by, made of the records pulled or built that never ran and of
//! the records of the containers made from it. A reference names one image: the last
//! one pulled of the records that are images, or else of the containers.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Mutex;

use serde_json::{json, Map, Value};

use super::filters::{Filters, Invalid};
use super::util::familiar;
use crate::api::Context;
use crate::reference::ImageRef;
use crate::store::{ImageRecord, Store};

/// One image.
#[derive(Debug, Clone)]
pub struct Image {
    /// The config's digest, `sha256:...`.
    pub id: String,
    /// The references that name it, in full (`docker.io/library/busybox:1.37`).
    pub tags: BTreeSet<String>,
    /// `repository@digest` of the manifests it was pulled by, in full.
    pub digests: BTreeSet<String>,
    /// The records that hold it as an image, the last pulled first.
    pub records: Vec<String>,
    /// The containers made from it.
    pub containers: Vec<String>,
    /// The image config.
    pub config: Value,
    /// Its layers' digests, the lowest first.
    pub layers: Vec<String>,
    /// Bytes of its layers as extracted.
    pub size: u64,
    /// When the last of its records was made (unix seconds).
    pub pulled: u64,
}

impl Image {
    /// When the image was made, as its config says (unix seconds).
    pub fn created(&self) -> i64 {
        self.config
            .get("created")
            .and_then(Value::as_str)
            .and_then(crate::compose::gotime::parse_rfc3339)
            .unwrap_or(0)
    }

    pub fn labels(&self) -> BTreeMap<String, String> {
        self.config
            .pointer("/config/Labels")
            .and_then(Value::as_object)
            .map(|labels| {
                labels
                    .iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Neither a reference nor a record of its own names it.
    pub fn dangling(&self) -> bool {
        self.tags.is_empty()
    }

    pub fn repo_tags(&self) -> Vec<String> {
        self.tags.iter().map(|t| familiar(t)).collect()
    }

    pub fn repo_digests(&self) -> Vec<String> {
        self.digests.iter().map(|d| familiar(d)).collect()
    }
}

/// Bytes of each layer by its digest. A layer never changes once extracted, so what was
/// counted stays for as long as the server runs.
static LAYER_SIZES: Mutex<BTreeMap<String, u64>> = Mutex::new(BTreeMap::new());

/// Bytes of a record's image: its layers, or the tree of a flat one, whose layers are
/// kept nowhere else.
fn image_size(store: &Store, record: &ImageRecord) -> u64 {
    use crate::backend::BackendChoice;
    if matches!(record.backend, BackendChoice::Flat | BackendChoice::Auto) {
        let tree = store.machines_dir.join(&record.name);
        return crate::api::disk::tree_size(&tree, &mut std::collections::HashSet::new());
    }
    let mut sizes = LAYER_SIZES.lock().unwrap();
    record
        .layers
        .iter()
        .map(|digest| crate::api::disk::layer_size(store, digest, &mut sizes))
        .sum()
}

/// A record's reference in full, the way it names an image: None for one pinned by a
/// digest alone.
fn full_tag(reference: &str, registry: &str) -> Option<String> {
    let parsed = ImageRef::parse(reference, registry).ok()?;
    parsed.tag.as_ref()?;
    Some(
        ImageRef {
            digest: None,
            ..parsed
        }
        .to_string(),
    )
}

/// `repository@digest` of the manifest a record was pulled by, in full; a built image
/// was pulled by none.
fn full_digest(record: &ImageRecord, registry: &str) -> Option<String> {
    if record.origin == "build" {
        return None;
    }
    let parsed = ImageRef::parse(&record.reference, registry).ok()?;
    let digest = record
        .index_digest
        .clone()
        .unwrap_or_else(|| record.manifest_digest.clone());
    Some(
        ImageRef {
            tag: None,
            digest: Some(digest),
            ..parsed
        }
        .to_string(),
    )
}

/// Every image, the last made first, as the engine lists them.
pub fn list(ctx: &Context) -> anyhow::Result<Vec<Image>> {
    let store = &ctx.store;
    let registry = &ctx.config.registry;
    let mut images: BTreeMap<String, Image> = BTreeMap::new();
    // For each reference, the image of its last pulled record that is an image, and of
    // its last container as well.
    let mut named: HashMap<String, ((bool, u64), String)> = HashMap::new();
    let mut records = store.list_images()?;
    records.sort_by_key(|r| std::cmp::Reverse(r.created));
    for record in &records {
        let Ok(bytes) = store.load_manifest(&record.name) else {
            continue;
        };
        let Ok(manifest) = serde_json::from_slice::<oci_client::manifest::OciImageManifest>(&bytes)
        else {
            continue;
        };
        let id = manifest.config.digest.clone();
        let image = images.entry(id.clone()).or_insert_with(|| Image {
            id: id.clone(),
            tags: BTreeSet::new(),
            digests: BTreeSet::new(),
            records: Vec::new(),
            containers: Vec::new(),
            config: std::fs::read(store.blob_path(&id))
                .ok()
                .and_then(|blob| serde_json::from_slice(&blob).ok())
                .unwrap_or(Value::Null),
            layers: record.layers.clone(),
            size: image_size(store, record),
            pulled: record.created,
        });
        let is_image = crate::api::disk::image_only(store, record);
        if is_image {
            image.records.push(record.name.clone());
        } else {
            image.containers.push(record.name.clone());
        }
        image.pulled = image.pulled.max(record.created);
        if let Some(digest) = full_digest(record, registry) {
            image.digests.insert(digest);
        }
        if let Some(tag) = full_tag(&record.reference, registry) {
            let rank = (is_image, record.created);
            match named.get(&tag) {
                Some((best, _)) if *best >= rank => {}
                _ => {
                    named.insert(tag, (rank, id.clone()));
                }
            }
        }
    }
    for (tag, (_, id)) in named {
        if let Some(image) = images.get_mut(&id) {
            image.tags.insert(tag);
        }
    }
    let mut out: Vec<Image> = images.into_values().collect();
    out.sort_by_key(|i| std::cmp::Reverse((i.created(), i.pulled)));
    Ok(out)
}

/// The image `key` names, as the engine looks one up: a reference, then the config's
/// digest or a prefix of it, then the name of a record that is an image.
pub fn find<'a>(images: &'a [Image], key: &str) -> Result<&'a Image, String> {
    // Named as the engine names it: a reference with its tag, short.
    let missing = || match ImageRef::normalized(key) {
        Ok(wanted) if !key.starts_with("sha256:") => {
            format!("No such image: {}", familiar(&wanted.to_string()))
        }
        _ => format!("No such image: {key}"),
    };
    if key.is_empty() {
        return Err(missing());
    }
    if let Ok(wanted) = ImageRef::normalized(key) {
        let wanted_text = wanted.to_string();
        let found = match &wanted.digest {
            Some(_) if wanted.tag.is_none() => {
                images.iter().find(|i| i.digests.contains(&wanted_text))
            }
            Some(_) => None,
            None => images.iter().find(|i| i.tags.contains(&wanted_text)),
        };
        if let Some(image) = found {
            return Ok(image);
        }
    }
    let hex = key.strip_prefix("sha256:").unwrap_or(key);
    if !hex.is_empty() && hex.len() <= 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        let mut found = images
            .iter()
            .filter(|i| i.id.trim_start_matches("sha256:").starts_with(hex));
        match (found.next(), found.next()) {
            (Some(image), None) => return Ok(image),
            (Some(_), Some(_)) => {
                return Err(format!("multiple IDs found with provided prefix: {hex}"))
            }
            _ => {}
        }
    }
    images
        .iter()
        .find(|i| i.records.iter().any(|r| r == key))
        .ok_or_else(missing)
}

/// The keys GET /images/json takes.
const FILTERS: &[&str] = &[
    "before",
    "dangling",
    "label",
    "label!",
    "reference",
    "since",
    "until",
];

/// The images GET /images/json lists: with `all`, the intermediate ones too, which this
/// store has none of; `filters` as the engine reads them.
pub fn select<'a>(images: &'a [Image], filters: &Filters) -> Result<Vec<&'a Image>, Invalid> {
    filters.only(FILTERS)?;
    if let Some(values) = filters.values("dangling") {
        if let Some(bad) = values
            .iter()
            .find(|v| !matches!(v.as_str(), "true" | "false" | "1" | "0"))
        {
            return Err(Invalid(format!("invalid filter 'dangling={bad}'")));
        }
    }
    let point = |key: &str| -> Result<Option<i64>, Invalid> {
        let Some(values) = filters.values(key) else {
            return Ok(None);
        };
        let mut out = None;
        for value in values {
            let image = find(images, value).map_err(Invalid)?;
            out = Some(image.created());
        }
        Ok(out)
    };
    let before = point("before")?;
    let since = point("since")?;
    let mut out = Vec::new();
    for image in images {
        if let Some(values) = filters.values("dangling") {
            let want = values.iter().any(|v| v == "true" || v == "1");
            if image.dangling() != want {
                continue;
            }
        }
        if filters.values("label").is_some() && !filters.labels("label", &image.labels()) {
            continue;
        }
        if let Some(values) = filters.values("label!") {
            let labels = image.labels();
            if values.iter().any(|v| match v.split_once('=') {
                Some((k, val)) => labels.get(k).is_some_and(|l| l == val),
                None => labels.contains_key(v.as_str()),
            }) {
                continue;
            }
        }
        if let Some(values) = filters.values("reference") {
            let tags = image.repo_tags();
            let matched = values
                .iter()
                .any(|pattern| tags.iter().any(|tag| reference_matches(pattern, tag)));
            if !matched {
                continue;
            }
        }
        if before.is_some_and(|t| image.created() >= t)
            || since.is_some_and(|t| image.created() <= t)
        {
            continue;
        }
        out.push(image);
    }
    Ok(out)
}

/// Whether `tag` (familiar, `busybox:1.37`) matches a reference filter: a pattern of
/// the repository, with the tag when it gives one, `*` within one path component.
fn reference_matches(pattern: &str, tag: &str) -> bool {
    let (repository, version) = tag.rsplit_once(':').unwrap_or((tag, "latest"));
    let pattern_has_tag = pattern
        .rsplit_once(':')
        .is_some_and(|(_, t)| !t.contains('/'));
    if pattern_has_tag {
        glob(pattern, &format!("{repository}:{version}"))
    } else {
        glob(pattern, repository)
    }
}

/// path.Match's `*` and `?`, which do not cross a `/`.
fn glob(pattern: &str, text: &str) -> bool {
    fn go(p: &[u8], t: &[u8]) -> bool {
        match (p.first(), t.first()) {
            (None, None) => true,
            (Some(b'*'), _) => go(&p[1..], t) || (!t.is_empty() && t[0] != b'/' && go(p, &t[1..])),
            (Some(b'?'), Some(c)) if *c != b'/' => go(&p[1..], &t[1..]),
            (Some(a), Some(b)) if a == b => go(&p[1..], &t[1..]),
            _ => false,
        }
    }
    go(pattern.as_bytes(), text.as_bytes())
}

/// An entry of GET /images/json.
pub fn summary_json(image: &Image) -> Value {
    let labels = image.labels();
    json!({
        "Containers": image.containers.len(),
        "Created": image.created(),
        "Id": image.id,
        "Labels": if labels.is_empty() { Value::Null } else { json!(labels) },
        "ParentId": "",
        "RepoDigests": image.repo_digests(),
        "RepoTags": image.repo_tags(),
        "SharedSize": -1,
        "Size": image.size,
    })
}

/// The entries of GET /system/df: the images with their containers counted and the
/// size of the layers they share with another image, and the size of every layer once.
pub fn disk_usage(ctx: &Context, images: &[Image]) -> (Vec<Value>, u64) {
    let store = &ctx.store;
    let mut layers_of: Vec<Vec<String>> = Vec::new();
    let mut users: BTreeMap<String, usize> = BTreeMap::new();
    for image in images {
        let layers = image.layers.clone();
        for layer in layers.iter().collect::<BTreeSet<_>>() {
            *users.entry(layer.clone()).or_default() += 1;
        }
        layers_of.push(layers);
    }
    let mut sizes = LAYER_SIZES.lock().unwrap();
    let total = users
        .keys()
        .map(|layer| crate::api::disk::layer_size(store, layer, &mut sizes))
        .sum();
    let entries = images
        .iter()
        .zip(&layers_of)
        .map(|(image, layers)| {
            let shared: u64 = layers
                .iter()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .filter(|layer| users.get(*layer).is_some_and(|n| *n > 1))
                .map(|layer| crate::api::disk::layer_size(store, layer, &mut sizes))
                .sum();
            let mut entry = summary_json(image);
            entry["SharedSize"] = json!(shared);
            if image.digests.is_empty() {
                entry["RepoDigests"] = Value::Null;
            }
            entry
        })
        .collect();
    (entries, total)
}

/// GET /images/{name}/json.
pub fn inspect_json(store: &Store, image: &Image) -> Value {
    let config = &image.config;
    let text = |key: &str| config.get(key).and_then(Value::as_str).unwrap_or_default();
    let created = config
        .get("created")
        .and_then(Value::as_str)
        .unwrap_or("0001-01-01T00:00:00Z");
    // The keys the engine always gives, then what the image's config sets.
    let mut image_config = Map::new();
    for (key, value) in [
        ("Cmd", Value::Null),
        ("Entrypoint", Value::Null),
        ("Env", Value::Null),
        ("Labels", Value::Null),
        ("OnBuild", Value::Null),
        ("User", json!("")),
        ("Volumes", Value::Null),
        ("WorkingDir", json!("")),
    ] {
        image_config.insert(key.to_string(), value);
    }
    if let Some(Value::Object(c)) = config.get("config") {
        for (key, value) in c {
            image_config.insert(key.clone(), value.clone());
        }
    }
    json!({
        "Id": image.id,
        "RepoTags": image.repo_tags(),
        "RepoDigests": image.repo_digests(),
        "Parent": "",
        "Comment": text("comment"),
        "Created": created,
        "DockerVersion": text("docker_version"),
        "Author": text("author"),
        "Config": image_config,
        "Architecture": text("architecture"),
        "Os": text("os"),
        "Size": image.size,
        "GraphDriver": { "Data": layer_dirs(store, &image.layers), "Name": "overlay" },
        "RootFS": {
            "Type": "layers",
            "Layers": config.pointer("/rootfs/diff_ids").cloned().unwrap_or(json!([])),
        },
        "Metadata": { "LastTagTime": "0001-01-01T00:00:00Z" },
    })
}

/// Where the image's layers are: the top one, and those below it, the lowest last, as
/// overlay's options list them.
fn layer_dirs(store: &Store, layers: &[String]) -> Value {
    use crate::store::Ownership;
    let mut dirs = layers.iter().rev().map(|digest| {
        store
            .layer_dir(digest, Ownership::Root)
            .to_string_lossy()
            .into_owned()
    });
    let Some(upper) = dirs.next() else {
        return Value::Null;
    };
    let lower: Vec<String> = dirs.collect();
    let mut data = Map::new();
    if !lower.is_empty() {
        data.insert("LowerDir".into(), json!(lower.join(":")));
    }
    data.insert("UpperDir".into(), json!(upper));
    Value::Object(data)
}

/// GET /images/{name}/history: the config's history, the last step first, each step
/// that made a layer with that layer's size.
pub fn history_json(ctx: &Context, image: &Image) -> Value {
    let store = &ctx.store;
    let mut sizes = LAYER_SIZES.lock().unwrap();
    let mut layer = image.layers.iter();
    let mut steps: Vec<Value> = image
        .config
        .get("history")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|step| {
            let empty = step.get("empty_layer").and_then(Value::as_bool).unwrap_or(false);
            let size = if empty {
                0
            } else {
                layer
                    .next()
                    .map(|d| crate::api::disk::layer_size(store, d, &mut sizes))
                    .unwrap_or(0)
            };
            json!({
                "Id": "<missing>",
                "Created": step.get("created").and_then(Value::as_str).and_then(crate::compose::gotime::parse_rfc3339).unwrap_or(0),
                "CreatedBy": step.get("created_by").and_then(Value::as_str).unwrap_or_default(),
                "Tags": null,
                "Size": size,
                "Comment": step.get("comment").and_then(Value::as_str).unwrap_or_default(),
            })
        })
        .collect();
    steps.reverse();
    if let Some(Value::Object(top)) = steps.first_mut() {
        top.insert("Id".into(), json!(image.id));
        let tags = image.repo_tags();
        top.insert(
            "Tags".into(),
            if tags.is_empty() {
                Value::Null
            } else {
                json!(tags)
            },
        );
    }
    json!(steps)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(id: &str, tags: &[&str], created: &str) -> Image {
        Image {
            id: id.to_string(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            digests: BTreeSet::new(),
            records: vec![format!("rec-{id}")],
            containers: Vec::new(),
            config: json!({ "created": created, "config": { "Labels": { "a": "1" } } }),
            layers: Vec::new(),
            size: 10,
            pulled: 0,
        }
    }

    /// busybox:1.37 as the oracle recorded it, with one container made from it.
    fn busybox() -> Image {
        let id = "sha256:30ecbe1509090b38ef3260f3609d64d8261230464eaa4d235901db496e9647fb";
        let layer = "sha256:5a9bfd996c6d49c6c3cb4ddfb76e8d09cf16b6b7cf6601d1f2dffbdbc3edb5d4";
        Image {
            id: id.to_string(),
            tags: ["docker.io/library/busybox:1.37".to_string()].into(),
            digests: [format!(
                "docker.io/library/busybox@sha256:{}",
                "a".repeat(64)
            )]
            .into(),
            records: vec!["busybox-1.37".into()],
            containers: vec!["keep".into()],
            config: json!({
                "architecture": "amd64", "os": "linux", "created": "2024-09-26T21:31:42Z",
                "config": { "Cmd": ["sh"], "Env": ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"] },
                "history": [{ "created": "2024-09-26T21:31:42Z", "created_by": "BusyBox 1.37.0 (glibc), Debian 13" }],
                "rootfs": { "type": "layers", "diff_ids": [layer] },
            }),
            layers: vec![layer.to_string()],
            size: 4_421_246,
            pulled: 0,
        }
    }

    fn context() -> (tempfile::TempDir, Context) {
        let tmp = tempfile::tempdir().unwrap();
        let mut config =
            crate::config::Config::merge(crate::config::FileConfig::default(), None, None).unwrap();
        config.machines_dir = tmp.path().join("machines");
        config.state_dir = tmp.path().join("state");
        (tmp, Context::new(config))
    }

    #[test]
    fn shapes_against_docker() {
        use crate::dockerapi::shapes::{diff, golden};
        let (_tmp, ctx) = context();
        let image = busybox();
        let list = golden(include_str!("../../tests/dockerapi/golden/images.body"));
        let docker = list
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["Id"] == image.id.as_str())
            .unwrap();
        let ours = summary_json(&image);
        assert_eq!(diff(&ours, docker), (vec![], vec![]), "summary");
        assert_eq!(ours["Containers"], docker["Containers"]);
        assert_eq!(ours["RepoTags"], docker["RepoTags"]);
        let docker = golden(include_str!(
            "../../tests/dockerapi/golden/image-busybox.body"
        ));
        let ours = inspect_json(&ctx.store, &image);
        // The engine's layers are mounted for the image; this store's are directories.
        let (extra, missing) = diff(&ours, &docker);
        assert!(extra.is_empty(), "inspect: extra {extra:?}");
        assert_eq!(
            missing,
            [".GraphDriver.Data.MergedDir", ".GraphDriver.Data.WorkDir"],
            "inspect"
        );
        assert_eq!(ours["Config"], docker["Config"]);
        assert_eq!(ours["RootFS"], docker["RootFS"]);
        assert_eq!(ours["Created"], docker["Created"]);
        let docker = golden(include_str!(
            "../../tests/dockerapi/golden/history-busybox.body"
        ));
        let mut ours = history_json(&ctx, &image);
        // Sizes are of the layers here, which this store has none of.
        ours[0]["Size"] = docker[0]["Size"].clone();
        assert_eq!(ours, docker, "history");
        let docker = golden(include_str!("../../tests/dockerapi/golden/system-df.body"));
        let docker = docker["Images"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["Id"] == image.id.as_str())
            .unwrap()
            .clone();
        let (entries, _) = disk_usage(&ctx, std::slice::from_ref(&image));
        assert_eq!(diff(&entries[0], &docker), (vec![], vec![]), "system df");
        let docker = golden(include_str!(
            "../../tests/dockerapi/golden/image-missing.body"
        ));
        assert_eq!(
            find(&[image], "nope").unwrap_err(),
            docker["message"].as_str().unwrap()
        );
    }

    #[test]
    fn an_image_is_found_by_reference_id_prefix_or_record() {
        let a = "sha256:aaaa1111".to_string() + &"0".repeat(56);
        let b = "sha256:aaab2222".to_string() + &"0".repeat(56);
        let images = vec![
            image(
                &a,
                &["docker.io/library/busybox:1.37"],
                "2024-01-01T00:00:00Z",
            ),
            image(&b, &["ghcr.io/x/y:latest"], "2024-02-01T00:00:00Z"),
        ];
        assert_eq!(find(&images, "busybox:1.37").unwrap().id, a);
        assert_eq!(
            find(&images, "docker.io/library/busybox:1.37").unwrap().id,
            a
        );
        assert_eq!(find(&images, "ghcr.io/x/y").unwrap().id, b);
        assert_eq!(find(&images, &a).unwrap().id, a);
        assert_eq!(
            find(&images, a.trim_start_matches("sha256:")).unwrap().id,
            a
        );
        assert_eq!(find(&images, "aaaa").unwrap().id, a);
        assert!(find(&images, "aaa")
            .unwrap_err()
            .starts_with("multiple IDs"));
        assert_eq!(find(&images, &format!("rec-{b}")).unwrap().id, b);
        assert_eq!(
            find(&images, "busybox").unwrap_err(),
            "No such image: busybox:latest"
        );
        assert_eq!(find(&images, "").unwrap_err(), "No such image: ");
    }

    #[test]
    fn references_match_as_the_engine_filters_them() {
        assert!(reference_matches("busybox", "busybox:1.37"));
        assert!(reference_matches("busybox:1.37", "busybox:1.37"));
        assert!(!reference_matches("busybox:1.36", "busybox:1.37"));
        assert!(reference_matches("busy*", "busybox:1.37"));
        assert!(!reference_matches("*", "ghcr.io/x/y:latest"));
        assert!(reference_matches("*/*/*", "ghcr.io/x/y:latest"));
        assert!(reference_matches(
            "ghcr.io/x/*:lat?st",
            "ghcr.io/x/y:latest"
        ));
    }

    #[test]
    fn filters_select_by_dangling_label_and_time() {
        let images = vec![
            image(
                "sha256:1",
                &["docker.io/library/a:1"],
                "2024-01-01T00:00:00Z",
            ),
            image("sha256:2", &[], "2024-02-01T00:00:00Z"),
            image(
                "sha256:3",
                &["docker.io/library/c:1"],
                "2024-03-01T00:00:00Z",
            ),
        ];
        let ids = |text: &str| -> Vec<String> {
            let filters = Filters::parse(Some(text)).unwrap();
            select(&images, &filters)
                .unwrap()
                .iter()
                .map(|i| i.id.clone())
                .collect()
        };
        assert_eq!(ids(r#"{"dangling":["true"]}"#), ["sha256:2"]);
        assert_eq!(ids(r#"{"dangling":["false"]}"#), ["sha256:1", "sha256:3"]);
        assert_eq!(ids(r#"{"reference":["a"]}"#), ["sha256:1"]);
        assert_eq!(ids(r#"{"label":["a=1"]}"#).len(), 3);
        assert!(ids(r#"{"label!":["a"]}"#).is_empty());
        assert_eq!(ids(r#"{"before":["c:1"]}"#), ["sha256:1", "sha256:2"]);
        assert_eq!(ids(r#"{"since":["a:1"]}"#), ["sha256:2", "sha256:3"]);
        let bad = Filters::parse(Some(r#"{"nope":["x"]}"#)).unwrap();
        assert!(select(&images, &bad).is_err());
        let bad = Filters::parse(Some(r#"{"dangling":["maybe"]}"#)).unwrap();
        assert_eq!(
            select(&images, &bad).unwrap_err().0,
            "invalid filter 'dangling=maybe'"
        );
    }

    #[test]
    fn a_tag_and_a_digest_are_written_in_full() {
        assert_eq!(
            full_tag("busybox:1.37", "docker.io").as_deref(),
            Some("docker.io/library/busybox:1.37")
        );
        assert_eq!(
            full_tag(&format!("busybox@sha256:{}", "a".repeat(64)), "docker.io"),
            None
        );
        let record: ImageRecord = serde_json::from_value(json!({
            "name": "b", "reference": "docker.io/library/busybox:1.37", "manifest_digest": format!("sha256:{}", "b".repeat(64)),
            "index_digest": format!("sha256:{}", "c".repeat(64)), "layers": [], "backend": "overlay", "created": 0, "origin": "pull",
        }))
        .unwrap();
        assert_eq!(
            full_digest(&record, "docker.io").unwrap(),
            format!("docker.io/library/busybox@sha256:{}", "c".repeat(64))
        );
        let built = ImageRecord {
            origin: "build".into(),
            ..record
        };
        assert_eq!(full_digest(&built, "docker.io"), None);
    }
}
