use anyhow::Result;

use crate::backend::Backend;
use crate::cli::BackendChoice;
use crate::cli::PullArgs;
use crate::commands::require_root;
use crate::config::Config;
use crate::hub::{short_digest, Hub};
use crate::install::{ensure_replaceable, install, remove_existing, Install};
use crate::reference::{validate_machine_name, ImageRef};
use crate::store::{validate_digest, Store};
use crate::systemd::Systemd;

pub async fn run(args: PullArgs, config: &Config) -> Result<()> {
    require_root("pull")?;
    let image = ImageRef::parse(&args.reference, &config.registry)?;
    let oci = image.to_oci()?;
    let name = args.name.clone().unwrap_or_else(|| image.local_name());
    validate_machine_name(&name)?;

    let sd = Systemd::connect().await?;
    let store = Store::new(&config.machines_dir, &config.state_dir);
    store.init()?;
    ensure_replaceable(&store, &sd, &name, args.force).await?;

    let choice = if args.backend == BackendChoice::Auto {
        config.backend
    } else {
        args.backend
    };
    let backend = Backend::choose(choice, &sd).await?;
    let hub = Hub::new(config)?;
    let (manifest, manifest_digest) = hub.resolve(&oci).await?;
    // Digests become path components in the store; the registry does not get to choose them.
    validate_digest(&manifest_digest)?;
    for descriptor in manifest
        .layers
        .iter()
        .chain(std::iter::once(&manifest.config))
    {
        validate_digest(&descriptor.digest)?;
    }
    let manifest_bytes = hub.manifest_bytes(&oci, &manifest_digest).await?;
    println!(
        "{image}: manifest {} with {} layer(s), assembling as {}",
        short_digest(&manifest_digest),
        manifest.layers.len(),
        backend.name()
    );

    for descriptor in manifest
        .layers
        .iter()
        .chain(std::iter::once(&manifest.config))
    {
        if store.has_blob(&descriptor.digest) {
            println!("blob {}: already present", short_digest(&descriptor.digest));
        } else {
            println!("blob {}: downloading", short_digest(&descriptor.digest));
            hub.download_blob(&oci, descriptor, &store.blob_path(&descriptor.digest))
                .await?;
        }
    }

    // The store is locked only now: a long download must not hold up other commands or
    // the unit hooks. The name is checked again, the old image goes only at this point.
    let _lock = store.lock()?;
    ensure_replaceable(&store, &sd, &name, args.force).await?;
    remove_existing(&store, &sd, &name).await?;
    let mode = install(
        &store,
        &sd,
        config,
        backend,
        Install {
            name: &name,
            reference: &image.to_string(),
            manifest_bytes: &manifest_bytes,
            manifest: &manifest,
            manifest_digest: &manifest_digest,
            origin: "pull",
            mode: args.mode.to_mode(),
        },
    )
    .await?;
    println!(
        "image {name} ({} image) is ready: nspawn start {name}",
        mode.name()
    );
    Ok(())
}
