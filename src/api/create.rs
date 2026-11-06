//! create: another machine from an image that is already local. No registry
//! involved, layers shared, and a writable layer, address and settings of its own.

use std::collections::BTreeMap;

use anyhow::{bail, Context as _, Result};
use oci_client::manifest::OciImageManifest;

use crate::api::failure::invalid;
use crate::api::{line, note, require_root, Context, FailureKind, Report};
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
    /// Name of the new machine; empty for one made of `api_name`.
    pub name: String,
    /// The name the Docker API knows the machine by, kept when it is not the machine's.
    pub api_name: Option<String>,
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
    /// Publish each exposed port of the image to a free host port (-P).
    pub publish_all: bool,
    pub force: bool,
    /// Replaces the image's entrypoint, program and arguments; an empty one runs the
    /// command alone.
    pub entrypoint: Option<Vec<String>>,
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
    /// The program wants a terminal, its input stays open, and closes once the client
    /// attached to it leaves: kept for whoever attaches.
    pub tty: bool,
    pub interactive: bool,
    pub stdin_once: bool,
    /// Removed once its run ends.
    pub remove: bool,
}

/// What a machine made with `remove` and a restart policy is refused with.
pub const REMOVE_AND_RESTART: &str = "--rm and a restart policy exclude each other: a machine removed when it ends cannot be restarted (--restart no)";

/// The longest name the Docker API takes for a machine.
const API_NAME_MAX: usize = 1024;
/// How much of an API name a machine name made of it keeps, before its hash.
const DERIVED_BASE_MAX: usize = 55;

/// A name the Docker API takes: [a-zA-Z0-9][a-zA-Z0-9_.-]*, 1024 bytes at most.
pub fn validate_api_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    let valid = name.len() <= API_NAME_MAX
        && chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
    if !valid {
        return Err(FailureKind::Invalid.error(format!(
            "invalid API name {name:?}: [a-zA-Z0-9][a-zA-Z0-9_.-]*, {API_NAME_MAX} bytes at most"
        )));
    }
    Ok(())
}

/// The machine name made of an API name that is none itself, or is taken: each run of
/// other characters than letters and digits one '-', cut short, and a hash of the whole
/// name, so that two names that read alike get machines of their own.
pub fn derived_machine_name(api_name: &str) -> String {
    use sha2::Digest as _;
    let mut base = String::new();
    for c in api_name.chars() {
        if c.is_ascii_alphanumeric() {
            base.push(c);
        } else if !base.ends_with('-') {
            base.push('-');
        }
    }
    let base = base.trim_matches('-');
    // Only ASCII is left: a byte count cuts no character.
    let base = base[..base.len().min(DERIVED_BASE_MAX)].trim_matches('-');
    let hash = hex::encode(sha2::Sha256::digest(api_name.as_bytes()));
    if base.is_empty() {
        return hash[..8].to_string();
    }
    format!("{base}-{}", &hash[..8])
}

/// The machine's name: the one asked for, or without one the API name itself when
/// systemd takes it and neither a record nor machined (`foreign`: images and machines
/// that are not nspawn's) holds it, else one made of it.
fn machine_name(
    request: &CreateRequest,
    records: &[ImageRecord],
    foreign: &[String],
) -> Result<String> {
    let Some(api_name) = request
        .api_name
        .as_deref()
        .filter(|_| request.name.is_empty())
    else {
        return Ok(request.name.clone());
    };
    let held =
        |name: &str| records.iter().any(|r| r.name == name) || foreign.iter().any(|f| f == name);
    if validate_machine_name(api_name).is_ok() && !held(api_name) {
        return Ok(api_name.to_string());
    }
    let derived = derived_machine_name(api_name);
    if held(&derived) {
        return Err(FailureKind::Exists.error(format!(
            "the name {api_name} would be machine {derived}, which exists already"
        )));
    }
    Ok(derived)
}

/// Refuses a name the Docker API knows another machine by: the API name given, or the
/// machine's own without one. Images that never ran are no machines there; the record
/// of the name replaced is going.
fn check_api_name(
    store: &crate::store::Store,
    records: &[ImageRecord],
    name: &str,
    api_name: Option<&str>,
) -> Result<()> {
    let wanted = api_name.unwrap_or(name);
    if let Some(other) = records.iter().find(|r| {
        r.name != name
            && r.effective_api_name() == wanted
            && !crate::api::disk::image_only(store, r)
    }) {
        return Err(FailureKind::Exists.error(format!(
            "the name {wanted} is in use by machine {}",
            other.name
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Created {
    pub name: String,
    pub mode: Mode,
}

/// What a create is refused for before anything is looked at: a name that is none, and
/// a removal at the end beside a restart policy.
pub fn check_request(request: &CreateRequest) -> Result<()> {
    if let Some(api_name) = &request.api_name {
        validate_api_name(api_name)?;
    }
    if !request.name.is_empty() || request.api_name.is_none() {
        validate_machine_name(&request.name)?;
    }
    if request.remove
        && request
            .restart
            .is_some_and(|r| r != crate::policy::Restart::No)
    {
        return Err(FailureKind::Invalid.error(REMOVE_AND_RESTART));
    }
    Ok(())
}

pub async fn create(ctx: &Context, request: &CreateRequest, report: Report<'_>) -> Result<Created> {
    require_root("create")?;
    check_request(request)?;
    let config = &ctx.config;
    let sd = ctx.sd().await?;
    let store = &ctx.store;
    store.init()?;
    let _lock = store.lock().await?;
    // Names are chosen and checked under the lock: two creates of one API name at once
    // make one machine.
    let records = store.list_images()?;
    let foreign = if request.name.is_empty() && request.api_name.is_some() {
        let mut names: Vec<String> = sd
            .list_images()
            .await?
            .into_iter()
            .map(|i| i.name)
            .collect();
        names.extend(sd.list_machines().await?.into_iter().map(|m| m.name));
        names
    } else {
        Vec::new()
    };
    let name = machine_name(request, &records, &foreign)?;
    let name = name.as_str();
    ensure_replaceable(store, sd, name, request.force).await?;
    let api_name = request.api_name.as_deref().filter(|a| *a != name);
    check_api_name(store, &records, name, request.api_name.as_deref())?;
    let source = store
        .find_image(&request.source, &config.registry)?
        .ok_or_else(|| {
            FailureKind::NoSuchImage.error(format!(
                "{}: no such local image; pull or build it first",
                request.source
            ))
        })?;
    if source.name == name {
        return Err(FailureKind::Invalid.error(format!(
            "{name} is the source image itself; pick another name"
        )));
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
    let mut ports = bridge::parse_publish(&request.publish).map_err(invalid)?;
    if request.publish_all {
        bridge::publish_exposed(&mut ports, &source.exposed_ports(store));
    }
    bridge::assign_host_ports(store, sd, name, &mut ports).await?;
    let network = if request.network.is_empty() {
        None
    } else {
        Some(crate::api::network::choices(&request.network).map_err(invalid)?)
    };
    let env = volume::parse_env(&request.env).map_err(invalid)?;
    let volumes = volume::parse_volumes(&request.volume).map_err(invalid)?;
    let labels = volume::parse_labels(&request.label).map_err(invalid)?;
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
        limits.milli_cpus = crate::policy::milli_cpus_from(cpus).map_err(invalid)?;
    }
    if let Some(pids) = request.pids_limit {
        limits.pids = pids;
    }
    limits.check(source.mode).map_err(invalid)?;
    if source.mode == Mode::Boot
        && (!request.command.is_empty() || request.entrypoint.is_some() || !request.env.is_empty())
    {
        return Err(FailureKind::Invalid.error(format!(
            "{name} boots an init system; a command, an entrypoint and variables only apply to the program of an app image"
        )));
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
        name: name.to_string(),
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
        let owner_record = store.load_image(owner)?.ok_or_else(|| {
            FailureKind::Invalid.error(format!(
                "--network container:{owner}: {owner} is not a machine of nspawn's"
            ))
        })?;
        if !bridge::bridge_kind(&owner_record) {
            return Err(FailureKind::Invalid.error(format!(
                "--network container:{owner}: {owner} has no network of its own to share"
            )));
        }
    }
    if !request.aliases.is_empty() {
        if !bridge::bridge_kind(&draft) {
            return Err(FailureKind::Invalid.error(format!(
                "aliases are names on a bridge network; {name} joins none"
            )));
        }
        draft.aliases =
            crate::api::network::parse_aliases(&request.aliases, &bridge::networks_of(&draft))
                .map_err(invalid)?;
    }
    crate::api::machines::apply_addresses(store, config, &mut draft, &request.addresses)
        .map_err(invalid)?;
    if let Some(entrypoint) = &request.entrypoint {
        for arg in entrypoint {
            volume::reject_argument_control_characters(arg).map_err(invalid)?;
        }
    }
    draft.entrypoint = request.entrypoint.clone();
    // An entrypoint given drops the image's command: only the arguments given follow it.
    draft.cmd = request.entrypoint.as_ref().map(|_| Vec::new());
    if !request.command.is_empty() {
        for arg in &request.command {
            volume::reject_argument_control_characters(arg).map_err(invalid)?;
        }
        draft.cmd = Some(request.command.clone());
    }
    draft.env = env;
    draft.volumes = volumes;
    draft.labels = labels;
    draft.restart = request.restart.unwrap_or_default();
    draft.limits = limits;
    draft.ports = ports;
    draft.remove_on_exit = request.remove;
    draft.api_name = api_name.map(str::to_string);
    draft.tty = request.tty;
    draft.interactive = request.interactive;
    draft.stdin_once = request.stdin_once;
    if !request.health.is_empty() {
        let hc = request
            .health
            .apply(draft.effective_healthcheck())
            .map_err(invalid)?;
        draft.healthcheck = Some(hc);
    }
    request.tuning.apply(&mut draft.tuning).map_err(invalid)?;
    // Refused when the machine is made, not at its first start: a machine that could
    // never start is not made.
    crate::api::machines::duplicate_mount_points(&draft).map_err(invalid)?;
    // Refused here as start refuses them: a booted machine that kept them would never
    // start.
    if source.mode == Mode::Boot
        && (draft.tuning.user.is_some()
            || draft.tuning.working_dir.is_some()
            || draft.tuning.init.is_some())
    {
        return Err(FailureKind::Invalid.error(format!(
            "{name} boots an init system; --user, --workdir and --init only apply to the program of an app image"
        )));
    }
    if source.mode == Mode::App && draft.effective_command().is_empty() {
        return Err(FailureKind::Invalid.error(crate::api::machines::no_command(name)));
    }
    crate::api::dependencies::check(
        store,
        name,
        &draft.tuning.depends_on,
        draft.network_container.as_deref(),
    )
    .map_err(invalid)?;
    crate::install::stage_layers(store, backend, &manifest, Some(source.mode), name).await?;
    remove_existing(store, sd, name, report).await?;
    // An update that was deciding about the machine replaced has nothing left to decide:
    // putting it back would take the new one away.
    store.forget_rollback(name)?;
    line(
        report,
        format!(
            "{}: {} layer(s) shared with {}, assembling as {}",
            name,
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
            name,
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
        .load_image(name)?
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
        name,
        &[
            ("from", &request.source),
            ("image", &record.reference),
            ("reference", &record.reference),
        ],
    );
    Ok(Created {
        name: name.to_string(),
        mode,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(name: &str, api_name: Option<&str>) -> CreateRequest {
        CreateRequest {
            source: "busybox".to_string(),
            name: name.to_string(),
            api_name: api_name.map(str::to_string),
            backend: BackendChoice::Auto,
            network: Vec::new(),
            aliases: Vec::new(),
            addresses: Vec::new(),
            publish: Vec::new(),
            publish_all: false,
            force: false,
            entrypoint: None,
            env: Vec::new(),
            unset_env: Vec::new(),
            volume: Vec::new(),
            label: Vec::new(),
            restart: None,
            memory: None,
            memory_swap: None,
            cpus: None,
            pids_limit: None,
            command: Vec::new(),
            health: Default::default(),
            tuning: Default::default(),
            tty: false,
            interactive: false,
            stdin_once: false,
            remove: false,
        }
    }

    fn record(name: &str, api_name: Option<&str>, origin: &str) -> ImageRecord {
        let mut r: ImageRecord = serde_json::from_value(serde_json::json!({
            "name": name, "reference": "r", "manifest_digest": "d", "layers": [],
            "backend": "overlay", "created": 1, "origin": origin,
        }))
        .unwrap();
        r.api_name = api_name.map(str::to_string);
        r
    }

    #[test]
    fn api_names_follow_the_docker_engine_api() {
        for good in ["a", "e2e_dk_b", "web.1-x", "A9", &"x".repeat(1024)] {
            assert!(validate_api_name(good).is_ok(), "{good}");
        }
        for bad in ["", "_a", ".a", "-a", "a b", "a/b", "ä", &"x".repeat(1025)] {
            let e = validate_api_name(bad).unwrap_err();
            assert_eq!(crate::api::kind_of(&e), Some(FailureKind::Invalid), "{bad}");
        }
    }

    #[test]
    fn a_machine_name_is_made_of_an_api_name_systemd_refuses() {
        let hash = |name: &str| {
            use sha2::Digest as _;
            hex::encode(sha2::Sha256::digest(name.as_bytes()))[..8].to_string()
        };
        assert_eq!(
            derived_machine_name("e2e_dk_b"),
            format!("e2e-dk-b-{}", hash("e2e_dk_b"))
        );
        // Runs of other characters are one '-', none at the edges.
        assert_eq!(
            derived_machine_name("a__.-b_"),
            format!("a-b-{}", hash("a__.-b_"))
        );
        // Cut to 55 before the hash, and never ending in '-' there.
        let long = format!("{}_{}", "x".repeat(54), "y".repeat(30));
        let made = derived_machine_name(&long);
        assert_eq!(made, format!("{}-{}", "x".repeat(54), hash(&long)));
        assert!(made.len() <= 64);
        let long = "z".repeat(1024);
        let made = derived_machine_name(&long);
        assert_eq!(made.len(), 64);
        validate_machine_name(&made).unwrap();
        // Two names that read alike get machines of their own.
        assert_ne!(derived_machine_name("a_b"), derived_machine_name("a.b"));
    }

    #[test]
    fn the_machine_name_is_the_one_asked_or_made_under_the_lock() {
        let records = [
            record("web", None, "create"),
            record("nginx", None, "pull"),
            record(&derived_machine_name("taken_name"), None, "create"),
        ];
        // A name given is the machine's, the API name kept beside it.
        assert_eq!(
            machine_name(&request("site", Some("my_site")), &records, &[]).unwrap(),
            "site"
        );
        // None given: the API name when systemd takes it and no record holds it.
        assert_eq!(
            machine_name(&request("", Some("blog")), &records, &[]).unwrap(),
            "blog"
        );
        assert_eq!(
            machine_name(&request("", Some("nginx")), &records, &[]).unwrap(),
            derived_machine_name("nginx"),
            "an image holds the name"
        );
        assert_eq!(
            machine_name(&request("", Some("my_blog")), &records, &[]).unwrap(),
            derived_machine_name("my_blog")
        );
        let held = machine_name(&request("", Some("taken_name")), &records, &[]).unwrap_err();
        assert_eq!(crate::api::kind_of(&held), Some(FailureKind::Exists));
        // An image or a machine of machined's that is not nspawn's holds its name too.
        assert_eq!(
            machine_name(&request("", Some("blog")), &records, &["blog".to_string()]).unwrap(),
            derived_machine_name("blog")
        );
    }

    #[test]
    fn an_api_name_is_one_machine_s() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::new(&dir.path().join("m"), &dir.path().join("s"));
        let records = [
            record("web", None, "create"),
            record("site-1a2b3c4d", Some("site"), "create"),
            record("nginx", None, "pull"),
        ];
        let exists = |r: Result<()>| crate::api::kind_of(&r.unwrap_err());
        // The API name given, held by another machine's name or API name.
        assert_eq!(
            exists(check_api_name(&store, &records, "web-2", Some("web"))),
            Some(FailureKind::Exists)
        );
        let e = check_api_name(&store, &records, "x", Some("site")).unwrap_err();
        assert_eq!(
            e.to_string(),
            "the name site is in use by machine site-1a2b3c4d"
        );
        // A machine named as another's API name.
        assert_eq!(
            exists(check_api_name(&store, &records, "site", None)),
            Some(FailureKind::Exists)
        );
        // An image only is no machine there; the record replaced is going.
        check_api_name(&store, &records, "x", Some("nginx")).unwrap();
        check_api_name(&store, &records, "web", None).unwrap();
        check_api_name(&store, &records, "site-1a2b3c4d", Some("site")).unwrap();
        check_api_name(&store, &records, "blog", Some("blog_1")).unwrap();
    }

    #[test]
    fn a_request_is_refused_before_anything_is_looked_at() {
        let invalid = Some(FailureKind::Invalid);
        let kind = |r: &CreateRequest| crate::api::kind_of(&check_request(r).unwrap_err());
        assert_eq!(kind(&request("", None)), invalid, "no name at all");
        assert_eq!(kind(&request("a_b", None)), invalid);
        assert_eq!(kind(&request("", Some("_x"))), invalid);
        check_request(&request("", Some("my_blog"))).unwrap();
        check_request(&request("blog", Some("my_blog"))).unwrap();
        let mut removed = request("web", None);
        removed.remove = true;
        check_request(&removed).unwrap();
        removed.restart = Some(crate::policy::Restart::No);
        check_request(&removed).unwrap();
        removed.restart = Some(crate::policy::Restart::Always);
        let e = check_request(&removed).unwrap_err();
        assert_eq!(crate::api::kind_of(&e), invalid);
        assert_eq!(e.to_string(), REMOVE_AND_RESTART);
    }
}
