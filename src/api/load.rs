//! load and import: images brought in from a stream rather than a registry.
//!
//! load reads an archive as `save` writes it, or as docker writes it: a tar, compressed
//! or not, of an OCI image layout (index.json) or of a Docker image archive
//! (manifest.json, with the layers wherever it says). Every file of it goes to the
//! store as it comes, its digest taken on the way; the images it names are checked,
//! then installed as images made here.
//!
//! import takes a tar of a root filesystem and makes it the one layer of a new image.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::os::fd::OwnedFd;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context as _, Result};
use flate2::write::GzEncoder;
use oci_client::manifest::{OciImageIndex, OciImageManifest};
use sha2::{Digest, Sha256};

use crate::api::save::{IMAGE_NAME, REF_NAME};
use crate::api::{line, Context, FailureKind, Report};
use crate::backend::Backend;
use crate::build::dispatch::change;
use crate::build::exec::{timestamp_now, LAYER_MEDIA_TYPE};
use crate::build::image::{self, History};
use crate::build::layer::GZIP_LEVEL;
use crate::build::untar::{detect, Compression};
use crate::reference::ImageRef;
use crate::store::{unique_suffix, validate_digest, Store};

/// The most a JSON file of an archive (index, manifest, config) may take.
const JSON_MAX: u64 = 8 << 20;
/// The links followed from one name of an archive to the file it stands for.
const LINK_HOPS: usize = 16;

/// An image a load or an import installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    pub name: String,
    pub reference: String,
    /// The digest of its config.
    pub id: String,
}

/// `input`, decompressed when its first bytes say it is gzip, zstd or bzip2. The first
/// eight bytes are read whole first: a pipe may hand over fewer than a magic number has.
fn decompressed(mut input: impl Read + 'static, what: &str) -> Result<Box<dyn Read>> {
    let mut head = Vec::with_capacity(8);
    let mut buf = [0u8; 8];
    while head.len() < buf.len() {
        let want = buf.len() - head.len();
        match input.read(&mut buf[..want]) {
            Ok(0) => break,
            Ok(n) => head.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e).with_context(|| format!("reading {what}")),
        }
    }
    let kind = detect(&head);
    let reader = BufReader::with_capacity(1 << 20, io::Cursor::new(head).chain(input));
    Ok(match kind {
        Compression::None => Box::new(reader),
        Compression::Gzip => Box::new(flate2::read::MultiGzDecoder::new(reader)),
        Compression::Zstd => Box::new(zstd::stream::read::Decoder::with_buffer(reader)?),
        Compression::Bzip2 => Box::new(bzip2::read::MultiBzDecoder::new(reader)),
        Compression::Xz => bail!("{what} is compressed with xz, which nspawn does not read"),
    })
}

/// A file of the archive, in the store's blob directory under a name of its own until
/// an image needs it.
#[derive(Debug, Clone)]
struct Part {
    path: PathBuf,
    digest: String,
    size: u64,
}

/// What an archive brought.
#[derive(Debug, Default)]
struct Received {
    /// Its files by their name in the archive.
    files: HashMap<String, Part>,
    /// Its links (symbolic and hard) by their name, to the name they stand for.
    links: HashMap<String, String>,
    /// The directory of the load its files are in, removed with what is left in it once
    /// the load is over, however it ends.
    dir: Option<PathBuf>,
}

impl Drop for Received {
    fn drop(&mut self) {
        if let Some(dir) = &self.dir {
            let _ = crate::store::remove_tree(dir);
        }
    }
}

impl Received {
    fn file(&self, name: &str) -> Option<&Part> {
        let mut name = name.to_string();
        for _ in 0..LINK_HOPS {
            if let Some(part) = self.files.get(&name) {
                return Some(part);
            }
            name = self.links.get(&name)?.clone();
        }
        None
    }

    /// The blob of `digest`, which an OCI layout keeps under blobs/sha256/.
    fn blob(&self, digest: &str) -> Result<&Part> {
        validate_digest(digest)?;
        let name = format!("blobs/sha256/{}", digest.trim_start_matches("sha256:"));
        let part = self
            .file(&name)
            .ok_or_else(|| anyhow!("the archive has no blob {digest}"))?;
        if part.digest != digest {
            bail!(
                "{name} of the archive is not what its name says: its digest is {}",
                part.digest
            );
        }
        Ok(part)
    }
}

fn read_json(part: &Part, what: &str) -> Result<Vec<u8>> {
    if part.size > JSON_MAX {
        bail!(
            "{what} takes {} bytes, more than a JSON file of an image does",
            part.size
        );
    }
    fs::read(&part.path).with_context(|| format!("reading {what}"))
}

/// A name in the archive as a relative path, None for one that climbs out of it.
fn archive_name(path: &Path) -> Option<String> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(c) => out.push(c),
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) => return None,
        }
    }
    (!out.as_os_str().is_empty()).then(|| out.to_string_lossy().into_owned())
}

/// Copies `from` into a new part file under `dir`, its sha256 taken on the way.
fn take_part(dir: &Path, n: usize, from: &mut dyn Read) -> Result<Part> {
    let path = dir.join(format!("part-{n}"));
    let file = File::create(&path).with_context(|| format!("creating {}", path.display()))?;
    let mut out = BufWriter::new(file);
    let mut hash = Sha256::new();
    let mut size = 0u64;
    let mut buf = vec![0u8; 1 << 16];
    let copied = (|| -> Result<()> {
        loop {
            let n = match from.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e).context("reading the archive"),
            };
            hash.update(&buf[..n]);
            out.write_all(&buf[..n])
                .with_context(|| format!("writing {}", path.display()))?;
            size += n as u64;
        }
        out.flush()
            .with_context(|| format!("writing {}", path.display()))
    })();
    if let Err(e) = copied {
        let _ = fs::remove_file(&path);
        return Err(e);
    }
    Ok(Part {
        path,
        digest: format!("sha256:{}", hex::encode(hash.finalize())),
        size,
    })
}

/// Reads the archive on `input` into part files under `dir`, the load's own directory,
/// which goes with what the load leaves in it.
fn receive(dir: &Path, input: OwnedFd) -> Result<Received> {
    let mut received = Received {
        files: HashMap::new(),
        links: HashMap::new(),
        dir: Some(dir.to_path_buf()),
    };
    let result = (|| -> Result<()> {
        let reader = decompressed(File::from(input), "the archive")?;
        let mut archive = tar::Archive::new(reader);
        for (n, entry) in archive
            .entries()
            .context("reading the archive")?
            .enumerate()
        {
            let mut entry = entry.context("reading the archive")?;
            let path = entry
                .path()
                .context("reading a name in the archive")?
                .into_owned();
            let Some(name) = archive_name(&path) else {
                continue;
            };
            match entry.header().entry_type() {
                tar::EntryType::Regular | tar::EntryType::Continuous => {
                    let part = take_part(dir, n, &mut entry)
                        .with_context(|| format!("reading {name} of the archive"))?;
                    if let Some(old) = received.files.insert(name, part) {
                        let _ = fs::remove_file(old.path);
                    }
                }
                kind @ (tar::EntryType::Symlink | tar::EntryType::Link) => {
                    let Some(target) = entry.link_name().ok().flatten() else {
                        continue;
                    };
                    // A symlink is relative to its directory, a hard link names a file of
                    // the archive.
                    let target = if kind == tar::EntryType::Symlink && target.is_relative() {
                        Path::new(&name)
                            .parent()
                            .unwrap_or_else(|| Path::new(""))
                            .join(target)
                    } else {
                        target.into_owned()
                    };
                    if let Some(target) = normalized(&target) {
                        received.links.insert(name, target);
                    }
                }
                _ => {}
            }
        }
        Ok(())
    })();
    result.map(|()| received)
}

/// A path with its `..` taken against what comes before them, None for one that leaves
/// the archive.
fn normalized(path: &Path) -> Option<String> {
    let mut parts: Vec<&std::ffi::OsStr> = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(c) => parts.push(c),
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::RootDir | Component::CurDir => {}
            Component::Prefix(_) => return None,
        }
    }
    let out: PathBuf = parts.iter().collect();
    (!out.as_os_str().is_empty()).then(|| out.to_string_lossy().into_owned())
}

/// The blobs an image needs, by digest, where the archive brought them.
type Blobs = Vec<(String, PathBuf)>;

/// An image of the archive, checked: its manifest, the parts it needs in the store (its
/// config and layers, by digest) and its config's digest.
#[derive(Debug, PartialEq, Eq)]
struct Checked {
    manifest: Vec<u8>,
    blobs: Blobs,
    id: String,
}

/// An image the archive holds, to install under `reference`; the names of one image
/// share it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Found {
    reference: String,
    image: Arc<Checked>,
}

/// An image name of an archive: without a registry, Docker Hub's, as the Docker image
/// archive's names go; but under the configured `registry` for one whose first part is
/// that registry, which nspawn writes without a dot or a port where it has none.
fn image_name(name: &str, registry: &str) -> Result<String> {
    let parsed = match name.split_once('/') {
        Some((first, _)) if first == registry => ImageRef::parse(name, registry),
        _ => ImageRef::normalized(name),
    };
    parsed.map(|r| r.to_string())
}

/// The reference an index entry's annotations name, read by `image_name`; None when
/// they name only a tag, or nothing.
fn annotated_reference(
    annotations: Option<&std::collections::BTreeMap<String, String>>,
    registry: &str,
) -> Option<String> {
    let annotations = annotations?;
    if let Some(name) = annotations.get(IMAGE_NAME) {
        return image_name(name, registry).ok();
    }
    let name = annotations.get(REF_NAME)?;
    // The OCI spec lets it be a tag alone; one with a repository is a reference.
    if !name.contains('/') && !name.contains(':') {
        return None;
    }
    image_name(name, registry).ok()
}

/// Checks an image manifest of the archive and what it needs.
fn checked(received: &Received, manifest_bytes: Vec<u8>, what: &str) -> Result<Checked> {
    let manifest: OciImageManifest = serde_json::from_slice(&manifest_bytes)
        .with_context(|| format!("reading the manifest of {what}"))?;
    let config = received.blob(&manifest.config.digest)?;
    if i64::try_from(config.size).ok() != Some(manifest.config.size) {
        bail!("the config of {what} is not the size its manifest gives");
    }
    let config_bytes = read_json(config, &format!("the config of {what}"))?;
    let (runs, platform) = crate::hub::host_runs_config(&config_bytes);
    if !runs {
        bail!(
            "{what} is an image for {platform}, which this host ({}) does not run",
            crate::hub::host_platform_name()
        );
    }
    let parsed: image::Config = serde_json::from_slice(&config_bytes)
        .with_context(|| format!("reading the config of {what}"))?;
    if parsed.rootfs.diff_ids.len() != manifest.layers.len() {
        bail!(
            "the config of {what} lists {} layers, its manifest {}",
            parsed.rootfs.diff_ids.len(),
            manifest.layers.len()
        );
    }
    let mut blobs = vec![(manifest.config.digest.clone(), config.path.clone())];
    for layer in &manifest.layers {
        let part = received.blob(&layer.digest)?;
        if i64::try_from(part.size).ok() != Some(layer.size) {
            bail!(
                "layer {} of {what} is not the size its manifest gives",
                layer.digest
            );
        }
        blobs.push((layer.digest.clone(), part.path.clone()));
    }
    Ok(Checked {
        manifest: manifest_bytes,
        blobs,
        id: manifest.config.digest,
    })
}

/// The image manifest of an index entry for this host: the entry's own, or the one an
/// index it names (several platforms) has for this host.
fn host_manifest(
    received: &Received,
    digest: &str,
    media_type: &str,
    what: &str,
) -> Result<Vec<u8>> {
    let part = received.blob(digest)?;
    let bytes = read_json(part, &format!("the manifest of {what}"))?;
    let nested = media_type.contains("index") || media_type.contains("manifest.list");
    if !nested {
        return Ok(bytes);
    }
    let index: OciImageIndex =
        serde_json::from_slice(&bytes).with_context(|| format!("reading the index of {what}"))?;
    // An index may come without the blobs of the platforms not saved; a blob that is
    // there is what its name says, or the load fails.
    let present = |digest: &str| -> Result<Option<&Part>> {
        validate_digest(digest)?;
        let name = format!("blobs/sha256/{}", digest.trim_start_matches("sha256:"));
        match received.file(&name) {
            Some(_) => received.blob(digest).map(Some),
            None => Ok(None),
        }
    };
    let mut held = Vec::new();
    'entries: for i in crate::hub::host_entries(&index.manifests) {
        let entry = &index.manifests[i];
        let Some(part) = present(&entry.digest)? else {
            continue;
        };
        let bytes = read_json(part, &format!("the manifest of {what}"))?;
        let Ok(manifest) = serde_json::from_slice::<OciImageManifest>(&bytes) else {
            continue;
        };
        let Some(config) = present(&manifest.config.digest)? else {
            continue;
        };
        for layer in &manifest.layers {
            if present(&layer.digest)?.is_none() {
                continue 'entries;
            }
        }
        let (runs, platform) = crate::hub::host_runs_config(&read_json(config, "a config")?);
        if runs {
            return Ok(bytes);
        }
        held.push(platform);
    }
    let held = if held.is_empty() {
        String::new()
    } else {
        format!("; it holds {}", held.join(", "))
    };
    bail!(
        "the archive has no image of {what} for this host ({}){held}",
        crate::hub::host_platform_name()
    )
}

/// The OCI media type of a layer of a Docker image archive, from its first bytes.
fn layer_media_type(part: &Part) -> Result<String> {
    let mut head = [0u8; 8];
    let n = File::open(&part.path)
        .and_then(|mut f| f.read(&mut head))
        .with_context(|| format!("reading {}", part.path.display()))?;
    Ok(match detect(&head[..n]) {
        Compression::None => "application/vnd.oci.image.layer.v1.tar".to_string(),
        Compression::Gzip => LAYER_MEDIA_TYPE.to_string(),
        Compression::Zstd => "application/vnd.oci.image.layer.v1.tar+zstd".to_string(),
        Compression::Bzip2 | Compression::Xz => {
            bail!("a layer of the archive is compressed in a way OCI images are not")
        }
    })
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ArchiveEntry {
    config: String,
    #[serde(default)]
    repo_tags: Option<Vec<String>>,
    #[serde(default)]
    layers: Vec<String>,
}

/// The images of a Docker image archive's manifest.json, each with a manifest made for
/// it from its config and layers.
fn from_manifest_json(
    received: &Received,
    bytes: &[u8],
    registry: &str,
) -> Result<(Vec<Found>, Vec<String>)> {
    let entries: Option<Vec<ArchiveEntry>> =
        serde_json::from_slice(bytes).context("reading manifest.json of the archive")?;
    let entries = entries.ok_or_else(|| anyhow!("manifest.json of the archive is null"))?;
    let mut found = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    let mut unnamed = Vec::new();
    for entry in entries {
        let config = received
            .file(&normalized(Path::new(&entry.config)).unwrap_or_default())
            .ok_or_else(|| anyhow!("the archive has no {}", entry.config))?;
        let tags = entry.repo_tags.unwrap_or_default();
        if tags.is_empty() {
            unnamed.push(config.digest.clone());
            continue;
        }
        let config_bytes = read_json(config, &entry.config)?;
        let mut layers = Vec::new();
        for path in &entry.layers {
            let part = received
                .file(&normalized(Path::new(path)).unwrap_or_default())
                .ok_or_else(|| anyhow!("the archive has no {path}"))?;
            layers.push((part.digest.clone(), layer_media_type(part)?, part.size));
        }
        let manifest = image::manifest(&config_bytes, &layers, &Default::default());
        // The blobs it names are the parts by digest: a layout with them is what
        // `checked` reads.
        let mut by_digest = Received::default();
        by_digest.files.insert(
            format!(
                "blobs/sha256/{}",
                config.digest.trim_start_matches("sha256:")
            ),
            config.clone(),
        );
        for path in &entry.layers {
            let part = received
                .file(&normalized(Path::new(path)).unwrap_or_default())
                .expect("found above");
            by_digest.files.insert(
                format!("blobs/sha256/{}", part.digest.trim_start_matches("sha256:")),
                part.clone(),
            );
        }
        let references = tags
            .iter()
            .map(|tag| {
                image_name(tag, registry)
                    .map_err(|e| anyhow!("invalid tag {tag:?} in the archive: {e:#}"))
            })
            .collect::<Result<Vec<_>>>()?;
        let image = Arc::new(checked(&by_digest, manifest, &references[0])?);
        // A tag given again names the image of its last entry, as a load of each entry
        // in turn leaves it.
        for reference in references {
            match at.get(&reference) {
                Some(&i) => {
                    found[i] = Found {
                        reference,
                        image: image.clone(),
                    }
                }
                None => {
                    at.insert(reference.clone(), found.len());
                    found.push(Found {
                        reference,
                        image: image.clone(),
                    });
                }
            }
        }
    }
    Ok((found, unnamed))
}

/// The images of an OCI layout's index.json; an entry that names no reference takes the
/// tags manifest.json gives its config.
fn from_index(
    received: &Received,
    bytes: &[u8],
    archive: Option<&[u8]>,
    registry: &str,
) -> Result<(Vec<Found>, Vec<String>)> {
    let index: OciImageIndex =
        serde_json::from_slice(bytes).context("reading index.json of the archive")?;
    let tags_of: HashMap<String, Vec<String>> = archive
        .and_then(|a| serde_json::from_slice::<Vec<ArchiveEntry>>(a).ok())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|e| {
            let digest = received
                .file(&normalized(Path::new(&e.config))?)?
                .digest
                .clone();
            Some((digest, e.repo_tags.unwrap_or_default()))
        })
        .collect();
    let mut found: Vec<Found> = Vec::new();
    let mut named = HashSet::new();
    let mut unnamed = Vec::new();
    // An image many entries name (a tag each), or many indexes, is read and checked once.
    let mut seen: HashMap<String, Arc<Checked>> = HashMap::new();
    let mut manifests: HashMap<String, Arc<Checked>> = HashMap::new();
    for entry in &index.manifests {
        let what = annotated_reference(entry.annotations.as_ref(), registry);
        let label = what.clone().unwrap_or_else(|| entry.digest.clone());
        let image = match seen.get(&entry.digest) {
            Some(done) => done.clone(),
            None => {
                let manifest = host_manifest(received, &entry.digest, &entry.media_type, &label)?;
                let digest = crate::layout::sha256_digest(&manifest);
                let done = match manifests.get(&digest) {
                    Some(done) => done.clone(),
                    None => {
                        let done = Arc::new(checked(received, manifest, &label)?);
                        manifests.insert(digest, done.clone());
                        done
                    }
                };
                seen.insert(entry.digest.clone(), done.clone());
                done
            }
        };
        let references: Vec<String> = match what {
            Some(reference) => vec![reference],
            None => tags_of
                .get(&image.id)
                .map(|tags| {
                    tags.iter()
                        .filter_map(|t| image_name(t, registry).ok())
                        .collect()
                })
                .unwrap_or_default(),
        };
        if references.is_empty() {
            unnamed.push(image.id.clone());
            continue;
        }
        for reference in references {
            if named.insert(reference.clone()) {
                found.push(Found {
                    reference,
                    image: image.clone(),
                });
            }
        }
    }
    Ok((found, unnamed))
}

/// The images of a received archive, every one checked.
fn images_of(received: &Received, registry: &str) -> Result<Vec<Found>> {
    let small = |name: &str| -> Result<Option<Vec<u8>>> {
        received
            .file(name)
            .map(|part| read_json(part, name))
            .transpose()
    };
    let index = small("index.json")?;
    let archive = small("manifest.json")?;
    let (found, unnamed) = match (&index, &archive) {
        (Some(index), archive) => from_index(received, index, archive.as_deref(), registry)?,
        (None, Some(archive)) => from_manifest_json(received, archive, registry)?,
        (None, None) => bail!(
            "the archive has neither an index.json nor a manifest.json; it is no image archive save writes"
        ),
    };
    if found.is_empty() {
        if unnamed.is_empty() {
            bail!("the archive holds no image");
        }
        bail!(
            "the archive holds no image with a name; nspawn keeps an image under its name (save it by name): {}",
            unnamed.join(", ")
        );
    }
    if !unnamed.is_empty() {
        bail!(
            "the archive holds images without a name, which nspawn has no way to keep (save them by name): {}",
            unnamed.join(", ")
        );
    }
    Ok(found)
}

/// Moves the parts `found` needs into the store's blobs, under the store lock, and holds
/// them; the rest goes with the load's directory.
async fn keep(
    store: &Store,
    received: &Received,
    found: &[Found],
) -> Result<crate::store::BlobHold> {
    let needed: HashMap<String, PathBuf> = {
        let mut images = HashSet::new();
        found
            .iter()
            .filter(|f| images.insert(Arc::as_ptr(&f.image)))
            .flat_map(|f| f.image.blobs.iter().cloned())
            .collect()
    };
    let digests: Vec<String> = needed.keys().cloned().collect();
    let hold = {
        let _still = store.holds_still()?;
        store.hold_blobs(&digests)?
    };
    let _lock = store.lock().await?;
    for (digest, part) in &needed {
        let blob = store.blob_path(digest);
        if blob.exists() {
            continue;
        }
        fs::rename(part, &blob).with_context(|| format!("keeping {}", blob.display()))?;
    }
    let _ = received;
    Ok(hold)
}

/// Loads the images of the archive on `input`.
pub async fn load(ctx: &Context, input: OwnedFd, report: Report<'_>) -> Result<Vec<Loaded>> {
    crate::api::require_root("load")?;
    let store = &ctx.store;
    let sd = ctx.sd().await?;
    store.init()?;
    let dir = store.load_dir()?;
    let received = crate::api::copy::on_thread(move || receive(&dir, input)).await?;
    let found = images_of(&received, &ctx.config.registry)
        .map_err(|e| FailureKind::Invalid.error(format!("{e:#}")))?;
    // Every name is checked before a blob is kept.
    let mut named = Vec::new();
    for f in &found {
        named.push(crate::api::commit::target(
            &f.reference,
            &ctx.config.registry,
        )?);
    }
    let hold = keep(store, &received, &found).await?;
    drop(received);
    let backend = Backend::choose(ctx.config.backend, sd).await?;
    let mut loaded = Vec::new();
    for (f, (name, reference)) in found.iter().zip(&named) {
        let reserved: BTreeSet<String> = named
            .iter()
            .map(|(n, _)| n.clone())
            .filter(|n| n != name)
            .collect();
        let installed = crate::install::install_made_here(
            store,
            sd,
            &ctx.config,
            backend,
            (name, reference),
            &f.image.manifest,
            "load",
            None,
            &reserved,
            report,
        )
        .await;
        match installed {
            Ok(name) => {
                line(report, format!("Loaded image: {reference}"));
                loaded.push(Loaded {
                    name,
                    reference: reference.clone(),
                    id: f.image.id.clone(),
                });
            }
            Err(e) if loaded.is_empty() => return Err(e),
            Err(e) => {
                let done: Vec<&str> = loaded.iter().map(|l| l.reference.as_str()).collect();
                return Err(e.context(format!("{} loaded already", done.join(", "))));
            }
        }
    }
    drop(hold);
    Ok(loaded)
}

#[derive(Debug, Clone, Default)]
pub struct ImportRequest {
    /// REPOSITORY[:TAG] of the new image.
    pub reference: String,
    /// The comment of its history entry.
    pub message: Option<String>,
    /// Dockerfile instructions that set its settings.
    pub changes: Vec<String>,
}

/// The archive an import reads, passed on to the gzip blob as it is read, its digest and
/// its last two blocks taken on the way.
struct Teed<R: Read, W: Write> {
    from: R,
    gzip: GzEncoder<W>,
    diff: Sha256,
    size: u64,
    /// The last bytes read, up to two blocks.
    tail: Vec<u8>,
    /// Whether the input has ended.
    ended: bool,
}

impl<R: Read, W: Write> Read for Teed<R, W> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.from.read(buf)?;
        if n == 0 && !buf.is_empty() {
            self.ended = true;
        }
        self.gzip.write_all(&buf[..n])?;
        self.diff.update(&buf[..n]);
        self.size += n as u64;
        self.tail.extend_from_slice(&buf[..n]);
        if self.tail.len() > 1024 {
            self.tail.drain(..self.tail.len() - 1024);
        }
        Ok(n)
    }
}

/// Writes the tar on `input` (compressed or not) as a gzip layer blob into a part file
/// under `dir`; its diff_id, digest and size. The tar is read through, entry by entry, and
/// must end with its two zero blocks: a stream cut short is refused, not taken for the
/// whole of a root filesystem.
fn layer_from(dir: &Path, input: OwnedFd) -> Result<(PathBuf, String, String, u64)> {
    let path = dir.join(format!(".part-import-{}", unique_suffix()));
    let made = (|| -> Result<(String, String, u64)> {
        let from = decompressed(File::from(input), "the archive")?;
        let file = File::create(&path).with_context(|| format!("creating {}", path.display()))?;
        let blob = Hashed {
            inner: BufWriter::new(file),
            hash: Sha256::new(),
            size: 0,
        };
        let mut teed = Teed {
            from,
            gzip: GzEncoder::new(blob, flate2::Compression::new(GZIP_LEVEL)),
            diff: Sha256::new(),
            size: 0,
            tail: Vec::new(),
            ended: false,
        };
        {
            let mut archive = tar::Archive::new(&mut teed);
            for entry in archive.entries().context("reading the archive")? {
                let mut entry = entry.context("reading the archive")?;
                io::copy(&mut entry, &mut io::sink()).context("reading the archive")?;
            }
        }
        // The entries end at a zero block, which a whole tar has before its input ends;
        // a stream cut where a header would start ends them too.
        let cut = teed.ended;
        // What follows the end of the archive: the rest of its last record.
        io::copy(&mut teed, &mut io::sink()).context("reading the archive")?;
        if teed.size == 0 {
            bail!("the archive is empty");
        }
        if cut
            || teed.size % 512 != 0
            || teed.tail.len() < 1024
            || teed.tail.iter().any(|b| *b != 0)
        {
            bail!("the archive ends before the end of the tar: it was cut short");
        }
        let Teed { gzip, diff, .. } = teed;
        let mut blob = gzip.finish().context("writing the layer")?;
        blob.inner.flush().context("writing the layer")?;
        Ok((
            format!("sha256:{}", hex::encode(diff.finalize())),
            format!("sha256:{}", hex::encode(blob.hash.finalize())),
            blob.size,
        ))
    })();
    match made {
        Ok((diff_id, digest, size)) => Ok((path, diff_id, digest, size)),
        Err(e) => {
            let _ = fs::remove_file(&path);
            Err(e)
        }
    }
}

/// A part file, removed when it goes out of scope unless it was renamed meanwhile.
pub(crate) struct PartFile(pub PathBuf);

impl Drop for PartFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

struct Hashed<W: Write> {
    inner: W,
    hash: Sha256,
    size: u64,
}

impl<W: Write> Write for Hashed<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hash.update(&buf[..n]);
        self.size += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Imports the tar of a root filesystem on `input` as image `req.reference`.
pub async fn import(
    ctx: &Context,
    input: OwnedFd,
    req: &ImportRequest,
    report: Report<'_>,
) -> Result<Loaded> {
    crate::api::require_root("import")?;
    let (own, reference) = crate::api::commit::target(&req.reference, &ctx.config.registry)?;
    change::apply(&mut image::Config::scratch(), &req.changes)
        .map_err(|e| FailureKind::Invalid.error(format!("{e:#}")))?;
    let store = &ctx.store;
    let sd = ctx.sd().await?;
    store.init()?;
    line(report, format!("importing {reference}"));
    let dir = store.blobs_dir();
    let (part, diff_id, digest, size) =
        crate::api::copy::on_thread(move || layer_from(&dir, input)).await?;
    // Gone however the import ends; kept only by its rename into the blobs.
    let part = PartFile(part);
    let created = timestamp_now();
    let mut config = image::Config::scratch();
    config.created = Some(created.clone());
    config.rootfs.diff_ids.push(diff_id);
    config.history.push(History {
        created: Some(created),
        created_by: String::new(),
        author: None,
        comment: req
            .message
            .clone()
            .unwrap_or_else(|| "Imported from -".to_string()),
        empty_layer: false,
    });
    change::apply(&mut config, &req.changes)?;
    let config_bytes = config.to_bytes();
    let id = crate::layout::sha256_digest(&config_bytes);
    let manifest = image::manifest(
        &config_bytes,
        &[(digest.clone(), LAYER_MEDIA_TYPE.to_string(), size)],
        &Default::default(),
    );
    let hold = {
        let _still = store.holds_still()?;
        store.hold_blobs(&[digest.clone(), id.clone()])?
    };
    {
        let _lock = store.lock().await?;
        let blob = store.blob_path(&digest);
        if !blob.exists() {
            fs::rename(&part.0, &blob).with_context(|| format!("keeping {}", blob.display()))?;
        }
        let path = store.blob_path(&id);
        if !path.exists() {
            fs::write(&path, &config_bytes)
                .with_context(|| format!("writing {}", path.display()))?;
        }
    }
    let backend = Backend::choose(ctx.config.backend, sd).await?;
    let name = crate::install::install_made_here(
        store,
        sd,
        &ctx.config,
        backend,
        (&own, &reference),
        &manifest,
        "import",
        None,
        &BTreeSet::new(),
        report,
    )
    .await?;
    drop(hold);
    Ok(Loaded {
        name,
        reference,
        id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tar_of(files: &[(&str, &[u8])], links: &[(&str, &str)]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (name, data) in files {
            let mut h = tar::Header::new_ustar();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_entry_type(tar::EntryType::Regular);
            b.append_data(&mut h, name, *data).unwrap();
        }
        for (name, target) in links {
            let mut h = tar::Header::new_ustar();
            h.set_entry_type(tar::EntryType::Symlink);
            h.set_size(0);
            b.append_link(&mut h, name, target).unwrap();
        }
        b.into_inner().unwrap()
    }

    /// What a load of `bytes` receives, in a directory of its own below `dir`.
    fn received(bytes: Vec<u8>, dir: &Path) -> Received {
        let dir = dir.join(unique_suffix());
        fs::create_dir_all(&dir).unwrap();
        let (r, w) = nix::unistd::pipe().unwrap();
        let writer = std::thread::spawn(move || File::from(w).write_all(&bytes).unwrap());
        let got = receive(&dir, r).unwrap();
        writer.join().unwrap();
        got
    }

    fn digest(bytes: &[u8]) -> String {
        crate::layout::sha256_digest(bytes)
    }

    fn host_config(layers: &[&[u8]]) -> Vec<u8> {
        let mut config = image::Config::scratch();
        config.rootfs.diff_ids = layers.iter().map(|l| digest(l)).collect();
        config.to_bytes()
    }

    #[test]
    fn names_never_leave_the_archive() {
        assert_eq!(
            archive_name(Path::new("./blobs/sha256/x")).as_deref(),
            Some("blobs/sha256/x")
        );
        assert_eq!(archive_name(Path::new("../x")), None);
        assert_eq!(
            normalized(Path::new("a/../b/layer.tar")).as_deref(),
            Some("b/layer.tar")
        );
        assert_eq!(normalized(Path::new("../b")), None);
    }

    #[test]
    fn a_name_under_the_configured_registry_is_that_registry_s() {
        assert_eq!(image_name("myhub/app:1", "myhub").unwrap(), "myhub/app:1");
        assert_eq!(
            image_name("myhub/app:1", "hub.nspawn.org").unwrap(),
            "docker.io/myhub/app:1"
        );
        assert_eq!(
            image_name("busybox", "myhub").unwrap(),
            "docker.io/library/busybox:latest"
        );
        assert_eq!(
            image_name("hub.nspawn.org/fedora:44", "myhub").unwrap(),
            "hub.nspawn.org/fedora:44"
        );
    }

    #[test]
    fn an_oci_layout_loads_by_its_index() {
        let tmp = tempfile::tempdir().unwrap();
        let layer = b"layer bytes".as_slice();
        let config = host_config(&[layer]);
        let manifest = image::manifest(
            &config,
            &[(
                digest(layer),
                "application/vnd.oci.image.layer.v1.tar".into(),
                layer.len() as u64,
            )],
            &Default::default(),
        );
        let index = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "manifests": [{
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "digest": digest(&manifest), "size": manifest.len(),
                "annotations": {IMAGE_NAME: "docker.io/library/busybox:1", REF_NAME: "1"},
            }],
        }))
        .unwrap();
        let blob = |b: &[u8]| format!("blobs/sha256/{}", digest(b).trim_start_matches("sha256:"));
        let (lb, cb, mb) = (blob(layer), blob(&config), blob(&manifest));
        let archive = tar_of(
            &[
                ("index.json", &index),
                (&lb, layer),
                (&cb, &config),
                (&mb, &manifest),
            ],
            &[],
        );
        let got = received(archive, tmp.path());
        let found = images_of(&got, "hub.nspawn.org").unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].reference, "docker.io/library/busybox:1");
        assert_eq!(found[0].image.manifest, manifest);
        assert_eq!(found[0].image.id, digest(&config));
        assert_eq!(found[0].image.blobs.len(), 2);
        // What is left of a load goes with it.
        let dir = got.dir.clone().unwrap();
        drop(got);
        assert!(!dir.exists());
    }

    #[test]
    fn an_index_of_platforms_gives_the_best_one_the_archive_holds() {
        let tmp = tempfile::tempdir().unwrap();
        let config = host_config(&[]);
        let manifest = image::manifest(&config, &[], &Default::default());
        let host = crate::hub::host_platform_name();
        let mut parts = host.split('/');
        let (os, architecture) = (parts.next().unwrap(), parts.next().unwrap());
        let missing = format!("sha256:{}", "e".repeat(64));
        let entry = |digest: &str, platform: Option<serde_json::Value>| {
            let mut e = serde_json::json!({
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "digest": digest, "size": 2,
            });
            if let Some(p) = platform {
                e["platform"] = p;
            }
            e
        };
        let platforms = |first: &str| {
            serde_json::to_vec(&serde_json::json!({
                "schemaVersion": 2,
                "mediaType": "application/vnd.oci.image.index.v1+json",
                "manifests": [
                    entry(first, Some(serde_json::json!({"os": os, "architecture": architecture}))),
                    entry(&digest(&manifest), None),
                ],
            }))
            .unwrap()
        };
        let blob = |b: &[u8]| format!("blobs/sha256/{}", digest(b).trim_start_matches("sha256:"));
        let archive = |nested: &[u8], extra: &[(&str, &[u8])]| {
            let index = serde_json::to_vec(&serde_json::json!({
                "schemaVersion": 2,
                "manifests": [{
                    "mediaType": "application/vnd.oci.image.index.v1+json",
                    "digest": digest(nested), "size": nested.len(),
                    "annotations": {IMAGE_NAME: "docker.io/library/app:1"},
                }],
            }))
            .unwrap();
            let mut files: Vec<(String, Vec<u8>)> = vec![
                ("index.json".into(), index),
                (blob(nested), nested.to_vec()),
                (blob(&config), config.clone()),
                (blob(&manifest), manifest.clone()),
            ];
            files.extend(extra.iter().map(|(n, b)| (n.to_string(), b.to_vec())));
            let refs: Vec<(&str, &[u8])> = files
                .iter()
                .map(|(n, b)| (n.as_str(), b.as_slice()))
                .collect();
            tar_of(&refs, &[])
        };
        // The host's own platform is not in the archive: the next one it runs is taken.
        let nested = platforms(&missing);
        let got = received(archive(&nested, &[]), tmp.path());
        let found = images_of(&got, "hub.nspawn.org").unwrap();
        assert_eq!(found[0].image.manifest, manifest);
        drop(got);
        // One that is there and is not what its name says fails the load.
        let wrong = format!("blobs/sha256/{}", "e".repeat(64));
        let got = received(archive(&nested, &[(&wrong, b"{}")]), tmp.path());
        let e = images_of(&got, "hub.nspawn.org").unwrap_err();
        assert!(format!("{e:#}").contains("not what its name says"), "{e:#}");
    }

    #[test]
    fn a_docker_archive_loads_by_its_manifest_json_links_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let layer = b"layer bytes".as_slice();
        let config = host_config(&[layer, layer]);
        let archive_json = serde_json::to_vec(&serde_json::json!([{
            "Config": "abc.json",
            "RepoTags": ["me/app:2", "hub.nspawn.org/web:latest", "me/app:2"],
            "Layers": ["one/layer.tar", "two/layer.tar"],
        }]))
        .unwrap();
        let archive = tar_of(
            &[
                ("manifest.json", &archive_json),
                ("abc.json", &config),
                ("one/layer.tar", layer),
            ],
            &[("two/layer.tar", "../one/layer.tar")],
        );
        let got = received(archive, tmp.path());
        let found = images_of(&got, "hub.nspawn.org").unwrap();
        let references: Vec<&str> = found.iter().map(|f| f.reference.as_str()).collect();
        assert_eq!(
            references,
            ["docker.io/me/app:2", "hub.nspawn.org/web:latest"]
        );
        // The names of one image share it, checked once.
        assert!(Arc::ptr_eq(&found[0].image, &found[1].image));
        let manifest: OciImageManifest = serde_json::from_slice(&found[0].image.manifest).unwrap();
        assert_eq!(manifest.layers.len(), 2);
        assert_eq!(manifest.layers[1].digest, digest(layer));
        assert_eq!(
            manifest.layers[0].media_type,
            "application/vnd.oci.image.layer.v1.tar"
        );
        drop(got);
    }

    #[test]
    fn what_cannot_be_kept_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let config = host_config(&[]);
        // No name.
        let archive_json = serde_json::to_vec(
            &serde_json::json!([{"Config": "c.json", "RepoTags": null, "Layers": []}]),
        )
        .unwrap();
        let got = received(
            tar_of(
                &[("manifest.json", &archive_json), ("c.json", &config)],
                &[],
            ),
            tmp.path(),
        );
        let e = images_of(&got, "hub.nspawn.org").unwrap_err().to_string();
        assert!(e.contains("no image with a name"), "{e}");
        drop(got);
        // Another platform.
        let mut other = image::Config::scratch();
        other.architecture = "s390x".into();
        let other = other.to_bytes();
        let archive_json = serde_json::to_vec(
            &serde_json::json!([{"Config": "c.json", "RepoTags": ["x:1"], "Layers": []}]),
        )
        .unwrap();
        let got = received(
            tar_of(&[("manifest.json", &archive_json), ("c.json", &other)], &[]),
            tmp.path(),
        );
        assert!(images_of(&got, "hub.nspawn.org")
            .unwrap_err()
            .to_string()
            .contains("linux/s390x"));
        drop(got);
        // A blob that is not what its name says.
        let index = serde_json::to_vec(&serde_json::json!({"schemaVersion": 2, "manifests": [{
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": format!("sha256:{}", "0".repeat(64)), "size": 2,
            "annotations": {IMAGE_NAME: "docker.io/library/x:1"}}]}))
        .unwrap();
        let wrong = format!("blobs/sha256/{}", "0".repeat(64));
        let got = received(
            tar_of(&[("index.json", &index), (&wrong, b"{}")], &[]),
            tmp.path(),
        );
        assert!(images_of(&got, "hub.nspawn.org")
            .unwrap_err()
            .to_string()
            .contains("not what its name says"));
        drop(got);
        // Nothing an image archive has.
        let got = received(tar_of(&[("hello", b"x")], &[]), tmp.path());
        assert!(images_of(&got, "hub.nspawn.org")
            .unwrap_err()
            .to_string()
            .contains("neither"));
        drop(got);
    }

    /// `bytes` on a pipe, a few at a time, as a slow client sends them.
    fn trickled(bytes: Vec<u8>) -> (OwnedFd, std::thread::JoinHandle<()>) {
        let (r, w) = nix::unistd::pipe().unwrap();
        let writer = std::thread::spawn(move || {
            let mut w = File::from(w);
            for chunk in bytes.chunks(3) {
                if w.write_all(chunk).is_err() {
                    return;
                }
                w.flush().unwrap();
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        });
        (r, writer)
    }

    #[test]
    fn a_compressed_stream_is_told_by_its_first_bytes_however_they_come() {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(b"plain text inside").unwrap();
        let (r, writer) = trickled(gz.finish().unwrap());
        let mut out = String::new();
        decompressed(File::from(r), "x")
            .unwrap()
            .read_to_string(&mut out)
            .unwrap();
        writer.join().unwrap();
        assert_eq!(out, "plain text inside");
    }

    #[test]
    fn an_import_cut_short_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let tar = tar_of(&[("etc/hostname", b"x\n"), ("etc/big", &[7u8; 4096])], &[]);
        // Cut inside the data of an entry, and at the end of one before the end blocks.
        for cut in [700, tar.len() - 1024] {
            let (r, writer) = trickled(tar[..cut].to_vec());
            let e = layer_from(tmp.path(), r).unwrap_err();
            writer.join().unwrap();
            assert!(format!("{e:#}").contains("archive"), "{cut}: {e:#}");
        }
        // Cut at the end of a file whose last blocks are zeros, as the end blocks are.
        let tar = tar_of(
            &[("etc/hostname", b"x\n"), ("var/zeros", &[0u8; 4096])],
            &[],
        );
        let (r, writer) = trickled(tar[..tar.len() - 1024].to_vec());
        let e = layer_from(tmp.path(), r).unwrap_err();
        writer.join().unwrap();
        assert!(format!("{e:#}").contains("cut short"), "{e:#}");
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 0);
        let (r, writer) = trickled(b"not a tar at all, this one".to_vec());
        assert!(layer_from(tmp.path(), r).is_err());
        writer.join().unwrap();
    }

    #[test]
    fn an_import_is_gzip_of_the_tar_it_was_given() {
        let tmp = tempfile::tempdir().unwrap();
        let tar = tar_of(&[("etc/hostname", b"x\n")], &[]);
        let (r, w) = nix::unistd::pipe().unwrap();
        let copy = tar.clone();
        let writer = std::thread::spawn(move || File::from(w).write_all(&copy).unwrap());
        let (path, diff_id, blob_digest, size) = layer_from(tmp.path(), r).unwrap();
        writer.join().unwrap();
        assert_eq!(diff_id, digest(&tar));
        let blob = fs::read(&path).unwrap();
        assert_eq!(blob.len() as u64, size);
        assert_eq!(blob_digest, digest(&blob));
        let mut back = Vec::new();
        flate2::read::GzDecoder::new(&blob[..])
            .read_to_end(&mut back)
            .unwrap();
        assert_eq!(back, tar);
    }
}
