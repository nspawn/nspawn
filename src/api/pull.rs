//! docker pull: resolve the reference on the registry, fetch the blobs the store lacks,
//! assemble the image with a backend and record it.

use anyhow::Result;

use crate::api::{line, note, require_root, Context, Report};
use crate::backend::{Backend, BackendChoice};
use crate::hub::{short_digest, Hub};
use crate::install::{ensure_replaceable, install, remove_existing, Install};
use crate::oci::Mode;
use crate::reference::{validate_machine_name, ImageRef};
use crate::store::validate_digest;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequest {
    /// [registry/]repository[:tag|@digest], for example fedora:44.
    pub reference: String,
    /// Local name; derived from the reference when missing (fedora-44).
    pub name: Option<String>,
    pub backend: BackendChoice,
    /// None: decided from the image's contents.
    pub mode: Option<Mode>,
    /// Replace an existing image of that name.
    pub force: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pulled {
    pub name: String,
    pub reference: String,
    pub mode: Mode,
}

pub async fn pull(ctx: &Context, request: &PullRequest, report: Report<'_>) -> Result<Pulled> {
    require_root("pull")?;
    let config = &ctx.config;
    let image = ImageRef::parse(&request.reference, &config.registry)?;
    let oci = image.to_oci()?;
    let name = request.name.clone().unwrap_or_else(|| image.local_name());
    validate_machine_name(&name)?;

    let sd = ctx.sd().await?;
    let store = &ctx.store;
    store.init()?;
    ensure_replaceable(store, sd, &name, request.force).await?;

    let choice = if request.backend == BackendChoice::Auto {
        config.backend
    } else {
        request.backend
    };
    let backend = Backend::choose(choice, sd).await?;
    if backend == Backend::Mstack {
        note(report, crate::backend::MSTACK_EXPERIMENTAL);
    }
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
    // Held from here until the record refers to them: an images rm meanwhile must not
    // collect what this pull is bringing in.
    let digests: Vec<String> = manifest
        .layers
        .iter()
        .chain(std::iter::once(&manifest.config))
        .map(|d| d.digest.clone())
        .collect();
    let _hold = store.hold_blobs(&digests)?;
    line(
        report,
        format!(
            "{image}: manifest {} with {} layer(s), assembling as {}",
            short_digest(&manifest_digest),
            manifest.layers.len(),
            backend.name()
        ),
    );

    for descriptor in manifest
        .layers
        .iter()
        .chain(std::iter::once(&manifest.config))
    {
        if store.has_blob(&descriptor.digest) {
            line(
                report,
                format!("blob {}: already present", short_digest(&descriptor.digest)),
            );
        } else {
            line(
                report,
                format!("blob {}: downloading", short_digest(&descriptor.digest)),
            );
            hub.download_blob(
                &oci,
                descriptor,
                &store.blob_path(&descriptor.digest),
                report,
            )
            .await?;
        }
    }

    // The store is locked only now: a long download must not hold up other commands or
    // the unit hooks. The name is checked again, the old image goes only at this point.
    let _lock = store.lock().await?;
    ensure_replaceable(store, sd, &name, request.force).await?;
    remove_existing(store, sd, &name, report).await?;
    let mode = install(
        store,
        sd,
        config,
        backend,
        Install {
            name: &name,
            reference: &image.to_string(),
            manifest_bytes: &manifest_bytes,
            manifest: &manifest,
            manifest_digest: &manifest_digest,
            origin: "pull",
            mode: request.mode,
        },
        report,
    )
    .await?;
    crate::api::events::emit(
        "machine",
        "pull",
        &name,
        &[("reference", &image.to_string())],
    );
    Ok(Pulled {
        name,
        reference: image.to_string(),
        mode,
    })
}
