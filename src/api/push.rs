//! docker push: the blobs the registry lacks, then the manifest.

use anyhow::{bail, Context as _, Result};
use oci_client::manifest::OciImageManifest;

use crate::api::{line, Context, Report};
use crate::hub::{short_digest, Hub};
use crate::reference::ImageRef;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushRequest {
    /// Local image name, or the reference it was pulled from or built as.
    pub image: String,
    /// Push under another reference than the recorded one.
    pub to: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pushed {
    pub name: String,
    pub destination: String,
    /// Where the manifest now lives.
    pub url: String,
}

pub async fn push(ctx: &Context, request: &PushRequest, report: Report<'_>) -> Result<Pushed> {
    let config = &ctx.config;
    let store = &ctx.store;
    let record = store
        .find_image(&request.image, &config.registry)?
        .with_context(|| {
            format!(
                "no local image named {} (see nspawn images ls)",
                request.image
            )
        })?;
    let destination = match &request.to {
        Some(to) => ImageRef::parse(to, &config.registry)?,
        None => ImageRef::parse(&record.reference, &config.registry)?,
    };
    if destination.digest.is_some() {
        bail!("push needs a tag, not a digest");
    }
    let dest = destination.to_oci()?;
    let manifest_bytes = store.load_manifest(&record.name).with_context(|| {
        format!(
            "image {} has no stored manifest; pull or build it again",
            record.name
        )
    })?;
    let manifest: OciImageManifest = serde_json::from_slice(&manifest_bytes)
        .with_context(|| format!("parsing the stored manifest of {}", record.name))?;
    for descriptor in manifest
        .layers
        .iter()
        .chain(std::iter::once(&manifest.config))
    {
        if !store.has_blob(&descriptor.digest) {
            bail!(
                "blob {} of image {} is missing from {}",
                descriptor.digest,
                record.name,
                store.blobs_dir().display()
            );
        }
    }

    let hub = Hub::new(config)?;
    hub.authenticate_push(&dest).await?;
    line(
        report,
        format!(
            "pushing {} ({}) to {destination}",
            record.name,
            short_digest(&record.manifest_digest)
        ),
    );
    for descriptor in manifest
        .layers
        .iter()
        .chain(std::iter::once(&manifest.config))
    {
        if hub.blob_exists(&dest, &descriptor.digest).await? {
            line(
                report,
                format!(
                    "blob {}: already on the registry",
                    short_digest(&descriptor.digest)
                ),
            );
        } else {
            line(
                report,
                format!("blob {}: uploading", short_digest(&descriptor.digest)),
            );
            hub.upload_blob(
                &dest,
                &descriptor.digest,
                &store.blob_path(&descriptor.digest),
                report,
            )
            .await?;
        }
    }
    let media_type = manifest
        .media_type
        .as_deref()
        .unwrap_or("application/vnd.oci.image.manifest.v1+json");
    let url = hub
        .push_manifest(&dest, &manifest_bytes, media_type)
        .await?;
    crate::api::events::emit(
        "machine",
        "push",
        &record.name,
        &[
            ("image", &record.reference),
            ("reference", &destination.to_string()),
        ],
    );
    Ok(Pushed {
        name: record.name,
        destination: destination.to_string(),
        url,
    })
}
