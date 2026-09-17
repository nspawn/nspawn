//! Turns blobs in the store into a usable local image: assembly with a backend plus the
//! record and manifest that `images ls`, `images rm` and `push` rely on.

use anyhow::Result;
use oci_client::manifest::OciImageManifest;

use crate::backend::{Assembler, Backend, Layer};
use crate::store::{now_unix, ImageRecord, Store};
use crate::systemd::Systemd;

pub struct Install<'a> {
    pub name: &'a str,
    pub reference: &'a str,
    pub manifest_bytes: &'a [u8],
    pub manifest: &'a OciImageManifest,
    pub manifest_digest: &'a str,
    pub origin: &'a str,
}

/// All blobs named in the manifest must already be in the store.
pub async fn install(
    store: &Store,
    sd: &Systemd,
    backend: Backend,
    spec: Install<'_>,
) -> Result<()> {
    let layers: Vec<Layer> = spec
        .manifest
        .layers
        .iter()
        .map(|d| Layer {
            digest: d.digest.clone(),
            media_type: d.media_type.clone(),
            blob: store.blob_path(&d.digest),
        })
        .collect();
    let assembler = Assembler { store, sd };
    assembler.assemble(backend, spec.name, &layers).await?;
    store.save_manifest(spec.name, spec.manifest_bytes)?;
    store.record_image(&ImageRecord {
        name: spec.name.to_string(),
        reference: spec.reference.to_string(),
        manifest_digest: spec.manifest_digest.to_string(),
        layers: spec
            .manifest
            .layers
            .iter()
            .map(|l| l.digest.clone())
            .collect(),
        backend: backend.as_choice(),
        created: now_unix(),
        origin: spec.origin.to_string(),
    })?;
    Ok(())
}

/// Removes an existing image with the same name, or refuses when it is running or
/// `force` is not set.
pub async fn replace_existing(store: &Store, sd: &Systemd, name: &str, force: bool) -> Result<()> {
    let existing_record = store.load_image(name)?;
    let existing_image = sd.list_images().await?.into_iter().any(|i| i.name == name);
    if existing_record.is_none() && !existing_image {
        return Ok(());
    }
    if !force {
        anyhow::bail!(
            "image {name} already exists; use --force to replace it or --name for another name"
        );
    }
    if sd.machine_exists(name).await? {
        anyhow::bail!("machine {name} is running; stop it before replacing its image");
    }
    match existing_record {
        Some(rec) => {
            Assembler { store, sd }.remove(name, rec.backend).await?;
            store.remove_record(name)?;
        }
        None => sd.remove_image(name).await?,
    }
    Ok(())
}
