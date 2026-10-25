//! create: another machine from an image that is already local. No registry
//! involved, layers shared, and a writable layer, address and settings of its own.

use std::collections::BTreeMap;

use anyhow::{bail, Context as _, Result};
use oci_client::manifest::OciImageManifest;

use crate::api::{line, note, require_root, Context, Report};
use crate::backend::{Backend, BackendChoice};
use crate::bridge;
use crate::hub::short_digest;
use crate::install::{ensure_replaceable, install, remove_existing, Install};
use crate::oci::Mode;
use crate::reference::validate_machine_name;
use crate::store::{validate_digest, ImageRecord};
use crate::volume;

#[derive(Debug, Clone, PartialEq)]
pub struct CreateRequest {
    /// Local image to start from: its name, or the reference it was pulled from.
    pub source: String,
    /// Name of the new machine.
    pub name: String,
    /// Auto: like the source.
    pub backend: BackendChoice,
    /// bridge, veth, host, none or networks' names, the first one primary; empty: the
    /// source's kind.
    pub network: Vec<String>,
    /// NAME or NETWORK=NAME.
    pub aliases: Vec<String>,
    /// Fixed addresses: IP on the primary network or NETWORK=IP.
    pub addresses: Vec<String>,
    /// HOST:CONTAINER[/udp], like start -p.
    pub publish: Vec<String>,
    pub force: bool,
    /// Replaces the image's entrypoint; an empty string runs the arguments alone.
    pub entrypoint: Option<String>,
    /// VAR=value or VAR (copied from the environment).
    pub env: Vec<String>,
    /// Variables of the image's that the machine goes without, as the engine drops one a
    /// container's environment names without a value.
    pub unset_env: Vec<String>,
    /// SOURCE:TARGET[:ro].
    pub volume: Vec<String>,
    /// KEY=VALUE labels on top of the image's.
    pub label: Vec<String>,
    /// --restart; None is no.
    pub restart: Option<crate::policy::Restart>,
    /// Bytes; None or 0 for no limit.
    pub memory: Option<u64>,
    /// Memory and swap together in bytes, -1 for no bound on swap; None or 0 for as much
    /// swap again as memory.
    pub memory_swap: Option<i64>,
    /// CPUs (0.5); None or 0 for no limit.
    pub cpus: Option<f64>,
    /// Processes; None or 0 for no limit.
    pub pids_limit: Option<u64>,
    /// App images: replaces the image's cmd and follows its entrypoint.
    pub command: Vec<String>,
    /// The --health-* flags.
    pub health: crate::health::Overrides,
    /// hostname, user, capabilities and the rest.
    pub tuning: crate::tuning::Overrides,
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
    let network = if request.network.is_empty() {
        None
    } else {
        Some(crate::api::network::choices(&request.network)?)
    };
    let env = volume::parse_env(&request.env)?;
    let volumes = volume::parse_volumes(&request.volume)?;
    let labels = volume::parse_labels(&request.label)?;
    // Nothing of this comes from the source: a new machine never starts at boot or
    // inherits limits unless it is told so.
    let mut limits = crate::policy::Limits::default();
    if let Some(memory) = request.memory {
        limits.memory = memory;
    }
    if let Some(memory_swap) = request.memory_swap {
        limits.memory_swap = memory_swap;
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
    if backend == Backend::Mstack {
        note(report, crate::backend::MSTACK_EXPERIMENTAL);
    }
    // The rest of the flags, applied to a draft of the record before the old machine
    // goes: a flag refused after that would leave a machine with none of them. The
    // network kind is inherited (none included); ports are not (two machines cannot
    // publish the same one), nor a user-defined network or another machine's
    // namespace, which a machine joins only when told to.
    // A source found by its reference that ran as a machine gives the image, not the
    // network that machine was given.
    let by_reference = store.load_image(&request.source)?.is_none();
    let inherit_network = !by_reference || crate::api::disk::image_only(store, &source);
    let mut draft = ImageRecord {
        name: request.name.clone(),
        started: None,
        network_name: None,
        address: None,
        fixed_addresses: Default::default(),
        // A machine of a built image keeps saying so once the build's record is gone.
        built: source.built || source.origin == "build",
        extra_networks: Vec::new(),
        aliases: BTreeMap::new(),
        network_container: None,
        healthcheck: None,
        tuning: Default::default(),
        ..source.clone()
    };
    if !inherit_network {
        draft.network = crate::settings::Network::Bridge;
        draft.no_network = false;
    }
    match &network {
        Some(choice) => crate::api::network::apply(&mut draft, choice),
        None => draft.network_name = None,
    }
    // What its start would refuse at once is refused now: a network that does not exist,
    // another machine's namespace that is no nspawn machine with a network to share.
    if bridge::bridge_kind(&draft) {
        crate::api::network::nets_of(store, config, &draft)?;
    }
    if let Some(owner) = bridge::shares_network(&draft) {
        let owner_record = store.load_image(owner)?.with_context(|| {
            format!("--network container:{owner}: {owner} is not a machine of nspawn's")
        })?;
        if !bridge::bridge_kind(&owner_record) {
            bail!("--network container:{owner}: {owner} has no network of its own to share");
        }
    }
    if !request.aliases.is_empty() {
        if !bridge::bridge_kind(&draft) {
            bail!(
                "aliases are names on a bridge network; {} joins none",
                request.name
            );
        }
        draft.aliases =
            crate::api::network::parse_aliases(&request.aliases, &bridge::networks_of(&draft))?;
    }
    crate::api::machines::apply_addresses(store, config, &mut draft, &request.addresses)?;
    if let Some(entrypoint) = &request.entrypoint {
        volume::reject_argument_control_characters(entrypoint)?;
        draft.entrypoint = Some(if entrypoint.is_empty() {
            Vec::new()
        } else {
            vec![entrypoint.clone()]
        });
    } else {
        draft.entrypoint = None;
    }
    // An entrypoint given drops the image's command: only the arguments given follow it.
    draft.cmd = request.entrypoint.as_ref().map(|_| Vec::new());
    if !request.command.is_empty() {
        for arg in &request.command {
            volume::reject_argument_control_characters(arg)?;
        }
        draft.cmd = Some(request.command.clone());
    }
    draft.env = env;
    draft.volumes = volumes;
    draft.labels = labels;
    draft.restart = request.restart.unwrap_or_default();
    draft.limits = limits;
    draft.ports = ports;
    draft.remove_on_exit = false;
    if !request.health.is_empty() {
        let hc = request.health.apply(draft.effective_healthcheck())?;
        draft.healthcheck = Some(hc);
    }
    request.tuning.apply(&mut draft.tuning)?;
    // Refused when the machine is made, not at its first start: a machine that could
    // never start is not made.
    crate::api::machines::duplicate_mount_points(&draft)?;
    // Refused here as start refuses them: a booted machine that kept them would never
    // start.
    if source.mode == Mode::Boot
        && (draft.tuning.user.is_some()
            || draft.tuning.working_dir.is_some()
            || draft.tuning.init.is_some())
    {
        bail!(
            "{} boots an init system; --user, --workdir and --init only apply to the program of an app image",
            request.name
        );
    }
    if source.mode == Mode::App && draft.effective_command().is_empty() {
        bail!("{}", crate::api::machines::no_command(&request.name));
    }
    crate::api::dependencies::check(
        store,
        &request.name,
        &draft.tuning.depends_on,
        draft.network_container.as_deref(),
    )?;
    crate::install::stage_layers(store, backend, &manifest, Some(source.mode), &request.name)
        .await?;
    remove_existing(store, sd, &request.name, report).await?;
    // An update that was deciding about the machine replaced has nothing left to decide:
    // putting it back would take the new one away.
    store.forget_rollback(&request.name)?;
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
            index_digest: source.index_digest.as_deref(),
            origin: "create",
            mode: Some(source.mode),
            signed_by: source.signed_by.as_deref(),
            signed_at: source.signed_at,
        },
        report,
    )
    .await?;
    // What install wrote, with the draft's choices on top.
    let installed = store
        .load_image(&request.name)?
        .context("the record of the new machine is missing")?;
    let record = ImageRecord {
        name: installed.name,
        reference: installed.reference,
        manifest_digest: installed.manifest_digest,
        index_digest: installed.index_digest,
        layers: installed.layers,
        backend: installed.backend,
        created: installed.created,
        origin: installed.origin,
        mode: installed.mode,
        pid1: installed.pid1,
        run: {
            let mut run = installed.run;
            run.env.retain(|v| {
                let name = v.split_once('=').map_or(v.as_str(), |(k, _)| k);
                !request.unset_env.iter().any(|u| u == name)
            });
            run
        },
        signed_by: installed.signed_by,
        signed_at: installed.signed_at,
        ..draft
    };
    store.record_image(&record)?;
    // install wrote the drop-in of an image; the machine's has its own dependencies,
    // policy and limits.
    crate::api::machines::rewrite_hooks(ctx, sd, &record).await?;
    crate::api::events::emit_machine(
        store,
        "create",
        &request.name,
        &[
            ("from", &request.source),
            ("image", &record.reference),
            ("reference", &record.reference),
        ],
    );
    Ok(Created {
        name: request.name.clone(),
        mode,
    })
}
