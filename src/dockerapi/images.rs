//! Images as the Docker API shows them: one for each image config, which is what the
//! engine keys its images by, made of the records pulled or built that never ran and of
//! the records of the containers made from it. A reference names one image: the last
//! one pulled of the records that are images, or else of the containers.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Mutex;

use serde_json::{json, Map, Value};

use super::filters::{Filters, Invalid};
use super::util::familiar;
pub use crate::api::resolve::by_digest;
use crate::api::resolve::{full_digest, full_tag, Resolved};
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

/// Runs a walk of the disk where it does not hold up the runtime's few threads.
pub(crate) fn walking<T>(walk: impl FnOnce() -> T) -> T {
    use tokio::runtime::{Handle, RuntimeFlavor};
    match Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(walk)
        }
        _ => walk(),
    }
}

/// Bytes of each of `layers`: what was counted already, the rest counted now without
/// holding the cache, so that the walk of a big layer holds up no other request.
fn layer_sizes<'a>(
    store: &Store,
    layers: impl IntoIterator<Item = &'a String>,
) -> BTreeMap<String, u64> {
    let mut sizes = BTreeMap::new();
    let mut missing = Vec::new();
    {
        let known = LAYER_SIZES.lock().unwrap();
        for layer in layers {
            match known.get(layer) {
                Some(size) => {
                    sizes.insert(layer.clone(), *size);
                }
                None => missing.push(layer.clone()),
            }
        }
    }
    if !missing.is_empty() {
        let counted = walking(|| {
            let mut counted = BTreeMap::new();
            for layer in &missing {
                crate::api::disk::layer_size(store, layer, &mut counted);
            }
            counted
        });
        LAYER_SIZES.lock().unwrap().extend(counted.clone());
        sizes.extend(counted);
    }
    sizes
}

/// Bytes of a record's image: its layers, or the tree of a flat one, whose layers are
/// kept nowhere else.
fn image_size(store: &Store, record: &ImageRecord) -> u64 {
    use crate::backend::BackendChoice;
    if matches!(record.backend, BackendChoice::Flat | BackendChoice::Auto) {
        let tree = store.machines_dir.join(&record.name);
        return walking(|| {
            crate::api::disk::tree_size(&tree, &mut std::collections::HashSet::new())
        });
    }
    let sizes = layer_sizes(store, &record.layers);
    record.layers.iter().filter_map(|d| sizes.get(d)).sum()
}

/// What the service resolves `key` to, read as clients of the Docker Engine API read the
/// name of an image (ResolveImage with docker_api): None when nothing here goes by it, or it
/// is no reference.
pub async fn resolve(key: &str) -> anyhow::Result<Option<Resolved>> {
    let client = crate::client::Client::connect().await?;
    let mut options = crate::client::Options::new();
    options.insert(
        crate::api::resolve::DOCKER_API_OPTION,
        zbus::zvariant::Value::from(true),
    );
    match client.manager.resolve_image(key, options).await {
        Ok(found) => Ok(Some(Resolved {
            name: crate::client::string(&found, "name"),
            reference: crate::client::string(&found, "reference"),
        })),
        Err(e) => {
            let e = crate::client::error(e);
            match crate::client::error_name(&e) {
                Some("NoSuchImage" | "Invalid") => Ok(None),
                _ => Err(e),
            }
        }
    }
}

/// The record a reference with a tag names (`wanted` in full), ranked as `list` ranks
/// them: the last pulled of the records that are images, else of the containers.
pub fn tagged<'a>(
    records: &'a [ImageRecord],
    wanted: &str,
    registry: &str,
    is_image: impl Fn(&ImageRecord) -> bool,
) -> Option<&'a ImageRecord> {
    records
        .iter()
        .filter(|r| full_tag(&r.reference, registry).as_deref() == Some(wanted))
        .max_by_key(|r| (is_image(r), r.created))
}

/// The last pulled record of a reference with a tag (`wanted` in full) that is an
/// image, which a pull of the reference replaces.
pub fn image_of<'a>(
    records: &'a [ImageRecord],
    wanted: &str,
    registry: &str,
    is_image: impl Fn(&ImageRecord) -> bool,
) -> Option<&'a ImageRecord> {
    records
        .iter()
        .filter(|r| is_image(r) && full_tag(&r.reference, registry).as_deref() == Some(wanted))
        .max_by_key(|r| r.created)
}

/// A record pulled by the digest `wanted` (`repository@digest` in full) names, as
/// `find` looks one up: by the digest its reference named, an index's for a tag of
/// several platforms.
pub fn pinned<'a>(
    records: &'a [ImageRecord],
    wanted: &str,
    registry: &str,
) -> Option<&'a ImageRecord> {
    records
        .iter()
        .find(|r| full_digest(r, registry).as_deref() == Some(wanted))
}

/// The digest a record's reference named when it was pulled: its index's, or its
/// manifest's.
pub fn pulled_digest(record: &ImageRecord) -> &str {
    record
        .index_digest
        .as_deref()
        .unwrap_or(&record.manifest_digest)
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

/// The image `key` names, as the engine looks one up: a reference, the record the service
/// resolves it to (`resolve`), then the name of a record (nspawn's own way, which a name of
/// hex digits would otherwise lose to an ID that starts with them), then the config's
/// digest or a prefix of it.
pub fn find<'a>(
    images: &'a [Image],
    key: &str,
    resolved: Option<&Resolved>,
) -> Result<&'a Image, String> {
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
    if let Some(found) = resolved {
        let holds =
            |i: &&Image| i.records.contains(&found.name) || i.containers.contains(&found.name);
        if let Some(image) = images.iter().find(holds) {
            return Ok(image);
        }
    }
    if let Some(image) = images.iter().find(|i| i.records.iter().any(|r| r == key)) {
        return Ok(image);
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
    Err(missing())
}

/// The record a container of `image` is made from, `key` being how it was asked for: the
/// one the service resolves the reference to, so that the container keeps that reference;
/// the record of that name for a record's name; the image's first record otherwise.
pub fn source(image: &Image, key: &str, resolved: Option<&Resolved>) -> Option<String> {
    let mut held = image.records.iter().chain(&image.containers);
    if let Some(found) = resolved {
        if let Some(name) = held.clone().find(|name| **name == found.name) {
            return Some(name.clone());
        }
    }
    if let Some(name) = held.clone().find(|name| *name == key) {
        return Some(name.clone());
    }
    held.next().cloned()
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
pub fn select<'a>(
    images: &'a [Image],
    filters: &Filters,
    resolved: &HashMap<String, Option<Resolved>>,
) -> Result<Vec<&'a Image>, Invalid> {
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
            let found = resolved.get(value).and_then(Option::as_ref);
            let image = find(images, value, found).map_err(Invalid)?;
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
    let sizes = layer_sizes(store, users.keys());
    let total = sizes.values().sum();
    let entries = images
        .iter()
        .zip(&layers_of)
        .map(|(image, layers)| {
            let shared: u64 = layers
                .iter()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .filter(|layer| users.get(*layer).is_some_and(|n| *n > 1))
                .filter_map(|layer| sizes.get(layer))
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
    let sizes = layer_sizes(&ctx.store, &image.layers);
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
                    .and_then(|d| sizes.get(d).copied())
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

/// A change of images, by method and path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    /// DELETE /images/{name}, the name with its slashes.
    Remove(String),
    /// POST /images/prune.
    Prune,
}

impl Call {
    pub fn of(method: &hyper::Method, parts: &[&str]) -> Option<Call> {
        match parts {
            ["images", "prune"] if *method == hyper::Method::POST => Some(Call::Prune),
            ["images", name @ ..] if *method == hyper::Method::DELETE && !name.is_empty() => {
                Some(Call::Remove(name.join("/")))
            }
            _ => None,
        }
    }
}

type Answer = super::containers::Answer;

fn says(status: hyper::StatusCode, text: impl Into<String>) -> Answer {
    super::containers::Answer(status, Some(text.into()))
}

fn failed(e: impl std::fmt::Display) -> Answer {
    says(hyper::StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

/// A container made from an image, as a removal of the image names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub id: String,
    pub running: bool,
}

/// What a removal of an image does: the records that go, and the engine's answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Untag {
    pub names: Vec<String>,
    pub answer: Vec<Value>,
}

/// isImageIDPrefix: `key` begins the image's ID, with its algorithm or without.
fn is_id_prefix(id: &str, key: &str) -> bool {
    id.starts_with(key)
        || id
            .split_once(':')
            .is_some_and(|(_, hex)| hex.starts_with(key))
}

fn untagged(reference: &str) -> Value {
    json!({ "Untagged": familiar(reference) })
}

/// What DELETE /images/{key} does to `image`, whose records that are images are
/// `records` and whose containers are `users`. By an ID every record of the image goes;
/// by a reference the records of that reference; by a record's name that record. A
/// container made from the image keeps it from going without force, as the engine
/// refuses to leave a container's image without a name; with force the records go and
/// the containers stay, each still holding the image, which the answer then does not
/// say was deleted.
pub fn removal(
    image: &Image,
    records: &[ImageRecord],
    key: &str,
    force: bool,
    registry: &str,
    users: &[User],
    resolved: Option<&Resolved>,
) -> Result<Untag, Answer> {
    use hyper::StatusCode;
    let short =
        |id: &str| super::util::short(id.split_once(':').map_or(id, |(_, hex)| hex)).to_string();
    let id12 = short(&image.id);
    let conflict = |text: String| says(StatusCode::CONFLICT, text);
    let held: Vec<&ImageRecord> = records
        .iter()
        .filter(|r| image.records.contains(&r.name))
        .collect();
    // One tag at most: the engine's "single reference", which a container keeps.
    let single = image.tags.len() <= 1;
    // A record's name is the record, even made of hex digits as an ID is.
    if is_id_prefix(&image.id, key) && !image.records.iter().any(|r| r == key) {
        if !force {
            if let Some(user) = users.iter().find(|u| u.running) {
                return Err(conflict(format!(
                    "conflict: unable to delete {id12} (cannot be forced) - image is being used by running container {}",
                    short(&user.id)
                )));
            }
            if !single {
                return Err(conflict(format!(
                    "conflict: unable to delete {id12} (must be forced) - image is referenced in multiple repositories"
                )));
            }
            if let Some(user) = users.first() {
                return Err(conflict(format!(
                    "conflict: unable to delete {id12} (must be forced) - image is being used by stopped container {}",
                    short(&user.id)
                )));
            }
        }
        let mut answer: Vec<Value> = image
            .tags
            .iter()
            .chain(&image.digests)
            .map(|r| untagged(r))
            .collect();
        if users.is_empty() {
            answer.push(json!({ "Deleted": image.id }));
        }
        return Ok(Untag {
            names: held.iter().map(|r| r.name.clone()).collect(),
            answer,
        });
    }
    // The reference the service resolved the key to, in full.
    let wanted = resolved.map(|found| found.reference.clone());
    let (chosen, asked, by_tag): (Vec<&ImageRecord>, String, bool) = match wanted {
        Some(wanted) if image.tags.contains(&wanted) => (
            held.iter()
                .copied()
                .filter(|r| full_tag(&r.reference, registry).as_deref() == Some(wanted.as_str()))
                .collect(),
            wanted,
            true,
        ),
        Some(wanted) if image.digests.contains(&wanted) => (
            held.iter()
                .copied()
                .filter(|r| full_digest(r, registry).as_deref() == Some(wanted.as_str()))
                .collect(),
            wanted,
            false,
        ),
        _ => {
            let Some(record) = held.iter().copied().find(|r| r.name == key) else {
                return Err(says(StatusCode::NOT_FOUND, format!("No such image: {key}")));
            };
            match full_tag(&record.reference, registry) {
                Some(tag) => (vec![record], tag, true),
                None => (
                    vec![record],
                    full_digest(record, registry).unwrap_or_else(|| key.to_string()),
                    false,
                ),
            }
        }
    };
    if !force && single {
        if let Some(user) = users.first() {
            return Err(conflict(format!(
                "conflict: unable to remove repository reference {key:?} (must force) - container {} is using its referenced image {id12}",
                short(&user.id)
            )));
        }
    }
    let staying: Vec<&ImageRecord> = held
        .iter()
        .copied()
        .filter(|r| !chosen.iter().any(|c| c.name == r.name))
        .collect();
    let mut answer = vec![untagged(&asked)];
    // A tag that goes takes the digests it was pulled by along, unless what stays still
    // has a tag.
    if by_tag
        && !staying
            .iter()
            .any(|r| full_tag(&r.reference, registry).is_some())
    {
        let mut digests: Vec<String> = chosen
            .iter()
            .filter_map(|r| full_digest(r, registry))
            .filter(|d| {
                !staying
                    .iter()
                    .any(|r| full_digest(r, registry).as_deref() == Some(d.as_str()))
            })
            .collect();
        digests.sort();
        digests.dedup();
        answer.extend(digests.iter().map(|d| untagged(d)));
    }
    if staying.is_empty() && users.is_empty() && !chosen.is_empty() {
        answer.push(json!({ "Deleted": image.id }));
    }
    Ok(Untag {
        names: chosen.iter().map(|r| r.name.clone()).collect(),
        answer,
    })
}

/// DELETE /images/{key}: RemoveImages of the records `removal` picks.
async fn remove(ctx: &Context, key: &str, force: bool) -> Result<Value, Answer> {
    use hyper::StatusCode;
    let all = list(ctx).map_err(|e| failed(format!("{e:#}")))?;
    let resolved = resolve(key)
        .await
        .map_err(|e| says(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    let image =
        find(&all, key, resolved.as_ref()).map_err(|e| says(super::lookup_status(&e), e))?;
    let records: Vec<ImageRecord> = image
        .records
        .iter()
        .filter_map(|name| ctx.store.load_image(name).ok().flatten())
        .collect();
    let snapshot = super::model::Snapshot::take(ctx)
        .await
        .map_err(|e| failed(format!("{e:#}")))?;
    let users: Vec<User> = image
        .containers
        .iter()
        .filter_map(|name| snapshot.machines.iter().find(|m| m.name() == name))
        .map(|m| User {
            id: m.id.clone(),
            running: m.running(),
        })
        .collect();
    let untag = removal(
        image,
        &records,
        key,
        force,
        &ctx.config.registry,
        &users,
        resolved.as_ref(),
    )?;
    if !untag.names.is_empty() {
        let client = crate::client::Client::connect()
            .await
            .map_err(|e| failed(format!("{e:#}")))?;
        let names = untag.names.clone();
        if let Err(e) = client
            .run_job_silent(|| client.manager.remove_images(&names), true)
            .await
        {
            let kind = crate::client::error_name(&e);
            return Err(match kind {
                Some("NoSuchImage") | Some("NoSuchMachine") => {
                    says(StatusCode::NOT_FOUND, format!("No such image: {key}"))
                }
                Some("InUse") => says(
                    StatusCode::CONFLICT,
                    format!(
                        "conflict: unable to delete {} (cannot be forced) - {e:#}",
                        super::util::short(image.id.trim_start_matches("sha256:"))
                    ),
                ),
                kind => says(super::containers::status_of(kind), format!("{e:#}")),
            });
        }
    }
    Ok(Value::Array(untag.answer))
}

/// The filters of POST /images/prune as PruneImages' options: `dangling` (true unless
/// said otherwise) names the images without a tag no container was made from, false
/// every image no container was made from. `label` narrows them, and `until` (a time or
/// a duration before `now`); `label!` has nothing to narrow them with on the bus.
pub fn prune_options(filters: &Filters, now: f64, offset: i64) -> Result<Prune, Answer> {
    use super::create::Opt;
    use hyper::StatusCode;
    let bad = |text: String| says(StatusCode::BAD_REQUEST, text);
    filters
        .only(&["dangling", "label", "label!", "until"])
        .map_err(|Invalid(text)| bad(text))?;
    let dangling = match filters.values("dangling") {
        None if filters.keys().any(|k| k == "dangling") => {
            return Err(bad("invalid filter 'dangling'".to_string()))
        }
        None => true,
        Some(values) => {
            let off = values.contains("0") || values.contains("false");
            let on = values.contains("1") || values.contains("true");
            if off == on {
                let listed: Vec<&str> = values.iter().map(String::as_str).collect();
                return Err(bad(format!(
                    "invalid filter 'dangling=[{}]'",
                    listed.join(" ")
                )));
            }
            on
        }
    };
    let until = super::networks::prune_until(filters, now, offset)?;
    if filters.values("label!").is_some() {
        return Err(bad(
            "invalid filter 'label!': not supported by nspawn's image prune, which takes label and until"
                .to_string(),
        ));
    }
    let mut opts = super::create::Opts::from([("all", Opt::Flag(true))]);
    if let Some(labels) = filters.values("label") {
        opts.insert("labels", Opt::Texts(labels.iter().cloned().collect()));
    }
    if let Some(until) = until {
        opts.insert("until", Opt::Unsigned(until));
    }
    Ok(Prune { opts, dangling })
}

/// What an image prune asks PruneImages for: with `dangling`, the images without a tag
/// alone (`dangling_records`).
#[derive(Debug)]
pub struct Prune {
    pub opts: super::create::Opts,
    pub dangling: bool,
}

/// The records of the images a dangling prune takes: those without a tag that no
/// container was made from, pulled by digest alone or whose tag went to a newer image.
fn dangling_records(images: &[Image]) -> Vec<String> {
    images
        .iter()
        .filter(|i| i.dangling() && i.containers.is_empty())
        .flat_map(|i| i.records.iter().cloned())
        .collect()
}

/// The body of a prune that removed `removed` of `records`: an image's names untagged,
/// and the image deleted once none of its records nor containers is left.
pub fn pruned_body(
    images: &[Image],
    records: &[ImageRecord],
    removed: &[String],
    registry: &str,
    reclaimed: u64,
) -> Value {
    let mut deleted = Vec::new();
    for image in images {
        let gone: Vec<&ImageRecord> = records
            .iter()
            .filter(|r| image.records.contains(&r.name) && removed.contains(&r.name))
            .collect();
        if gone.is_empty() {
            continue;
        }
        let mut names: Vec<String> = Vec::new();
        for name in gone
            .iter()
            .filter_map(|r| full_tag(&r.reference, registry))
            .chain(gone.iter().filter_map(|r| full_digest(r, registry)))
        {
            if !names.contains(&name) {
                names.push(name);
            }
        }
        deleted.extend(names.iter().map(|n| untagged(n)));
        if gone.len() == image.records.len() && image.containers.is_empty() {
            deleted.push(json!({ "Deleted": image.id }));
        }
    }
    json!({
        "ImagesDeleted": if deleted.is_empty() { Value::Null } else { json!(deleted) },
        "SpaceReclaimed": reclaimed,
    })
}

/// POST /images/prune: PruneImages with every image the filters select.
async fn prune(ctx: &Context, query: &super::Query) -> Result<Value, Answer> {
    use hyper::StatusCode;
    let filters = Filters::parse(query.get("filters"))
        .map_err(|Invalid(text)| says(StatusCode::BAD_REQUEST, text))?;
    let (now, offset) = super::networks::now_and_offset();
    let Prune { mut opts, dangling } = prune_options(&filters, now, offset)?;
    let images = list(ctx).map_err(|e| failed(format!("{e:#}")))?;
    if dangling {
        let names = dangling_records(&images);
        if names.is_empty() {
            return Ok(json!({ "ImagesDeleted": null, "SpaceReclaimed": 0 }));
        }
        opts.insert("names", super::create::Opt::Texts(names));
    }
    let records = ctx
        .store
        .list_images()
        .map_err(|e| failed(format!("{e:#}")))?;
    let client = crate::client::Client::connect()
        .await
        .map_err(|e| failed(format!("{e:#}")))?;
    let outcome = client
        .run_job_silent(
            || client.manager.prune_images(super::create::to_bus(&opts)),
            true,
        )
        .await;
    let result = match outcome {
        Ok(result) => result,
        Err(e) => match crate::client::removed_before(e) {
            Ok(result) => result,
            Err(e) => {
                return Err(says(
                    super::containers::status_of(crate::client::error_name(&e)),
                    format!("{e:#}"),
                ))
            }
        },
    };
    Ok(pruned_body(
        &images,
        &records,
        &crate::client::strings(&result, "removed"),
        &ctx.config.registry,
        crate::client::u64(&result, "reclaimed"),
    ))
}

/// Serves a change of images.
pub(super) async fn serve(
    ctx: &std::sync::Arc<Context>,
    call: Call,
    query: &super::Query,
) -> hyper::Response<super::Body> {
    let served = match call {
        Call::Remove(key) => remove(ctx, &key, query.flag("force", false)).await,
        Call::Prune => prune(ctx, query).await,
    };
    match served {
        Ok(body) => super::json_response(hyper::StatusCode::OK, &body),
        Err(answer) => answer.respond(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images_are_counted_without_their_sizes_and_layers_walked_once() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config =
            crate::config::Config::merge(crate::config::FileConfig::default(), None, None).unwrap();
        config.machines_dir = tmp.path().join("machines");
        config.state_dir = tmp.path().join("state");
        let ctx = Context::new(config);
        ctx.store.init().unwrap();
        let manifest = |config: &str| {
            format!(
                r#"{{"schemaVersion": 2, "config": {{"mediaType": "application/vnd.oci.image.config.v1+json", "digest": "{config}", "size": 2}}, "layers": []}}"#
            )
        };
        for (name, config) in [("a", "sha256:c1"), ("b", "sha256:c1"), ("c", "sha256:c2")] {
            let record: ImageRecord = serde_json::from_value(json!({
                "name": name, "reference": "docker.io/library/x:1", "manifest_digest": "sha256:m",
                "layers": [], "backend": "overlay", "created": 0,
            }))
            .unwrap();
            ctx.store.record_image(&record).unwrap();
            ctx.store
                .save_manifest(name, manifest(config).as_bytes())
                .unwrap();
        }
        let count = crate::api::info::image_count(&ctx.store).unwrap();
        assert_eq!(count, 2, "one image per config");
        assert_eq!(
            list(&ctx).unwrap().len() as u64,
            count,
            "the images the list has"
        );
        let layer = "sha256:counted-once-e2e".to_string();
        let dir = ctx.store.layer_dir(&layer, crate::store::Ownership::Root);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f"), vec![0u8; 4096]).unwrap();
        let first = layer_sizes(&ctx.store, [&layer])[&layer];
        assert!(first >= 4096, "{first}");
        std::fs::write(dir.join("g"), vec![0u8; 4096]).unwrap();
        assert_eq!(
            layer_sizes(&ctx.store, [&layer])[&layer],
            first,
            "from the cache"
        );
    }

    pub(super) fn image(id: &str, tags: &[&str], created: &str) -> Image {
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

    #[test]
    fn a_reference_with_a_digest_is_looked_up_by_the_digest_alone() {
        let digest = format!("sha256:{}", "a".repeat(64));
        let images = [busybox()];
        // The image of the record the service resolves the reference to; nothing when it
        // resolves none.
        let held = by(
            &images[0].records[0],
            &format!("docker.io/library/busybox@{digest}"),
        );
        let found = find(&images, &format!("busybox:1.37@{digest}"), Some(&held)).unwrap();
        assert_eq!(found.id, images[0].id);
        assert!(find(
            &images,
            &format!("busybox:1.37@sha256:{}", "b".repeat(64)),
            None
        )
        .is_err());
        // A record pulled as tag and digest holds the digest, not the tag.
        let pinned = format!("docker.io/library/postgres:16@{digest}");
        assert_eq!(full_tag(&pinned, "hub.nspawn.org"), None);
        assert_eq!(
            full_tag("docker.io/library/postgres:16", "hub.nspawn.org").as_deref(),
            Some("docker.io/library/postgres:16")
        );
    }

    /// What the service resolves a name to.
    pub(super) fn by(name: &str, reference: &str) -> Resolved {
        Resolved {
            name: name.to_string(),
            reference: reference.to_string(),
        }
    }

    #[test]
    fn a_container_is_made_from_the_record_its_reference_names() {
        let image = Image {
            records: vec!["nginxstable".into(), "nginx127".into(), "pinned".into()],
            containers: vec!["web".into()],
            ..busybox()
        };
        let source = |key: &str, found: Option<Resolved>| source(&image, key, found.as_ref());
        assert_eq!(
            source(
                "nginx:1.27",
                Some(by("nginx127", "docker.io/library/nginx:1.27"))
            )
            .as_deref(),
            Some("nginx127"),
            "the record the service resolves the reference to"
        );
        assert_eq!(
            source(
                "nginx:1.27",
                Some(by("web", "docker.io/library/nginx:1.27"))
            )
            .as_deref(),
            Some("web"),
            "a container that alone holds the reference"
        );
        assert_eq!(
            source("pinned", None).as_deref(),
            Some("pinned"),
            "a record's name"
        );
        assert_eq!(
            source(&image.id, None).as_deref(),
            Some("nginxstable"),
            "an ID"
        );
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
    fn shapes_match_the_recorded_answers() {
        use crate::dockerapi::shapes::{diff, golden};
        let (_tmp, ctx) = context();
        let image = busybox();
        let list = golden(include_str!("../../tests/dockerapi/golden/images.body"));
        let recorded = list
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["Id"] == image.id.as_str())
            .unwrap();
        let ours = summary_json(&image);
        assert_eq!(diff(&ours, recorded), (vec![], vec![]), "summary");
        assert_eq!(ours["Containers"], recorded["Containers"]);
        assert_eq!(ours["RepoTags"], recorded["RepoTags"]);
        let recorded = golden(include_str!(
            "../../tests/dockerapi/golden/image-busybox.body"
        ));
        let ours = inspect_json(&ctx.store, &image);
        // The engine's layers are mounted for the image; this store's are directories.
        let (extra, missing) = diff(&ours, &recorded);
        assert!(extra.is_empty(), "inspect: extra {extra:?}");
        assert_eq!(
            missing,
            [".GraphDriver.Data.MergedDir", ".GraphDriver.Data.WorkDir"],
            "inspect"
        );
        assert_eq!(ours["Config"], recorded["Config"]);
        assert_eq!(ours["RootFS"], recorded["RootFS"]);
        assert_eq!(ours["Created"], recorded["Created"]);
        let recorded = golden(include_str!(
            "../../tests/dockerapi/golden/history-busybox.body"
        ));
        let mut ours = history_json(&ctx, &image);
        // Sizes are of the layers here, which this store has none of.
        ours[0]["Size"] = recorded[0]["Size"].clone();
        assert_eq!(ours, recorded, "history");
        let recorded = golden(include_str!("../../tests/dockerapi/golden/system-df.body"));
        let recorded = recorded["Images"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["Id"] == image.id.as_str())
            .unwrap()
            .clone();
        let (entries, _) = disk_usage(&ctx, std::slice::from_ref(&image));
        assert_eq!(diff(&entries[0], &recorded), (vec![], vec![]), "system df");
        let recorded = golden(include_str!(
            "../../tests/dockerapi/golden/image-missing.body"
        ));
        assert_eq!(
            find(&[image], "nope", None).unwrap_err(),
            recorded["message"].as_str().unwrap()
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
        let rec_a = format!("rec-{a}");
        let busybox = by(&rec_a, "docker.io/library/busybox:1.37");
        assert_eq!(find(&images, "busybox:1.37", Some(&busybox)).unwrap().id, a);
        assert_eq!(find(&images, &a, None).unwrap().id, a);
        assert_eq!(
            find(&images, a.trim_start_matches("sha256:"), None)
                .unwrap()
                .id,
            a
        );
        assert_eq!(find(&images, "aaaa", None).unwrap().id, a);
        assert!(find(&images, "aaa", None)
            .unwrap_err()
            .starts_with("multiple IDs"));
        assert_eq!(find(&images, &format!("rec-{b}"), None).unwrap().id, b);
        // A record named with hex digits is that record, not the ID they start.
        let mut named = images.clone();
        named[1].records.push("aaaa".to_string());
        assert_eq!(find(&named, "aaaa", None).unwrap().id, b);
        assert_eq!(
            find(&images, "busybox", None).unwrap_err(),
            "No such image: busybox:latest"
        );
        assert_eq!(find(&images, "", None).unwrap_err(), "No such image: ");
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
            let resolved = HashMap::from([
                (
                    "a:1".to_string(),
                    Some(by("rec-sha256:1", "docker.io/library/a:1")),
                ),
                (
                    "c:1".to_string(),
                    Some(by("rec-sha256:3", "docker.io/library/c:1")),
                ),
            ]);
            select(&images, &filters, &resolved)
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
        assert!(select(&images, &bad, &HashMap::new()).is_err());
        let bad = Filters::parse(Some(r#"{"dangling":["maybe"]}"#)).unwrap();
        assert_eq!(
            select(&images, &bad, &HashMap::new()).unwrap_err().0,
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

#[cfg(test)]
mod changes {
    use super::tests::by;
    use super::*;
    use hyper::{Method, StatusCode};

    const REGISTRY: &str = "hub.nspawn.org";

    fn id() -> String {
        format!("sha256:{}", "c".repeat(64))
    }

    fn digest() -> String {
        format!("sha256:{}", "d".repeat(64))
    }

    fn record(name: &str, reference: &str) -> ImageRecord {
        serde_json::from_value(json!({
            "name": name, "reference": reference, "manifest_digest": digest(),
            "layers": [], "backend": "overlay", "created": 1,
        }))
        .unwrap()
    }

    fn nginx(records: &[ImageRecord], containers: &[&str]) -> Image {
        Image {
            id: id(),
            tags: records
                .iter()
                .filter_map(|r| full_tag(&r.reference, REGISTRY))
                .collect(),
            digests: records
                .iter()
                .filter_map(|r| full_digest(r, REGISTRY))
                .collect(),
            records: records.iter().map(|r| r.name.clone()).collect(),
            containers: containers.iter().map(|c| c.to_string()).collect(),
            config: json!({}),
            layers: Vec::new(),
            size: 0,
            pulled: 0,
        }
    }

    fn user(running: bool) -> User {
        User {
            id: "e".repeat(64),
            running,
        }
    }

    fn untag(names: &[&str]) -> Vec<Value> {
        names.iter().map(|n| json!({ "Untagged": n })).collect()
    }

    #[test]
    fn a_reference_goes_with_its_digests_and_the_image_when_nothing_holds_it() {
        let records = [record("nginx", "docker.io/library/nginx:latest")];
        let image = nginx(&records, &[]);
        let pinned = format!("nginx@{}", digest());
        let mut answer = untag(&["nginx:latest", &pinned]);
        answer.push(json!({ "Deleted": id() }));
        for key in ["nginx", "nginx:latest", "docker.io/library/nginx:latest"] {
            assert_eq!(
                removal(
                    &image,
                    &records,
                    key,
                    false,
                    REGISTRY,
                    &[],
                    Some(&by("nginx", "docker.io/library/nginx:latest"))
                ),
                Ok(Untag {
                    names: vec!["nginx".into()],
                    answer: answer.clone(),
                }),
                "{key}"
            );
        }
        // By the name of the record that holds it, as `nspawn images` lists it.
        assert_eq!(
            removal(
                &image,
                &records,
                "nginx",
                true,
                REGISTRY,
                &[],
                Some(&by("nginx", "docker.io/library/nginx:latest"))
            )
            .unwrap()
            .names,
            ["nginx"]
        );
        // By the digest: that reference alone is untagged.
        let pinned_full = format!("docker.io/library/nginx@{}", digest());
        let by_digest = removal(
            &image,
            &records,
            &pinned,
            false,
            REGISTRY,
            &[],
            Some(&by("nginx", &pinned_full)),
        )
        .unwrap();
        assert_eq!(by_digest.names, ["nginx"]);
        assert_eq!(by_digest.answer[0], json!({ "Untagged": pinned }));
        assert_eq!(
            by_digest.answer.last().unwrap(),
            &json!({ "Deleted": id() })
        );
    }

    #[test]
    fn a_container_keeps_its_image_unless_forced_and_then_stays() {
        let records = [record("nginx", "docker.io/library/nginx:latest")];
        let image = nginx(&records, &["web"]);
        let stopped = [user(false)];
        let (c12, i12) = ("e".repeat(12), "c".repeat(12));
        assert_eq!(
            removal(&image, &records, "nginx", false, REGISTRY, &stopped, Some(&by("nginx", "docker.io/library/nginx:latest"))),
            Err(says(
                StatusCode::CONFLICT,
                format!("conflict: unable to remove repository reference \"nginx\" (must force) - container {c12} is using its referenced image {i12}")
            ))
        );
        // With force the record goes and the container keeps the image: not deleted.
        let forced = removal(
            &image,
            &records,
            "nginx",
            true,
            REGISTRY,
            &stopped,
            Some(&by("nginx", "docker.io/library/nginx:latest")),
        )
        .unwrap();
        assert_eq!(forced.names, ["nginx"]);
        assert_eq!(
            forced.answer,
            untag(&["nginx:latest", &format!("nginx@{}", digest())])
        );
        // An image only a container holds: nothing to remove, the answer all the same.
        let held = nginx(&[], &["web"]);
        let mut only = held.clone();
        only.tags = ["docker.io/library/nginx:latest".to_string()].into();
        let web = by("web", "docker.io/library/nginx:latest");
        assert!(removal(&only, &[], "nginx", false, REGISTRY, &stopped, Some(&web)).is_err());
        let forced = removal(&only, &[], &id(), true, REGISTRY, &stopped, None).unwrap();
        assert!(forced.names.is_empty());
        assert_eq!(forced.answer, untag(&["nginx:latest"]));
        // By ID, the engine's words for each conflict.
        assert_eq!(
            removal(&image, &records, &"c".repeat(8), false, REGISTRY, &[user(true)], None),
            Err(says(
                StatusCode::CONFLICT,
                format!("conflict: unable to delete {i12} (cannot be forced) - image is being used by running container {c12}")
            ))
        );
        assert_eq!(
            removal(&image, &records, &id(), false, REGISTRY, &stopped, None),
            Err(says(
                StatusCode::CONFLICT,
                format!("conflict: unable to delete {i12} (must be forced) - image is being used by stopped container {c12}")
            ))
        );
    }

    #[test]
    fn an_image_built_here_is_removed_by_the_name_the_service_resolves() {
        let mut app = record("somos-app-1", "hub.nspawn.org/somos/app:1");
        app.origin = "build".into();
        let records = [app];
        let image = nginx(&records, &[]);
        // somos/app:1, which the service resolves to the record built here.
        let found = by("somos-app-1", "hub.nspawn.org/somos/app:1");
        assert_eq!(
            find(std::slice::from_ref(&image), "somos/app:1", Some(&found))
                .unwrap()
                .id,
            image.id
        );
        assert_eq!(
            source(&image, "somos/app:1", Some(&found)).as_deref(),
            Some("somos-app-1")
        );
        let gone = removal(
            &image,
            &records,
            "somos/app:1",
            false,
            REGISTRY,
            &[],
            Some(&found),
        )
        .unwrap();
        assert_eq!(gone.names, ["somos-app-1"]);
        assert_eq!(gone.answer[0], untag(&["hub.nspawn.org/somos/app:1"])[0]);
    }

    #[test]
    fn one_of_several_tags_is_untagged_alone() {
        let records = [
            record("nginx", "docker.io/library/nginx:latest"),
            record("nginx-alt", "docker.io/library/nginx:1.27"),
        ];
        let image = nginx(&records, &["web"]);
        // A container does not keep one of several names.
        let one = removal(
            &image,
            &records,
            "nginx:1.27",
            false,
            REGISTRY,
            &[user(false)],
            Some(&by("nginx-alt", "docker.io/library/nginx:1.27")),
        )
        .unwrap();
        assert_eq!(one.names, ["nginx-alt"]);
        assert_eq!(one.answer, untag(&["nginx:1.27"]));
        let by_record = removal(&image, &records, "nginx-alt", false, REGISTRY, &[], None).unwrap();
        assert_eq!(
            by_record,
            Untag {
                names: vec!["nginx-alt".into()],
                answer: untag(&["nginx:1.27"])
            }
        );
        assert_eq!(
            removal(&image, &records, &"c".repeat(12), false, REGISTRY, &[], None),
            Err(says(
                StatusCode::CONFLICT,
                format!("conflict: unable to delete {} (must be forced) - image is referenced in multiple repositories", "c".repeat(12))
            ))
        );
        let all = removal(
            &nginx(&records, &[]),
            &records,
            &id(),
            true,
            REGISTRY,
            &[],
            None,
        )
        .unwrap();
        assert_eq!(all.names, ["nginx", "nginx-alt"]);
        assert_eq!(all.answer.last().unwrap(), &json!({ "Deleted": id() }));
        assert_eq!(all.answer.len(), 4, "two tags, a digest, the image");
    }

    #[test]
    fn prunes_take_dangling_false_labels_and_until() {
        use super::super::create::Opt;
        let f = |text: &str| Filters::parse(Some(text)).unwrap();
        let now = 1_000_000.0;
        // The engine's default prunes the images without a tag.
        assert!(prune_options(&f("{}"), now, 0).unwrap().dangling);
        assert!(
            prune_options(&f(r#"{"dangling":["true"]}"#), now, 0)
                .unwrap()
                .dangling
        );
        let prune = prune_options(
            &f(r#"{"dangling":["false"],"label":["a=b"],"until":["10m"]}"#),
            now,
            0,
        )
        .unwrap();
        assert!(!prune.dangling);
        let opts = prune.opts;
        assert_eq!(opts["all"], Opt::Flag(true));
        assert_eq!(opts["labels"], Opt::Texts(vec!["a=b".into()]));
        assert_eq!(opts["until"], Opt::Unsigned(1_000_000 - 600 + 1));
        for (text, message) in [
            (r#"{"nope":["1"]}"#, "invalid filter 'nope'"),
            (
                r#"{"dangling":["maybe"]}"#,
                "invalid filter 'dangling=[maybe]'",
            ),
            (
                r#"{"dangling":["0","1"]}"#,
                "invalid filter 'dangling=[0 1]'",
            ),
            (r#"{"dangling":[]}"#, "invalid filter 'dangling'"),
            (
                r#"{"until":["1","2"]}"#,
                "more than one until filter specified",
            ),
        ] {
            assert_eq!(
                prune_options(&f(text), now, 0).unwrap_err(),
                says(StatusCode::BAD_REQUEST, message),
                "{text}"
            );
        }
        assert_eq!(
            prune_options(&f(r#"{"dangling":["false"],"label!":["a"]}"#), now, 0)
                .unwrap_err()
                .0,
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn a_dangling_prune_takes_the_images_without_a_tag_or_a_container() {
        let tagged = super::tests::image("sha256:aa", &["docker.io/library/busybox:1.37"], "2024");
        let bare = super::tests::image("sha256:bb", &[], "2024");
        let mut used = super::tests::image("sha256:cc", &[], "2024");
        used.containers.push("c".into());
        assert_eq!(
            dangling_records(&[tagged, bare, used]),
            vec!["rec-sha256:bb".to_string()]
        );
    }

    #[test]
    fn a_built_image_and_its_containers_have_no_repository_digest() {
        let mut built = record("ci", "docker.io/library/ci-image:latest");
        built.origin = "build".into();
        assert_eq!(full_digest(&built, REGISTRY), None);
        built.origin = "create".into();
        built.built = true;
        assert_eq!(full_digest(&built, REGISTRY), None);
        built.built = false;
        assert!(full_digest(&built, REGISTRY).is_some());
    }

    #[test]
    fn a_prune_says_what_it_untagged_and_deleted() {
        let records = [
            record("nginx", "docker.io/library/nginx:latest"),
            record("nginx-alt", "docker.io/library/nginx:1.27"),
        ];
        let image = nginx(&records, &[]);
        let body = pruned_body(
            std::slice::from_ref(&image),
            &records,
            &["nginx".into(), "nginx-alt".into()],
            REGISTRY,
            42,
        );
        let deleted = body["ImagesDeleted"].as_array().unwrap();
        assert_eq!(deleted.last().unwrap(), &json!({ "Deleted": id() }));
        assert!(deleted.contains(&json!({ "Untagged": "nginx:1.27" })));
        assert_eq!(body["SpaceReclaimed"], 42);
        // One of two records: untagged, not deleted.
        let body = pruned_body(
            std::slice::from_ref(&image),
            &records,
            &["nginx-alt".into()],
            REGISTRY,
            0,
        );
        assert!(!body["ImagesDeleted"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e.get("Deleted").is_some()));
        let body = pruned_body(&[image], &records, &[], REGISTRY, 0);
        assert_eq!(body, json!({ "ImagesDeleted": null, "SpaceReclaimed": 0 }));
        let recorded: Value =
            serde_json::from_str(r#"{"ImagesDeleted":null,"SpaceReclaimed":0}"#).unwrap();
        assert_eq!(
            crate::dockerapi::shapes::diff(&body, &recorded),
            (vec![], vec![])
        );
    }

    #[test]
    fn the_routes_of_changes() {
        fn parts(path: &str) -> Vec<&str> {
            path.split('/').collect()
        }
        assert_eq!(
            Call::of(&Method::DELETE, &parts("images/ghcr.io/x/y:1")),
            Some(Call::Remove("ghcr.io/x/y:1".into()))
        );
        assert_eq!(
            Call::of(&Method::POST, &parts("images/prune")),
            Some(Call::Prune)
        );
        assert_eq!(Call::of(&Method::DELETE, &parts("images")), None);
        assert_eq!(Call::of(&Method::GET, &parts("images/nginx/json")), None);
        assert_eq!(Call::of(&Method::POST, &parts("images/nginx/tag")), None);
    }
}
