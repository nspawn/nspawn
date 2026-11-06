//! The engine's exec. POST /containers/{id}/exec keeps an instance in this server's
//! memory, and POST /exec/{id}/start runs it with the bus's Exec, as `nspawn exec` runs a
//! command: on a terminal, which carries the client's bytes in and the output out, or
//! with pipes, the client's bytes going into its input, which ends when the client
//! closes its writing half, and its standard output and error out as streams 1 and 2. A
//! start that asks for a terminal gets the output as it is, one stream; others get it in
//! frames. The connection closes once the command exited and its exit code is kept, so
//! that an inspect (GET /exec/{id}/json) right after the stream reads it. POST
//! /exec/{id}/resize sizes the terminal, or the one the start makes when it comes before
//! it.
//!
//! A command whose client left, or that was started detached, runs on: this server reads
//! what it writes and stays until it exits, for its exit code. What a process the command
//! left running writes after its end is read and dropped, and the client's stream ends
//! at most two seconds after the command's. The server keeps 256 instances at most,
//! those whose command exited for ten minutes, and stays while it keeps one.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use bytes::Bytes;
use hyper::{Method, Response, StatusCode};
use serde_json::{json, Value};
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncReadExt, Interest};
use tokio::net::unix::pipe;
use tokio::sync::mpsc::{self, Sender};
use zbus::zvariant::Value as Bus;

use super::access::Need;
use super::attach::{content_type, deliver, read_some, set_nonblocking, Input, Sink};
use super::containers::{check_json, read_body, status_of, Answer, Sent};
use super::create::{self, Object, Refusal};
use super::model::Snapshot;
use super::streams::frame;
use super::{json_response, lookup_status, respond, Body, Query};
use crate::api::Context;
use crate::client::{self, Client, Ended, Options};

/// How long an instance is kept once its command exited.
const KEPT: Duration = Duration::from_secs(600);
/// How many instances a server keeps.
const MOST: usize = 256;
/// What one read of a command's output takes at most.
const CHUNK: usize = 16 * 1024;

/// An exec request, by its method and its path without the version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    /// POST /containers/{id}/exec, of a container.
    Create(String),
    /// POST /exec/{id}/start, of an instance.
    Start(String),
    /// POST /exec/{id}/resize.
    Resize(String),
    /// GET /exec/{id}/json.
    Inspect(String),
}

impl Call {
    pub fn of(method: &Method, parts: &[&str]) -> Option<Call> {
        let post = *method == Method::POST;
        let read = *method == Method::GET || *method == Method::HEAD;
        Some(match parts {
            ["containers", id, "exec"] if post => Call::Create(id.to_string()),
            ["exec", id, "start"] if post => Call::Start(id.to_string()),
            ["exec", id, "resize"] if post => Call::Resize(id.to_string()),
            ["exec", id, "json"] if read => Call::Inspect(id.to_string()),
            _ => return None,
        })
    }

    /// What it asks of its client: the bus's Exec asks org.nspawn.manage, and so does
    /// anything done to a command it started; an inspect looks.
    pub fn need(&self) -> Need {
        match self {
            Call::Inspect(_) => Need::Inspect,
            _ => Need::Manage,
        }
    }

    /// Whether the engine reads its query before anything else, and answers one that
    /// does not decode.
    pub fn parses_form(&self) -> bool {
        !matches!(self, Call::Inspect(_))
    }
}

type Served = Result<Response<Body>, Answer>;

pub async fn serve(ctx: &Arc<Context>, call: Call, query: &Query, sent: Sent) -> Response<Body> {
    let served = match call {
        Call::Create(key) => create(ctx, &key, sent).await,
        Call::Start(id) => start(ctx, &id, sent).await,
        Call::Resize(id) => resize(ctx, &id, query).await,
        Call::Inspect(id) => inspect(ctx, &id),
    };
    served.unwrap_or_else(Answer::respond)
}

/// What an instance runs, as its create asked for it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Config {
    pub argv: Vec<String>,
    /// "" for the machine's own.
    pub user: String,
    /// Taken, and of no use: the command gets the machine's own capabilities.
    pub privileged: bool,
    pub tty: bool,
    /// The terminal's rows and columns.
    pub size: Option<(u64, u64)>,
    pub stdin: bool,
    pub stdout: bool,
    pub stderr: bool,
    pub detach_keys: String,
    pub env: Vec<String>,
    /// "" for the machine's own.
    pub workdir: String,
}

/// What a start asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StartOptions {
    pub detach: bool,
    /// Whether the output goes as it is (one stream) rather than in frames.
    pub tty: bool,
    pub size: Option<(u64, u64)>,
}

/// Whether a client of `version` may send a console size: from API 1.42 on.
fn sizes_taken(version: Option<(u32, u32)>) -> bool {
    version.is_none_or(|v| v >= (1, 42))
}

/// A request's body as the engine reads it: none at all is the zero value.
fn body_value(bytes: &[u8]) -> Result<Value, Refusal> {
    if bytes.is_empty() {
        Ok(Value::Null)
    } else {
        create::decode(bytes)
    }
}

/// The body of a create (ExecOptions), as the engine reads it.
pub fn create_options(body: &Value, version: Option<(u32, u32)>) -> Result<Config, Refusal> {
    const OWNER: &str = "container.ExecOptions";
    let object = Object::of(body, OWNER)?;
    let config = Config {
        user: object.text("User")?,
        privileged: object.flag("Privileged")?,
        tty: object.flag("Tty")?,
        size: console_size(body, OWNER)?,
        stdin: object.flag("AttachStdin")?,
        stderr: object.flag("AttachStderr")?,
        stdout: object.flag("AttachStdout")?,
        detach_keys: object.text("DetachKeys")?,
        env: object.texts("Env")?,
        workdir: object.text("WorkingDir")?,
        argv: object.texts("Cmd")?,
    };
    // Not used by the engine either, but still read as a bool.
    object.flag("Detach")?;
    if config.argv.is_empty() {
        return Err(create::bad("No exec command specified"));
    }
    Ok(Config {
        size: config.size.filter(|_| sizes_taken(version)),
        ..config
    })
}

/// The body of a start (ExecStartOptions): a size only with a terminal.
pub fn start_options(body: &Value, version: Option<(u32, u32)>) -> Result<StartOptions, Refusal> {
    const OWNER: &str = "container.ExecStartOptions";
    let object = Object::of(body, OWNER)?;
    let detach = object.flag("Detach")?;
    let tty = object.flag("Tty")?;
    let size = console_size(body, OWNER)?.filter(|_| tty && sizes_taken(version));
    Ok(StartOptions { detach, tty, size })
}

/// A `*[2]uint` ConsoleSize: None when null or missing; the first two numbers otherwise,
/// a missing one 0, as Go fills an array.
fn console_size(body: &Value, owner: &str) -> Result<Option<(u64, u64)>, Refusal> {
    let Value::Object(map) = body else {
        return Ok(None);
    };
    let value = map.get("ConsoleSize").or_else(|| {
        map.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("ConsoleSize"))
            .map(|(_, v)| v)
    });
    let wrong = |value: &Value, go_type: &str| {
        create::bad(format!(
            "invalid JSON: json: cannot unmarshal {} into Go struct field {owner}.ConsoleSize of type {go_type}",
            create::kind(value)
        ))
    };
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => {
            let mut size = [0u64; 2];
            for (slot, item) in size.iter_mut().zip(items) {
                *slot = match item {
                    Value::Null => 0,
                    Value::Number(n) => n.as_u64().ok_or_else(|| wrong(item, "uint"))?,
                    other => return Err(wrong(other, "uint")),
                };
            }
            Ok(Some((size[0], size[1])))
        }
        Some(other) => Err(wrong(other, "[2]uint")),
    }
}

/// term.ToBytes: keys separated by commas, each a single character, `ctrl-@` to
/// `ctrl-_` or `DEL`; the engine's words when one is none of them.
pub fn escape_keys(text: &str) -> Result<Vec<u8>, String> {
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let key = |key: &str| -> Option<u8> {
        if key.len() == 1 {
            return Some(key.as_bytes()[0]);
        }
        if key == "DEL" {
            return Some(127);
        }
        let named = key.strip_prefix("ctrl-").filter(|c| c.len() == 1)?;
        match named.as_bytes()[0] {
            b'@' => Some(0),
            c @ b'a'..=b'z' => Some(c - b'a' + 1),
            b'[' => Some(27),
            b'\\' => Some(28),
            b']' => Some(29),
            b'^' => Some(30),
            b'_' => Some(31),
            _ => None,
        }
    };
    text.split(',')
        .map(key)
        .collect::<Option<Vec<u8>>>()
        .ok_or_else(|| format!("Invalid escape keys ({text}) provided"))
}

/// Why a container in `state` takes no exec, in the engine's words: `id` its ID, and
/// `paused` the name a paused one goes by (the create's names it as its request did).
pub fn inactive(state: &str, id: &str, paused: &str) -> Option<Answer> {
    let text = match state {
        "running" => return None,
        "paused" => format!("Container {paused} is paused, unpause the container before exec"),
        "restarting" => {
            format!("Container {id} is restarting, wait until the container is running")
        }
        _ => format!("container {id} is not running"),
    };
    Some(Answer(StatusCode::CONFLICT, Some(text)))
}

/// What a failed Exec answers: `id` the instance's, `container` its container's ID.
pub fn exec_failed(kind: Option<&str>, id: &str, container: &str, message: &str) -> Answer {
    let state = match kind {
        Some("NoSuchMachine") => {
            return Answer(
                StatusCode::NOT_FOUND,
                Some(format!("No such container: {id}")),
            )
        }
        Some("NotRunning") => "exited",
        Some("Paused") => "paused",
        kind => return Answer(status_of(kind), Some(message.to_string())),
    };
    inactive(state, container, container)
        .unwrap_or_else(|| Answer(StatusCode::CONFLICT, Some(message.to_string())))
}

fn no_such_exec(id: &str) -> Answer {
    Answer(
        StatusCode::NOT_FOUND,
        Some(format!("No such exec instance: {id}")),
    )
}

fn internal(e: anyhow::Error) -> Answer {
    Answer(StatusCode::INTERNAL_SERVER_ERROR, Some(format!("{e:#}")))
}

/// Uint32Value of a resize: digits, or the engine's words for what they are not.
pub fn dimension(query: &Query, key: &str, word: &str) -> Result<u32, Answer> {
    let raw = query.get(key).unwrap_or_default();
    let (digits, negative) = match raw.strip_prefix('-') {
        Some(rest) => (rest, true),
        None => (raw, false),
    };
    let reason = if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        "invalid syntax"
    } else {
        match digits.parse::<u32>() {
            Ok(n) if !negative => return Ok(n),
            _ => "value out of range",
        }
    };
    Err(Answer(
        StatusCode::BAD_REQUEST,
        Some(format!("invalid resize {word} {raw:?}: {reason}")),
    ))
}

/// Where an instance is.
enum Phase {
    Created,
    Running {
        /// 0 until the command runs.
        pid: u32,
        terminal: Option<Arc<OwnedFd>>,
    },
    Exited {
        pid: u32,
        code: i32,
        at: Instant,
    },
}

/// An exec instance.
struct Instance {
    id: String,
    /// The machine it runs in, and the creation of its record, which tells the container
    /// from a later one of the same name.
    machine: String,
    created: u64,
    /// Its container's ID.
    container: String,
    config: Config,
    /// The detach keys, as bytes.
    keys: Vec<u8>,
    /// The user an inspect shows: the one asked for, else the container's.
    shown_user: String,
    made: Instant,
    /// The size of a resize, which the start's terminal gets.
    size: Option<(u64, u64)>,
    phase: Phase,
}

impl Instance {
    /// Marks it running, or the engine's words for why it does not start.
    fn begin(&mut self) -> Result<(), Answer> {
        let text = match self.phase {
            Phase::Created => {
                self.phase = Phase::Running {
                    pid: 0,
                    terminal: None,
                };
                return Ok(());
            }
            Phase::Running { .. } => format!("exec command {} is already running", self.id),
            Phase::Exited { .. } => format!("exec command {} has already run", self.id),
        };
        Err(Answer(StatusCode::CONFLICT, Some(text)))
    }

    /// The command runs as `pid`, on `terminal`, which gets the size of a resize that
    /// came meanwhile.
    fn running(&mut self, pid: u32, terminal: Option<Arc<OwnedFd>>) {
        if let (Some(master), Some((rows, cols))) = (&terminal, self.size) {
            let _ = crate::nsenter::set_terminal_size(&**master, clamp(rows), clamp(cols));
        }
        if let Phase::Running { .. } = self.phase {
            self.phase = Phase::Running { pid, terminal };
        }
    }

    fn end(&mut self, code: i32, now: Instant) {
        let pid = match self.phase {
            Phase::Running { pid, .. } | Phase::Exited { pid, .. } => pid,
            Phase::Created => 0,
        };
        self.phase = Phase::Exited { pid, code, at: now };
    }

    /// Sizes the terminal, or keeps the size for the start's; nothing without one.
    fn resize(&mut self, rows: u32, cols: u32) -> Result<(), Answer> {
        let size = (u64::from(rows), u64::from(cols));
        match &self.phase {
            Phase::Created | Phase::Running { terminal: None, .. } => {
                self.size = Some(size);
                Ok(())
            }
            Phase::Running {
                terminal: Some(master),
                ..
            } => crate::nsenter::set_terminal_size(&**master, clamp(size.0), clamp(size.1))
                .map_err(|e| {
                    Answer(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Some(format!(
                            "resizing the terminal of exec {} in {}: {e}",
                            self.id, self.machine
                        )),
                    )
                }),
            Phase::Exited { .. } => Ok(()),
        }
    }

    /// The engine's ExecInspect.
    fn json(&self) -> Value {
        let (running, code, pid) = match self.phase {
            Phase::Created => (false, None, 0),
            Phase::Running { pid, .. } => (true, None, pid),
            Phase::Exited { pid, code, .. } => (false, Some(code), pid),
        };
        let argv = &self.config.argv;
        let mut process = json!({
            "tty": self.config.tty,
            "entrypoint": argv.first().cloned().unwrap_or_default(),
            "arguments": argv.get(1..).unwrap_or_default(),
            "privileged": self.config.privileged,
        });
        if !self.shown_user.is_empty() {
            process["user"] = json!(self.shown_user);
        }
        json!({
            "ID": self.id,
            "Running": running,
            "ExitCode": code,
            "ProcessConfig": process,
            "OpenStdin": self.config.stdin,
            "OpenStderr": self.config.stderr,
            "OpenStdout": self.config.stdout,
            "CanRemove": false,
            "ContainerID": self.container,
            "DetachKeys": base64::engine::general_purpose::STANDARD.encode(&self.keys),
            "Pid": pid,
        })
    }
}

fn clamp(n: u64) -> u16 {
    u16::try_from(n).unwrap_or(u16::MAX)
}

/// The instances this server keeps.
struct Table {
    all: Vec<Instance>,
}

impl Table {
    const fn new() -> Table {
        Table { all: Vec::new() }
    }

    /// Forgets the instances whose command exited `KEPT` ago.
    fn forget(&mut self, now: Instant) {
        self.all.retain(|i| match i.phase {
            Phase::Exited { at, .. } => now.saturating_duration_since(at) < KEPT,
            _ => true,
        });
    }

    /// Keeps `instance`, in the place of the one whose command exited first, or of the
    /// oldest never started, when there are `MOST`; false when all of them run.
    fn add(&mut self, instance: Instance) -> bool {
        if self.all.len() >= MOST {
            let victim = self
                .all
                .iter()
                .enumerate()
                .filter_map(|(at, i)| match i.phase {
                    Phase::Exited { at: when, .. } => Some(((0, when), at)),
                    Phase::Created => Some(((1, i.made), at)),
                    Phase::Running { .. } => None,
                })
                .min_by_key(|(rank, _)| *rank)
                .map(|(_, at)| at);
            match victim {
                Some(at) => {
                    self.all.remove(at);
                }
                None => return false,
            }
        }
        self.all.push(instance);
        true
    }

    fn get(&mut self, id: &str) -> Option<&mut Instance> {
        self.all.iter_mut().find(|i| i.id == id)
    }

    /// Whether an instance is still to be read: running, exited less than `KEPT` ago,
    /// or made that recently and not started.
    fn keeps(&self, now: Instant) -> bool {
        self.all.iter().any(|i| match i.phase {
            Phase::Running { .. } => true,
            Phase::Exited { at, .. } => now.saturating_duration_since(at) < KEPT,
            Phase::Created => now.saturating_duration_since(i.made) < KEPT,
        })
    }
}

/// Whether this server keeps an exec instance a client may still ask for: it stays
/// until then.
pub fn keeps_instances() -> bool {
    TABLE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .keeps(Instant::now())
}

static TABLE: Mutex<Table> = Mutex::new(Table::new());

/// `f` of the table, once what has expired is gone.
fn table<R>(f: impl FnOnce(&mut Table) -> R) -> R {
    let mut table = TABLE.lock().unwrap_or_else(|e| e.into_inner());
    table.forget(Instant::now());
    f(&mut table)
}

/// `id`'s command runs as `pid`, on `terminal` if it has one.
fn mark_running(id: &str, pid: u32, terminal: Option<Arc<OwnedFd>>) {
    table(|t| {
        if let Some(instance) = t.get(id) {
            instance.running(pid, terminal);
        }
    });
}

/// Keeps the exit code of `id`'s command.
fn finish(id: &str, code: i32) {
    table(|t| {
        if let Some(instance) = t.get(id) {
            instance.end(code, Instant::now());
        }
    });
}

/// 64 random hex digits.
fn new_id() -> anyhow::Result<String> {
    use anyhow::Context as _;
    use std::io::Read as _;
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .context("reading /dev/urandom for an exec ID")?;
    Ok(hex::encode(bytes))
}

/// POST /containers/{id}/exec: checked in the engine's order, the body, the container,
/// the detach keys.
async fn create(ctx: &Arc<Context>, key: &str, sent: Sent) -> Served {
    check_json(&sent.headers)?;
    let bytes = read_body(sent.body).await?;
    let config = create_options(&body_value(&bytes)?, sent.version)?;
    let snapshot = Snapshot::take(ctx).await.map_err(internal)?;
    let machine = snapshot
        .find(key)
        .map_err(|e| Answer(lookup_status(&e), Some(e)))?;
    if let Some(refusal) = inactive(machine.state(), &machine.id, key) {
        return Err(refusal);
    }
    let keys = escape_keys(&config.detach_keys)
        .map_err(|e| Answer(StatusCode::INTERNAL_SERVER_ERROR, Some(e)))?;
    let shown_user = match config.user.as_str() {
        "" => machine.record.effective_user().unwrap_or("").to_string(),
        user => user.to_string(),
    };
    let id = new_id().map_err(internal)?;
    let instance = Instance {
        id: id.clone(),
        machine: machine.name().to_string(),
        created: machine.record.created,
        container: machine.id.clone(),
        config,
        keys,
        shown_user,
        made: Instant::now(),
        size: None,
        phase: Phase::Created,
    };
    if !table(|t| t.add(instance)) {
        return Err(Answer(
            StatusCode::SERVICE_UNAVAILABLE,
            Some(format!(
                "too many exec instances run on this socket ({MOST})"
            )),
        ));
    }
    Ok(json_response(StatusCode::CREATED, &json!({ "Id": id })))
}

/// The machine and record creation of instance `id`.
fn located(id: &str) -> Result<(String, u64), Answer> {
    table(|t| t.get(id).map(|i| (i.machine.clone(), i.created))).ok_or_else(|| no_such_exec(id))
}

/// The container of instance `id`, when it can take a command: its ID.
async fn active_container(ctx: &Context, id: &str) -> Result<String, Answer> {
    let (machine, created) = located(id)?;
    let snapshot = Snapshot::take(ctx).await.map_err(internal)?;
    let container = snapshot
        .machines
        .iter()
        .find(|m| m.name() == machine && m.record.created == created)
        .ok_or_else(|| {
            Answer(
                StatusCode::NOT_FOUND,
                Some(format!("No such container: {id}")),
            )
        })?;
    match inactive(container.state(), &container.id, &container.id) {
        Some(refusal) => Err(refusal),
        None => Ok(container.id.clone()),
    }
}

/// An instance begun whose start went before its command was watched: it ends with an
/// exit code nobody could read, rather than run forever in the table.
struct Begun<'a>(Option<&'a str>);

impl Begun<'_> {
    fn watched(mut self) {
        self.0 = None;
    }
}

impl Drop for Begun<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.0 {
            finish(id, -1);
        }
    }
}

/// POST /exec/{id}/start: the body, the instance and its container, then the command,
/// whose failure to run is answered before the stream.
async fn start(ctx: &Arc<Context>, id: &str, sent: Sent) -> Served {
    let Sent {
        headers,
        body,
        upgrade,
        version,
        holder,
    } = sent;
    check_json(&headers)?;
    let bytes = read_body(body).await?;
    let options = start_options(&body_value(&bytes)?, version)?;
    let container = active_container(ctx, id).await?;
    // Each command that runs holds a connection to the system bus and its streams.
    let place = super::EXECS.take(holder).ok_or_else(|| {
        Answer(
            StatusCode::SERVICE_UNAVAILABLE,
            Some("too many exec commands run on this socket".to_string()),
        )
    })?;
    let (machine, config, size) = table(|t| {
        let instance = t.get(id).ok_or_else(|| no_such_exec(id))?;
        instance.begin()?;
        // A resize is the latest word on the terminal's size.
        let size = instance.size.or(options.size).or(instance.config.size);
        Ok::<_, Answer>((instance.machine.clone(), instance.config.clone(), size))
    })?;
    let begun = Begun(Some(id));
    let launched = launch(&machine, &config, options.detach, size).await;
    let (client, mut fds, ended) = match launched {
        Ok(launched) => launched,
        Err(e) => {
            // As the engine has it: a command that could not run exited 126.
            begun.watched();
            finish(id, 126);
            return Err(exec_failed(
                client::error_name(&e),
                id,
                &container,
                &format!("{e:#}"),
            ));
        }
    };
    let pid = ended.pid().await.unwrap_or(0);
    let server = super::Upgraded::hold();
    let watcher = Watcher {
        id: id.to_string(),
        ended,
        client,
        server,
        place,
    };
    begun.watched();
    if options.detach {
        mark_running(id, pid, None);
        tokio::spawn(watcher.wait(None));
        return Ok(respond(StatusCode::OK, "", Bytes::new()));
    }
    let streams = match Streams::of(&mut fds, &config) {
        Ok(streams) => streams,
        Err(e) => {
            mark_running(id, pid, None);
            tokio::spawn(watcher.wait(None));
            return Err(internal(
                e.context(format!("taking the streams of exec {id} in {machine}")),
            ));
        }
    };
    mark_running(id, pid, streams.terminal.clone());
    let outlet = Outlet {
        framed: !options.tty,
        stdout: config.stdout,
        stderr: config.stderr,
    };
    let kind = content_type(options.tty, upgrade.is_some(), version);
    let (tx, rx) = mpsc::channel::<Bytes>(64);
    let input = streams.input;
    tokio::spawn(watcher.wait(Some((streams.output, outlet, tx))));
    Ok(deliver(rx, upgrade, kind, (), input))
}

/// Calls Exec for `config` in `machine`: the bus connection it was called on, which alone
/// hears of the command's end, the command's streams, and its end.
async fn launch(
    machine: &str,
    config: &Config,
    detach: bool,
    size: Option<(u64, u64)>,
) -> anyhow::Result<(Client, HashMap<String, zbus::zvariant::OwnedFd>, Ended)> {
    use anyhow::Context as _;
    let client = Client::connect().await?;
    let mut options = Options::new();
    // Sent always: the bus's default is a terminal.
    options.insert("tty", Bus::from(config.tty));
    if let (true, Some((rows, cols))) = (config.tty, size) {
        options.insert("rows", Bus::from(rows));
        options.insert("cols", Bus::from(cols));
    }
    if !config.env.is_empty() {
        options.insert("env", Bus::from(config.env.clone()));
    }
    if !config.workdir.is_empty() {
        options.insert("workdir", Bus::from(config.workdir.as_str()));
    }
    if detach {
        options.insert("detach", Bus::from(true));
    }
    let (fds, process) = client
        .manager
        .exec(machine, &config.argv, &config.user, options)
        .await
        .map_err(client::error)
        .with_context(|| format!("running {} in {machine}", config.argv[0]))?;
    let ended = Ended::watch(&client.connection, process)
        .await
        .with_context(|| format!("watching the command run in {machine}"))?;
    Ok((client, fds, ended))
}

/// Where a command's output comes from.
enum Output {
    /// Its terminal's master, read as it becomes ready.
    Terminal(AsyncFd<OwnedFd>),
    Pipes {
        stdout: pipe::Receiver,
        stderr: pipe::Receiver,
    },
}

/// What a started command hands over.
struct Streams {
    output: Output,
    /// Its input, for the client's bytes: none when the create did not attach it.
    input: Option<Input>,
    /// The terminal, for resizes.
    terminal: Option<Arc<OwnedFd>>,
}

impl Streams {
    /// The streams Exec handed over: a terminal, or pipes. An input the create did not
    /// attach is closed at once: the command reads its end.
    fn of(
        fds: &mut HashMap<String, zbus::zvariant::OwnedFd>,
        config: &Config,
    ) -> anyhow::Result<Streams> {
        let mut take = |name: &str| fds.remove(name).map(OwnedFd::from);
        if let Some(master) = take("tty") {
            set_nonblocking(&master)?;
            let terminal = Arc::new(master.try_clone()?);
            let input = if config.stdin {
                let (handing, handed) = tokio::sync::oneshot::channel();
                let _ = handing.send(master.try_clone()?);
                Some(Input {
                    sink: Sink::Terminal(handed),
                    alive: Arc::new(()),
                })
            } else {
                None
            };
            let output = Output::Terminal(AsyncFd::with_interest(master, Interest::READABLE)?);
            return Ok(Streams {
                output,
                input,
                terminal: Some(terminal),
            });
        }
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (take("stdin"), take("stdout"), take("stderr"))
        else {
            anyhow::bail!("the service returned neither a terminal nor pipes");
        };
        let input = config.stdin.then(|| Input {
            sink: Sink::Pipe(stdin),
            alive: Arc::new(()),
        });
        Ok(Streams {
            output: Output::Pipes {
                stdout: pipe::Receiver::from_owned_fd(stdout)?,
                stderr: pipe::Receiver::from_owned_fd(stderr)?,
            },
            input,
            terminal: None,
        })
    }
}

/// How a command's output goes to the client: in frames by stream unless the start
/// asked for a terminal, and only the streams the create attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outlet {
    pub framed: bool,
    pub stdout: bool,
    pub stderr: bool,
}

impl Outlet {
    pub fn chunk(self, stream: u8, bytes: &[u8]) -> Option<Bytes> {
        let wanted = if stream == 1 {
            self.stdout
        } else {
            self.stderr
        };
        wanted.then(|| {
            if self.framed {
                frame(stream, bytes)
            } else {
                Bytes::copy_from_slice(bytes)
            }
        })
    }
}

/// What a started command needs until it exits: the bus connection Exec was called on,
/// which hears of its end, this server, which keeps its exit code, and its place among
/// the commands that run.
struct Watcher {
    id: String,
    ended: Ended,
    client: Client,
    server: super::Upgraded,
    place: super::Place,
}

/// How long the output may take to reach its end once the command exited: what it wrote
/// comes at once, and a process it left running may hold the output for as long as it
/// lives.
const AFTER_EXIT: Duration = Duration::from_secs(2);

impl Watcher {
    /// Carries the command's output to the client until it ends, keeps its exit code, and
    /// only then lets the client's stream end. Without a client, or once it went, the
    /// output is read all the same: the command would block on a full pipe.
    async fn wait(self, output: Option<(Output, Outlet, Sender<Bytes>)>) {
        let Watcher {
            id,
            ended,
            client,
            server,
            place,
        } = self;
        let Some((output, outlet, tx)) = output else {
            finish(&id, ended.status().await.unwrap_or(-1));
            drop((client, server, place));
            return;
        };
        let cut = Arc::new(tokio::sync::Notify::new());
        let mut pumping = tokio::spawn(pump(output, outlet, Some(tx), cut.clone()));
        // An end the bus cannot say is no success: -1, as the engine's command line
        // reads an exit code it could not get.
        let code = ended.status().await.unwrap_or(-1);
        finish(&id, code);
        match tokio::time::timeout(AFTER_EXIT, &mut pumping).await {
            // The output's end, and with it the client's stream.
            Ok(tx) => drop(tx),
            // What still holds the output is read and dropped on its own; the stream
            // ends now.
            Err(_) => cut.notify_one(),
        }
        drop((client, server, place));
    }
}

/// Copies the command's output into `tx` until it ends, and hands `tx` back; a `tx`
/// whose client went, or that `cut` takes, is let go, and the rest read and dropped.
async fn pump(
    output: Output,
    outlet: Outlet,
    mut tx: Option<Sender<Bytes>>,
    cut: Arc<tokio::sync::Notify>,
) -> Option<Sender<Bytes>> {
    let mut first = vec![0u8; CHUNK];
    let mut second = vec![0u8; CHUNK];
    match output {
        Output::Terminal(master) => loop {
            // EIO once every process on the terminal is gone.
            let n = tokio::select! {
                n = read_some(&master, &mut first) => n.unwrap_or(0),
                () = cut.notified(), if tx.is_some() => {
                    tx = None;
                    continue;
                }
            };
            if n == 0 {
                break;
            }
            send(&mut tx, outlet.chunk(1, &first[..n])).await;
        },
        Output::Pipes { stdout, stderr } => {
            let (mut stdout, mut stderr) = (Some(stdout), Some(stderr));
            while stdout.is_some() || stderr.is_some() {
                let (stream, n) = tokio::select! {
                    n = read_from(&mut stdout, &mut first) => (1, n),
                    n = read_from(&mut stderr, &mut second) => (2, n),
                    () = cut.notified(), if tx.is_some() => {
                        tx = None;
                        continue;
                    }
                };
                if n == 0 {
                    if stream == 1 {
                        stdout = None;
                    } else {
                        stderr = None;
                    }
                    continue;
                }
                let bytes = if stream == 1 {
                    &first[..n]
                } else {
                    &second[..n]
                };
                send(&mut tx, outlet.chunk(stream, bytes)).await;
            }
        }
    }
    tx
}

/// What one read of `source` gives, 0 at its end; never for none.
async fn read_from(source: &mut Option<pipe::Receiver>, buffer: &mut [u8]) -> usize {
    match source {
        Some(pipe) => pipe.read(buffer).await.unwrap_or(0),
        None => std::future::pending().await,
    }
}

async fn send(tx: &mut Option<Sender<Bytes>>, chunk: Option<Bytes>) {
    let (Some(sender), Some(chunk)) = (tx.as_ref(), chunk) else {
        return;
    };
    if sender.send(chunk).await.is_err() {
        *tx = None;
    }
}

/// POST /exec/{id}/resize.
async fn resize(ctx: &Arc<Context>, id: &str, query: &Query) -> Served {
    let rows = dimension(query, "h", "height")?;
    let cols = dimension(query, "w", "width")?;
    active_container(ctx, id).await?;
    table(|t| match t.get(id) {
        Some(instance) => instance.resize(rows, cols),
        None => Err(no_such_exec(id)),
    })?;
    Ok(respond(StatusCode::OK, "", Bytes::new()))
}

/// GET /exec/{id}/json: none once its container is gone.
fn inspect(ctx: &Arc<Context>, id: &str) -> Served {
    let (json, machine, created) =
        table(|t| t.get(id).map(|i| (i.json(), i.machine.clone(), i.created)))
            .ok_or_else(|| no_such_exec(id))?;
    match ctx.store.load_image(&machine) {
        Ok(Some(record)) if record.created == created => Ok(json_response(StatusCode::OK, &json)),
        Ok(_) => Err(no_such_exec(id)),
        Err(e) => Err(internal(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn instance(id: &str, phase: Phase, made: Instant) -> Instance {
        Instance {
            id: id.to_string(),
            machine: "web".to_string(),
            created: 7,
            container: "c0ffee".to_string(),
            config: Config {
                argv: vec!["sh".to_string(), "-c".to_string(), "true".to_string()],
                stdout: true,
                stderr: true,
                ..Config::default()
            },
            keys: Vec::new(),
            shown_user: String::new(),
            made,
            size: None,
            phase,
        }
    }

    #[test]
    fn exec_requests_are_routed_and_ask_what_exec_asks() {
        let post = Method::POST;
        assert_eq!(
            Call::of(&post, &["containers", "web", "exec"]),
            Some(Call::Create("web".into()))
        );
        assert_eq!(
            Call::of(&post, &["exec", "ab", "start"]),
            Some(Call::Start("ab".into()))
        );
        assert_eq!(
            Call::of(&post, &["exec", "ab", "resize"]),
            Some(Call::Resize("ab".into()))
        );
        assert_eq!(
            Call::of(&Method::GET, &["exec", "ab", "json"]),
            Some(Call::Inspect("ab".into()))
        );
        assert_eq!(
            Call::of(&Method::HEAD, &["exec", "ab", "json"]),
            Some(Call::Inspect("ab".into()))
        );
        assert_eq!(Call::of(&Method::GET, &["containers", "web", "exec"]), None);
        assert_eq!(Call::of(&Method::GET, &["exec", "ab", "start"]), None);
        assert_eq!(Call::of(&post, &["exec", "ab", "json"]), None);
        assert_eq!(Call::of(&post, &["exec", "ab"]), None);
        for call in [
            Call::Create("web".into()),
            Call::Start("ab".into()),
            Call::Resize("ab".into()),
        ] {
            assert_eq!(call.need(), Need::Manage, "{call:?}");
            assert!(call.parses_form(), "{call:?}");
        }
        assert_eq!(Call::Inspect("ab".into()).need(), Need::Inspect);
        assert!(!Call::Inspect("ab".into()).parses_form());
    }

    #[test]
    fn a_create_takes_the_engine_options() {
        let body = json!({
            "User": "nobody", "Privileged": true, "Tty": true, "ConsoleSize": [40, 120],
            "AttachStdin": true, "AttachStderr": true, "AttachStdout": true,
            "DetachKeys": "ctrl-x", "Env": ["A=1"], "WorkingDir": "/srv",
            "Cmd": ["sh", "-c", "echo hi"], "Detach": false, "Unknown": {"ignored": 1},
        });
        let config = create_options(&body, Some((1, 51))).unwrap();
        assert_eq!(
            config,
            Config {
                argv: vec!["sh".into(), "-c".into(), "echo hi".into()],
                user: "nobody".into(),
                privileged: true,
                tty: true,
                size: Some((40, 120)),
                stdin: true,
                stdout: true,
                stderr: true,
                detach_keys: "ctrl-x".into(),
                env: vec!["A=1".into()],
                workdir: "/srv".into(),
            }
        );
        // Keys of any case, as Go matches them.
        let lower = create_options(&json!({"cmd": ["id"], "tty": true}), None).unwrap();
        assert_eq!(lower.argv, ["id"]);
        assert!(lower.tty);
        // A console size from API 1.42 on.
        assert_eq!(create_options(&body, Some((1, 41))).unwrap().size, None);
        assert_eq!(create_options(&body, None).unwrap().size, Some((40, 120)));
    }

    #[test]
    fn a_create_without_a_command_or_with_wrong_types_is_refused() {
        let no_command = |body: Value| create_options(&body, None).unwrap_err().message;
        assert_eq!(no_command(Value::Null), "No exec command specified");
        assert_eq!(no_command(json!({"Cmd": []})), "No exec command specified");
        assert_eq!(
            no_command(json!({"Cmd": null})),
            "No exec command specified"
        );
        let refused = |body: Value| {
            let refusal = create_options(&body, None).unwrap_err();
            assert_eq!(refusal.status, StatusCode::BAD_REQUEST);
            refusal.message
        };
        assert_eq!(
            refused(json!({"Cmd": "id"})),
            "invalid JSON: json: cannot unmarshal string into Go struct field container.ExecOptions.Cmd of type []string"
        );
        assert_eq!(
            refused(json!({"Cmd": ["id"], "Tty": "yes"})),
            "invalid JSON: json: cannot unmarshal string into Go struct field container.ExecOptions.Tty of type bool"
        );
        assert_eq!(
            refused(json!({"Cmd": ["id"], "ConsoleSize": [-1, 80]})),
            "invalid JSON: json: cannot unmarshal number -1 into Go struct field container.ExecOptions.ConsoleSize of type uint"
        );
        assert_eq!(
            refused(json!({"Cmd": ["id"], "ConsoleSize": "24x80"})),
            "invalid JSON: json: cannot unmarshal string into Go struct field container.ExecOptions.ConsoleSize of type [2]uint"
        );
        assert_eq!(
            refused(json!(["id"])),
            "invalid JSON: json: cannot unmarshal array into Go value of type container.ExecOptions"
        );
        // Go fills an array from what there is, and drops the rest.
        let size = |sizes: Value| {
            create_options(&json!({"Cmd": ["id"], "ConsoleSize": sizes}), None)
                .unwrap()
                .size
        };
        assert_eq!(size(json!([30])), Some((30, 0)));
        assert_eq!(size(json!([30, 90, 7])), Some((30, 90)));
        assert_eq!(size(json!(null)), None);
        assert_eq!(body_value(b"").unwrap(), Value::Null);
        assert_eq!(
            body_value(b"  ").unwrap_err().message,
            "invalid JSON: got EOF while reading request body"
        );
    }

    #[test]
    fn a_start_takes_a_console_size_only_with_a_terminal() {
        let start = |body: Value, version| start_options(&body, version).unwrap();
        let sized = json!({"Detach": false, "Tty": true, "ConsoleSize": [50, 132]});
        assert_eq!(
            start(sized.clone(), Some((1, 51))),
            StartOptions {
                detach: false,
                tty: true,
                size: Some((50, 132)),
            }
        );
        assert_eq!(start(sized, Some((1, 41))).size, None);
        assert_eq!(
            start(json!({"Tty": false, "ConsoleSize": [50, 132]}), None).size,
            None
        );
        assert_eq!(start(Value::Null, None), StartOptions::default());
        assert!(start(json!({"Detach": true}), None).detach);
        assert_eq!(
            start_options(&json!({"Detach": 1}), None)
                .unwrap_err()
                .message,
            "invalid JSON: json: cannot unmarshal number 1 into Go struct field container.ExecStartOptions.Detach of type bool"
        );
    }

    #[test]
    fn detach_keys_are_read_as_the_engine_reads_them() {
        assert_eq!(escape_keys(""), Ok(vec![]));
        assert_eq!(escape_keys("ctrl-p,ctrl-q"), Ok(vec![16, 17]));
        assert_eq!(
            escape_keys("a,DEL,ctrl-@,ctrl-_"),
            Ok(vec![b'a', 127, 0, 31])
        );
        assert_eq!(
            escape_keys("ctrl-[,ctrl-\\,ctrl-],ctrl-^"),
            Ok(vec![27, 28, 29, 30])
        );
        for bad in ["ctrl-P", "ctrl-pq", ",", "esc", "é"] {
            assert_eq!(
                escape_keys(bad),
                Err(format!("Invalid escape keys ({bad}) provided")),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_container_that_does_not_run_takes_no_exec() {
        let words = |state, paused| inactive(state, "c0ffee", paused).map(|a| (a.0, a.1.unwrap()));
        assert_eq!(words("running", "web"), None);
        assert_eq!(
            words("paused", "web"),
            Some((
                StatusCode::CONFLICT,
                "Container web is paused, unpause the container before exec".to_string()
            ))
        );
        assert_eq!(
            words("restarting", "web"),
            Some((
                StatusCode::CONFLICT,
                "Container c0ffee is restarting, wait until the container is running".to_string()
            ))
        );
        for state in ["created", "exited"] {
            assert_eq!(
                words(state, "web"),
                Some((
                    StatusCode::CONFLICT,
                    "container c0ffee is not running".to_string()
                )),
                "{state}"
            );
        }
        let failed = |kind| exec_failed(kind, "ab", "c0ffee", "it broke");
        assert_eq!(
            failed(Some("NoSuchMachine")),
            Answer(StatusCode::NOT_FOUND, Some("No such container: ab".into()))
        );
        assert_eq!(
            failed(Some("NotRunning")),
            Answer(
                StatusCode::CONFLICT,
                Some("container c0ffee is not running".into())
            )
        );
        assert_eq!(
            failed(Some("Paused")).1.unwrap(),
            "Container c0ffee is paused, unpause the container before exec"
        );
        assert_eq!(
            failed(Some("Invalid")),
            Answer(StatusCode::BAD_REQUEST, Some("it broke".into()))
        );
        assert_eq!(
            failed(None),
            Answer(StatusCode::INTERNAL_SERVER_ERROR, Some("it broke".into()))
        );
    }

    #[test]
    fn an_instance_starts_once() {
        let now = Instant::now();
        let mut one = instance("ab", Phase::Created, now);
        one.begin().unwrap();
        assert_eq!(
            one.begin().unwrap_err(),
            Answer(
                StatusCode::CONFLICT,
                Some("exec command ab is already running".into())
            )
        );
        one.running(4242, None);
        one.end(3, now);
        assert!(matches!(
            one.phase,
            Phase::Exited {
                pid: 4242,
                code: 3,
                ..
            }
        ));
        assert_eq!(
            one.begin().unwrap_err(),
            Answer(
                StatusCode::CONFLICT,
                Some("exec command ab has already run".into())
            )
        );
        // A late word of the command's start does not bring it back.
        one.running(4243, None);
        assert!(matches!(one.phase, Phase::Exited { pid: 4242, .. }));
    }

    #[test]
    fn exited_instances_go_after_a_while_and_the_oldest_give_way() {
        let start = Instant::now();
        let mut table = Table::new();
        assert!(table.add(instance(
            "old",
            Phase::Exited {
                pid: 1,
                code: 0,
                at: start
            },
            start
        )));
        assert!(table.add(instance("new", Phase::Created, start)));
        table.forget(start + KEPT - Duration::from_secs(1));
        assert!(table.get("old").is_some());
        table.forget(start + KEPT);
        assert!(table.get("old").is_none(), "kept ten minutes");
        assert!(table.get("new").is_some(), "never started: kept");

        // Full: an exited one goes first, the one that exited first among them, then the
        // oldest never started; running ones never.
        let mut table = Table::new();
        let at = |s: u64| start + Duration::from_secs(s);
        for n in 0..MOST - 3 {
            let running = Phase::Running {
                pid: n as u32,
                terminal: None,
            };
            assert!(table.add(instance(&format!("run{n}"), running, start)));
        }
        assert!(table.add(instance("created", Phase::Created, at(0))));
        let exited = |s| Phase::Exited {
            pid: 1,
            code: 0,
            at: at(s),
        };
        assert!(table.add(instance("late", exited(9), at(1))));
        assert!(table.add(instance("early", exited(5), at(2))));
        assert!(table.add(instance("a", Phase::Created, at(3))));
        assert!(table.get("early").is_none());
        assert!(table.get("late").is_some());
        assert!(table.add(instance("b", Phase::Created, at(4))));
        assert!(table.get("late").is_none());
        assert!(table.add(instance("c", Phase::Created, at(5))));
        assert!(table.get("created").is_none(), "the oldest never started");
        assert!(table.get("a").is_some());
        assert_eq!(table.all.len(), MOST);
        // Every one running: no room.
        let mut full = Table::new();
        for n in 0..MOST {
            let running = Phase::Running {
                pid: 0,
                terminal: None,
            };
            assert!(full.add(instance(&format!("r{n}"), running, start)));
        }
        assert!(!full.add(instance("more", Phase::Created, start)));
    }

    #[test]
    fn an_inspect_has_the_engine_shape() {
        let now = Instant::now();
        let mut one = instance("ab", Phase::Created, now);
        one.keys = vec![16, 17];
        assert_eq!(
            one.json(),
            json!({
                "ID": "ab",
                "Running": false,
                "ExitCode": null,
                "ProcessConfig": {
                    "tty": false,
                    "entrypoint": "sh",
                    "arguments": ["-c", "true"],
                    "privileged": false,
                },
                "OpenStdin": false,
                "OpenStderr": true,
                "OpenStdout": true,
                "CanRemove": false,
                "ContainerID": "c0ffee",
                "DetachKeys": "EBE=",
                "Pid": 0,
            })
        );
        one.shown_user = "app".into();
        one.begin().unwrap();
        one.running(4242, None);
        let running = one.json();
        assert_eq!(running["Running"], json!(true));
        assert_eq!(running["ExitCode"], Value::Null);
        assert_eq!(running["Pid"], json!(4242));
        assert_eq!(running["ProcessConfig"]["user"], json!("app"));
        one.end(0, now);
        let exited = one.json();
        assert_eq!(exited["Running"], json!(false));
        assert_eq!(exited["ExitCode"], json!(0));
        assert_eq!(exited["Pid"], json!(4242));
        let mut single = instance("cd", Phase::Created, now);
        single.config.argv = vec!["id".into()];
        assert_eq!(single.json()["ProcessConfig"]["arguments"], json!([]));
        assert_eq!(single.json()["DetachKeys"], json!(""));
    }

    #[test]
    fn a_resize_reads_its_numbers_as_the_engine_does() {
        let resize = |raw: &str| dimension(&super::super::query(raw), "h", "height");
        assert_eq!(resize("h=40"), Ok(40));
        assert_eq!(resize("h=007"), Ok(7));
        for (raw, text) in [
            ("", "invalid resize height \"\": invalid syntax"),
            ("h=x", "invalid resize height \"x\": invalid syntax"),
            ("h=%2B4", "invalid resize height \"+4\": invalid syntax"),
            // A plus in a query is a space.
            ("h=+4", "invalid resize height \" 4\": invalid syntax"),
            ("h=-", "invalid resize height \"-\": invalid syntax"),
            ("h=-4", "invalid resize height \"-4\": value out of range"),
            ("h=-0", "invalid resize height \"-0\": value out of range"),
            (
                "h=4294967296",
                "invalid resize height \"4294967296\": value out of range",
            ),
        ] {
            assert_eq!(
                resize(raw),
                Err(Answer(StatusCode::BAD_REQUEST, Some(text.to_string()))),
                "{raw}"
            );
        }
    }

    #[test]
    fn a_resize_before_the_terminal_is_kept_for_it() {
        let now = Instant::now();
        let mut one = instance("ab", Phase::Created, now);
        one.resize(50, 132).unwrap();
        assert_eq!(one.size, Some((50, 132)));
        one.begin().unwrap();
        // Between the start and the terminal it is kept as well, for the terminal.
        one.resize(51, 133).unwrap();
        assert_eq!(one.size, Some((51, 133)));
        let pty = nix::pty::openpty(None, None).unwrap();
        one.running(9, Some(Arc::new(pty.master)));
        one.resize(30, 100).unwrap();
        one.end(0, now);
        // Without a terminal there is nothing to size.
        one.resize(10, 10).unwrap();
        assert!(matches!(one.phase, Phase::Exited { pid: 9, .. }));
    }

    fn pipe() -> (pipe::Receiver, std::fs::File) {
        let (read, write) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC).unwrap();
        (
            pipe::Receiver::from_owned_fd(read).unwrap(),
            std::fs::File::from(write),
        )
    }

    async fn frames_of(outlet: Outlet) -> Vec<Bytes> {
        let (stdout, mut out) = pipe();
        let (stderr, mut err) = pipe();
        out.write_all(b"to stdout\n").unwrap();
        err.write_all(b"to stderr\n").unwrap();
        drop((out, err));
        let (tx, mut rx) = mpsc::channel(8);
        let tx = pump(
            Output::Pipes { stdout, stderr },
            outlet,
            Some(tx),
            Default::default(),
        )
        .await;
        assert!(tx.is_some(), "handed back until the exit code is kept");
        drop(tx);
        let mut got = Vec::new();
        while let Some(chunk) = rx.recv().await {
            got.push(chunk);
        }
        got.sort();
        got
    }

    #[tokio::test]
    async fn without_a_terminal_stdout_and_stderr_are_streams_of_their_own() {
        let both = Outlet {
            framed: true,
            stdout: true,
            stderr: true,
        };
        assert_eq!(
            frames_of(both).await,
            [frame(1, b"to stdout\n"), frame(2, b"to stderr\n")]
        );
        let raw = Outlet {
            framed: false,
            ..both
        };
        assert_eq!(
            frames_of(raw).await,
            [
                Bytes::from_static(b"to stderr\n"),
                Bytes::from_static(b"to stdout\n")
            ]
        );
        let no_stderr = Outlet {
            stderr: false,
            ..both
        };
        assert_eq!(frames_of(no_stderr).await, [frame(1, b"to stdout\n")]);
    }

    #[tokio::test]
    async fn output_is_read_to_its_end_after_the_client_went() {
        let (stdout, mut out) = pipe();
        let (stderr, err) = pipe();
        drop(err);
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let writer = std::thread::spawn(move || {
            // More than a pipe holds: it ends only when read.
            out.write_all(&vec![b'x'; 1 << 20]).unwrap();
        });
        let outlet = Outlet {
            framed: true,
            stdout: true,
            stderr: true,
        };
        let tx = pump(
            Output::Pipes { stdout, stderr },
            outlet,
            Some(tx),
            Default::default(),
        )
        .await;
        assert!(tx.is_none());
        writer.join().unwrap();
    }

    #[tokio::test]
    async fn output_a_process_left_behind_holds_is_cut_from_the_client_and_read_on() {
        // A process the command left running holds the write ends.
        let (stdout, mut out) = pipe();
        let (stderr, err) = pipe();
        let (tx, mut rx) = mpsc::channel(8);
        let outlet = Outlet {
            framed: false,
            stdout: true,
            stderr: true,
        };
        let cut = Arc::new(tokio::sync::Notify::new());
        let pumping = tokio::spawn(pump(
            Output::Pipes { stdout, stderr },
            outlet,
            Some(tx),
            cut.clone(),
        ));
        out.write_all(b"before").unwrap();
        assert_eq!(&rx.recv().await.unwrap()[..], b"before");
        cut.notify_one();
        assert!(rx.recv().await.is_none(), "the client's stream ends");
        let writer = std::thread::spawn(move || {
            out.write_all(&vec![b'x'; 1 << 20]).unwrap();
            drop((out, err));
        });
        assert!(pumping.await.unwrap().is_none());
        writer.join().unwrap();
    }

    #[test]
    fn the_server_stays_while_it_keeps_an_instance() {
        let now = Instant::now();
        let mut table = Table::new();
        assert!(!table.keeps(now));
        table.add(instance(
            "exited",
            Phase::Exited {
                pid: 1,
                code: 0,
                at: now,
            },
            now,
        ));
        assert!(table.keeps(now));
        assert!(!table.keeps(now + KEPT));
        table.add(instance("made", Phase::Created, now));
        assert!(!table.keeps(now + KEPT));
        table.add(instance(
            "running",
            Phase::Running {
                pid: 2,
                terminal: None,
            },
            now,
        ));
        assert!(table.keeps(now + KEPT * 2));
    }

    #[tokio::test]
    async fn a_terminal_carries_its_output_as_it_is_to_the_end() {
        let pty = nix::pty::openpty(None, None).unwrap();
        set_nonblocking(&pty.master).unwrap();
        let master = AsyncFd::with_interest(pty.master, Interest::READABLE).unwrap();
        let mut slave = std::fs::File::from(pty.slave);
        let mut raw = nix::sys::termios::tcgetattr(&slave).unwrap();
        nix::sys::termios::cfmakeraw(&mut raw);
        nix::sys::termios::tcsetattr(&slave, nix::sys::termios::SetArg::TCSANOW, &raw).unwrap();
        slave.write_all(b"hello").unwrap();
        drop(slave);
        let (tx, mut rx) = mpsc::channel(8);
        let outlet = Outlet {
            framed: false,
            stdout: true,
            stderr: true,
        };
        let tx = pump(
            Output::Terminal(master),
            outlet,
            Some(tx),
            Default::default(),
        )
        .await;
        drop(tx);
        let mut got = Vec::new();
        while let Some(chunk) = rx.recv().await {
            got.extend_from_slice(&chunk);
        }
        assert_eq!(got, b"hello");
    }
}
