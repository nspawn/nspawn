//! org.nspawn.Manager at /org/nspawn: everything the command line does, as methods.
//! Dictionaries (a{sv}) carry the results; the long operations come back as jobs.
//! A method described as "Like `CMD`" does what `nspawn CMD` does.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue};

use crate::api::{self, Context};
use crate::auth::Credentials;
use crate::backend::BackendChoice;
use crate::daemon::jobs::{self, Dict};
use crate::daemon::polkit::{self, Action};
use crate::daemon::processes;
use crate::daemon::values::{self, Options};
use crate::daemon::State;
use crate::nsenter;
use crate::oci::Mode;
use crate::search::SearchSource;

/// Errors leave as org.nspawn.Error.Failed with the library's message.
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.nspawn.Error")]
pub enum Error {
    #[zbus(error)]
    ZBus(zbus::Error),
    Failed(String),
    /// The caller is not allowed to do this: polkit said so.
    NotAuthorized(String),
}

impl From<anyhow::Error> for Error {
    fn from(e: anyhow::Error) -> Self {
        Error::Failed(crate::daemon::values::bus_text(&format!("{e:#}")))
    }
}

type Result<T> = std::result::Result<T, Error>;

pub struct Manager {
    state: Arc<State>,
}

/// How many Events streams a user other than root holds at once.
const STREAMS_PER_USER: usize = 64;

/// What a copy did, as the job's result.
fn copy_result(stats: &api::copy::Stats) -> HashMap<String, OwnedValue> {
    HashMap::from([
        ("entries".to_string(), values::v(stats.entries)),
        ("bytes".to_string(), values::v(stats.bytes)),
    ])
}

impl Manager {
    pub fn new(state: Arc<State>) -> Self {
        Manager { state }
    }

    fn ctx(&self) -> &Context {
        &self.state.ctx
    }

    /// polkit's answer, or root only on a host without polkit. Returns the caller, whose
    /// uid owns what is started here and whose bus name gets its signals.
    async fn allow(&self, header: &Header<'_>, action: Action) -> Result<polkit::Caller> {
        let sender = header
            .sender()
            .map(|s| s.to_string())
            .ok_or_else(|| Error::NotAuthorized("the call carries no sender".to_string()))?;
        let interactive = header
            .primary()
            .flags()
            .contains(zbus::message::Flags::AllowInteractiveAuth);
        polkit::allows(self.state.connection(), &sender, action, interactive)
            .await
            .map_err(Error::NotAuthorized)?;
        let uid = polkit::caller_uid(self.state.connection(), &sender)
            .await
            .ok_or_else(|| Error::NotAuthorized("the bus cannot say who called".to_string()))?;
        Ok(polkit::Caller {
            uid,
            name: Some(sender),
        })
    }

    /// The context for a call: the service's own, or one with the registry and CA
    /// certificate the caller named in its options (registry (s), ca_cert (s)).
    fn context_for(&self, options: &mut Options<'_>) -> anyhow::Result<Arc<Context>> {
        let registry = options.string("registry")?.filter(|r| !r.is_empty());
        let ca_cert = options.string("ca_cert")?.filter(|c| !c.is_empty());
        if registry.is_none() && ca_cert.is_none() {
            return Ok(self.state.ctx.clone());
        }
        let mut config = self.ctx().config.clone();
        if let Some(registry) = registry {
            config.registry = registry;
        }
        if let Some(ca_cert) = ca_cert {
            config.ca_cert = Some(PathBuf::from(ca_cert));
        }
        Ok(Arc::new(self.ctx().with_config(config)))
    }
}

/// The lines and notes a call makes, for the methods that hand them back with their
/// result instead of streaming them.
#[derive(Default)]
struct Notes(Mutex<Vec<String>>);

impl Notes {
    fn report(&self) -> impl Fn(api::Event) + '_ {
        move |event| {
            if let api::Event::Line(text) | api::Event::Note(text) = event {
                self.0.lock().unwrap().push(values::bus_text(&text));
            }
        }
    }

    fn into_lines(self) -> Vec<String> {
        self.0.into_inner().unwrap()
    }
}

/// A child's exit code; 128 plus the signal when it died of one.
fn status_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .or_else(|| status.signal().map(|s| 128 + s))
        .unwrap_or(126)
}

fn backend_choice(text: Option<&str>) -> anyhow::Result<BackendChoice> {
    Ok(match text {
        None | Some("") | Some("auto") => BackendChoice::Auto,
        Some("overlay") => BackendChoice::Overlay,
        Some("flat") => BackendChoice::Flat,
        Some("mstack") => BackendChoice::Mstack,
        Some(other) => anyhow::bail!("backend {other}: expected auto, overlay, flat or mstack"),
    })
}

fn mode_choice(text: Option<&str>) -> anyhow::Result<Option<Mode>> {
    Ok(match text {
        None | Some("") | Some("auto") => None,
        Some("boot") => Some(Mode::Boot),
        Some("app") => Some(Mode::App),
        Some(other) => anyhow::bail!("mode {other}: expected auto, boot or app"),
    })
}

/// Ends when the client with this unique bus name leaves the bus; never without one.
async fn caller_gone(connection: zbus::Connection, name: Option<String>) {
    use futures_util::StreamExt;
    let Some(name) = name else {
        return std::future::pending().await;
    };
    let Ok(dbus) = zbus::fdo::DBusProxy::new(&connection).await else {
        return std::future::pending().await;
    };
    let Ok(mut changes) = dbus
        .receive_name_owner_changed_with_args(&[(0, name.as_str())])
        .await
    else {
        return std::future::pending().await;
    };
    // Gone before the watch began.
    if let Ok(bus_name) = zbus::names::BusName::try_from(name.as_str()) {
        if !dbus.name_has_owner(bus_name).await.unwrap_or(true) {
            return;
        }
    }
    while let Some(change) = changes.next().await {
        if change.args().is_ok_and(|args| args.new_owner().is_none()) {
            return;
        }
    }
}

/// What StartMachine and RunMachine are asked: wait (b, default `wait`), network (s) or
/// networks (as), aliases (as), addresses (as, IP or NETWORK=IP), publish (as), entrypoint (s), env (as), volume (as),
/// label (as), restart (s), memory (t), memory_swap (x), cpus (d), pids_limit (t),
/// image_command (b), command (as), remove (b), and the flags of `health_overrides` and `tuning_overrides`.
pub(crate) fn start_request(
    name: String,
    options: &mut Options<'_>,
    wait: bool,
) -> anyhow::Result<api::machines::StartRequest> {
    Ok(api::machines::StartRequest {
        name,
        wait: options.bool("wait", wait)?,
        network: network_choice(options.string("network")?, options.strings("networks")?)?,
        aliases: options.strings("aliases")?,
        addresses: options.strings("addresses")?,
        publish: options.strings("publish")?,
        entrypoint: options.string("entrypoint")?,
        env: options.strings("env")?,
        volume: options.strings("volume")?,
        label: options.strings("label")?,
        restart: options
            .string("restart")?
            .map(|r| crate::policy::Restart::parse(&r))
            .transpose()?,
        memory: options.maybe_u64("memory")?,
        memory_swap: options.i64("memory_swap")?,
        cpus: options.f64("cpus")?,
        pids_limit: options.maybe_u64("pids_limit")?,
        image_command: options.bool("image_command", false)?,
        command: options.strings("command")?,
        remove: options.bool("remove", false)?,
        health: health_overrides(options)?,
        tuning: tuning_overrides(options)?,
    })
}

/// The --health-* flags: health_cmd (s), health_test (as, the whole test),
/// health_interval, health_timeout, health_start_period,
/// health_start_interval (t, microseconds), health_retries (u), no_healthcheck (b).
fn health_overrides(options: &mut Options<'_>) -> anyhow::Result<crate::health::Overrides> {
    let test = options.strings("health_test")?;
    Ok(crate::health::Overrides {
        cmd: options.string("health_cmd")?,
        test: (!test.is_empty()).then_some(test),
        interval: options.maybe_u64("health_interval")?,
        timeout: options.maybe_u64("health_timeout")?,
        start_period: options.maybe_u64("health_start_period")?,
        start_interval: options.maybe_u64("health_start_interval")?,
        retries: options
            .maybe_u64("health_retries")?
            .map(|n| {
                u32::try_from(n).map_err(|_| anyhow::anyhow!("option health_retries is too large"))
            })
            .transpose()?,
        disable: options.bool("no_healthcheck", false)?,
    })
}

/// The per-machine settings: hostname, user, working_dir, stop_signal, timezone,
/// log_driver (s), cap_add,
/// cap_drop, tmpfs, devices (HOST:CONTAINER:PERMISSIONS or a CDI device name), gpus
/// (as --gpus takes them), interfaces (host interfaces
/// moved into the machine), dns, dns_search, extra_hosts (HOST:IP), ulimits
/// (NAME=SOFT:HARD), sysctls (KEY=VALUE), secrets (NAME[:TARGET[:MODE[:UID:GID]]]) (as,
/// "none" clears), privileged, read_only, init (b), shm_size, stop_timeout (t),
/// oom_score_adj (i).
fn tuning_overrides(options: &mut Options<'_>) -> anyhow::Result<crate::tuning::Overrides> {
    Ok(crate::tuning::Overrides {
        hostname: options.string("hostname")?,
        user: options.string("user")?,
        working_dir: options.string("working_dir")?,
        cap_add: options.strings("cap_add")?,
        cap_drop: options.strings("cap_drop")?,
        privileged: options.maybe_bool("privileged")?,
        read_only: options.maybe_bool("read_only")?,
        tmpfs: options.strings("tmpfs")?,
        shm_size: options.maybe_u64("shm_size")?,
        devices: options.strings("devices")?,
        gpus: options.strings("gpus")?,
        interfaces: options.strings("interfaces")?,
        dns: options.strings("dns")?,
        dns_search: options.strings("dns_search")?,
        extra_hosts: options.strings("extra_hosts")?,
        ulimits: options.strings("ulimits")?,
        oom_score_adj: options
            .i64("oom_score_adj")?
            .map(|n| {
                i32::try_from(n)
                    .map_err(|_| anyhow::anyhow!("option oom_score_adj is out of range"))
            })
            .transpose()?,
        stop_signal: options.string("stop_signal")?,
        timezone: options.string("timezone")?,
        log_driver: options.string("log_driver")?,
        stop_timeout: options.maybe_u64("stop_timeout")?,
        init: options.maybe_bool("init")?,
        sysctls: options.strings("sysctls")?,
        secrets: options.strings("secrets")?,
        depends_on: options.strings("depends_on")?,
    })
}

/// The network options: `networks` (as) when given, else `network` (s): bridge, veth,
/// host, none or networks' names, checked before anything is done.
fn network_choice(one: Option<String>, several: Vec<String>) -> anyhow::Result<Vec<String>> {
    let texts = if !several.is_empty() {
        several
    } else {
        one.filter(|t| !t.is_empty()).into_iter().collect()
    };
    if !texts.is_empty() {
        api::network::choices(&texts)?;
    }
    Ok(texts)
}

#[zbus::interface(name = "org.nspawn.Manager")]
impl Manager {
    /// Read first by every command line: it counts as a call, so that the service does
    /// not go idle between it and the call that follows.
    #[zbus(property)]
    fn version(&self) -> String {
        let _busy = self.state.enter();
        env!("CARGO_PKG_VERSION").to_string()
    }

    /// The hub: where references without a host part go.
    #[zbus(property)]
    fn registry(&self) -> String {
        self.ctx().config.registry.clone()
    }

    #[zbus(property)]
    fn bridge(&self) -> String {
        self.ctx().config.bridge.clone()
    }

    #[zbus(property)]
    fn subnet(&self) -> String {
        self.ctx().config.subnet.to_string()
    }

    /// Jobs running, and the last ones that ended; read when needed: a method's reply
    /// names a job it starts, JobRemoved its end.
    #[zbus(property(emits_changed_signal = "false"))]
    fn jobs(&self) -> Vec<OwnedObjectPath> {
        self.state.jobs.paths()
    }

    /// Commands running, and the last ones that exited; read when needed.
    #[zbus(property(emits_changed_signal = "false"))]
    fn processes(&self) -> Vec<OwnedObjectPath> {
        self.state.processes.paths()
    }

    /// Local images, like `images ls`: name, kind, backend, origin, reference, size,
    /// read_only, signed_by, image_only, created, pulled.
    async fn list_images(&self, #[zbus(header)] hdr: Header<'_>) -> Result<Vec<Dict>> {
        let _busy = self.state.enter();
        self.allow(&hdr, Action::Inspect).await?;
        let images = api::images::list(self.ctx()).await?;
        Ok(images.iter().map(values::image).collect())
    }

    /// Everything nspawn keeps about one image, as `values::record` has it.
    async fn get_image(&self, #[zbus(header)] hdr: Header<'_>, name: String) -> Result<Dict> {
        let _busy = self.state.enter();
        self.allow(&hdr, Action::Inspect).await?;
        let record = self
            .ctx()
            .store
            .load_image(&name)?
            .ok_or_else(|| Error::Failed(format!("no image named {name}")))?;
        Ok(values::record(&record))
    }

    /// Like `generate`: the nspawn commands that make the named machines again, every
    /// machine of nspawn's when none is named, with the networks they join and the
    /// machines they depend on, as a shell script. No options yet.
    async fn generate(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        names: Vec<String>,
        options: HashMap<String, OwnedValue>,
    ) -> Result<String> {
        let _busy = self.state.enter();
        self.allow(&hdr, Action::Inspect).await?;
        Options::new(&options).finish()?;
        Ok(api::generate::generate(self.ctx(), &names)?.render())
    }

    /// Like `pull`. Options: name (s), backend (s), mode (s), force (b), verify (b, default
    /// true: the signature check the registry's policy asks for), registry (s), ca_cert
    /// (s). The job's result carries name, reference, mode and signed_by.
    async fn pull_image(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        reference: String,
        options: HashMap<String, OwnedValue>,
    ) -> Result<OwnedObjectPath> {
        let _busy = self.state.enter();
        let owner = self.allow(&hdr, Action::Manage).await?;
        let mut options = Options::new(&options);
        let ctx = self.context_for(&mut options)?;
        let request = api::pull::PullRequest {
            reference,
            name: options.string("name")?,
            backend: backend_choice(options.string("backend")?.as_deref())?,
            mode: mode_choice(options.string("mode")?.as_deref())?,
            force: options.bool("force", false)?,
            verify: options.bool("verify", true)?,
        };
        options.finish()?;
        let target = request
            .name
            .clone()
            .unwrap_or_else(|| request.reference.clone());
        let state = self.state.clone();
        Ok(jobs::spawn(
            &self.state,
            owner,
            ctx,
            "pull",
            &target,
            move |ctx, reporter| async move {
                let pulled = api::pull::pull(&ctx, &request, jobs::report(&reporter)).await?;
                if let Ok(emitter) = state.emitter() {
                    let _ = Manager::image_added(&emitter, &pulled.name).await;
                }
                Ok(HashMap::from([
                    ("name".to_string(), values::v(pulled.name)),
                    ("reference".to_string(), values::v(pulled.reference)),
                    ("mode".to_string(), values::v(pulled.mode.name())),
                    (
                        "signed_by".to_string(),
                        values::v(pulled.signed_by.unwrap_or_default()),
                    ),
                ]))
            },
        )
        .await?)
    }

    /// Like `create`. Options: backend (s), network (s) or networks (as), aliases (as),
    /// addresses (as), publish (as), force (b), entrypoint (s), env (as), volume (as), label (as),
    /// restart (s), memory (t, bytes), memory_swap (x, bytes of memory and swap, -1 for
    /// no bound on swap), cpus (d), pids_limit (t), command (as), the flags of
    /// `health_overrides` and `tuning_overrides`, registry (s), ca_cert (s).
    async fn create_machine(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        source: String,
        name: String,
        options: HashMap<String, OwnedValue>,
    ) -> Result<OwnedObjectPath> {
        let _busy = self.state.enter();
        let owner = self.allow(&hdr, Action::Manage).await?;
        let mut options = Options::new(&options);
        let ctx = self.context_for(&mut options)?;
        let request = api::create::CreateRequest {
            source,
            name: name.clone(),
            backend: backend_choice(options.string("backend")?.as_deref())?,
            network: network_choice(options.string("network")?, options.strings("networks")?)?,
            aliases: options.strings("aliases")?,
            addresses: options.strings("addresses")?,
            publish: options.strings("publish")?,
            force: options.bool("force", false)?,
            entrypoint: options.string("entrypoint")?,
            env: options.strings("env")?,
            unset_env: options.strings("unset_env")?,
            volume: options.strings("volume")?,
            label: options.strings("label")?,
            restart: options
                .string("restart")?
                .map(|r| crate::policy::Restart::parse(&r))
                .transpose()?,
            memory: options.maybe_u64("memory")?,
            memory_swap: options.i64("memory_swap")?,
            cpus: options.f64("cpus")?,
            pids_limit: options.maybe_u64("pids_limit")?,
            command: options.strings("command")?,
            health: health_overrides(&mut options)?,
            tuning: tuning_overrides(&mut options)?,
        };
        options.finish()?;
        let state = self.state.clone();
        Ok(jobs::spawn(
            &self.state,
            owner,
            ctx,
            "create",
            &name,
            move |ctx, reporter| async move {
                let created = api::create::create(&ctx, &request, jobs::report(&reporter)).await?;
                if let Ok(emitter) = state.emitter() {
                    let _ = Manager::image_added(&emitter, &created.name).await;
                }
                Ok(HashMap::from([
                    ("name".to_string(), values::v(created.name)),
                    ("mode".to_string(), values::v(created.mode.name())),
                ]))
            },
        )
        .await?)
    }

    /// Like `push`. Options: to (s), registry (s), ca_cert (s). The job's result
    /// carries destination and url.
    async fn push_image(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        image: String,
        options: HashMap<String, OwnedValue>,
    ) -> Result<OwnedObjectPath> {
        let _busy = self.state.enter();
        let owner = self.allow(&hdr, Action::Manage).await?;
        let mut options = Options::new(&options);
        let ctx = self.context_for(&mut options)?;
        let request = api::push::PushRequest {
            image: image.clone(),
            to: options.string("to")?,
        };
        options.finish()?;
        Ok(jobs::spawn(
            &self.state,
            owner,
            ctx,
            "push",
            &image,
            move |ctx, reporter| async move {
                let pushed = api::push::push(&ctx, &request, jobs::report(&reporter)).await?;
                Ok(HashMap::from([
                    ("name".to_string(), values::v(pushed.name)),
                    ("destination".to_string(), values::v(pushed.destination)),
                    ("url".to_string(), values::v(pushed.url)),
                ]))
            },
        )
        .await?)
    }

    /// Like `build` of a Dockerfile: the build context a tar stream on `context`, the
    /// Dockerfile in options. Options: dockerfile (ay), tags (as), build_args (as,
    /// KEY=VALUE), target (s), no_cache (b), no_cache_filter (as, stages), pull (b),
    /// labels (as, KEY=VALUE), network (s: default, none or host), add_hosts (as,
    /// HOST:IP), allow (as, entitlements: network.host), secrets (a{say}, by id),
    /// shm_size (t, bytes), ulimits (as, NAME=SOFT[:HARD]), verify (b, default true),
    /// backend (s), mode (s), force (b), registry (s), ca_cert (s). The steps' output
    /// comes through the job; its result has id (the config digest), manifest_digest and
    /// names (as, the images made, one per tag). The build stops when its caller leaves
    /// the bus.
    async fn build_dockerfile(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        context: zbus::zvariant::OwnedFd,
        options: HashMap<String, OwnedValue>,
    ) -> Result<OwnedObjectPath> {
        let _busy = self.state.enter();
        let owner = self.allow(&hdr, Action::Manage).await?;
        let mut options = Options::new(&options);
        let ctx = self.context_for(&mut options)?;
        let pairs = |items: Vec<String>,
                     what: &str|
         -> anyhow::Result<std::collections::BTreeMap<String, String>> {
            items
                .into_iter()
                .map(|item| {
                    item.split_once('=')
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .ok_or_else(|| anyhow::anyhow!("{what} {item}: KEY=VALUE"))
                })
                .collect()
        };
        let network = match options.string("network")?.as_deref() {
            None | Some("") | Some("default") => crate::build::plan::Network::Default,
            Some("none") => crate::build::plan::Network::None,
            Some("host") => crate::build::plan::Network::Host,
            Some(other) => {
                return Err(anyhow::anyhow!("--network {other}: default, none or host").into())
            }
        };
        let mut allow_host_network = false;
        for entitlement in options.strings("allow")? {
            match entitlement.as_str() {
                "network.host" => allow_host_network = true,
                "security.insecure" => {
                    return Err(anyhow::anyhow!(
                    "--allow security.insecure: steps run with the confinement of an app machine"
                )
                    .into())
                }
                other => return Err(anyhow::anyhow!("--allow {other}: unknown entitlement").into()),
            }
        }
        let add_hosts = options
            .strings("add_hosts")?
            .iter()
            .map(|h| crate::tuning::extra_host(h).map(|e| (e.host, e.ip)))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let ulimits = options
            .strings("ulimits")?
            .iter()
            .map(|u| crate::tuning::ulimit(u))
            .collect::<anyhow::Result<_>>()?;
        let request = crate::build::exec::Request {
            dockerfile: options
                .bytes("dockerfile")?
                .ok_or_else(|| anyhow::anyhow!("option dockerfile is missing"))?,
            context: PathBuf::new(),
            tags: options.strings("tags")?,
            options: crate::build::plan::Options {
                build_args: pairs(options.strings("build_args")?, "--build-arg")?,
                target: options.string("target")?.filter(|t| !t.is_empty()),
                labels: pairs(options.strings("labels")?, "--label")?,
            },
            no_cache: options.bool("no_cache", false)?,
            no_cache_filter: options.strings("no_cache_filter")?,
            pull: options.bool("pull", false)?,
            network,
            allow_host_network,
            add_hosts,
            secrets: options.byte_map("secrets")?,
            shm_size: options.maybe_u64("shm_size")?,
            ulimits,
            verify: options.bool("verify", true)?,
            backend: backend_choice(options.string("backend")?.as_deref())?,
            mode: mode_choice(options.string("mode")?.as_deref())?,
            force: options.bool("force", false)?,
        };
        options.finish()?;
        let input = std::os::fd::OwnedFd::from(context);
        let target = request.tags.first().cloned().unwrap_or_default();
        let state = self.state.clone();
        // A build whose client goes away stops.
        let cancel = tokio_util::sync::CancellationToken::new();
        let watch = {
            let gone = caller_gone(self.state.connection().clone(), owner.name.clone());
            let cancel = cancel.clone();
            tokio::spawn(async move {
                gone.await;
                cancel.cancel();
            })
        };
        Ok(jobs::spawn(
            &self.state,
            owner,
            ctx,
            "build",
            &target,
            move |ctx, reporter| async move {
                let built = crate::build::exec::build(
                    &ctx,
                    request,
                    input,
                    jobs::report(&reporter),
                    cancel,
                )
                .await;
                watch.abort();
                let built = built?;
                if let Ok(emitter) = state.emitter() {
                    for name in &built.names {
                        let _ = Manager::image_added(&emitter, name).await;
                    }
                }
                Ok(HashMap::from([
                    ("id".to_string(), values::v(built.id)),
                    (
                        "manifest_digest".to_string(),
                        values::v(built.manifest_digest),
                    ),
                    ("names".to_string(), values::strings(&built.names)),
                ]))
            },
        )
        .await?)
    }

    /// Like `build`. Options: name (s), distribution (s), release (s), profile (as),
    /// backend (s), mode (s), force (b), keep_output (b), mkosi_args (as), registry
    /// (s), ca_cert (s). mkosi's output comes through the job, line by line.
    async fn build_image(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        directory: String,
        tag: String,
        options: HashMap<String, OwnedValue>,
    ) -> Result<OwnedObjectPath> {
        let _busy = self.state.enter();
        let owner = self.allow(&hdr, Action::Manage).await?;
        let mut options = Options::new(&options);
        let ctx = self.context_for(&mut options)?;
        let request = api::build::BuildRequest {
            directory: PathBuf::from(directory),
            tag: tag.clone(),
            name: options.string("name")?,
            distribution: options.string("distribution")?,
            release: options.string("release")?,
            profile: options.strings("profile")?,
            backend: backend_choice(options.string("backend")?.as_deref())?,
            mode: mode_choice(options.string("mode")?.as_deref())?,
            force: options.bool("force", false)?,
            keep_output: options.bool("keep_output", false)?,
            mkosi_args: options.strings("mkosi_args")?,
        };
        options.finish()?;
        let state = self.state.clone();
        Ok(jobs::spawn(
            &self.state,
            owner,
            ctx,
            "build",
            &tag,
            move |ctx, reporter| async move {
                let built = api::build::build(&ctx, &request, jobs::report(&reporter)).await?;
                if let Ok(emitter) = state.emitter() {
                    let _ = Manager::image_added(&emitter, &built.name).await;
                }
                Ok(HashMap::from([
                    ("name".to_string(), values::v(built.name)),
                    ("reference".to_string(), values::v(built.reference)),
                    ("mode".to_string(), values::v(built.mode.name())),
                    (
                        "output".to_string(),
                        values::v(
                            built
                                .output
                                .map(|p| p.display().to_string())
                                .unwrap_or_default(),
                        ),
                    ),
                ]))
            },
        )
        .await?)
    }

    /// Like `images rm`: a job whose lines say what was removed and freed, with the
    /// removed names as its result. Every name is tried; the job fails at the end when
    /// one could not be removed.
    async fn remove_images(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        names: Vec<String>,
    ) -> Result<OwnedObjectPath> {
        let _busy = self.state.enter();
        let owner = self.allow(&hdr, Action::Manage).await?;
        let state = self.state.clone();
        let target = names.join(" ");
        Ok(jobs::spawn(
            &self.state,
            owner,
            self.state.ctx.clone(),
            "rm",
            &target,
            move |ctx, reporter| async move {
                let removal = api::images::remove(&ctx, &names, jobs::report(&reporter)).await?;
                for name in &removal.removed {
                    if let Ok(emitter) = state.emitter() {
                        let _ = Manager::image_removed(&emitter, name).await;
                    }
                }
                let result =
                    HashMap::from([("removed".to_string(), values::v(removal.removed.clone()))]);
                if let Some(error) = removal.error() {
                    return Err(jobs::Partly { result, error }.into());
                }
                Ok(result)
            },
        )
        .await?)
    }

    /// Like `rm`: `RemoveImages` with the option force (b), which stops a running machine
    /// first (SIGKILL) instead of refusing it. A job; its result lists `removed`.
    async fn remove_machines(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        names: Vec<String>,
        options: HashMap<String, OwnedValue>,
    ) -> Result<OwnedObjectPath> {
        let _busy = self.state.enter();
        let owner = self.allow(&hdr, Action::Manage).await?;
        let mut options = Options::new(&options);
        let force = options.bool("force", false)?;
        options.finish()?;
        let state = self.state.clone();
        let target = names.join(" ");
        Ok(jobs::spawn(
            &self.state,
            owner,
            self.state.ctx.clone(),
            "rm",
            &target,
            move |ctx, reporter| async move {
                let removal =
                    api::images::remove_machines(&ctx, &names, force, jobs::report(&reporter))
                        .await?;
                for name in &removal.removed {
                    if let Ok(emitter) = state.emitter() {
                        let _ = Manager::image_removed(&emitter, name).await;
                    }
                }
                let result =
                    HashMap::from([("removed".to_string(), values::v(removal.removed.clone()))]);
                if let Some(error) = removal.error() {
                    return Err(jobs::Partly { result, error }.into());
                }
                Ok(result)
            },
        )
        .await?)
    }

    /// Like `cp NAME:PATH ...`: `path` of the machine (running, or a stopped overlay or
    /// flat one) as a tar stream on the returned pipe, owners as the machine sees them,
    /// and a job that ends with its result (entries, bytes) or why it could not.
    async fn copy_from(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        machine: String,
        path: String,
        options: HashMap<String, OwnedValue>,
    ) -> Result<(zbus::zvariant::OwnedFd, OwnedObjectPath)> {
        let _busy = self.state.enter();
        let owner = self.allow(&hdr, Action::Manage).await?;
        Options::new(&options).finish()?;
        let (read, write) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)
            .map_err(|e| Error::Failed(format!("creating a pipe: {e}")))?;
        let target = format!("{machine}:{path}");
        let job = jobs::spawn(
            &self.state,
            owner,
            self.state.ctx.clone(),
            "cp",
            &target,
            move |ctx, reporter| async move {
                let stats =
                    api::copy::copy_from(&ctx, &machine, &path, write, jobs::report(&reporter))
                        .await?;
                Ok(copy_result(&stats))
            },
        )
        .await?;
        Ok((zbus::zvariant::OwnedFd::from(read), job))
    }

    /// Like `cp ... NAME:PATH`: the tar stream read from `stream` unpacked at `path` of
    /// the machine by cp's rules, everything owned by root inside. Options:
    /// contents (b), the source was written DIR/. and its contents go into `path`. A job.
    async fn copy_to(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        machine: String,
        path: String,
        stream: zbus::zvariant::OwnedFd,
        options: HashMap<String, OwnedValue>,
    ) -> Result<OwnedObjectPath> {
        let _busy = self.state.enter();
        let owner = self.allow(&hdr, Action::Manage).await?;
        let mut options = Options::new(&options);
        let contents = options.bool("contents", false)?;
        options.finish()?;
        let input = std::os::fd::OwnedFd::from(stream);
        let target = format!("{machine}:{path}");
        Ok(jobs::spawn(
            &self.state,
            owner,
            self.state.ctx.clone(),
            "cp",
            &target,
            move |ctx, reporter| async move {
                let stats = api::copy::copy_to(
                    &ctx,
                    &machine,
                    &path,
                    contents,
                    input,
                    jobs::report(&reporter),
                )
                .await?;
                Ok(copy_result(&stats))
            },
        )
        .await?)
    }

    /// Like `volume ls`: every named volume with name, path, used_by (the machines whose
    /// records mount it) and created (unix seconds).
    async fn list_volumes(&self, #[zbus(header)] hdr: Header<'_>) -> Result<Vec<Dict>> {
        let _busy = self.state.enter();
        self.allow(&hdr, Action::Inspect).await?;
        let volumes = api::volumes::list(&self.ctx().store)?;
        Ok(volumes.iter().map(values::volume).collect())
    }

    /// Like `volume create`: makes the volume's directory ahead of its first use and
    /// returns its path. One that exists already is not an error.
    async fn create_volume(&self, #[zbus(header)] hdr: Header<'_>, name: String) -> Result<String> {
        let _busy = self.state.enter();
        self.allow(&hdr, Action::Manage).await?;
        let path = api::volumes::create(self.ctx(), &name).await?;
        Ok(path.to_string_lossy().into_owned())
    }

    /// Like `volume rm`: a job, since a big volume takes a while to delete. Every name is
    /// tried; the result lists the ones removed, and the job fails at the end when one
    /// was in use, unknown or not a volume.
    async fn remove_volumes(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        names: Vec<String>,
    ) -> Result<OwnedObjectPath> {
        let _busy = self.state.enter();
        let owner = self.allow(&hdr, Action::Manage).await?;
        let target = names.join(" ");
        Ok(jobs::spawn(
            &self.state,
            owner,
            self.state.ctx.clone(),
            "volume-rm",
            &target,
            move |ctx, reporter| async move {
                let removal = api::volumes::remove(&ctx, &names, jobs::report(&reporter)).await?;
                let result =
                    HashMap::from([("removed".to_string(), values::v(removal.removed.clone()))]);
                if let Some(error) = removal.error() {
                    return Err(jobs::Partly { result, error }.into());
                }
                Ok(result)
            },
        )
        .await?)
    }

    /// Like `auto-update`: the machines labelled org.nspawn.auto-update=registry, or the
    /// ones named, get the image their reference names now at its registry, made anew
    /// with every option they had and put back when they do not come up. Option dry_run
    /// (b) only looks. The job's result: machines (aa{ss}: name, image, policy, updated
    /// (false, pending, true, failed, rolled back or skipped), reason, from, to).
    async fn auto_update(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        names: Vec<String>,
        options: HashMap<String, OwnedValue>,
    ) -> Result<OwnedObjectPath> {
        let _busy = self.state.enter();
        let owner = self.allow(&hdr, Action::Manage).await?;
        let mut options = Options::new(&options);
        let dry_run = options.bool("dry_run", false)?;
        options.finish()?;
        for name in &names {
            crate::reference::validate_entry_name(name)?;
        }
        Ok(jobs::spawn(
            &self.state,
            owner,
            self.state.ctx.clone(),
            "auto-update",
            "",
            move |ctx, reporter| async move {
                let request = api::autoupdate::Request { names, dry_run };
                let outcomes =
                    api::autoupdate::run(&ctx, &request, jobs::report(&reporter)).await?;
                let machines: Vec<HashMap<String, String>> = outcomes
                    .into_iter()
                    .map(|o| {
                        HashMap::from([
                            ("name".to_string(), o.name),
                            ("image".to_string(), o.image),
                            ("policy".to_string(), o.policy),
                            ("updated".to_string(), o.updated.word().to_string()),
                            ("reason".to_string(), o.reason),
                            ("from".to_string(), o.from),
                            ("to".to_string(), o.to),
                        ])
                    })
                    .collect();
                Ok(HashMap::from([(
                    "machines".to_string(),
                    values::v(machines),
                )]))
            },
        )
        .await?)
    }

    /// Like `images prune`: with option all (b), every image (a record pulled or built
    /// and never started) no machine was made from; without, the dangling ones, which
    /// nspawn's store does not have. Options labels (as, KEY or KEY=VALUE) and until (t,
    /// unix seconds) narrow it. The job's result: removed (as), reclaimed (t, bytes).
    async fn prune_images(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        options: HashMap<String, OwnedValue>,
    ) -> Result<OwnedObjectPath> {
        let _busy = self.state.enter();
        let owner = self.allow(&hdr, Action::Manage).await?;
        let mut options = Options::new(&options);
        let all = options.bool("all", false)?;
        let labels = options.strings("labels")?;
        let until = options.maybe_u64("until")?;
        options.finish()?;
        let state = self.state.clone();
        Ok(jobs::spawn(
            &self.state,
            owner,
            self.state.ctx.clone(),
            "image-prune",
            "",
            move |ctx, reporter| async move {
                let pruned =
                    api::disk::prune_images(&ctx, all, &labels, until, jobs::report(&reporter))
                        .await?;
                // As rm says it: a client following the images learns of these too.
                for name in &pruned.removed {
                    if let Ok(emitter) = state.emitter() {
                        let _ = Manager::image_removed(&emitter, name).await;
                    }
                }
                Ok(HashMap::from([
                    ("removed".to_string(), values::v(pruned.removed)),
                    ("reclaimed".to_string(), values::v(pruned.reclaimed)),
                ]))
            },
        )
        .await?)
    }

    /// Like `builder prune`: options all (b, what images take too), until (t, unix
    /// seconds: entries last used before), keep_storage (t, bytes of the most recently
    /// used kept). A job; its result has removed (as) and reclaimed (t, bytes).
    async fn prune_build_cache(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        options: HashMap<String, OwnedValue>,
    ) -> Result<OwnedObjectPath> {
        let _busy = self.state.enter();
        let owner = self.allow(&hdr, Action::Manage).await?;
        let mut options = Options::new(&options);
        let all = options.bool("all", false)?;
        let until = options.maybe_u64("until")?;
        let keep_storage = options.maybe_u64("keep_storage")?;
        options.finish()?;
        Ok(jobs::spawn(
            &self.state,
            owner,
            self.state.ctx.clone(),
            "builder-prune",
            "",
            move |ctx, reporter| async move {
                let pruned = api::disk::prune_build_cache(
                    &ctx,
                    all,
                    until,
                    keep_storage,
                    jobs::report(&reporter),
                )
                .await?;
                Ok(HashMap::from([
                    ("removed".to_string(), values::strings(&pruned.removed)),
                    ("reclaimed".to_string(), values::v(pruned.reclaimed)),
                ]))
            },
        )
        .await?)
    }

    /// Like `builder du`: what the build cache holds, most recently used first: id, kind
    /// (layer, base or mount), description, size (t), used (t, unix seconds), shared (b,
    /// an image takes it too).
    async fn build_cache_usage(&self, #[zbus(header)] hdr: Header<'_>) -> Result<Vec<Dict>> {
        let _busy = self.state.enter();
        self.allow(&hdr, Action::Inspect).await?;
        let rows = crate::build::cache::usage(&self.ctx().store)?;
        Ok(rows
            .iter()
            .map(|r| {
                HashMap::from([
                    ("id".to_string(), values::v(r.id.as_str())),
                    ("kind".to_string(), values::v(r.kind)),
                    ("description".to_string(), values::v(r.description.as_str())),
                    ("size".to_string(), values::v(r.size)),
                    ("used".to_string(), values::v(r.used)),
                    ("shared".to_string(), values::v(r.shared)),
                ])
            })
            .collect())
    }

    /// Like `system df`: images, containers (machines), local_volumes and build_cache,
    /// each a{st} of total, active, size and reclaimable (bytes), and the rows of
    /// `system df -v`: image_rows (aa{sv}: name, reference, created, size, containers),
    /// container_rows (name, reference, size, running, made: made from an image by
    /// create or run, which system prune removes once stopped) and volume_rows (name,
    /// links, size).
    async fn disk_usage(&self, #[zbus(header)] hdr: Header<'_>) -> Result<Dict> {
        let _busy = self.state.enter();
        self.allow(&hdr, Action::Inspect).await?;
        let usage = api::disk::usage(self.ctx()).await?;
        let kind = |k: &api::disk::Kind| {
            values::v(HashMap::from([
                ("total".to_string(), k.total as u64),
                ("active".to_string(), k.active as u64),
                ("size".to_string(), k.size),
                ("reclaimable".to_string(), k.reclaimable),
            ]))
        };
        let images: Vec<Dict> = usage
            .image_rows
            .iter()
            .map(|r| {
                HashMap::from([
                    ("name".to_string(), values::v(r.name.as_str())),
                    ("reference".to_string(), values::v(r.reference.as_str())),
                    ("created".to_string(), values::v(r.created)),
                    ("size".to_string(), values::v(r.size)),
                    ("containers".to_string(), values::v(r.containers as u64)),
                ])
            })
            .collect();
        let machines: Vec<Dict> = usage
            .machine_rows
            .iter()
            .map(|r| {
                HashMap::from([
                    ("name".to_string(), values::v(r.name.as_str())),
                    ("reference".to_string(), values::v(r.reference.as_str())),
                    ("size".to_string(), values::v(r.size)),
                    ("running".to_string(), values::v(r.running)),
                    ("made".to_string(), values::v(r.made)),
                ])
            })
            .collect();
        let volumes: Vec<Dict> = usage
            .volume_rows
            .iter()
            .map(|r| {
                HashMap::from([
                    ("name".to_string(), values::v(r.name.as_str())),
                    ("links".to_string(), values::v(r.links as u64)),
                    ("size".to_string(), values::v(r.size)),
                ])
            })
            .collect();
        Ok(HashMap::from([
            ("images".to_string(), kind(&usage.images)),
            ("containers".to_string(), kind(&usage.containers)),
            ("local_volumes".to_string(), kind(&usage.volumes)),
            ("build_cache".to_string(), kind(&usage.build_cache)),
            ("image_rows".to_string(), values::v(images)),
            ("container_rows".to_string(), values::v(machines)),
            ("volume_rows".to_string(), values::v(volumes)),
        ]))
    }

    /// Like `volume prune`: a job removing every volume no machine uses; its result lists
    /// them.
    async fn prune_volumes(&self, #[zbus(header)] hdr: Header<'_>) -> Result<OwnedObjectPath> {
        let _busy = self.state.enter();
        let owner = self.allow(&hdr, Action::Manage).await?;
        Ok(jobs::spawn(
            &self.state,
            owner,
            self.state.ctx.clone(),
            "volume-prune",
            "",
            move |ctx, reporter| async move {
                let removed = api::volumes::prune(&ctx, jobs::report(&reporter)).await?;
                Ok(HashMap::from([("removed".to_string(), values::v(removed))]))
            },
        )
        .await?)
    }

    /// Like `secret ls`: every secret with name, created (unix seconds), size (t, bytes
    /// of the plaintext), labels (a{ss}) and used_by (as); never the content.
    async fn list_secrets(&self, #[zbus(header)] hdr: Header<'_>) -> Result<Vec<Dict>> {
        let _busy = self.state.enter();
        self.allow(&hdr, Action::Inspect).await?;
        let secrets = api::secrets::list(&self.ctx().store)?;
        Ok(secrets.iter().map(values::secret).collect())
    }

    /// Like `secret inspect`: one secret as ListSecrets has it.
    async fn get_secret(&self, #[zbus(header)] hdr: Header<'_>, name: String) -> Result<Dict> {
        let _busy = self.state.enter();
        self.allow(&hdr, Action::Inspect).await?;
        Ok(values::secret(&api::secrets::get(
            &self.ctx().store,
            &name,
        )?))
    }

    /// Like `secret create`: the content, encrypted for this host with systemd-creds.
    /// Options: labels (as, KEY=VALUE). A name in use is refused.
    async fn create_secret(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        name: String,
        content: Vec<u8>,
        options: HashMap<String, OwnedValue>,
    ) -> Result<()> {
        let _busy = self.state.enter();
        self.allow(&hdr, Action::Manage).await?;
        let mut options = Options::new(&options);
        let labels = crate::volume::parse_labels(&options.strings("labels")?)?;
        let replace = match (
            options.bool("replace", false)?,
            options.bool("replace_if_changed", false)?,
        ) {
            (true, _) => api::secrets::Replace::Always,
            (false, true) => api::secrets::Replace::IfChanged,
            (false, false) => api::secrets::Replace::Refuse,
        };
        options.finish()?;
        Ok(api::secrets::create(self.ctx(), &name, &content, labels, replace).await?)
    }

    /// Like `secret rm`: a job; every name is tried, the result lists the ones removed,
    /// and the job fails at the end when one was in use or unknown.
    async fn remove_secrets(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        names: Vec<String>,
    ) -> Result<OwnedObjectPath> {
        let _busy = self.state.enter();
        let owner = self.allow(&hdr, Action::Manage).await?;
        let target = names.join(" ");
        Ok(jobs::spawn(
            &self.state,
            owner,
            self.state.ctx.clone(),
            "secret-rm",
            &target,
            move |ctx, reporter| async move {
                let removal = api::secrets::remove(&ctx, &names, jobs::report(&reporter)).await?;
                let result =
                    HashMap::from([("removed".to_string(), values::v(removal.removed.clone()))]);
                if let Some(error) = removal.error() {
                    return Err(jobs::Partly { result, error }.into());
                }
                Ok(result)
            },
        )
        .await?)
    }

    /// Like `network ls`: every network (the default one first) with name, interface,
    /// subnet, gateway, internal, created and machines (the ones whose records name it).
    async fn list_networks(&self, #[zbus(header)] hdr: Header<'_>) -> Result<Vec<Dict>> {
        let _busy = self.state.enter();
        self.allow(&hdr, Action::Inspect).await?;
        let networks = api::network::list_networks(self.ctx())?;
        Ok(networks.iter().map(values::network_summary).collect())
    }

    /// Like `network inspect`: one network ("bridge" for the default one) and its
    /// machines, with address, ports and whether they run.
    async fn get_network(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        name: String,
    ) -> Result<(Dict, Vec<Dict>)> {
        let _busy = self.state.enter();
        self.allow(&hdr, Action::Inspect).await?;
        let (spec, entries) = api::network::inspect(self.ctx(), &name).await?;
        Ok((
            values::network(&spec),
            entries.iter().map(values::network_entry).collect(),
        ))
    }

    /// Like `network create`: options subnet (s, CIDR; the next free /24 of
    /// network_pool otherwise), internal (b), labels (as, KEY=VALUE). The bridge comes up
    /// at once. Returns the network as ListNetworks has it, and the notes made on the way
    /// under "notes".
    async fn create_network(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        name: String,
        options: HashMap<String, OwnedValue>,
    ) -> Result<Dict> {
        let _busy = self.state.enter();
        self.allow(&hdr, Action::Manage).await?;
        let mut options = Options::new(&options);
        let subnet = options.string("subnet")?;
        let internal = options.bool("internal", false)?;
        let labels = crate::volume::parse_labels(&options.strings("labels")?)?;
        options.finish()?;
        let notes = Notes::default();
        let spec = api::network::create(
            self.ctx(),
            &name,
            subnet.as_deref(),
            internal,
            labels,
            &notes.report(),
        )
        .await?;
        let mut dict = values::network(&spec);
        dict.insert("notes".to_string(), values::v(notes.into_lines()));
        Ok(dict)
    }

    /// Like `network rm`: a job; every name is tried, the result lists the ones removed,
    /// and the job fails at the end when one was in use, unknown or the default network.
    async fn remove_networks(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        names: Vec<String>,
    ) -> Result<OwnedObjectPath> {
        let _busy = self.state.enter();
        let owner = self.allow(&hdr, Action::Manage).await?;
        let target = names.join(" ");
        Ok(jobs::spawn(
            &self.state,
            owner,
            self.state.ctx.clone(),
            "network-rm",
            &target,
            move |ctx, reporter| async move {
                let removal = api::network::remove(&ctx, &names, jobs::report(&reporter)).await?;
                let result =
                    HashMap::from([("removed".to_string(), values::v(removal.removed.clone()))]);
                if let Some(error) = removal.error() {
                    return Err(jobs::Partly { result, error }.into());
                }
                Ok(result)
            },
        )
        .await?)
    }

    /// Like `network connect`: the machine joins the network, at once when it runs.
    /// Options aliases (as), address (s, a fixed IPv4 address on a network made with
    /// CreateNetwork). A job.
    async fn connect_network(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        network: String,
        machine: String,
        options: HashMap<String, OwnedValue>,
    ) -> Result<OwnedObjectPath> {
        let _busy = self.state.enter();
        let owner = self.allow(&hdr, Action::Manage).await?;
        let mut options = Options::new(&options);
        let aliases = options.strings("aliases")?;
        let address = match options.string("address")?.filter(|a| !a.is_empty()) {
            Some(text) => Some(
                text.parse::<std::net::Ipv4Addr>()
                    .map_err(|_| Error::Failed(format!("{text} is not an IPv4 address")))?,
            ),
            None => None,
        };
        options.finish()?;
        Ok(jobs::spawn(
            &self.state,
            owner,
            self.state.ctx.clone(),
            "network-connect",
            &machine.clone(),
            move |ctx, reporter| async move {
                api::network::connect(
                    &ctx,
                    &network,
                    &machine,
                    &aliases,
                    address,
                    jobs::report(&reporter),
                )
                .await?;
                Ok(HashMap::new())
            },
        )
        .await?)
    }

    /// Like `network disconnect`: the machine leaves the network, at once when it runs.
    /// Option force (b): a machine or a membership that is not there is no error. A job.
    async fn disconnect_network(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        network: String,
        machine: String,
        options: HashMap<String, OwnedValue>,
    ) -> Result<OwnedObjectPath> {
        let _busy = self.state.enter();
        let owner = self.allow(&hdr, Action::Manage).await?;
        let mut options = Options::new(&options);
        let force = options.bool("force", false)?;
        options.finish()?;
        Ok(jobs::spawn(
            &self.state,
            owner,
            self.state.ctx.clone(),
            "network-disconnect",
            &machine.clone(),
            move |ctx, reporter| async move {
                api::network::disconnect(&ctx, &network, &machine, force, jobs::report(&reporter))
                    .await?;
                Ok(HashMap::new())
            },
        )
        .await?)
    }

    /// Like `network prune`: a job removing every user-defined network no machine names;
    /// its result lists them.
    async fn prune_networks(&self, #[zbus(header)] hdr: Header<'_>) -> Result<OwnedObjectPath> {
        let _busy = self.state.enter();
        let owner = self.allow(&hdr, Action::Manage).await?;
        Ok(jobs::spawn(
            &self.state,
            owner,
            self.state.ctx.clone(),
            "network-prune",
            "",
            move |ctx, reporter| async move {
                let removal = api::network::prune(&ctx, jobs::report(&reporter)).await?;
                let result =
                    HashMap::from([("removed".to_string(), values::v(removal.removed.clone()))]);
                if let Some(error) = removal.error() {
                    return Err(jobs::Partly { result, error }.into());
                }
                Ok(result)
            },
        )
        .await?)
    }

    /// Like `search`: source "" (both), "hub" or "dockerhub"; limit per source. Returns
    /// the hits and the notes (a source that could not be reached, say). Options:
    /// registry (s), ca_cert (s).
    async fn search_images(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        term: String,
        source: String,
        limit: u32,
        options: HashMap<String, OwnedValue>,
    ) -> Result<(Vec<Dict>, Vec<String>)> {
        let _busy = self.state.enter();
        self.allow(&hdr, Action::Manage).await?;
        let mut options = Options::new(&options);
        let ctx = self.context_for(&mut options)?;
        options.finish()?;
        let source = match source.as_str() {
            "" => None,
            "hub" => Some(SearchSource::Hub),
            "dockerhub" => Some(SearchSource::Dockerhub),
            other => {
                return Err(Error::Failed(format!(
                    "source {other}: expected hub, dockerhub or an empty string for both"
                )))
            }
        };
        let notes = Notes::default();
        let hits =
            api::search::search(&ctx, &term, source, limit as usize, &notes.report()).await?;
        Ok((hits.iter().map(values::hit).collect(), notes.into_lines()))
    }

    /// Like `hub ls`: repositories containing `filter` ("" for all), with their tags when
    /// asked. Options: registry (s), ca_cert (s).
    async fn list_repositories(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        filter: String,
        with_tags: bool,
        options: HashMap<String, OwnedValue>,
    ) -> Result<Vec<Dict>> {
        let _busy = self.state.enter();
        self.allow(&hdr, Action::Manage).await?;
        let mut options = Options::new(&options);
        let ctx = self.context_for(&mut options)?;
        options.finish()?;
        let filter = if filter.is_empty() {
            None
        } else {
            Some(filter.as_str())
        };
        let repos = api::hub::repositories(&ctx, filter, with_tags).await?;
        Ok(repos
            .into_iter()
            .map(|r| {
                HashMap::from([
                    ("name".to_string(), values::v(r.name)),
                    (
                        "tags".to_string(),
                        values::strings(&r.tags.unwrap_or_default()),
                    ),
                ])
            })
            .collect())
    }

    /// Like `hub tags`. Options: registry (s), ca_cert (s).
    async fn list_tags(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        repository: String,
        options: HashMap<String, OwnedValue>,
    ) -> Result<Vec<String>> {
        let _busy = self.state.enter();
        self.allow(&hdr, Action::Manage).await?;
        let mut options = Options::new(&options);
        let ctx = self.context_for(&mut options)?;
        options.finish()?;
        Ok(api::hub::tags(&ctx, &repository)
            .await?
            .iter()
            .map(|t| values::bus_text(t))
            .collect())
    }

    /// The digest a reference names on its registry now, an index's for a tag of
    /// several platforms, without pulling it: what compose config pins images to.
    /// Options: registry (s), ca_cert (s).
    async fn image_digest(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        reference: String,
        options: HashMap<String, OwnedValue>,
    ) -> Result<String> {
        let _busy = self.state.enter();
        self.allow(&hdr, Action::Manage).await?;
        let mut options = Options::new(&options);
        let ctx = self.context_for(&mut options)?;
        options.finish()?;
        Ok(api::hub::digest(&ctx, &reference).await?)
    }

    /// Like `ps` (and `ps -a` with `all`): every machine with its state, started time,
    /// leader, os and, when nspawn installed its image, the image's record and
    /// machine_path, its object in machined.
    async fn list_machines(&self, #[zbus(header)] hdr: Header<'_>, all: bool) -> Result<Vec<Dict>> {
        let _busy = self.state.enter();
        self.allow(&hdr, Action::Inspect).await?;
        let machines = api::machines::list(self.ctx(), all).await?;
        Ok(machines.iter().map(values::machine).collect())
    }

    /// Like `stats`: one sample of the counters of each named machine that runs (every
    /// running container when none is named): name, time_usec (CLOCK_MONOTONIC),
    /// cpu_usec, memory, memory_limit, pids, io_read, io_write, net_rx, net_tx, each
    /// left out when it cannot be read (net_* on the host's network). Rates come from
    /// two samples.
    async fn machine_stats(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        names: Vec<String>,
    ) -> Result<Vec<Dict>> {
        let _busy = self.state.enter();
        self.allow(&hdr, Action::Inspect).await?;
        let samples = api::stats::sample(self.ctx(), &names).await?;
        Ok(samples.iter().map(values::sample).collect())
    }

    /// Like `inspect`: one machine as `ListMachines` has it (running or not), or the
    /// record of an image that is not running, with state "stopped".
    async fn get_machine(&self, #[zbus(header)] hdr: Header<'_>, name: String) -> Result<Dict> {
        let _busy = self.state.enter();
        self.allow(&hdr, Action::Inspect).await?;
        let machine = api::machines::get(self.ctx(), &name).await?;
        Ok(values::machine(&machine))
    }

    /// Like `start`. Options: those of `start_request`, wait defaulting to true. Returns
    /// "started", "ended" when the program returned before the machine registered, or
    /// "restarting" when it ended and its restart policy brings it back, and the notes
    /// made on the way.
    async fn start_machine(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        name: String,
        options: HashMap<String, OwnedValue>,
    ) -> Result<(String, Vec<String>)> {
        let _busy = self.state.enter();
        self.allow(&hdr, Action::Manage).await?;
        let mut options = Options::new(&options);
        let request = start_request(name, &mut options, true)?;
        options.finish()?;
        let notes = Notes::default();
        let outcome = match api::machines::start(self.ctx(), &request, &notes.report()).await? {
            api::machines::StartOutcome::Started => "started",
            api::machines::StartOutcome::Ended => "ended",
            api::machines::StartOutcome::Restarting => "restarting",
        };
        Ok((outcome.to_string(), notes.into_lines()))
    }

    /// Like `run` without -d. Options: StartMachine's (wait defaults to false), tty (b),
    /// rows and cols (t, 24 x 80), term (s, xterm); descriptor stdin (the program's input
    /// without tty). With tty an app's program gets a pseudo terminal whose master comes
    /// back under "tty"; otherwise the output comes back under "stdout", a line at a
    /// time. The process object's Signal reaches the program (a booted machine is
    /// powered off), and its exit status is the one run exits with. Also returns the start's
    /// notes.
    async fn run_machine(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        name: String,
        options: HashMap<String, OwnedValue>,
        mut fds: HashMap<String, zbus::zvariant::OwnedFd>,
    ) -> Result<(
        HashMap<String, zbus::zvariant::OwnedFd>,
        OwnedObjectPath,
        Vec<String>,
    )> {
        let _busy = self.state.enter();
        let owner = self.allow(&hdr, Action::Manage).await?;
        let mut options = Options::new(&options);
        let tty = options.bool("tty", false)?;
        let rows = options.u64("rows", 24)?.clamp(1, u16::MAX as u64) as u16;
        let cols = options.u64("cols", 80)?.clamp(1, u16::MAX as u64) as u16;
        let term = options
            .string("term")?
            .filter(|t| !t.is_empty() && t.chars().all(|c| c.is_ascii_graphic()))
            .unwrap_or_else(|| "xterm".to_string());
        let request = start_request(name, &mut options, false)?;
        options.finish()?;
        let stdin = fds.remove("stdin").map(std::os::fd::OwnedFd::from);
        if let Some(other) = fds.keys().next() {
            return Err(Error::Failed(format!("unknown descriptor {other}")));
        }
        if let Err(e) = api::network::remove_ended(self.ctx()).await {
            eprintln!("warning: removing the machines of run --rm that ended: {e:#}");
        }
        let notes = Notes::default();
        let run = api::run::RunRequest {
            start: request,
            terminal: tty.then_some(api::run::Terminal { rows, cols, term }),
            stdin: if tty { None } else { stdin },
        };
        let mut attached = api::run::attach(self.ctx(), run, &notes.report()).await?;
        let mut handed = HashMap::new();
        if let Some(master) = attached.terminal.take() {
            handed.insert("tty".to_string(), zbus::zvariant::OwnedFd::from(master));
        }
        if let Some(output) = attached.output.take() {
            handed.insert("stdout".to_string(), zbus::zvariant::OwnedFd::from(output));
        }
        let signals = processes::Signals {
            poweroff: (!attached.app).then(|| attached.name.clone()),
            program: attached.app.then(|| (attached.name.clone(), attached.pid1)),
            sent: Default::default(),
        };
        let sent = signals.sent.clone();
        let pid = attached.main_pid;
        let name = attached.name.clone();
        let caller_gone = caller_gone(self.state.connection().clone(), owner.name.clone());
        let wait = attached.finish(self.state.ctx.clone(), caller_gone, sent);
        let argv = vec!["run".to_string(), name.clone()];
        let path = processes::register_with(
            &self.state,
            owner,
            &name,
            &argv,
            pid,
            None,
            signals,
            false,
            wait,
        )
        .await?;
        Ok((handed, path, notes.into_lines()))
    }

    /// Like `stop`. Options: force (b), wait (b, default true), timeout (t, seconds
    /// before SIGKILL; the machine's stop_timeout by default, else 10; a day at most, or
    /// u64::MAX for none). Returns "stopped" or "was-not-running", and the notes made on
    /// the way (a program that had to be killed, say).
    async fn stop_machine(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        name: String,
        options: HashMap<String, OwnedValue>,
    ) -> Result<(String, Vec<String>)> {
        let _busy = self.state.enter();
        self.allow(&hdr, Action::Manage).await?;
        let mut options = Options::new(&options);
        let request = api::machines::StopRequest {
            name,
            force: options.bool("force", false)?,
            wait: options.bool("wait", true)?,
            // A day at most, or none at all.
            timeout: options.maybe_u64("timeout")?.map(|t| match t {
                crate::policy::NO_STOP_TIMEOUT => t,
                t => t.min(86_400),
            }),
        };
        options.finish()?;
        let notes = Notes::default();
        let outcome = match api::machines::stop(self.ctx(), &request, &notes.report()).await? {
            api::machines::StopOutcome::Stopped => "stopped",
            api::machines::StopOutcome::WasNotRunning => "was-not-running",
        };
        Ok((outcome.to_string(), notes.into_lines()))
    }

    /// Like `update`: options restart (s), memory (t, bytes), memory_swap (x, bytes of
    /// memory and swap, -1 for no bound on swap), cpus (d), pids_limit (t), 0 removing a
    /// limit, absent keeping it, and the flags of `health_overrides`. A
    /// running machine gets the limits at once (the restart policy applies to its next
    /// ending anyway) and its probes start over. Returns whether it was running.
    async fn update_machine(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        name: String,
        options: HashMap<String, OwnedValue>,
    ) -> Result<bool> {
        let _busy = self.state.enter();
        self.allow(&hdr, Action::Manage).await?;
        let mut options = Options::new(&options);
        let request = api::machines::UpdateRequest {
            name,
            restart: options
                .string("restart")?
                .map(|r| crate::policy::Restart::parse(&r))
                .transpose()?,
            memory: options.maybe_u64("memory")?,
            memory_swap: options.i64("memory_swap")?,
            cpus: options.f64("cpus")?,
            pids_limit: options.maybe_u64("pids_limit")?,
            health: health_overrides(&mut options)?,
        };
        options.finish()?;
        Ok(api::machines::update(self.ctx(), &request).await?)
    }

    /// Like `kill`: option signal (s, a name like KILL or SIGHUP, or a number; SIGKILL
    /// when absent). SIGKILL stops the machine for good, like StopMachine with force;
    /// other signals go to an app's program or a booted machine's init. Returns the
    /// notes made on the way.
    async fn kill_machine(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        name: String,
        options: HashMap<String, OwnedValue>,
    ) -> Result<Vec<String>> {
        let _busy = self.state.enter();
        self.allow(&hdr, Action::Manage).await?;
        let mut options = Options::new(&options);
        let request = api::machines::KillRequest {
            name,
            signal: options
                .string("signal")?
                .unwrap_or_else(|| "SIGKILL".to_string()),
        };
        options.finish()?;
        let notes = Notes::default();
        api::machines::kill(self.ctx(), &request, &notes.report()).await?;
        Ok(notes.into_lines())
    }

    /// Like `pause`: the machine's cgroup is frozen, every process in it with it.
    async fn pause_machine(&self, #[zbus(header)] hdr: Header<'_>, name: String) -> Result<()> {
        let _busy = self.state.enter();
        self.allow(&hdr, Action::Manage).await?;
        Ok(api::machines::pause(self.ctx(), &name, true).await?)
    }

    /// Like `unpause`.
    async fn unpause_machine(&self, #[zbus(header)] hdr: Header<'_>, name: String) -> Result<()> {
        let _busy = self.state.enter();
        self.allow(&hdr, Action::Manage).await?;
        Ok(api::machines::pause(self.ctx(), &name, false).await?)
    }

    /// Like `top`: the processes of a running machine: pid (u), user (s, the uid as the
    /// machine sees it), time (s, CPU time), command (s).
    async fn machine_processes(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        name: String,
    ) -> Result<Vec<Dict>> {
        let _busy = self.state.enter();
        self.allow(&hdr, Action::Inspect).await?;
        let processes = api::machines::processes(self.ctx(), &name).await?;
        Ok(processes.iter().map(values::process).collect())
    }

    /// The login session machined offers for a booted machine, like `shell`: a pseudo
    /// terminal running the user's shell ("" for root), and the terminal's path. Apps
    /// have no login inside: Exec with a shell and a tty is the way for them. Options:
    /// env (as), the caller's TERM among them (xterm otherwise).
    async fn shell(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        machine: String,
        user: String,
        options: HashMap<String, OwnedValue>,
    ) -> Result<(zbus::zvariant::OwnedFd, String)> {
        let _busy = self.state.enter();
        self.allow(&hdr, Action::Manage).await?;
        let mut options = Options::new(&options);
        let mut env = options.strings("env")?;
        options.finish()?;
        if !env.iter().any(|v| v.starts_with("TERM=")) {
            env.push("TERM=xterm".to_string());
        }
        let user = if user.is_empty() {
            "root".to_string()
        } else {
            user
        };
        let (fd, pty) =
            api::machines::open_shell(self.ctx(), &machine, &user, "", Vec::new(), env).await?;
        Ok((zbus::zvariant::OwnedFd::from(fd), pty))
    }

    /// Like `logs`: journalctl's output under "stdout" and "stderr", and a process
    /// object for its exit status. Options: follow (b), lines (t), since (s), until (s),
    /// timestamps (b), all (b), inside (b), json (b: journalctl's JSON entries, one a
    /// line, for a client that writes each line's time itself). journalctl is stopped
    /// once nobody reads its output any more, so a --follow ends with its client.
    async fn logs(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        machine: String,
        options: HashMap<String, OwnedValue>,
    ) -> Result<(HashMap<String, zbus::zvariant::OwnedFd>, OwnedObjectPath)> {
        let _busy = self.state.enter();
        let owner = self.allow(&hdr, Action::Manage).await?;
        let mut options = Options::new(&options);
        let request = api::machines::LogsRequest {
            machine,
            follow: options.bool("follow", false)?,
            // 0 is a value: only what comes from now on.
            lines: options
                .maybe_u64("lines")?
                .map(|n| n.min(u32::MAX as u64) as u32),
            since: options.string("since")?,
            until: options.string("until")?,
            timestamps: options.bool("timestamps", false)?,
            all: options.bool("all", false)?,
            inside: options.bool("inside", false)?,
            namespace: None,
            json: options.bool("json", false)?,
        };
        options.finish()?;
        crate::reference::validate_entry_name(&request.machine)?;
        // Logs are what one reads when machined is in trouble: only its clear answer that
        // the name is somebody else's machine stands in the way.
        if let Ok(sd) = self.ctx().sd().await {
            if let Ok(Some(runner)) = sd.foreign_machine(&request.machine).await {
                return Err(Error::Failed(format!(
                    "{} is a machine of {runner}, not a container; nspawn manages systemd-nspawn machines only",
                    request.machine
                )));
            }
        }
        let mut request = request;
        api::machines::prepare_logs(self.ctx(), &mut request)
            .await
            .map_err(|e| Error::Failed(format!("{e:#}")))?;
        let argv = api::machines::journalctl_arguments(&request);
        let pipe = || {
            nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)
                .map_err(|e| Error::Failed(format!("creating a pipe: {e}")))
        };
        let (out_r, out_w) = pipe()?;
        let (err_r, err_w) = pipe()?;
        // A copy of the writing end stays here: it reports an error once nobody reads,
        // which is how a journalctl --follow learns that its client is gone.
        let watch = out_w
            .try_clone()
            .map_err(|e| Error::Failed(e.to_string()))?;
        let mut child = tokio::process::Command::new("journalctl")
            .args(&argv)
            .stdin(Stdio::null())
            .stdout(Stdio::from(out_w))
            .stderr(Stdio::from(err_w))
            .spawn()
            .map_err(|e| Error::Failed(format!("running journalctl: {e}")))?;
        let pid = child.id().unwrap_or(0);
        let pidfd = nsenter::pidfd_open(nix::unistd::Pid::from_raw(pid as i32)).ok();
        let wait = async move {
            let reader_gone = async {
                match tokio::io::unix::AsyncFd::with_interest(watch, tokio::io::Interest::ERROR) {
                    Ok(watch) => {
                        let _ = watch.ready(tokio::io::Interest::ERROR).await;
                    }
                    Err(_) => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                status = child.wait() => status.map(status_code).unwrap_or(126),
                _ = reader_gone => {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    128 + nix::libc::SIGKILL
                }
            }
        };
        let mut command = vec!["journalctl".to_string()];
        command.extend(argv);
        let path = processes::register(
            &self.state,
            owner,
            &request.machine,
            &command,
            pid,
            pidfd,
            wait,
        )
        .await?;
        Ok((
            HashMap::from([
                ("stdout".to_string(), zbus::zvariant::OwnedFd::from(out_r)),
                ("stderr".to_string(), zbus::zvariant::OwnedFd::from(err_r)),
            ]),
            path,
        ))
    }

    /// Like `events`: what happens to machines, networks, volumes and secrets, one JSON
    /// object per line under "stdout" (time, time_usec, type, action, name, attributes,
    /// labels), journalctl's complaints under "stderr". Options since and until (s,
    /// journalctl's time syntax; without until it follows), filters (as, KEY=VALUE:
    /// name, type, event, label). The process object is journalctl's; it ends when
    /// the reader leaves, or at until.
    async fn events(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        options: HashMap<String, OwnedValue>,
    ) -> Result<(HashMap<String, zbus::zvariant::OwnedFd>, OwnedObjectPath)> {
        let _busy = self.state.enter();
        let owner = self.allow(&hdr, Action::Inspect).await?;
        // Each stream is a journalctl and a few descriptors of the service's: a user who
        // may only look holds a bounded number, its place taken before it is set up.
        let slot = if owner.uid == 0 {
            None
        } else {
            match self
                .state
                .processes
                .reserve_stream(owner.uid, STREAMS_PER_USER)
            {
                Some(slot) => Some(slot),
                None => {
                    return Err(Error::Failed(format!(
                        "{STREAMS_PER_USER} streams of yours are open; close one first"
                    )))
                }
            }
        };
        let mut options = Options::new(&options);
        let since = options.string("since")?;
        let until = options.string("until")?;
        let filters = api::events::Filters::parse(&options.strings("filters")?)?;
        options.finish()?;
        // A window of the past: without its start, it would read nothing and end.
        if until.is_some() && since.is_none() {
            return Err(Error::Failed(
                "until reads events back and needs since as well".to_string(),
            ));
        }
        let argv = api::events::journalctl_arguments(since.as_deref(), until.as_deref());
        let pipe = || {
            nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)
                .map_err(|e| Error::Failed(format!("creating a pipe: {e}")))
        };
        let (out_r, out_w) = pipe()?;
        let (err_r, err_w) = pipe()?;
        // As for Logs: a copy of the writing end reports an error once nobody reads.
        let watch = out_w
            .try_clone()
            .map_err(|e| Error::Failed(e.to_string()))?;
        let mut follow =
            api::events::Follow::spawn(since.as_deref(), until.as_deref(), Stdio::from(err_w))
                .map_err(|e| Error::Failed(format!("{e:#}")))?;
        let pid = follow.pid();
        let pidfd = nsenter::pidfd_open(nix::unistd::Pid::from_raw(pid as i32)).ok();
        let mut out = tokio::net::unix::pipe::Sender::from_owned_fd(out_w)
            .map_err(|e| Error::Failed(format!("preparing the events' pipe: {e}")))?;
        let ctx = self.state.ctx.clone();
        let wait = async move {
            use tokio::io::AsyncWriteExt;
            let reader_gone = async {
                match tokio::io::unix::AsyncFd::with_interest(watch, tokio::io::Interest::ERROR) {
                    Ok(watch) => {
                        let _ = watch.ready(tokio::io::Interest::ERROR).await;
                    }
                    Err(_) => std::future::pending::<()>().await,
                }
            };
            tokio::pin!(reader_gone);
            loop {
                tokio::select! {
                    event = follow.next(&ctx.store) => {
                        let Some(event) = event else { break };
                        if !filters.matches(&event) {
                            continue;
                        }
                        let text = format!("{}\n", event.to_json());
                        if out.write_all(text.as_bytes()).await.is_err() {
                            break;
                        }
                    }
                    _ = &mut reader_gone => break,
                }
            }
            drop(out);
            follow.finish().await
        };
        let mut command = vec!["journalctl".to_string()];
        command.extend(argv);
        let path =
            processes::register_stream(&self.state, owner, &command, pid, pidfd, wait).await?;
        // Counted as a running stream from here on.
        drop(slot);
        Ok((
            HashMap::from([
                ("stdout".to_string(), zbus::zvariant::OwnedFd::from(out_r)),
                ("stderr".to_string(), zbus::zvariant::OwnedFd::from(err_r)),
            ]),
            path,
        ))
    }

    /// Like `exec`: runs argv inside the machine as `user` ("" for the machine's own), and
    /// hands out its streams: with option tty (b, default true) a pseudo terminal of rows
    /// x cols (t, default 24 x 80) under "tty", otherwise pipes under "stdin", "stdout"
    /// and "stderr"; with detach (b) none, the output discarded. Option env (as) adds
    /// variables, the caller's TERM among them (xterm otherwise, on a terminal), and
    /// workdir (s) replaces the machine's. The process object says how it ends.
    async fn exec(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        machine: String,
        argv: Vec<String>,
        user: String,
        options: HashMap<String, OwnedValue>,
    ) -> Result<(HashMap<String, zbus::zvariant::OwnedFd>, OwnedObjectPath)> {
        let _busy = self.state.enter();
        let owner = self.allow(&hdr, Action::Manage).await?;
        let mut options = Options::new(&options);
        let detach = options.bool("detach", false)?;
        let tty = options.bool("tty", true)? && !detach;
        let rows = options.u64("rows", 24)?;
        let cols = options.u64("cols", 80)?;
        let env = options.strings("env")?;
        let workdir = options.string("workdir")?;
        options.finish()?;
        let stdio = if tty {
            crate::nsenter::Stdio::Pty {
                rows: rows.clamp(1, u16::MAX as u64) as u16,
                cols: cols.clamp(1, u16::MAX as u64) as u16,
            }
        } else if detach {
            crate::nsenter::Stdio::Null
        } else {
            crate::nsenter::Stdio::Pipes
        };
        let nsenter::Process {
            helper,
            pid,
            pidfd,
            master,
            stdin,
            stdout,
            stderr,
        } = api::machines::spawn_in_namespaces(
            self.ctx(),
            &machine,
            &argv,
            &user,
            &env,
            workdir.as_deref(),
            stdio,
        )
        .await?;
        let handle = pidfd
            .try_clone()
            .map_err(|e| Error::Failed(format!("duplicating the command's pidfd: {e}")))?;
        let wait = async move {
            tokio::task::spawn_blocking(move || nsenter::wait(helper))
                .await
                .ok()
                .and_then(|r| r.ok())
                .unwrap_or(126)
        };
        let path =
            match processes::register(&self.state, owner, &machine, &argv, pid, Some(pidfd), wait)
                .await
            {
                Ok(path) => path,
                Err(e) => {
                    // Nobody will hold its streams: the command must not linger.
                    let _ = nsenter::pidfd_signal(&handle, nix::libc::SIGKILL);
                    let _ = tokio::task::spawn_blocking(move || nsenter::wait(helper)).await;
                    return Err(e.into());
                }
            };
        let mut fds = HashMap::new();
        // A detached command writes to /dev/null: nothing in the service has to stay to
        // read it, which a restart of the service would end.
        if detach {
            return Ok((fds, path));
        }
        for (name, fd) in [
            ("tty", master),
            ("stdin", stdin),
            ("stdout", stdout),
            ("stderr", stderr),
        ] {
            if let Some(fd) = fd {
                fds.insert(name.to_string(), zbus::zvariant::OwnedFd::from(fd));
            }
        }
        Ok((fds, path))
    }

    /// Like `network up`: the default network (bridge, subnet, gateway, host_name), the
    /// names of the networks brought up under "networks", and the notes made on the way
    /// under "notes".
    async fn network_up(&self, #[zbus(header)] hdr: Header<'_>) -> Result<Dict> {
        let _busy = self.state.enter();
        self.allow(&hdr, Action::Manage).await?;
        let notes = Notes::default();
        let (info, networks) = api::network::up(self.ctx(), &notes.report()).await?;
        let mut dict = values::bridge(&info);
        dict.insert("networks".to_string(), values::strings(&networks));
        dict.insert("notes".to_string(), values::v(notes.into_lines()));
        Ok(dict)
    }

    /// Like `login`: registry "" for the hub. Options: registry (s, the hub "" stands
    /// for), ca_cert (s). Returns registry, username and whether the registry asked for
    /// credentials.
    async fn login(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        registry: String,
        username: String,
        password: String,
        options: HashMap<String, OwnedValue>,
    ) -> Result<Dict> {
        let _busy = self.state.enter();
        self.allow(&hdr, Action::Manage).await?;
        let mut options = Options::new(&options);
        let ctx = self.context_for(&mut options)?;
        options.finish()?;
        let registry = if registry.is_empty() {
            None
        } else {
            Some(registry)
        };
        let done = api::login::login(&ctx, registry, Credentials { username, password }).await?;
        Ok(HashMap::from([
            ("registry".to_string(), values::v(done.registry)),
            ("username".to_string(), values::v(done.username)),
            ("asked".to_string(), values::v(done.asked)),
        ]))
    }

    /// Like `logout`: true when credentials were stored for the registry.
    async fn logout(&self, #[zbus(header)] hdr: Header<'_>, registry: String) -> Result<bool> {
        let _busy = self.state.enter();
        self.allow(&hdr, Action::Manage).await?;
        let registry = if registry.is_empty() {
            None
        } else {
            Some(registry)
        };
        Ok(api::login::logout(self.ctx(), registry)?.removed)
    }

    /// A line a job said: kind "line" (progress or a result) or "note" (a remark).
    #[zbus(signal)]
    pub async fn job_output(
        emitter: &SignalEmitter<'_>,
        job: ObjectPath<'_>,
        kind: &str,
        line: &str,
    ) -> zbus::Result<()>;

    /// How far a transfer of a job got: `done` bytes of `total` (0 when unknown) of
    /// `item`, the short digest of a blob; a few times a second while it runs.
    #[zbus(signal)]
    pub async fn job_progress(
        emitter: &SignalEmitter<'_>,
        job: ObjectPath<'_>,
        item: &str,
        done: u64,
        total: u64,
    ) -> zbus::Result<()>;

    /// A job ended: result "done" or "failed"; the job object keeps the details.
    #[zbus(signal)]
    pub async fn job_removed(
        emitter: &SignalEmitter<'_>,
        job: ObjectPath<'_>,
        result: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn image_added(emitter: &SignalEmitter<'_>, name: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn image_removed(emitter: &SignalEmitter<'_>, name: &str) -> zbus::Result<()>;

    /// A machine nspawn installed registered with machined.
    #[zbus(signal)]
    pub async fn machine_started(emitter: &SignalEmitter<'_>, name: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn machine_stopped(emitter: &SignalEmitter<'_>, name: &str) -> zbus::Result<()>;
}
