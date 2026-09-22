use anyhow::{bail, Context, Result};
use oci_client::manifest::OciImageManifest;

use crate::cli::PushArgs;
use crate::config::Config;
use crate::hub::{short_digest, Hub};
use crate::reference::ImageRef;
use crate::store::Store;

pub async fn run(args: PushArgs, config: &Config) -> Result<()> {
    let store = Store::new(&config.machines_dir, &config.state_dir);
    let record = store
        .find_image(&args.image, &config.registry)?
        .with_context(|| format!("no local image named {} (see nspawn images ls)", args.image))?;
    let destination = match &args.to {
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
    let manifest: OciImageManifest = serde_json::from_slice(&manifest_bytes)?;
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
    println!(
        "pushing {} ({}) to {destination}",
        record.name,
        short_digest(&record.manifest_digest)
    );
    for descriptor in manifest
        .layers
        .iter()
        .chain(std::iter::once(&manifest.config))
    {
        if hub.blob_exists(&dest, &descriptor.digest).await? {
            println!(
                "blob {}: already on the registry",
                short_digest(&descriptor.digest)
            );
        } else {
            println!("blob {}: uploading", short_digest(&descriptor.digest));
            hub.upload_blob(
                &dest,
                &descriptor.digest,
                &store.blob_path(&descriptor.digest),
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
    println!("pushed {destination}: {url}");
    Ok(())
}
