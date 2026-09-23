//! org.nspawn.Manager at /org/nspawn: everything the command line does, as methods.
//! Dictionaries (a{sv}) carry the results; the long operations come back as jobs.

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
use crate::settings::Network;

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
        Error::Failed(format!("{e:#}"))
    }
}

type Result<T> = std::result::Result<T, Error>;

pub struct Manager {
    state: Arc<State>,
}

impl Manager {
    pub fn new(state: Arc<State>) -> Self {
        Manager { state }
    }

    fn ctx(&self) -> &Context {
        &self.state.ctx
    }

    /// Whether the caller may do this, which on a host with polkit is polkit's
    /// answer and on one without is root or nothing.
    /// Returns the caller's uid, which is what a job or a command started here belongs
    /// to afterwards.
    async fn allow(&self, header: &Header<'_>, action: Action) -> Result<u32> {
        let sender = header
            .sender()
            .map(|s| s.to_string())
            .ok_or_else(|| Error::NotAuthorized("the call carries no sender".to_string()))?;
        polkit::allows(self.state.connection(), &sender, action)
            .await
            .map_err(Error::NotAuthorized)?;
        Ok(polkit::caller_uid(self.state.connection(), &sender)
            .await
            .unwrap_or(0))
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
            let text = match event {
                api::Event::Line(t) | api::Event::Note(t) => t,
            };
            self.0.lock().unwrap().push(text);
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

fn backend_choice(text: Option<String>) -> anyhow::Result<BackendChoice> {
    Ok(match text.as_deref() {
        None | Some("") | Some("auto") => BackendChoice::Auto,
        Some("overlay") => BackendChoice::Overlay,
        Some("flat") => BackendChoice::Flat,
        Some("mstack") => BackendChoice::Mstack,
        Some(other) => anyhow::bail!("backend {other}: expected auto, overlay, flat or mstack"),
    })
}

fn mode_choice(text: Option<String>) -> anyhow::Result<Option<Mode>> {
    Ok(match text.as_deref() {
        None | Some("") | Some("auto") => None,
        Some("boot") => Some(Mode::Boot),
        Some("app") => Some(Mode::App),
        Some(other) => anyhow::bail!("mode {other}: expected auto, boot or app"),
    })
}

fn network_choice(text: Option<String>) -> anyhow::Result<Option<Network>> {
    Ok(match text.as_deref() {
        None | Some("") => None,
        Some("bridge") => Some(Network::Bridge),
        Some("veth") => Some(Network::Veth),
        Some("host") => Some(Network::Host),
        Some(other) => anyhow::bail!("network {other}: expected bridge, veth or host"),
    })
}

#[zbus::interface(name = "org.nspawn.Manager")]
impl Manager {
    #[zbus(property)]
    fn version(&self) -> String {
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

    /// Jobs started since the service came up, done ones included.
    #[zbus(property)]
    fn jobs(&self) -> Vec<OwnedObjectPath> {
        self.state.jobs.paths()
    }

    /// Commands started with Exec since the service came up, exited ones included.
    #[zbus(property)]
    fn processes(&self) -> Vec<OwnedObjectPath> {
        self.state.processes.paths()
    }

    /// Local images, like `images ls`: name, kind, backend, origin, reference, size,
    /// read_only.
    async fn list_images(&self, #[zbus(header)] hdr: Header<'_>) -> Result<Vec<Dict>> {
        self.allow(&hdr, Action::Inspect).await?;
        let _busy = self.state.enter();
        let images = api::images::list(self.ctx()).await?;
        Ok(images.iter().map(values::image).collect())
    }

    /// Everything nspawn keeps about one image: reference, digest, backend, mode,
    /// network, address, ports, volumes, env, entrypoint, cmd, command, and the OCI
    /// config's image_env, working_dir, user and stop_signal.
    async fn get_image(&self, #[zbus(header)] hdr: Header<'_>, name: String) -> Result<Dict> {
        self.allow(&hdr, Action::Inspect).await?;
        let _busy = self.state.enter();
        let record = self
            .ctx()
            .store
            .load_image(&name)?
            .ok_or_else(|| Error::Failed(format!("no image named {name}")))?;
        Ok(values::record(&record))
    }

    /// Like `pull`. Options: name (s), backend (s), mode (s), force (b), registry (s),
    /// ca_cert (s). The job's result carries name, reference and mode.
    async fn pull_image(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        reference: String,
        options: HashMap<String, OwnedValue>,
    ) -> Result<OwnedObjectPath> {
        let owner = self.allow(&hdr, Action::Manage).await?;
        let _busy = self.state.enter();
        let mut options = Options::new(&options);
        let ctx = self.context_for(&mut options)?;
        let request = api::pull::PullRequest {
            reference,
            name: options.string("name")?,
            backend: backend_choice(options.string("backend")?)?,
            mode: mode_choice(options.string("mode")?)?,
            force: options.bool("force", false)?,
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
                ]))
            },
        )
        .await?)
    }

    /// Like `create`. Options: backend (s), network (s), publish (as), force (b),
    /// entrypoint (s), env (as), volume (as), command (as), registry (s), ca_cert (s).
    async fn create_machine(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        source: String,
        name: String,
        options: HashMap<String, OwnedValue>,
    ) -> Result<OwnedObjectPath> {
        let owner = self.allow(&hdr, Action::Manage).await?;
        let _busy = self.state.enter();
        let mut options = Options::new(&options);
        let ctx = self.context_for(&mut options)?;
        let request = api::create::CreateRequest {
            source,
            name: name.clone(),
            backend: backend_choice(options.string("backend")?)?,
            network: network_choice(options.string("network")?)?,
            publish: options.strings("publish")?,
            force: options.bool("force", false)?,
            entrypoint: options.string("entrypoint")?,
            env: options.strings("env")?,
            volume: options.strings("volume")?,
            command: options.strings("command")?,
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
        let owner = self.allow(&hdr, Action::Manage).await?;
        let _busy = self.state.enter();
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
        let owner = self.allow(&hdr, Action::Manage).await?;
        let _busy = self.state.enter();
        let mut options = Options::new(&options);
        let ctx = self.context_for(&mut options)?;
        let request = api::build::BuildRequest {
            directory: PathBuf::from(directory),
            tag: tag.clone(),
            name: options.string("name")?,
            distribution: options.string("distribution")?,
            release: options.string("release")?,
            profile: options.strings("profile")?,
            backend: backend_choice(options.string("backend")?)?,
            mode: mode_choice(options.string("mode")?)?,
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
        let owner = self.allow(&hdr, Action::Manage).await?;
        let _busy = self.state.enter();
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
                if let Some(error) = removal.error() {
                    anyhow::bail!("{error}");
                }
                Ok(HashMap::from([(
                    "removed".to_string(),
                    values::v(removal.removed),
                )]))
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
        self.allow(&hdr, Action::Manage).await?;
        let _busy = self.state.enter();
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
        self.allow(&hdr, Action::Manage).await?;
        let _busy = self.state.enter();
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
        self.allow(&hdr, Action::Manage).await?;
        let _busy = self.state.enter();
        let mut options = Options::new(&options);
        let ctx = self.context_for(&mut options)?;
        options.finish()?;
        Ok(api::hub::tags(&ctx, &repository).await?)
    }

    /// Like `ps` (and `ps -a` with `all`): every machine with its state, started time,
    /// leader, os and, when nspawn installed its image, the image's record and
    /// machine_path, its object in machined.
    async fn list_machines(&self, #[zbus(header)] hdr: Header<'_>, all: bool) -> Result<Vec<Dict>> {
        self.allow(&hdr, Action::Inspect).await?;
        let _busy = self.state.enter();
        let machines = api::machines::list(self.ctx(), all).await?;
        Ok(machines.iter().map(values::machine).collect())
    }

    /// Like `start`. Options: wait (b, default true), network (s), publish (as),
    /// entrypoint (s), env (as), volume (as), image_command (b), command (as). Returns
    /// "started", or "ended" when the program returned before the machine registered,
    /// and the notes made on the way.
    async fn start_machine(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        name: String,
        options: HashMap<String, OwnedValue>,
    ) -> Result<(String, Vec<String>)> {
        self.allow(&hdr, Action::Manage).await?;
        let _busy = self.state.enter();
        let mut options = Options::new(&options);
        let request = api::machines::StartRequest {
            name,
            wait: options.bool("wait", true)?,
            network: network_choice(options.string("network")?)?,
            publish: options.strings("publish")?,
            entrypoint: options.string("entrypoint")?,
            env: options.strings("env")?,
            volume: options.strings("volume")?,
            image_command: options.bool("image_command", false)?,
            command: options.strings("command")?,
        };
        options.finish()?;
        let notes = Notes::default();
        let outcome = match api::machines::start(self.ctx(), &request, &notes.report()).await? {
            api::machines::StartOutcome::Started => "started",
            api::machines::StartOutcome::Ended => "ended",
        };
        Ok((outcome.to_string(), notes.into_lines()))
    }

    /// Like `stop`. Options: force (b), wait (b, default true), timeout (t, seconds
    /// before SIGKILL for an app, default 10, a day at most). Returns "stopped" or
    /// "was-not-running", and the notes made on the way (a program that had to be
    /// killed, say).
    async fn stop_machine(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        name: String,
        options: HashMap<String, OwnedValue>,
    ) -> Result<(String, Vec<String>)> {
        self.allow(&hdr, Action::Manage).await?;
        let _busy = self.state.enter();
        let mut options = Options::new(&options);
        let request = api::machines::StopRequest {
            name,
            force: options.bool("force", false)?,
            wait: options.bool("wait", true)?,
            timeout: options.u64("timeout", 10)?.min(86_400),
        };
        options.finish()?;
        let notes = Notes::default();
        let outcome = match api::machines::stop(self.ctx(), &request, &notes.report()).await? {
            api::machines::StopOutcome::Stopped => "stopped",
            api::machines::StopOutcome::WasNotRunning => "was-not-running",
        };
        Ok((outcome.to_string(), notes.into_lines()))
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
        self.allow(&hdr, Action::Manage).await?;
        let _busy = self.state.enter();
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
    /// object for its exit status. Options: follow (b), lines (t), since (s),
    /// timestamps (b), all (b), inside (b). journalctl is stopped once nobody reads its
    /// output any more, so a --follow ends with its client.
    async fn logs(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        machine: String,
        options: HashMap<String, OwnedValue>,
    ) -> Result<(HashMap<String, zbus::zvariant::OwnedFd>, OwnedObjectPath)> {
        let owner = self.allow(&hdr, Action::Manage).await?;
        let _busy = self.state.enter();
        let mut options = Options::new(&options);
        let request = api::machines::LogsRequest {
            machine,
            follow: options.bool("follow", false)?,
            lines: Some(options.u64("lines", 0)?.min(u32::MAX as u64) as u32).filter(|n| *n > 0),
            since: options.string("since")?,
            timestamps: options.bool("timestamps", false)?,
            all: options.bool("all", false)?,
            inside: options.bool("inside", false)?,
        };
        options.finish()?;
        let argv = api::machines::journalctl_arguments(&request);
        let pipe = || {
            nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)
                .map_err(|e| Error::Failed(format!("creating a pipe: {e}")))
        };
        let (out_r, out_w) = pipe()?;
        let (err_r, err_w) = pipe()?;
        // A copy of the output's writing end stays here: it reports an error once nobody
        // reads, which is how a journalctl --follow learns that its client is gone
        // instead of waiting for the next line forever.
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

    /// Like `exec`: runs argv inside the machine as `user` ("" for root), and hands out
    /// its streams: with option tty (b, default true) a pseudo terminal of rows x cols
    /// (t, default 24 x 80) under "tty", otherwise pipes under "stdin", "stdout" and
    /// "stderr". Option env (as) adds variables, the caller's TERM among them (xterm
    /// otherwise, on a terminal). The process object says how it ends.
    async fn exec(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        machine: String,
        argv: Vec<String>,
        user: String,
        options: HashMap<String, OwnedValue>,
    ) -> Result<(HashMap<String, zbus::zvariant::OwnedFd>, OwnedObjectPath)> {
        let owner = self.allow(&hdr, Action::Manage).await?;
        let _busy = self.state.enter();
        let mut options = Options::new(&options);
        let tty = options.bool("tty", true)?;
        let rows = options.u64("rows", 24)?;
        let cols = options.u64("cols", 80)?;
        let env = options.strings("env")?;
        options.finish()?;
        let stdio = if tty {
            crate::nsenter::Stdio::Pty {
                rows: rows.clamp(1, u16::MAX as u64) as u16,
                cols: cols.clamp(1, u16::MAX as u64) as u16,
            }
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
        } = api::machines::spawn_in_namespaces(self.ctx(), &machine, &argv, &user, &env, stdio)
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

    /// Like `network ls`: the bridge (bridge, subnet, gateway, host_name) and the
    /// machines on it (name, address, ports, running).
    async fn list_network(&self, #[zbus(header)] hdr: Header<'_>) -> Result<(Dict, Vec<Dict>)> {
        self.allow(&hdr, Action::Inspect).await?;
        let _busy = self.state.enter();
        let (info, entries) = api::network::list(self.ctx()).await?;
        Ok((
            values::bridge(&info),
            entries.iter().map(values::network_entry).collect(),
        ))
    }

    /// Like `network up`: the bridge, plus the notes made on the way under "notes".
    async fn network_up(&self, #[zbus(header)] hdr: Header<'_>) -> Result<Dict> {
        self.allow(&hdr, Action::Manage).await?;
        let _busy = self.state.enter();
        let notes = Notes::default();
        let info = api::network::up(self.ctx(), &notes.report()).await?;
        let mut dict = values::bridge(&info);
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
        self.allow(&hdr, Action::Manage).await?;
        let _busy = self.state.enter();
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
        self.allow(&hdr, Action::Manage).await?;
        let _busy = self.state.enter();
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
