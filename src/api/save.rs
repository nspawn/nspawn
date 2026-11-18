//! save: images as one tar that `load` here and elsewhere reads: an OCI image layout
//! (oci-layout, index.json, blobs/sha256/) with the manifest.json of the Docker image
//! archive beside it. The blobs go as the store keeps them, so an image pulled keeps the
//! digests its registry gave it.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::os::fd::OwnedFd;

use anyhow::{bail, Context as _, Result};
use oci_client::manifest::OciImageManifest;
use serde_json::json;

use crate::api::{line, Context, FailureKind, Report};
use crate::reference::ImageRef;
use crate::store::{validate_digest, Store};

const OCI_LAYOUT: &[u8] = br#"{"imageLayoutVersion":"1.0.0"}"#;
const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";
/// The name annotations of an index entry: the whole reference, and the tag alone.
pub const IMAGE_NAME: &str = "io.containerd.image.name";
pub const REF_NAME: &str = "org.opencontainers.image.ref.name";

/// One image of the archive.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Saved {
    reference: String,
    manifest: Vec<u8>,
    manifest_digest: String,
    media_type: String,
    config: String,
    layers: Vec<String>,
}

/// A reference as the Docker image archive's RepoTags write it: Docker Hub's without its
/// host, its official images without library/.
pub fn familiar(reference: &str) -> String {
    let rest = match reference.strip_prefix("docker.io/") {
        Some(rest) => rest,
        None => return reference.to_string(),
    };
    rest.strip_prefix("library/")
        .filter(|r| !r.contains('/'))
        .unwrap_or(rest)
        .to_string()
}

/// The tag of a reference, which the OCI ref.name annotation carries.
fn tag_of(reference: &str) -> String {
    ImageRef::parse(reference, "docker.io")
        .ok()
        .and_then(|r| r.tag)
        .unwrap_or_else(|| "latest".to_string())
}

/// The reference an archive names an image of `name` by: its record's, read as every
/// command reads it (`registry` the configured one), by its tag alone when it also has a
/// digest; an image pulled by its digest alone has no name an archive can give, as load
/// takes an image only under a tag.
fn archive_reference(name: &str, reference: &str, registry: &str) -> Result<String> {
    let parsed = ImageRef::parse(reference, registry)?;
    match parsed.tag {
        Some(tag) => Ok(format!("{}/{}:{tag}", parsed.registry, parsed.repository)),
        None => Err(FailureKind::Invalid.error(format!(
            "image {name} was pulled by its digest alone ({reference}); an archive names an image by a tag"
        ))),
    }
}

/// Refuses an image `save` cannot write: none of that name, or one without a tag.
pub fn check(store: &Store, name: &str, registry: &str) -> Result<()> {
    let record = store
        .load_image(name)?
        .ok_or_else(|| FailureKind::NoSuchImage.error(format!("no local image of {name}")))?;
    archive_reference(name, &record.reference, registry).map(|_| ())
}

fn saved(store: &Store, name: &str, registry: &str) -> Result<Saved> {
    let record = store
        .load_image(name)?
        .ok_or_else(|| FailureKind::NoSuchImage.error(format!("no local image of {name}")))?;
    let reference = archive_reference(name, &record.reference, registry)?;
    let manifest_bytes = store.load_manifest(name)?;
    let manifest: OciImageManifest = serde_json::from_slice(&manifest_bytes)
        .with_context(|| format!("reading the manifest of {name}"))?;
    let media_type = manifest
        .media_type
        .clone()
        .filter(|m| m == DOCKER_MANIFEST)
        .unwrap_or_else(|| OCI_MANIFEST.to_string());
    let layers: Vec<String> = manifest.layers.iter().map(|l| l.digest.clone()).collect();
    for digest in layers.iter().chain([&manifest.config.digest]) {
        validate_digest(digest)?;
        if !store.has_blob(digest) {
            bail!("blob {digest} of image {name} is missing from the store");
        }
    }
    Ok(Saved {
        reference,
        manifest_digest: crate::layout::sha256_digest(&manifest_bytes),
        manifest: manifest_bytes,
        media_type,
        config: manifest.config.digest,
        layers,
    })
}

/// index.json and manifest.json of `images`: an index entry per image, an archive entry
/// per manifest with the tags of every image that has it.
fn indexes(images: &[Saved]) -> (Vec<u8>, Vec<u8>) {
    let entries: Vec<serde_json::Value> = images
        .iter()
        .map(|s| {
            json!({
                "mediaType": s.media_type,
                "digest": s.manifest_digest,
                "size": s.manifest.len(),
                "annotations": {
                    IMAGE_NAME: s.reference,
                    REF_NAME: tag_of(&s.reference),
                },
            })
        })
        .collect();
    let index = json!({
        "schemaVersion": 2,
        "mediaType": OCI_INDEX,
        "manifests": entries,
    });
    let mut archive: Vec<(String, serde_json::Value)> = Vec::new();
    for s in images {
        let tag = familiar(&s.reference);
        if let Some((_, entry)) = archive.iter_mut().find(|(m, _)| *m == s.manifest_digest) {
            if let Some(tags) = entry["RepoTags"].as_array_mut() {
                if !tags.iter().any(|t| t == &json!(tag)) {
                    tags.push(json!(tag));
                }
            }
            continue;
        }
        archive.push((
            s.manifest_digest.clone(),
            json!({
                "Config": blob_name(&s.config),
                "RepoTags": [tag],
                "Layers": s.layers.iter().map(|l| blob_name(l)).collect::<Vec<_>>(),
            }),
        ));
    }
    let archive: Vec<serde_json::Value> = archive.into_iter().map(|(_, v)| v).collect();
    let mut manifest_json = serde_json::to_vec(&archive).expect("JSON serializes");
    manifest_json.push(b'\n');
    (
        serde_json::to_vec(&index).expect("JSON serializes"),
        manifest_json,
    )
}

/// Where a blob goes in the archive.
fn blob_name(digest: &str) -> String {
    format!("blobs/sha256/{}", digest.trim_start_matches("sha256:"))
}

fn header(kind: tar::EntryType, size: u64, mode: u32) -> tar::Header {
    let mut header = tar::Header::new_ustar();
    header.set_entry_type(kind);
    header.set_size(size);
    header.set_mode(mode);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header
}

/// Writes the archive of the images named `names` (local names) to `out`.
pub async fn save(ctx: &Context, names: &[String], out: OwnedFd, report: Report<'_>) -> Result<()> {
    crate::api::require_root("save")?;
    if names.is_empty() {
        return Err(FailureKind::Invalid.error("save takes at least one image"));
    }
    let store = &ctx.store;
    let mut images = Vec::new();
    for name in names {
        images.push(saved(store, name, &ctx.config.registry)?);
    }
    let digests: Vec<String> = images
        .iter()
        .flat_map(|s| s.layers.iter().cloned().chain([s.config.clone()]))
        .collect();
    let hold = {
        let _still = store.holds_still()?;
        store.hold_blobs(&digests)?
    };
    // Under the hold, each is there for as long as the archive is written.
    for digest in &digests {
        if !store.has_blob(digest) {
            bail!("blob {digest} went from the store while the save began");
        }
    }
    for s in &images {
        line(report, format!("saving {}", s.reference));
    }
    let (index, manifest_json) = indexes(&images);
    let mut blobs: BTreeMap<String, std::path::PathBuf> = BTreeMap::new();
    for s in &images {
        for digest in s.layers.iter().chain([&s.config]) {
            blobs.insert(digest.clone(), store.blob_path(digest));
        }
    }
    let manifests: BTreeMap<String, Vec<u8>> = images
        .iter()
        .map(|s| (s.manifest_digest.clone(), s.manifest.clone()))
        .collect();
    crate::api::copy::on_thread(move || {
        let mut tar = tar::Builder::new(BufWriter::new(File::from(out)));
        let mut put = |path: &str, bytes: &[u8]| -> Result<()> {
            let mut h = header(tar::EntryType::Regular, bytes.len() as u64, 0o644);
            tar.append_data(&mut h, path, bytes)
                .with_context(|| format!("writing {path}"))
        };
        put("oci-layout", OCI_LAYOUT)?;
        put("index.json", &index)?;
        put("manifest.json", &manifest_json)?;
        for dir in ["blobs/", "blobs/sha256/"] {
            let mut h = header(tar::EntryType::Directory, 0, 0o755);
            tar.append_data(&mut h, dir, std::io::empty())
                .with_context(|| format!("writing {dir}"))?;
        }
        for (digest, bytes) in &manifests {
            let mut h = header(tar::EntryType::Regular, bytes.len() as u64, 0o644);
            let path = blob_name(digest);
            tar.append_data(&mut h, &path, &bytes[..])
                .with_context(|| format!("writing {path}"))?;
        }
        for (digest, from) in &blobs {
            let file = File::open(from).with_context(|| format!("opening {}", from.display()))?;
            let size = file
                .metadata()
                .with_context(|| format!("looking at {}", from.display()))?
                .len();
            let mut h = header(tar::EntryType::Regular, size, 0o644);
            let path = blob_name(digest);
            tar.append_data(&mut h, &path, file)
                .with_context(|| format!("writing {path}"))?;
        }
        let mut out = tar.into_inner().context("finishing the archive")?;
        out.flush().context("writing the archive")?;
        Ok(())
    })
    .await?;
    drop(hold);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_archive_names_an_image_by_its_tag() {
        let digest = format!("sha256:{}", "a".repeat(64));
        assert_eq!(
            archive_reference(
                "b",
                &format!("docker.io/library/busybox:1@{digest}"),
                "hub.nspawn.org"
            )
            .unwrap(),
            "docker.io/library/busybox:1"
        );
        let e = archive_reference(
            "b",
            &format!("docker.io/library/busybox@{digest}"),
            "hub.nspawn.org",
        )
        .unwrap_err();
        assert_eq!(crate::api::kind_of(&e), Some(FailureKind::Invalid));
        // A configured registry without a dot or a port names its own images.
        assert_eq!(
            archive_reference("app-1", "myhub/app:1", "myhub").unwrap(),
            "myhub/app:1"
        );
    }

    #[test]
    fn docker_hub_names_are_written_short() {
        assert_eq!(familiar("docker.io/library/busybox:1.37"), "busybox:1.37");
        assert_eq!(familiar("docker.io/me/app:2"), "me/app:2");
        assert_eq!(
            familiar("hub.nspawn.org/fedora:44"),
            "hub.nspawn.org/fedora:44"
        );
        assert_eq!(tag_of("hub.nspawn.org/fedora:44"), "44");
    }

    #[test]
    fn one_archive_entry_per_manifest_with_every_tag() {
        let image = |reference: &str, manifest: &str| Saved {
            reference: reference.to_string(),
            manifest: b"{}".to_vec(),
            manifest_digest: manifest.to_string(),
            media_type: OCI_MANIFEST.to_string(),
            config: format!("sha256:{}", "c".repeat(64)),
            layers: vec![format!("sha256:{}", "a".repeat(64))],
        };
        let one = format!("sha256:{}", "1".repeat(64));
        let two = format!("sha256:{}", "2".repeat(64));
        let (index, archive) = indexes(&[
            image("docker.io/library/busybox:1", &one),
            image("docker.io/library/busybox:latest", &one),
            image("hub.nspawn.org/web:latest", &two),
        ]);
        let index: serde_json::Value = serde_json::from_slice(&index).unwrap();
        assert_eq!(index["manifests"].as_array().unwrap().len(), 3);
        assert_eq!(
            index["manifests"][0]["annotations"][IMAGE_NAME],
            "docker.io/library/busybox:1"
        );
        assert_eq!(index["manifests"][0]["annotations"][REF_NAME], "1");
        assert!(archive.ends_with(b"\n"));
        let archive: serde_json::Value = serde_json::from_slice(&archive).unwrap();
        assert_eq!(archive.as_array().unwrap().len(), 2);
        assert_eq!(
            archive[0]["RepoTags"],
            json!(["busybox:1", "busybox:latest"])
        );
        assert_eq!(
            archive[0]["Layers"][0],
            format!("blobs/sha256/{}", "a".repeat(64))
        );
        assert_eq!(archive[1]["RepoTags"], json!(["hub.nspawn.org/web:latest"]));
    }
}
