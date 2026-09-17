use anyhow::{bail, Context, Result};
use oci_client::manifest::OciImageManifest;

use crate::backend::Backend;
use crate::bridge;
use crate::cli::{BackendChoice, CreateArgs};
use crate::commands::require_root;
use crate::config::Config;
use crate::hub::short_digest;
use crate::install::{install, replace_existing, Install};
use crate::reference::validate_machine_name;
use crate::store::Store;
use crate::systemd::Systemd;

/// Makes another machine from an image that is already local, like docker create: no
/// registry involved, layers shared, and a writable layer, address and settings of its own.
pub async fn run(args: CreateArgs, config: &Config) -> Result<()> {
    require_root("create")?;
    validate_machine_name(&args.name)?;
    let sd = Systemd::connect().await?;
    let store = Store::new(&config.machines_dir, &config.state_dir);
    store.init()?;
    let source = store
        .find_image(&args.source, &config.registry)?
        .with_context(|| {
            format!(
                "{}: no such local image; pull or build it first",
                args.source
            )
        })?;
    if source.name == args.name {
        bail!(
            "{} is the source image itself; pick another name",
            args.name
        );
    }
    let manifest_bytes = store.load_manifest(&source.name)?;
    let manifest: OciImageManifest = serde_json::from_slice(&manifest_bytes)
        .with_context(|| format!("parsing the manifest of {}", source.name))?;
    for descriptor in manifest
        .layers
        .iter()
        .chain(std::iter::once(&manifest.config))
    {
        if !store.has_blob(&descriptor.digest) {
            bail!(
                "blob {} of {} is missing from the store; pull the image again",
                short_digest(&descriptor.digest),
                source.name
            );
        }
    }
    replace_existing(&store, &sd, &args.name, args.force).await?;
    let choice = if args.backend == BackendChoice::Auto {
        source.backend
    } else {
        args.backend
    };
    let backend = Backend::choose(choice, &sd).await?;
    println!(
        "{}: {} layer(s) shared with {}, assembling as {}",
        args.name,
        manifest.layers.len(),
        source.name,
        backend.name()
    );
    let mode = install(
        &store,
        &sd,
        backend,
        Install {
            name: &args.name,
            reference: &source.reference,
            manifest_bytes: &manifest_bytes,
            manifest: &manifest,
            manifest_digest: &source.manifest_digest,
            origin: "create",
            mode: Some(source.mode),
        },
    )
    .await?;
    // The network kind is inherited; ports are not, two machines cannot publish the same.
    let mut record = store
        .load_image(&args.name)?
        .context("the record of the new machine is missing")?;
    record.network = args.network.unwrap_or(source.network);
    record.ports = bridge::parse_publish(&args.publish)?;
    store.record_image(&record)?;
    println!(
        "machine {} ({} image) is ready: nspawn start {}",
        args.name,
        mode.name(),
        args.name
    );
    Ok(())
}
