//! org.nspawn.Manager at /org/nspawn: everything the command line does, as methods.
//! Dictionaries (a{sv}) carry the results; the long operations come back as jobs.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue};

use crate::api::{self, Context, Report};
use crate::auth::Credentials;
use crate::backend::BackendChoice;
use crate::daemon::jobs::{self, Dict};
use crate::daemon::processes;
use crate::daemon::values::{self, Options};
use crate::daemon::State;
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
    async fn list_images(&self) -> Result<Vec<Dict>> {
        let _busy = self.state.enter();
        let images = api::images::list(self.ctx()).await?;
        Ok(images.iter().map(values::image).collect())
    }

    /// Everything nspawn keeps about one image: reference, digest, backend, mode,
    /// network, address, ports, volumes, env, entrypoint, cmd, command, and the OCI
    /// config's image_env, working_dir, user and stop_signal.
    async fn get_image(&self, name: String) -> Result<Dict> {
        let _busy = self.state.enter();
        let record = self
            .ctx()
            .store
            .load_image(&name)?
            .ok_or_else(|| Error::Failed(format!("no image named {name}")))?;
        Ok(values::record(&record))
    }

    /// Like `pull`. Options: name (s), backend (s), mode (s), force (b). The job's
    /// result carries name, reference and mode.
    async fn pull_image(
        &self,
        reference: String,
        options: HashMap<String, OwnedValue>,
    ) -> Result<OwnedObjectPath> {
        let _busy = self.state.enter();
        let mut options = Options::new(&options);
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
    /// entrypoint (s), env (as), volume (as), command (as).
    async fn create_machine(
        &self,
        source: String,
        name: String,
        options: HashMap<String, OwnedValue>,
    ) -> Result<OwnedObjectPath> {
        let _busy = self.state.enter();
        let mut options = Options::new(&options);
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

    /// Like `push`. Options: to (s). The job's result carries destination and url.
    async fn push_image(
        &self,
        image: String,
        options: HashMap<String, OwnedValue>,
    ) -> Result<OwnedObjectPath> {
        let _busy = self.state.enter();
        let mut options = Options::new(&options);
        let request = api::push::PushRequest {
            image: image.clone(),
            to: options.string("to")?,
        };
        options.finish()?;
        Ok(jobs::spawn(
            &self.state,
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
    /// backend (s), mode (s), force (b), keep_output (b), mkosi_args (as). mkosi's own
    /// output goes to the service's log, not to the job.
    async fn build_image(
        &self,
        directory: String,
        tag: String,
        options: HashMap<String, OwnedValue>,
    ) -> Result<OwnedObjectPath> {
        let _busy = self.state.enter();
        let mut options = Options::new(&options);
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

    /// Like `images rm`: what was removed and freed, line by line.
    async fn remove_images(&self, names: Vec<String>) -> Result<Vec<String>> {
        let _busy = self.state.enter();
        let lines = std::sync::Mutex::new(Vec::new());
        let report = |event: api::Event| {
            let text = match event {
                api::Event::Line(t) | api::Event::Note(t) => t,
            };
            lines.lock().unwrap().push(text);
        };
        let outcome = api::images::remove(self.ctx(), &names, &report).await;
        let lines = lines.into_inner().unwrap();
        for line in &lines {
            if let Some(name) = line.strip_prefix("removed ") {
                if let Ok(emitter) = self.state.emitter() {
                    let _ = Manager::image_removed(&emitter, name).await;
                }
            }
        }
        outcome?;
        Ok(lines)
    }

    /// Like `search`: source "" (both), "hub" or "dockerhub"; limit per source.
    async fn search_images(&self, term: String, source: String, limit: u32) -> Result<Vec<Dict>> {
        let _busy = self.state.enter();
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
        let quiet = |_: api::Event| {};
        let report: Report<'_> = &quiet;
        let hits = api::search::search(self.ctx(), &term, source, limit as usize, report).await?;
        Ok(hits.iter().map(values::hit).collect())
    }

    /// Like `hub ls`: repositories containing `filter` ("" for all), with their tags when
    /// asked.
    async fn list_repositories(&self, filter: String, with_tags: bool) -> Result<Vec<Dict>> {
        let _busy = self.state.enter();
        let filter = if filter.is_empty() {
            None
        } else {
            Some(filter.as_str())
        };
        let repos = api::hub::repositories(self.ctx(), filter, with_tags).await?;
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

    /// Like `hub tags`.
    async fn list_tags(&self, repository: String) -> Result<Vec<String>> {
        let _busy = self.state.enter();
        Ok(api::hub::tags(self.ctx(), &repository).await?)
    }

    /// Like `ps` (and `ps -a` with `all`): every machine with its state, started time,
    /// leader, os and, when nspawn installed its image, the image's record and
    /// machine_path, its object in machined.
    async fn list_machines(&self, all: bool) -> Result<Vec<Dict>> {
        let _busy = self.state.enter();
        let machines = api::machines::list(self.ctx(), all).await?;
        Ok(machines.iter().map(values::machine).collect())
    }

    /// Like `start`. Options: wait (b, default true), network (s), publish (as),
    /// entrypoint (s), env (as), volume (as), image_command (b), command (as). Returns
    /// "started", or "ended" when the program returned before the machine registered.
    async fn start_machine(
        &self,
        name: String,
        options: HashMap<String, OwnedValue>,
    ) -> Result<String> {
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
        let quiet = |_: api::Event| {};
        let report: Report<'_> = &quiet;
        Ok(
            match api::machines::start(self.ctx(), &request, report).await? {
                api::machines::StartOutcome::Started => "started",
                api::machines::StartOutcome::Ended => "ended",
            }
            .to_string(),
        )
    }

    /// Like `stop`. Options: force (b), wait (b, default true), timeout (t, seconds
    /// before SIGKILL for an app, default 10). Returns "stopped" or "was-not-running".
    async fn stop_machine(
        &self,
        name: String,
        options: HashMap<String, OwnedValue>,
    ) -> Result<String> {
        let _busy = self.state.enter();
        let mut options = Options::new(&options);
        let request = api::machines::StopRequest {
            name,
            force: options.bool("force", false)?,
            wait: options.bool("wait", true)?,
            timeout: options.u64("timeout", 10)?,
        };
        options.finish()?;
        let quiet = |_: api::Event| {};
        let report: Report<'_> = &quiet;
        Ok(
            match api::machines::stop(self.ctx(), &request, report).await? {
                api::machines::StopOutcome::Stopped => "stopped",
                api::machines::StopOutcome::WasNotRunning => "was-not-running",
            }
            .to_string(),
        )
    }

    /// Like `logs`: a pipe from which the lines come, journalctl behind it. Options:
    /// follow (b), lines (t), since (s), timestamps (b), all (b), inside (b).
    async fn logs(
        &self,
        machine: String,
        options: HashMap<String, OwnedValue>,
    ) -> Result<zbus::zvariant::OwnedFd> {
        let _busy = self.state.enter();
        let mut options = Options::new(&options);
        let request = api::machines::LogsRequest {
            machine,
            follow: options.bool("follow", false)?,
            lines: options.u64("lines", 0)?.try_into().ok().filter(|n| *n > 0),
            since: options.string("since")?,
            timestamps: options.bool("timestamps", false)?,
            all: options.bool("all", false)?,
            inside: options.bool("inside", false)?,
        };
        options.finish()?;
        let argv = api::machines::journalctl_arguments(&request);
        let (read, write) =
            nix::unistd::pipe().map_err(|e| Error::Failed(format!("creating a pipe: {e}")))?;
        let mut child = tokio::process::Command::new("journalctl")
            .args(&argv)
            .stdin(Stdio::null())
            .stdout(Stdio::from(
                write
                    .try_clone()
                    .map_err(|e| Error::Failed(e.to_string()))?,
            ))
            .stderr(Stdio::from(write))
            .spawn()
            .map_err(|e| Error::Failed(format!("running journalctl: {e}")))?;
        tokio::spawn(async move {
            let _ = child.wait().await;
        });
        Ok(zbus::zvariant::OwnedFd::from(read))
    }

    /// Like `exec`: runs argv inside the machine as `user` ("" for root), and hands out
    /// its streams: with option tty (b, default true) a pseudo terminal of rows x cols
    /// (u, default 24 x 80) under "tty", otherwise pipes under "stdin", "stdout" and
    /// "stderr". Option env (as) adds variables. The process object says how it ends.
    async fn exec(
        &self,
        machine: String,
        argv: Vec<String>,
        user: String,
        options: HashMap<String, OwnedValue>,
    ) -> Result<(HashMap<String, zbus::zvariant::OwnedFd>, OwnedObjectPath)> {
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
        let mut process =
            api::machines::spawn_in_namespaces(self.ctx(), &machine, &argv, &user, &env, stdio)
                .await?;
        let path = processes::register(&self.state, &machine, &argv, &process).await?;
        let mut fds = HashMap::new();
        if let Some(master) = process.master.take() {
            fds.insert("tty".to_string(), zbus::zvariant::OwnedFd::from(master));
        }
        if let Some(fd) = process.stdin.take() {
            fds.insert("stdin".to_string(), zbus::zvariant::OwnedFd::from(fd));
        }
        if let Some(fd) = process.stdout.take() {
            fds.insert("stdout".to_string(), zbus::zvariant::OwnedFd::from(fd));
        }
        if let Some(fd) = process.stderr.take() {
            fds.insert("stderr".to_string(), zbus::zvariant::OwnedFd::from(fd));
        }
        Ok((fds, path))
    }

    /// Like `network ls`: the bridge (bridge, subnet, gateway, host_name) and the
    /// machines on it (name, address, ports, running).
    async fn list_network(&self) -> Result<(Dict, Vec<Dict>)> {
        let _busy = self.state.enter();
        let (info, entries) = api::network::list(self.ctx()).await?;
        Ok((
            values::bridge(&info),
            entries.iter().map(values::network_entry).collect(),
        ))
    }

    /// Like `network up`.
    async fn network_up(&self) -> Result<Dict> {
        let _busy = self.state.enter();
        let info = api::network::up(self.ctx()).await?;
        Ok(values::bridge(&info))
    }

    /// Like `login`: registry "" for the hub. Returns registry, username and whether
    /// the registry asked for credentials.
    async fn login(&self, registry: String, username: String, password: String) -> Result<Dict> {
        let _busy = self.state.enter();
        let registry = if registry.is_empty() {
            None
        } else {
            Some(registry)
        };
        let done =
            api::login::login(self.ctx(), registry, Credentials { username, password }).await?;
        Ok(HashMap::from([
            ("registry".to_string(), values::v(done.registry)),
            ("username".to_string(), values::v(done.username)),
            ("asked".to_string(), values::v(done.asked)),
        ]))
    }

    /// Like `logout`: true when credentials were stored for the registry.
    async fn logout(&self, registry: String) -> Result<bool> {
        let _busy = self.state.enter();
        let registry = if registry.is_empty() {
            None
        } else {
            Some(registry)
        };
        Ok(api::login::logout(self.ctx(), registry)?.removed)
    }

    /// A line a job said.
    #[zbus(signal)]
    pub async fn job_output(
        emitter: &SignalEmitter<'_>,
        job: ObjectPath<'_>,
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
