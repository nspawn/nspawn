//! docker pull: resolve the reference on the registry, fetch the blobs the store lacks,
//! assemble the image with a backend and record it.

use anyhow::Result;
use futures_util::{StreamExt, TryStreamExt};
use oci_client::manifest::OciDescriptor;

use crate::api::{line, note, require_root, Context, Report};
use crate::backend::{Backend, BackendChoice};
use crate::hub::{short_digest, Hub, Resolved};
use crate::install::{ensure_replaceable, install, remove_existing, Install};
use crate::oci::Mode;
use crate::reference::{validate_machine_name, ImageRef};
use crate::store::validate_digest;

/// Blobs fetched at once, docker's default for a pull.
const CONCURRENT_DOWNLOADS: usize = 3;

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
    /// Check the image's signature as the registry's policy says; off with --no-verify.
    pub verify: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pulled {
    pub name: String,
    pub reference: String,
    pub mode: Mode,
    /// Who signed it, when the pull verified a signature.
    pub signed_by: Option<String>,
}

/// The blobs to fetch, each once (a manifest may name a layer twice), and the digests
/// of those already there, in the manifest's order.
fn missing_once<'a>(
    descriptors: impl Iterator<Item = &'a OciDescriptor>,
    present: impl Fn(&str) -> bool,
) -> (Vec<&'a OciDescriptor>, Vec<&'a str>) {
    let mut wanted: Vec<&OciDescriptor> = Vec::new();
    let mut there = Vec::new();
    for descriptor in descriptors {
        if present(&descriptor.digest) {
            there.push(descriptor.digest.as_str());
        } else if !wanted.iter().any(|d| d.digest == descriptor.digest) {
            wanted.push(descriptor);
        }
    }
    (wanted, there)
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
    let resolved = hub.resolve(&oci).await?;
    // Digests become path components in the store; the registry does not get to choose them.
    validate_digest(&resolved.digest)?;
    if let Some(index) = &resolved.index_digest {
        validate_digest(index)?;
    }
    for descriptor in resolved
        .manifest
        .layers
        .iter()
        .chain(std::iter::once(&resolved.manifest.config))
    {
        validate_digest(&descriptor.digest)?;
    }
    // Before a single layer comes down: an image that is not what it claims is not
    // worth the download.
    let signed = if !request.verify {
        note(
            report,
            format!("note: signature verification of {image} skipped (--no-verify)"),
        );
        None
    } else {
        match crate::verify::Policy::for_registry(config, &image.registry)? {
            Some(policy) => {
                crate::verify::check(&hub, &oci, &image.to_string(), &resolved, &policy, report)
                    .await?
            }
            None => None,
        }
    };
    let Resolved {
        manifest,
        digest: manifest_digest,
        ..
    } = resolved;
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

    let (wanted, present) = missing_once(
        manifest
            .layers
            .iter()
            .chain(std::iter::once(&manifest.config)),
        |digest| store.has_blob(digest),
    );
    for digest in present {
        line(
            report,
            format!("blob {}: already present", short_digest(digest)),
        );
    }
    // Several at a time, as docker fetches layers; each transfer reports under its own
    // digest, so their bars keep apart. The first failure ends the others, whose part
    // files go with them.
    let (hub, oci) = (&hub, &oci);
    // Owned descriptors: a future that borrowed its argument could not be handed to
    // the job's task.
    let wanted: Vec<OciDescriptor> = wanted.into_iter().cloned().collect();
    let downloads = wanted.into_iter().map(|descriptor| async move {
        line(
            report,
            format!("blob {}: downloading", short_digest(&descriptor.digest)),
        );
        hub.download_blob(
            oci,
            &descriptor,
            &store.blob_path(&descriptor.digest),
            report,
        )
        .await?;
        line(
            report,
            format!("blob {}: downloaded", short_digest(&descriptor.digest)),
        );
        Ok::<(), anyhow::Error>(())
    });
    futures_util::stream::iter(downloads)
        .buffer_unordered(CONCURRENT_DOWNLOADS)
        .try_collect::<Vec<()>>()
        .await?;

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
            signed_by: signed.as_ref().map(|s| s.signed_by.as_str()),
            signed_at: signed.as_ref().and_then(|s| s.signed_at),
        },
        report,
    )
    .await?;
    let reference = image.to_string();
    let mut attributes = vec![
        ("image", reference.as_str()),
        ("reference", reference.as_str()),
    ];
    if let Some(signed) = &signed {
        attributes.push(("signed_by", signed.signed_by.as_str()));
    }
    crate::api::events::emit("machine", "pull", &name, &attributes);
    Ok(Pulled {
        name,
        reference,
        mode,
        signed_by: signed.map(|s| s.signed_by),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(digest: &str) -> OciDescriptor {
        OciDescriptor {
            digest: digest.to_string(),
            ..OciDescriptor::default()
        }
    }

    #[test]
    fn each_missing_blob_is_fetched_once_and_the_rest_reported() {
        let descriptors =
            ["sha256:a", "sha256:b", "sha256:a", "sha256:c", "sha256:b"].map(descriptor);
        let (wanted, present) = missing_once(descriptors.iter(), |d| d == "sha256:b");
        let wanted: Vec<&str> = wanted.iter().map(|d| d.digest.as_str()).collect();
        assert_eq!(wanted, ["sha256:a", "sha256:c"]);
        assert_eq!(present, ["sha256:b", "sha256:b"]);
    }
}
