//! docker create: another machine from an image that is already local. No registry
//! involved, layers shared, and a writable layer, address and settings of its own.

use anyhow::{bail, Context as _, Result};
use oci_client::manifest::OciImageManifest;

use crate::api::{line, require_root, Context, Report};
use crate::backend::{Backend, BackendChoice};
use crate::bridge;
use crate::hub::short_digest;
use crate::install::{ensure_replaceable, install, remove_existing, Install};
use crate::oci::Mode;
use crate::reference::validate_machine_name;
use crate::settings::Network;
use crate::store::validate_digest;
use crate::volume;

#[derive(Debug, Clone, PartialEq)]
pub struct CreateRequest {
    /// Local image to start from: its name, or the reference it was pulled from.
    pub source: String,
    /// Name of the new machine.
    pub name: String,
    /// Auto: like the source.
    pub backend: BackendChoice,
    /// None: like the source.
    pub network: Option<Network>,
    /// HOST:CONTAINER[/udp], like start -p.
    pub publish: Vec<String>,
    pub force: bool,
    /// Replaces the image's entrypoint; an empty string runs the arguments alone.
    pub entrypoint: Option<String>,
    /// VAR=value or VAR (copied from the environment), like docker -e.
    pub env: Vec<String>,
    /// SOURCE:TARGET[:ro], like docker -v.
    pub volume: Vec<String>,
    /// KEY=VALUE labels on top of the image's, like docker --label.
    pub label: Vec<String>,
    /// docker's --restart; None is no.
    pub restart: Option<crate::policy::Restart>,
    /// Bytes; None or 0 for no limit.
    pub memory: Option<u64>,
    /// CPUs (0.5); None or 0 for no limit.
    pub cpus: Option<f64>,
    /// Processes; None or 0 for no limit.
    pub pids_limit: Option<u64>,
    /// App images: replaces the image's cmd and follows its entrypoint.
    pub command: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Created {
    pub name: String,
    pub mode: Mode,
}

pub async fn create(ctx: &Context, request: &CreateRequest, report: Report<'_>) -> Result<Created> {
    require_root("create")?;
    validate_machine_name(&request.name)?;
    let config = &ctx.config;
    let sd = ctx.sd().await?;
    let store = &ctx.store;
    store.init()?;
    let _lock = store.lock().await?;
    ensure_replaceable(store, sd, &request.name, request.force).await?;
    let source = store
        .find_image(&request.source, &config.registry)?
        .with_context(|| {
            format!(
                "{}: no such local image; pull or build it first",
                request.source
            )
        })?;
    if source.name == request.name {
        bail!(
            "{} is the source image itself; pick another name",
            request.name
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
    // Everything given is checked before the old machine goes or the new one is
    // assembled.
    let ports = bridge::parse_publish(&request.publish)?;
    let env = volume::parse_env(&request.env)?;
    let volumes = volume::parse_volumes(&request.volume)?;
    let labels = volume::parse_labels(&request.label)?;
    // Nothing of this comes from the source: a new machine never starts at boot or
    // inherits limits unless it is told so.
    let mut limits = crate::policy::Limits::default();
    if let Some(memory) = request.memory {
        limits.memory = memory;
    }
    if let Some(cpus) = request.cpus {
        limits.milli_cpus = crate::policy::milli_cpus_from(cpus)?;
    }
    if let Some(pids) = request.pids_limit {
        limits.pids = pids;
    }
    limits.check(source.mode)?;
    if source.mode == Mode::Boot
        && (!request.command.is_empty() || request.entrypoint.is_some() || !request.env.is_empty())
    {
        bail!(
            "{} boots an init system; a command, an entrypoint and variables only apply to the program of an app image",
            request.name
        );
    }
    for descriptor in manifest
        .layers
        .iter()
        .chain(std::iter::once(&manifest.config))
    {
        validate_digest(&descriptor.digest)?;
    }
    let choice = if request.backend == BackendChoice::Auto {
        source.backend
    } else {
        request.backend
    };
    let backend = Backend::choose(choice, sd).await?;
    remove_existing(store, sd, &request.name).await?;
    line(
        report,
        format!(
            "{}: {} layer(s) shared with {}, assembling as {}",
            request.name,
            manifest.layers.len(),
            source.name,
            backend.name()
        ),
    );
    let mode = install(
        store,
        sd,
        config,
        backend,
        Install {
            name: &request.name,
            reference: &source.reference,
            manifest_bytes: &manifest_bytes,
            manifest: &manifest,
            manifest_digest: &source.manifest_digest,
            origin: "create",
            mode: Some(source.mode),
        },
        report,
    )
    .await?;
    // The network kind is inherited; ports are not, two machines cannot publish the same.
    let mut record = store
        .load_image(&request.name)?
        .context("the record of the new machine is missing")?;
    record.network = request.network.unwrap_or(source.network);
    record.ports = ports;
    if let Some(entrypoint) = &request.entrypoint {
        record.entrypoint = Some(if entrypoint.is_empty() {
            Vec::new()
        } else {
            vec![entrypoint.clone()]
        });
    }
    if !request.command.is_empty() {
        record.cmd = Some(request.command.clone());
    }
    record.env = env;
    record.volumes = volumes;
    record.labels = labels;
    record.restart = request.restart.unwrap_or_default();
    record.limits = limits;
    store.record_image(&record)?;
    Ok(Created {
        name: request.name.clone(),
        mode,
    })
}
