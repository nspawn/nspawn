use anyhow::Result;

use crate::backend::Backend;
use crate::cli::BackendChoice;
use crate::cli::PullArgs;
use crate::commands::require_root;
use crate::config::Config;
use crate::hub::{short_digest, Hub};
use crate::install::{install, replace_existing, Install};
use crate::reference::{validate_machine_name, ImageRef};
use crate::store::Store;
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
    replace_existing(&store, &sd, &name, args.force).await?;

    let choice = if args.backend == BackendChoice::Auto {
        config.backend
    } else {
        args.backend
    };
    let backend = Backend::choose(choice, &sd).await?;
    let hub = Hub::new(config)?;
    let (manifest, manifest_digest) = hub.resolve(&oci).await?;
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

    install(
        &store,
        &sd,
        backend,
        Install {
            name: &name,
            reference: &image.to_string(),
            manifest_bytes: &manifest_bytes,
            manifest: &manifest,
            manifest_digest: &manifest_digest,
            origin: "pull",
        },
    )
    .await?;
    println!("image {name} is ready: nspawn start {name}");
    Ok(())
}
