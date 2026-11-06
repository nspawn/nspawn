//! POST /containers/{id}/attach: what a container writes from the attach on (from its
//! creation with `logs`), until its run ends, and its input from its next start on. The
//! output is the journal's, followed as an attached run follows it, so that it comes
//! whoever starts the machine; the end is a WaitMachine taken before the answer, so that
//! a start sent right after it is not missed. A client that asks for an upgrade gets the
//! connection with 101, as the engine's command line wants it; others read the stream as
//! the body of a 200.
//!
//! The input of a container made with OpenStdin is kept for the next start this server
//! makes of it (`start_with_input`), which runs the machine with RunMachine: the client's
//! bytes go into a pipe the program reads, closed when the client closes its writing half
//! or hangs up, so that the program reads the end of its input; with a terminal (Tty) the
//! attach carries the terminal's master both ways instead of the journal. What the run
//! needs stays in this server until it ends (`hold_run`): the bus connection RunMachine
//! was called on, whose end would stop a run with a terminal, the terminal for resizes,
//! and an input that outlives its client (no StdinOnce).

use std::collections::HashMap;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use bytes::Bytes;
use hyper::upgrade::OnUpgrade;
use hyper::{Response, StatusCode};
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, Interest, Ready};
use tokio::net::UnixStream;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::{oneshot, watch};
use zbus::zvariant::{OwnedObjectPath, Value as Bus};

use super::access::Need;
use super::containers::status_of;
use super::model::Snapshot;
use super::streams::{frame, LogsQuery};
use super::{error, lookup_status, Body, Query};
use crate::api::machines::{prepare_logs, LogsRequest};
use crate::api::run::{ensure_journal_namespace, journal_cursor, FollowFrom, Follower};
use crate::api::Context;
use crate::client::{self, Client, Ended, JobWatch, Options};

/// Where an attach is: following a run, handing over what is left once the run ended,
/// or done (its client has all of it, or went).
const FOLLOWING: u8 = 0;
const DRAINING: u8 = 1;
const DONE: u8 = 2;

/// This server's attaches under way, by machine: a wait of the machine answers once
/// those of the run that ended are done, as the engine has the output out before the
/// exit code.
static ATTACHES: Mutex<Vec<(String, watch::Receiver<u8>)>> = Mutex::new(Vec::new());

/// An attach in the registry, done when this goes.
struct Attachment(Arc<watch::Sender<u8>>);

impl Drop for Attachment {
    fn drop(&mut self) {
        self.0.send_replace(DONE);
    }
}

fn register(name: &str) -> (Attachment, Arc<watch::Sender<u8>>) {
    let (state, seen) = watch::channel(FOLLOWING);
    let state = Arc::new(state);
    let mut all = ATTACHES.lock().unwrap();
    all.retain(|(_, seen)| live(seen));
    all.push((name.to_string(), seen));
    (Attachment(state.clone()), state)
}

/// An attach not done whose attach is still there to say so.
fn live(seen: &watch::Receiver<u8>) -> bool {
    *seen.borrow() != DONE && seen.has_changed().is_ok()
}

/// Marks the run an attach follows as ended, unless its client went meanwhile: done
/// stays done.
fn run_ended(state: &watch::Sender<u8>) {
    state.send_if_modified(|now| {
        let following = *now == FOLLOWING;
        if following {
            *now = DRAINING;
        }
        following
    });
}

/// Waits for the attaches of `name` to hand over what the run that just ended wrote. One
/// that is still following a moment after the end waits for another run, and is not
/// waited for.
pub async fn drained(name: &str) {
    let pending: Vec<watch::Receiver<u8>> = ATTACHES
        .lock()
        .unwrap()
        .iter()
        .filter(|(of, seen)| of == name && live(seen))
        .map(|(_, seen)| seen.clone())
        .collect();
    for mut seen in pending {
        let _ = tokio::time::timeout(
            Duration::from_secs(1),
            seen.wait_for(|state| *state != FOLLOWING),
        )
        .await;
        if *seen.borrow() == DRAINING {
            let gone = matches!(
                tokio::time::timeout(
                    DRAIN + Duration::from_secs(5),
                    seen.wait_for(|state| *state == DONE),
                )
                .await,
                Ok(Err(_))
            );
            // Its attach went without a word: nothing of it is coming.
            if gone {
                continue;
            }
            // Done once its connection has the last bytes, which it writes out a moment
            // later.
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

/// How long an attach reads what is left once the run ended: journalctl may be behind a
/// long output, which the client reads to its end.
const DRAIN: Duration = Duration::from_secs(30);

/// The stream's content type: the engine names its frames only on an upgraded
/// connection, of API 1.42 and later.
pub fn content_type(tty: bool, upgraded: bool, version: Option<(u32, u32)>) -> &'static str {
    if !tty && upgraded && version.is_none_or(|v| v >= (1, 42)) {
        "application/vnd.docker.multiplexed-stream"
    } else {
        "application/vnd.docker.raw-stream"
    }
}

/// How the output goes out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// As it is, for a container with a terminal, which has one stream.
    Raw,
    /// In frames of the first stream asked for: the console carries both as one.
    Framed(u8),
    /// Not at all: no stream was asked for.
    Nothing,
}

impl Shape {
    pub fn of(tty: bool, stdout: bool, stderr: bool) -> Shape {
        match (stdout, stderr) {
            (false, false) => Shape::Nothing,
            _ if tty => Shape::Raw,
            (true, _) => Shape::Framed(1),
            (false, true) => Shape::Framed(2),
        }
    }

    pub fn chunk(self, bytes: &[u8]) -> Option<Bytes> {
        match self {
            Shape::Raw => Some(Bytes::copy_from_slice(bytes)),
            Shape::Framed(stream) => Some(frame(stream, bytes)),
            Shape::Nothing => None,
        }
    }
}

/// What the engine answers an attach to a container in `state` with, when it refuses it.
pub fn refusal(state: &str, key: &str) -> Option<String> {
    match state {
        "paused" => Some(format!(
            "container {key} is paused, unpause the container before attach"
        )),
        "restarting" => Some(format!(
            "container {key} is restarting, wait until the container is running"
        )),
        _ => None,
    }
}

/// What an attach does with the input its client asked to send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputPlan {
    /// The output alone: no input was asked for, or the program takes none (no
    /// OpenStdin, a booted machine, which has a console instead), or there is no stream
    /// or no connection of the client's own to read it from.
    Ignored,
    /// Kept for the container's next start, which runs it with the input.
    Kept,
    /// The container runs, and its input was not kept.
    Refused,
}

/// The input plan of an attach: `asked` its stdin, `stream` and `upgraded` how it
/// attaches, `interactive` the container's OpenStdin, `app` an app machine, `running`
/// whether it runs.
pub fn input_plan(
    asked: bool,
    stream: bool,
    upgraded: bool,
    interactive: bool,
    app: bool,
    running: bool,
) -> InputPlan {
    if !asked || !stream || !interactive || !app {
        InputPlan::Ignored
    } else if running {
        InputPlan::Refused
    } else if !upgraded {
        InputPlan::Ignored
    } else {
        InputPlan::Kept
    }
}

/// The words of `InputPlan::Refused`.
pub const INPUT_REFUSED: &str = "attaching to the input of a running container is not supported by nspawn's Docker API: a container's input is taken by an attach made before its start";

pub async fn attach(
    ctx: &Arc<Context>,
    key: &str,
    query: &Query,
    upgrade: Option<OnUpgrade>,
    version: Option<(u32, u32)>,
    holder: super::access::Holder,
    access: &super::access::Client,
) -> Response<Body> {
    let snapshot = match Snapshot::take(ctx).await {
        Ok(snapshot) => snapshot,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    let machine = match snapshot.find(key) {
        Ok(machine) => machine,
        Err(e) => return error(lookup_status(&e), e),
    };
    if let Some(text) = refusal(machine.state(), key) {
        return error(StatusCode::CONFLICT, text);
    }
    let record = &machine.record;
    if record.tuning.drops_output() {
        return error(
            StatusCode::NOT_IMPLEMENTED,
            "attaching to a container whose logging driver is none is not supported by nspawn's Docker API",
        );
    }
    let (logs, stream) = (query.flag("logs", false), query.flag("stream", false));
    let plan_while = |running: bool| {
        input_plan(
            query.flag("stdin", false),
            stream,
            upgrade.is_some(),
            record.interactive,
            record.mode == crate::oci::Mode::App,
            running,
        )
    };
    let mut plan = plan_while(machine.running());
    // A run whose program ended, its machine going away: the client that waited for that
    // end attaches for the next run.
    if plan == InputPlan::Refused && program_ended(ctx, machine.name()).await {
        plan = plan_while(false);
    }
    match plan {
        InputPlan::Refused => return error(StatusCode::NOT_IMPLEMENTED, INPUT_REFUSED),
        // The input goes to the program, a change a read-only socket does not make.
        InputPlan::Kept => {
            if let Err((status, text)) = access.allows(ctx, Need::Manage).await {
                return error(status, text);
            }
        }
        InputPlan::Ignored => {}
    }
    let shape = Shape::of(
        record.tty,
        query.flag("stdout", false),
        query.flag("stderr", false),
    );
    let kind = content_type(record.tty, upgrade.is_some(), version);
    let name = machine.name().to_string();
    let created = record.created;
    let namespace = record.tuning.journal_namespace();
    let (tty, stdin_once) = (record.tty, record.stdin_once);
    drop(snapshot);
    let Some(permit) = super::STREAMS.take(holder) else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "too many streams are open on this socket",
        );
    };
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(64);
    if stream {
        let Some(waiting) = super::WAITS.take(holder) else {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "too many waits are open on this socket",
            );
        };
        if plan == InputPlan::Kept && tty {
            // The terminal's master carries the output, from the start on.
            let (attachment, state) = register(&name);
            let alive = Arc::new(());
            let (master, handed) = oneshot::channel();
            keep_input(
                &name,
                Pending {
                    tty: true,
                    stdin: None,
                    held: None,
                    output: Some(RunOutput { tx, shape, state }),
                    master: Some(master),
                    attach: Arc::downgrade(&alive),
                },
            );
            let input = Input {
                sink: Sink::Terminal(handed),
                alive,
            };
            return deliver(
                rx,
                upgrade,
                kind,
                (attachment, permit, waiting),
                Some(input),
            );
        }
        if plan == InputPlan::Kept {
            // The run's output comes on a pipe of its own, as the program writes it, from
            // the start on: the journal drops empty lines.
            let (attachment, state) = register(&name);
            let (pending, sink) = match input_pipe(stdin_once) {
                Ok(made) => made,
                Err(e) => {
                    return error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("the input of {name}: {e:#}"),
                    )
                }
            };
            let alive = Arc::new(());
            keep_input(
                &name,
                Pending {
                    output: Some(RunOutput { tx, shape, state }),
                    attach: Arc::downgrade(&alive),
                    ..pending
                },
            );
            let input = Input { sink, alive };
            return deliver(
                rx,
                upgrade,
                kind,
                (attachment, permit, waiting),
                Some(input),
            );
        }
        let from = logs.then(|| created.saturating_mul(1_000_000));
        let follow = match Follow::start(ctx, &name, namespace, from, key).await {
            Ok(follow) => follow,
            Err((status, text)) => return error(status, text),
        };
        let (attachment, state) = register(&name);
        tokio::spawn(async move {
            let _held = (permit, waiting);
            follow.pump(shape, tx, state).await;
        });
        return deliver(rx, upgrade, kind, attachment, None);
    } else if logs && shape != Shape::Nothing {
        // What it wrote so far, and the end.
        let mut request = LogsRequest {
            machine: name,
            json: true,
            ..Default::default()
        };
        if let Err(e) = prepare_logs(ctx, &mut request).await {
            return error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
        }
        let query = LogsQuery {
            stream: match shape {
                Shape::Framed(stream) => stream,
                _ => 1,
            },
            raw: shape == Shape::Raw,
            request,
        };
        tokio::spawn(async move {
            let _held = permit;
            super::streams::logs(query, tx).await;
        });
    }
    deliver(rx, upgrade, kind, (), None)
}

/// The pipe of a program's input without a terminal: what the start takes, and the
/// attach's end. Without StdinOnce a copy of the attach's end stays with the run, so
/// that the input outlives the client's.
fn input_pipe(stdin_once: bool) -> anyhow::Result<(Pending, Sink)> {
    use anyhow::Context as _;
    let (read, write) =
        nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC).context("creating the input's pipe")?;
    let held = if stdin_once {
        None
    } else {
        Some(write.try_clone().context("keeping the input's pipe")?)
    };
    let pending = Pending {
        tty: false,
        stdin: Some(read),
        held,
        output: None,
        master: None,
        attach: Weak::new(),
    };
    Ok((pending, Sink::Pipe(write)))
}

/// The stream to the client: on the connection it gave up, or as a body; `held` goes once
/// it is written. On the connection the client's input goes to `input`.
pub(super) fn deliver<H: Send + Sync + 'static>(
    rx: Receiver<Bytes>,
    upgrade: Option<OnUpgrade>,
    kind: &str,
    held: H,
    input: Option<Input>,
) -> Response<Body> {
    let Some(upgrade) = upgrade else {
        return super::streaming_rx(rx, kind, held);
    };
    // Counted from now: the connection leaves hyper's hands with the 101.
    let counted = super::Upgraded::hold();
    tokio::spawn(async move {
        let _counted = counted;
        let Ok(upgraded) = upgrade.await else { return };
        match upgraded.downcast::<hyper_util::rt::TokioIo<UnixStream>>() {
            Ok(parts) => {
                let stream = parts.io.into_inner();
                let hangup = hangup_watch(&stream);
                let (reader, writer) = stream.into_split();
                carry(reader, parts.read_buf, writer, rx, hangup, input, held).await;
            }
            Err(upgraded) => {
                let (reader, writer) = tokio::io::split(hyper_util::rt::TokioIo::new(upgraded));
                carry(reader, Bytes::new(), writer, rx, None, input, held).await;
            }
        }
    });
    let mut response = super::respond(StatusCode::SWITCHING_PROTOCOLS, kind, Bytes::new());
    let headers = response.headers_mut();
    headers.insert("Connection", "Upgrade".parse().unwrap());
    headers.insert("Upgrade", "tcp".parse().unwrap());
    response
}

/// An attach's connection both ways: the output until its end, which ends the attach,
/// and the client's input, `early` first (what came along with the request), until the
/// client's writing half closes.
async fn carry<R, W, H>(
    mut reader: R,
    early: Bytes,
    writer: W,
    rx: Receiver<Bytes>,
    hangup: Option<AsyncFd<OwnedFd>>,
    input: Option<Input>,
    held: H,
) where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let out = write_out(writer, rx, hangup);
    match input {
        None => out.await,
        Some(Input { sink, alive }) => {
            let _alive = alive;
            tokio::pin!(out);
            let fed = feed(sink, early, &mut reader);
            tokio::pin!(fed);
            let mut feeding = true;
            loop {
                tokio::select! {
                    () = &mut out => break,
                    () = &mut fed, if feeding => feeding = false,
                }
            }
        }
    }
    // The output is handed over: a wait answers now, while the client, which closes
    // once it has the exit code, may still be writing.
    drop(held);
    discard(&mut reader).await;
}

/// Reads and drops what the client still sends once the output ended, until it closes
/// the connection, for a few seconds at most: a connection closed with input unread is
/// reset, and the client would lose the end of the output with it. Its input outlives
/// the program's when that reads none of it.
async fn discard<R: AsyncRead + Unpin>(reader: &mut R) {
    let mut buffer = vec![0u8; 16 * 1024];
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        while let Ok(n) = reader.read(&mut buffer).await {
            if n == 0 {
                break;
            }
        }
    })
    .await;
}

/// Writes what `rx` receives to the client until the senders are gone, or the client
/// hung up: then `rx` goes, and with it the follow.
async fn write_out<W: AsyncWrite + Unpin>(
    mut io: W,
    mut rx: Receiver<Bytes>,
    hangup: Option<AsyncFd<OwnedFd>>,
) {
    loop {
        tokio::select! {
            bytes = rx.recv() => {
                let Some(bytes) = bytes else { break };
                if io.write_all(&bytes).await.is_err() || io.flush().await.is_err() {
                    return;
                }
            }
            () = hung_up(hangup.as_ref()) => return,
        }
    }
    let _ = io.shutdown().await;
}

/// A watch of a connection's hangup, on a descriptor of its own: clearing the readiness
/// of the connection itself would hold up its writes.
fn hangup_watch(stream: &UnixStream) -> Option<AsyncFd<OwnedFd>> {
    let fd = stream.as_fd().try_clone_to_owned().ok()?;
    AsyncFd::with_interest(fd, Interest::WRITABLE).ok()
}

/// Resolves once the client has hung up: the whole connection, not its writing half,
/// which the engine's command line closes when it has no input to send. The engine
/// watches for the same hangup. Never without a watch.
async fn hung_up(watch: Option<&AsyncFd<OwnedFd>>) {
    let Some(watch) = watch else {
        return std::future::pending().await;
    };
    loop {
        let Ok(mut guard) = watch.writable().await else {
            return;
        };
        if guard.ready().is_write_closed() {
            return;
        }
        guard.clear_ready_matching(Ready::WRITABLE);
    }
}

/// The input an attach sends on.
pub(super) struct Input {
    pub(super) sink: Sink,
    /// Kept as long as the attach: the start takes an input only from an attach that is
    /// still there.
    pub(super) alive: Arc<()>,
}

/// Where an attach's input goes.
pub(super) enum Sink {
    /// The attach's end of the program's input pipe.
    Pipe(OwnedFd),
    /// The terminal's master, once the start hands it over.
    Terminal(oneshot::Receiver<OwnedFd>),
}

/// Copies the client's input into `sink`, `early` first, until the client closes its
/// writing half or hangs up, or the program's end is gone; then the attach's end closes,
/// and a program on a pipe reads the end of its input unless its run holds it open.
async fn feed<R: AsyncRead + Unpin>(sink: Sink, early: Bytes, reader: &mut R) {
    let fd = match sink {
        Sink::Pipe(fd) => fd,
        Sink::Terminal(master) => match master.await {
            Ok(fd) => fd,
            Err(_) => return,
        },
    };
    if set_nonblocking(&fd).is_err() {
        return;
    }
    let Ok(fd) = AsyncFd::with_interest(fd, Interest::WRITABLE) else {
        return;
    };
    if write_all(&fd, &early).await.is_err() {
        return;
    }
    let mut buffer = vec![0u8; 16 * 1024];
    loop {
        let n = match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        if write_all(&fd, &buffer[..n]).await.is_err() {
            return;
        }
    }
}

pub(super) fn set_nonblocking(fd: &OwnedFd) -> nix::Result<()> {
    use nix::fcntl::{fcntl, FcntlArg, OFlag};
    let flags = OFlag::from_bits_truncate(fcntl(fd, FcntlArg::F_GETFL)?);
    fcntl(fd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)).map(drop)
}

async fn write_all(fd: &AsyncFd<OwnedFd>, mut bytes: &[u8]) -> std::io::Result<()> {
    while !bytes.is_empty() {
        let mut guard = fd.writable().await?;
        if let Ok(written) = guard.try_io(|inner| {
            nix::unistd::write(inner.get_ref(), bytes).map_err(std::io::Error::from)
        }) {
            bytes = &bytes[written?..];
        }
    }
    Ok(())
}

pub(super) async fn read_some(fd: &AsyncFd<OwnedFd>, buffer: &mut [u8]) -> std::io::Result<usize> {
    loop {
        let mut guard = fd.readable().await?;
        if let Ok(read) = guard.try_io(|inner| {
            nix::unistd::read(inner.get_ref(), buffer).map_err(std::io::Error::from)
        }) {
            return read;
        }
    }
}

/// The output of a run started with an input, its terminal or its pipe, for the attach
/// that waits for it.
struct RunOutput {
    tx: Sender<Bytes>,
    shape: Shape,
    /// The attach's state in ATTACHES: draining once the output closed.
    state: Arc<watch::Sender<u8>>,
}

/// An input an attach keeps for its container's next start.
struct Pending {
    /// The program gets a terminal.
    tty: bool,
    /// The program's end of the input's pipe (no terminal).
    stdin: Option<OwnedFd>,
    /// A copy of the attach's end of that pipe, kept until the run ends.
    held: Option<OwnedFd>,
    /// Where the run's output goes: from its terminal, or from its pipe without one.
    output: Option<RunOutput>,
    /// Hands the attach the terminal's master, for the input.
    master: Option<oneshot::Sender<OwnedFd>>,
    /// Gone with the attach.
    attach: Weak<()>,
}

impl Pending {
    fn live(&self) -> bool {
        self.attach.strong_count() > 0
    }
}

/// The inputs attaches keep for the next start of their containers, by machine.
static PENDING: Mutex<Vec<(String, Pending)>> = Mutex::new(Vec::new());

/// Keeps `pending` for `name`'s next start. An input kept before for it gives way: the
/// latest attach is the one whose client is about to start the container.
fn keep_input(name: &str, pending: Pending) {
    let mut all = PENDING.lock().unwrap();
    all.retain(|(of, kept)| of != name && kept.live());
    all.push((name.to_string(), pending));
}

/// The input an attach keeps for `name`, if that attach is still there.
fn take_input(name: &str) -> Option<Pending> {
    let mut all = PENDING.lock().unwrap();
    all.retain(|(_, kept)| kept.live());
    let at = all.iter().position(|(of, _)| of == name)?;
    Some(all.remove(at).1)
}

/// An input whose start failed, kept for the next one unless another attach left one
/// meanwhile.
fn put_back(name: &str, pending: Pending) {
    let mut all = PENDING.lock().unwrap();
    if pending.live() && !all.iter().any(|(of, _)| of == name) {
        all.push((name.to_string(), pending));
    }
}

/// Whether an attach keeps an input for `name`'s next start.
pub fn input_pending(name: &str) -> bool {
    PENDING
        .lock()
        .unwrap()
        .iter()
        .any(|(of, kept)| of == name && kept.live())
}

/// Whether the program of `name`'s run ended while its machine is still there, its
/// unit's stop hooks running: a wait for that end answers then, and the client's next
/// request comes before the machine is gone. systemd clears the code of a run's end when
/// the next one starts.
pub async fn program_ended(ctx: &Context, name: &str) -> bool {
    let Ok(sd) = ctx.sd().await else {
        return false;
    };
    let unit = format!("systemd-nspawn@{name}.service");
    matches!(sd.exec_main_exit(&unit).await, Ok(Some(_)))
}

/// Whether an attach keeps an input with a terminal for `name`'s next start.
pub fn terminal_pending(name: &str) -> bool {
    PENDING
        .lock()
        .unwrap()
        .iter()
        .any(|(of, kept)| of == name && kept.tty && kept.live())
}

/// The sizes the terminals of the next attached starts get, by machine: from the
/// create's ConsoleSize or a resize before the start.
static SIZES: Mutex<Vec<(String, (u16, u16))>> = Mutex::new(Vec::new());

/// How many sizes are kept: the oldest goes first.
const MOST_SIZES: usize = 256;

/// Keeps the size, rows and columns, the terminal of `name`'s next attached start gets.
pub fn remember_size(name: &str, rows: u16, cols: u16) {
    let mut all = SIZES.lock().unwrap();
    all.retain(|(of, _)| of != name);
    if all.len() >= MOST_SIZES {
        all.remove(0);
    }
    all.push((name.to_string(), (rows, cols)));
}

fn size_of(name: &str) -> Option<(u16, u16)> {
    SIZES
        .lock()
        .unwrap()
        .iter()
        .find(|(of, _)| of == name)
        .map(|(_, size)| *size)
}

fn forget_size(name: &str) {
    SIZES.lock().unwrap().retain(|(of, _)| of != name);
}

/// The terminals of the runs this server holds, by machine, for resizes.
static TERMINALS: Mutex<Vec<(String, Arc<OwnedFd>)>> = Mutex::new(Vec::new());

/// Sets the size of the terminal of `name`'s run: false when this server holds none.
pub fn resize_terminal(name: &str, rows: u16, cols: u16) -> nix::Result<bool> {
    let master = TERMINALS
        .lock()
        .unwrap()
        .iter()
        .find(|(of, _)| of == name)
        .map(|(_, master)| master.clone());
    match master {
        Some(master) => crate::nsenter::set_terminal_size(&*master, rows, cols).map(|()| true),
        None => Ok(false),
    }
}

/// RunMachine's options for a start with an input: it waits as StartMachine's does, so
/// that the ports are bound when it answers; a terminal gets the size kept for it, and a
/// run without one gives its output on a pipe, byte for byte.
fn run_options(tty: bool, size: Option<(u16, u16)>) -> Options<'static> {
    let mut options = Options::new();
    options.insert("wait", Bus::from(true));
    options.insert("tty", Bus::from(tty));
    if !tty {
        options.insert("straight", Bus::from(true));
    }
    if let (true, Some((rows, cols))) = (tty, size) {
        options.insert("rows", Bus::from(u64::from(rows)));
        options.insert("cols", Bus::from(u64::from(cols)));
    }
    options
}

/// Starts `name` with the input an attach of this server keeps for it, as RunMachine
/// starts a machine: what its record holds applies as it does to StartMachine's start.
/// None when no attach keeps one, for the caller to start it its own way. A start that
/// fails leaves the input for the next one.
pub async fn start_with_input(client: &Client, name: &str) -> Option<anyhow::Result<()>> {
    let mut pending = take_input(name)?;
    let size = size_of(name);
    let mut fds = HashMap::new();
    if let Some(stdin) = &pending.stdin {
        fds.insert("stdin", zbus::zvariant::Fd::from(stdin.as_fd()));
    }
    let called = client
        .manager
        .run_machine(name, run_options(pending.tty, size), fds)
        .await
        .map_err(client::error);
    let (mut handed, process, _notes) = match called {
        Ok(answer) => answer,
        Err(e) => {
            put_back(name, pending);
            return Some(Err(e));
        }
    };
    forget_size(name);
    // The program has its end of the input now: with ours gone, a write after its end
    // fails rather than fill the pipe.
    pending.stdin = None;
    let piped =
        handed
            .remove("stdout")
            .map(OwnedFd::from)
            .and_then(|read| match Piped::open(read) {
                Ok(piped) => Some(piped),
                Err(e) => {
                    eprintln!("warning: the output of {name}: {e}");
                    None
                }
            });
    // In place before the start answers: a client resizes the terminal right after.
    let terminal =
        handed.remove("tty").map(OwnedFd::from).and_then(|master| {
            match Terminal::open(name, master, pending.master.take()) {
                Ok(terminal) => Some(terminal),
                Err(e) => {
                    eprintln!("warning: the terminal of {name}: {e}");
                    None
                }
            }
        });
    let ended = match Ended::watch(&client.connection, process).await {
        Ok(ended) => Some(ended),
        Err(e) => {
            eprintln!("warning: watching the run of {name}: {e:#}");
            None
        }
    };
    tokio::spawn(hold_run(
        name.to_string(),
        pending,
        terminal,
        piped,
        ended,
        client.connection.clone(),
        super::Upgraded::hold(),
    ));
    Some(Ok(()))
}

/// Holds what a run started with an input needs until it ends: the bus connection it was
/// started on, whose end would stop a run with a terminal; this server, which would leave
/// with it; an input kept open; the terminal or the pipe, whose output it carries to the
/// attach while there is one and drops after, as the program would block on a full one.
async fn hold_run(
    name: String,
    pending: Pending,
    terminal: Option<Terminal>,
    piped: Option<Piped>,
    ended: Option<Ended>,
    connection: zbus::Connection,
    server: super::Upgraded,
) {
    let _held = (connection, server, pending.held);
    let end = async {
        if let Some(ended) = ended {
            let _ = ended.status().await;
        }
    };
    let output = pending.output;
    let carried = async {
        if let Some(terminal) = &terminal {
            carry_output(&terminal.master, output).await;
        } else if let Some(piped) = &piped {
            carry_output(&piped.0, output).await;
        }
    };
    tokio::join!(end, carried);
    if let Some(terminal) = terminal {
        terminal.forget(&name);
    }
}

/// The master of a run's terminal, read by this server for as long as the run goes.
struct Terminal {
    master: AsyncFd<OwnedFd>,
    /// The copy resizes go through.
    shared: Arc<OwnedFd>,
}

impl Terminal {
    /// Takes `master` for the run of `name`: a copy goes to the attach for the input, and
    /// one is kept for resizes.
    fn open(
        name: &str,
        master: OwnedFd,
        input: Option<oneshot::Sender<OwnedFd>>,
    ) -> std::io::Result<Terminal> {
        set_nonblocking(&master)?;
        let shared = Arc::new(master.try_clone()?);
        if let Some(input) = input {
            let _ = input.send(master.try_clone()?);
        }
        let master = AsyncFd::with_interest(master, Interest::READABLE)?;
        let mut all = TERMINALS.lock().unwrap();
        all.retain(|(of, _)| of != name);
        all.push((name.to_string(), shared.clone()));
        Ok(Terminal { master, shared })
    }

    fn forget(&self, name: &str) {
        TERMINALS
            .lock()
            .unwrap()
            .retain(|(of, master)| !(of == name && Arc::ptr_eq(master, &self.shared)));
    }
}

/// The read end of the pipe a run started with an input writes its output to, without a
/// terminal.
struct Piped(AsyncFd<OwnedFd>);

impl Piped {
    fn open(read: OwnedFd) -> std::io::Result<Piped> {
        set_nonblocking(&read)?;
        Ok(Piped(AsyncFd::with_interest(read, Interest::READABLE)?))
    }
}

/// Copies what the program writes, on its terminal or its pipe, to the attach's output
/// while the attach is there, until the program's side closes; then the attach hands
/// over the rest.
async fn carry_output(from: &AsyncFd<OwnedFd>, mut output: Option<RunOutput>) {
    let mut buffer = vec![0u8; 16 * 1024];
    loop {
        // A terminal reads EIO once the program's side is closed, a pipe its end.
        let n = match read_some(from, &mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let Some(out) = &output else { continue };
        if let Some(chunk) = out.shape.chunk(&buffer[..n]) {
            if out.tx.send(chunk).await.is_err() {
                output = None;
            }
        }
    }
    if let Some(out) = output {
        run_ended(&out.state);
    }
}

/// A task aborted when this goes.
struct Aborted(tokio::task::JoinHandle<()>);

impl Drop for Aborted {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A follow of a machine's output until the end of its run.
struct Follow {
    /// The read end of what the follower copies.
    output: OwnedFd,
    follower: Follower,
    watch: JobWatch,
    job: OwnedObjectPath,
    /// The connection the wait was asked on, which it ends with.
    client: Client,
}

impl Follow {
    /// Follows `name`'s output from now, or from `since` (unix microseconds), and waits
    /// for the end of its run, the one under way or the next. Both are in place before
    /// the answer, so that a start right after it is not missed.
    async fn start(
        ctx: &Context,
        name: &str,
        namespace: Option<&str>,
        since: Option<u64>,
        key: &str,
    ) -> Result<Follow, (StatusCode, String)> {
        let failed = |e: anyhow::Error| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
        if let Some(namespace) = namespace {
            let sd = ctx.sd().await.map_err(failed)?;
            ensure_journal_namespace(sd, namespace)
                .await
                .map_err(failed)?;
        }
        let cursor = match since {
            Some(_) => None,
            None => journal_cursor(namespace).await,
        };
        let from = match since {
            Some(usec) => FollowFrom::Since(usec),
            None => FollowFrom::After(cursor.as_deref()),
        };
        let (output, write) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)
            .map_err(|e| failed(anyhow::anyhow!("creating the output's pipe: {e}")))?;
        let unit = format!("systemd-nspawn@{name}.service");
        let follower = Follower::start(&unit, from, namespace, write).map_err(failed)?;
        let client = Client::connect().await.map_err(failed)?;
        let watch = client.watch_jobs_quiet(true).await.map_err(failed)?;
        let mut options = Options::new();
        // The end of the run under way, or of the next one.
        options.insert("condition", Bus::from("next-exit"));
        let job = client
            .manager
            .wait_machine(name, options)
            .await
            .map_err(client::error)
            .map_err(|e| match client::error_name(&e) {
                Some("NoSuchMachine") => {
                    (StatusCode::NOT_FOUND, format!("No such container: {key}"))
                }
                kind => (status_of(kind), format!("{e:#}")),
            })?;
        Ok(Follow {
            output,
            follower,
            watch,
            job,
            client,
        })
    }

    /// Copies the output into `tx` in `shape` until the run ended and its last lines
    /// were read, or the client went; `state` says when the run ended.
    async fn pump(self, shape: Shape, tx: Sender<Bytes>, state: Arc<watch::Sender<u8>>) {
        let Follow {
            output,
            follower,
            watch,
            job,
            client,
        } = self;
        let Ok(mut output) = tokio::net::unix::pipe::Receiver::from_owned_fd(output) else {
            return;
        };
        // The end of the run is followed in a task of its own, whatever the client's pace:
        // the follower closes the pipe once the output has been quiet for a moment. It
        // goes with the pump when the client does, journalctl and the wait with it.
        let end = Aborted(tokio::spawn(async move {
            let _client = client;
            let _ = watch.finish(job).await;
            run_ended(&state);
            follower.finish_within(DRAIN).await;
        }));
        let mut buffer = vec![0u8; 16 * 1024];
        loop {
            tokio::select! {
                read = output.read(&mut buffer) => {
                    let n = match read {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    if let Some(chunk) = shape.chunk(&buffer[..n]) {
                        if tx.send(chunk).await.is_err() {
                            break;
                        }
                    }
                }
                () = tx.closed() => break,
            }
        }
        drop(end);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stream_is_framed_unless_the_container_has_a_terminal() {
        assert_eq!(Shape::of(false, true, true), Shape::Framed(1));
        assert_eq!(Shape::of(false, false, true), Shape::Framed(2));
        assert_eq!(Shape::of(true, true, true), Shape::Raw);
        assert_eq!(Shape::of(true, false, true), Shape::Raw);
        assert_eq!(Shape::of(false, false, false), Shape::Nothing);
        assert_eq!(
            Shape::Framed(1).chunk(b"hi\n").unwrap(),
            Bytes::from_static(b"\x01\0\0\0\0\0\0\x03hi\n")
        );
        assert_eq!(
            Shape::Raw.chunk(b"hi\n").unwrap(),
            Bytes::from_static(b"hi\n")
        );
        assert_eq!(Shape::Nothing.chunk(b"hi\n"), None);
    }

    #[test]
    fn the_frames_are_named_on_an_upgraded_connection_of_a_recent_client() {
        let multiplexed = "application/vnd.docker.multiplexed-stream";
        let raw = "application/vnd.docker.raw-stream";
        assert_eq!(content_type(false, true, Some((1, 51))), multiplexed);
        assert_eq!(content_type(false, true, None), multiplexed);
        assert_eq!(content_type(false, true, Some((1, 41))), raw);
        assert_eq!(content_type(false, false, Some((1, 51))), raw);
        assert_eq!(content_type(true, true, Some((1, 51))), raw);
    }

    #[tokio::test]
    async fn a_wait_answers_once_the_attaches_of_the_run_that_ended_are_done() {
        let (attachment, state) = register("e2e-drain");
        let waited = tokio::spawn(async {
            drained("e2e-drain").await;
            std::time::Instant::now()
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        state.send_replace(DRAINING);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let done = std::time::Instant::now();
        drop(attachment);
        assert!(
            waited.await.unwrap() >= done,
            "the wait came before the output"
        );
        // One still following after the end waits for another run.
        let (_following, _) = register("e2e-next");
        let started = std::time::Instant::now();
        drained("e2e-next").await;
        assert!(started.elapsed() < Duration::from_secs(2));
        // Another machine's attach is not waited for.
        let started = std::time::Instant::now();
        drained("e2e-other").await;
        assert!(started.elapsed() < Duration::from_millis(100));
        let (gone, _) = register("e2e-gone");
        drop(gone);
        let started = std::time::Instant::now();
        drained("e2e-gone").await;
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    #[tokio::test]
    async fn an_attach_ends_when_its_client_hangs_up_not_when_it_only_stops_writing() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let watch = hangup_watch(&ours).expect("a watch");
        let (_, mut their_writes) = theirs.into_split();
        their_writes.shutdown().await.unwrap();
        let early = tokio::time::timeout(Duration::from_millis(200), hung_up(Some(&watch))).await;
        assert!(early.is_err(), "a half close is no hangup");
        let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(4);
        let mut ours = ours;
        tx.send(Bytes::from_static(b"still")).await.unwrap();
        drop(tx);
        write_out(&mut ours, rx, None).await;
        drop(their_writes);
        tokio::time::timeout(Duration::from_secs(2), hung_up(Some(&watch)))
            .await
            .expect("the hangup is seen");
    }

    #[tokio::test]
    async fn an_attach_its_client_left_does_not_hold_up_waits() {
        let (attachment, state) = register("e2e-left");
        drop(attachment);
        run_ended(&state);
        assert_eq!(*state.borrow(), DONE, "done stays done");
        // An entry left draining with nobody behind it is not waited for.
        let (attachment, state) = register("e2e-vanished");
        drop(attachment);
        state.send_replace(DRAINING);
        drop(state);
        let started = std::time::Instant::now();
        drained("e2e-vanished").await;
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn a_paused_or_restarting_container_is_not_attached_to() {
        assert_eq!(
            refusal("paused", "web").unwrap(),
            "container web is paused, unpause the container before attach"
        );
        assert_eq!(
            refusal("restarting", "web").unwrap(),
            "container web is restarting, wait until the container is running"
        );
        for state in ["created", "running", "exited"] {
            assert_eq!(refusal(state, "web"), None, "{state}");
        }
    }

    #[test]
    fn the_input_is_kept_for_a_container_made_to_take_it_that_does_not_run() {
        use InputPlan::*;
        // asked, stream, upgraded, interactive, app, running
        assert_eq!(input_plan(true, true, true, true, true, false), Kept);
        // Its input was not kept when it started.
        assert_eq!(input_plan(true, true, true, true, true, true), Refused);
        assert!(INPUT_REFUSED.contains("not supported"));
        // Without OpenStdin, on a booted machine, without stdin=1 or a stream: the output
        // alone, running or not.
        for running in [false, true] {
            assert_eq!(input_plan(true, true, true, false, true, running), Ignored);
            assert_eq!(input_plan(true, true, true, true, false, running), Ignored);
            assert_eq!(input_plan(false, true, true, true, true, running), Ignored);
            assert_eq!(input_plan(true, false, true, true, true, running), Ignored);
        }
        // A body has no input to read from.
        assert_eq!(input_plan(true, true, false, true, true, false), Ignored);
    }

    fn pending(tty: bool, alive: &Arc<()>) -> Pending {
        Pending {
            tty,
            stdin: None,
            held: None,
            output: None,
            master: None,
            attach: Arc::downgrade(alive),
        }
    }

    #[test]
    fn a_start_takes_the_input_of_an_attach_that_is_still_there() {
        let alive = Arc::new(());
        keep_input("e2e-input", pending(false, &alive));
        assert!(input_pending("e2e-input") && !terminal_pending("e2e-input"));
        let taken = take_input("e2e-input").expect("kept");
        assert!(!input_pending("e2e-input"));
        assert!(take_input("e2e-input").is_none(), "taken once");
        // A failed start leaves it for the next one.
        put_back("e2e-input", taken);
        assert!(take_input("e2e-input").is_some());
        // An attach that went leaves nothing.
        let gone = Arc::new(());
        keep_input("e2e-input-gone", pending(true, &gone));
        assert!(terminal_pending("e2e-input-gone"));
        drop(gone);
        assert!(!terminal_pending("e2e-input-gone") && !input_pending("e2e-input-gone"));
        assert!(take_input("e2e-input-gone").is_none());
        // The latest attach's input is the one kept.
        let (first, second) = (Arc::new(()), Arc::new(()));
        keep_input("e2e-input-twice", pending(false, &first));
        keep_input("e2e-input-twice", pending(true, &second));
        assert!(take_input("e2e-input-twice").unwrap().tty);
        assert!(take_input("e2e-input-twice").is_none());
        // Put back after another attach came: the newer stays.
        let (old, new) = (Arc::new(()), Arc::new(()));
        keep_input("e2e-input-race", pending(false, &old));
        let taken = take_input("e2e-input-race").unwrap();
        keep_input("e2e-input-race", pending(true, &new));
        put_back("e2e-input-race", taken);
        assert!(take_input("e2e-input-race").unwrap().tty);
        assert!(take_input("e2e-input-race").is_none());
    }

    #[test]
    fn a_terminal_gets_the_size_kept_for_its_start() {
        remember_size("e2e-size", 40, 120);
        remember_size("e2e-size", 50, 132);
        assert_eq!(size_of("e2e-size"), Some((50, 132)));
        let options = run_options(true, size_of("e2e-size"));
        assert_eq!(options["rows"], Bus::from(50u64));
        assert_eq!(options["cols"], Bus::from(132u64));
        assert_eq!(options["wait"], Bus::from(true));
        assert_eq!(options["tty"], Bus::from(true));
        // Without a terminal the size says nothing.
        assert!(!options.contains_key("straight"));
        let options = run_options(false, Some((50, 132)));
        assert_eq!(options["tty"], Bus::from(false));
        assert_eq!(options["straight"], Bus::from(true));
        assert!(!options.contains_key("rows") && !options.contains_key("cols"));
        assert!(!run_options(true, None).contains_key("rows"));
        forget_size("e2e-size");
        assert_eq!(size_of("e2e-size"), None);
        for n in 0..MOST_SIZES + 1 {
            remember_size(&format!("e2e-sizes-{n}"), 1, 1);
        }
        assert!(SIZES.lock().unwrap().len() <= MOST_SIZES);
        assert_eq!(size_of(&format!("e2e-sizes-{MOST_SIZES}")), Some((1, 1)));
    }

    /// Everything a pipe gives until its writers are gone.
    fn read_to_end(read: OwnedFd) -> Vec<u8> {
        let mut all = Vec::new();
        std::io::Read::read_to_end(&mut std::fs::File::from(read), &mut all).unwrap();
        all
    }

    #[tokio::test]
    async fn the_input_reaches_the_program_and_its_end_with_the_client_s() {
        let (Pending { stdin, held, .. }, sink) = input_pipe(true).unwrap();
        assert!(held.is_none(), "StdinOnce closes it with the client's");
        let program = stdin.unwrap();
        let (client, server) = UnixStream::pair().unwrap();
        let (reader, _writer) = server.into_split();
        let fed = tokio::spawn(async move {
            let mut reader = reader;
            feed(sink, Bytes::from_static(b"early "), &mut reader).await
        });
        let (_, mut client_writes) = client.into_split();
        client_writes.write_all(b"echo hi\n").await.unwrap();
        // CloseWrite: the program reads the end of its input.
        client_writes.shutdown().await.unwrap();
        let read = tokio::task::spawn_blocking(move || read_to_end(program));
        assert_eq!(read.await.unwrap(), b"early echo hi\n");
        fed.await.unwrap();
    }

    #[tokio::test]
    async fn without_stdin_once_the_input_stays_open_after_the_client_s() {
        let (Pending { stdin, held, .. }, sink) = input_pipe(false).unwrap();
        let program = stdin.unwrap();
        let (client, server) = UnixStream::pair().unwrap();
        let (mut reader, _writer) = server.into_split();
        drop(client);
        feed(sink, Bytes::new(), &mut reader).await;
        let read = tokio::task::spawn_blocking(move || read_to_end(program));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!read.is_finished(), "held open by the run");
        drop(held);
        assert!(read.await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_input_of_a_terminal_waits_for_its_start() {
        let (handed, master) = oneshot::channel();
        let (client, server) = UnixStream::pair().unwrap();
        let (reader, _writer) = server.into_split();
        let fed = tokio::spawn(async move {
            let mut reader = reader;
            feed(Sink::Terminal(master), Bytes::new(), &mut reader).await
        });
        let (_, mut client_writes) = client.into_split();
        client_writes.write_all(b"ls\r").await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        // Here a pipe stands for the terminal's master.
        let (read, write) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC).unwrap();
        handed.send(write).unwrap();
        client_writes.shutdown().await.unwrap();
        fed.await.unwrap();
        assert_eq!(read_to_end(read), b"ls\r");
        // A start that never comes ends the input.
        let (handed, master) = oneshot::channel::<OwnedFd>();
        let (_client, server) = UnixStream::pair().unwrap();
        let (mut reader, _writer) = server.into_split();
        drop(handed);
        feed(Sink::Terminal(master), Bytes::new(), &mut reader).await;
    }

    #[tokio::test]
    async fn an_attach_carries_the_input_and_ends_with_the_output() {
        let (Pending { stdin, .. }, sink) = input_pipe(true).unwrap();
        let program = stdin.unwrap();
        let (client, server) = UnixStream::pair().unwrap();
        let (reader, writer) = server.into_split();
        let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(4);
        let input = Input {
            sink,
            alive: Arc::new(()),
        };
        let carried = tokio::spawn(carry(
            reader,
            Bytes::new(),
            writer,
            rx,
            None,
            Some(input),
            (),
        ));
        let (mut client_reads, mut client_writes) = client.into_split();
        client_writes.write_all(b"in").await.unwrap();
        client_writes.shutdown().await.unwrap();
        let read = tokio::task::spawn_blocking(move || read_to_end(program));
        assert_eq!(read.await.unwrap(), b"in");
        // The output goes on after the input's end, and its end ends the attach.
        tx.send(Bytes::from_static(b"out")).await.unwrap();
        drop(tx);
        let mut got = Vec::new();
        client_reads.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, b"out");
        carried.await.unwrap();
    }

    #[tokio::test]
    async fn an_attach_is_done_once_its_output_is_out_though_the_client_still_writes() {
        let (client, server) = UnixStream::pair().unwrap();
        let (reader, writer) = server.into_split();
        let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(4);
        let (attachment, state) = register("e2e-handed-over");
        let carried = tokio::spawn(carry(
            reader,
            Bytes::new(),
            writer,
            rx,
            None,
            None,
            attachment,
        ));
        // A terminal's client keeps its writing half open until it has the exit code.
        let (mut client_reads, client_writes) = client.into_split();
        tx.send(Bytes::from_static(b"out")).await.unwrap();
        drop(tx);
        let mut got = Vec::new();
        client_reads.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, b"out");
        let done = async {
            while *state.borrow() != DONE {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(2), done)
            .await
            .expect("the attach is done before the client closes");
        drop(client_writes);
        carried.await.unwrap();
    }

    #[tokio::test]
    async fn input_the_program_never_read_does_not_reset_the_output() {
        let (Pending { stdin, .. }, sink) = input_pipe(true).unwrap();
        // The program went without reading its input.
        drop(stdin);
        let (client, server) = UnixStream::pair().unwrap();
        let (reader, writer) = server.into_split();
        let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(4);
        let input = Input {
            sink,
            alive: Arc::new(()),
        };
        let carried = tokio::spawn(carry(
            reader,
            Bytes::new(),
            writer,
            rx,
            None,
            Some(input),
            (),
        ));
        let (mut client_reads, mut client_writes) = client.into_split();
        client_writes.write_all(&[7u8; 64 * 1024]).await.unwrap();
        tx.send(Bytes::from_static(b"out")).await.unwrap();
        drop(tx);
        let mut got = Vec::new();
        client_reads.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, b"out");
        drop((client_reads, client_writes));
        carried.await.unwrap();
    }

    #[tokio::test]
    async fn a_terminal_s_output_goes_to_its_attach_and_is_dropped_once_it_left() {
        // Here a pipe stands for the terminal's master: its end reads as the program's
        // side closing.
        let (read, write) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC).unwrap();
        let (handed, master) = oneshot::channel();
        let terminal = Terminal::open("e2e-terminal", read, Some(handed)).unwrap();
        assert!(master.await.is_ok(), "the attach gets the master");
        assert!(TERMINALS
            .lock()
            .unwrap()
            .iter()
            .any(|(of, _)| of == "e2e-terminal"));
        let (attachment, state) = register("e2e-terminal");
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Bytes>(4);
        let output = RunOutput {
            tx,
            shape: Shape::Raw,
            state,
        };
        let mut program = std::fs::File::from(write);
        std::io::Write::write_all(&mut program, b"$ ").unwrap();
        let carried = tokio::spawn(async move {
            carry_output(&terminal.master, Some(output)).await;
            terminal
        });
        assert_eq!(rx.recv().await.unwrap(), Bytes::from_static(b"$ "));
        // The attach goes; what the program writes after is read and dropped.
        drop(rx);
        let writes = tokio::task::spawn_blocking(move || {
            for _ in 0..64 {
                std::io::Write::write_all(&mut program, &[b'x'; 4096]).unwrap();
            }
        });
        writes.await.unwrap();
        let terminal = carried.await.unwrap();
        terminal.forget("e2e-terminal");
        assert!(!TERMINALS
            .lock()
            .unwrap()
            .iter()
            .any(|(of, _)| of == "e2e-terminal"));
        drop(attachment);
        // The end of a terminal an attach still reads marks its run ended.
        let (read, write) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC).unwrap();
        let terminal = Terminal::open("e2e-terminal-end", read, None).unwrap();
        let (_attachment, state) = register("e2e-terminal-end");
        let (tx, _rx) = tokio::sync::mpsc::channel::<Bytes>(4);
        drop(write);
        carry_output(
            &terminal.master,
            Some(RunOutput {
                tx,
                shape: Shape::Raw,
                state: state.clone(),
            }),
        )
        .await;
        assert_eq!(*state.borrow(), DRAINING);
        terminal.forget("e2e-terminal-end");
    }

    #[tokio::test]
    async fn a_piped_run_s_output_reaches_its_attach_byte_for_byte() {
        let (read, write) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC).unwrap();
        let piped = Piped::open(read).unwrap();
        let (_attachment, state) = register("e2e-piped");
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Bytes>(64);
        let output = RunOutput {
            tx,
            shape: Shape::Framed(1),
            state: state.clone(),
        };
        // Empty lines, NULs, CRs and a syslog prefix, which the journal would not keep.
        let written: Vec<u8> = b"a\n\n\0\0<3>b\r\n\r\n"
            .iter()
            .copied()
            .chain((0..=255u8).cycle().take(70_000))
            .collect();
        let mut program = std::fs::File::from(write);
        let sent = written.clone();
        let writes = tokio::task::spawn_blocking(move || {
            std::io::Write::write_all(&mut program, &sent).unwrap();
        });
        let carried = tokio::spawn(async move { carry_output(&piped.0, Some(output)).await });
        let mut got = Vec::new();
        while let Some(frame) = rx.recv().await {
            assert_eq!(frame[0], 1, "framed as standard output");
            let size = u32::from_be_bytes([frame[4], frame[5], frame[6], frame[7]]) as usize;
            assert_eq!(frame.len(), 8 + size);
            got.extend_from_slice(&frame[8..]);
        }
        writes.await.unwrap();
        carried.await.unwrap();
        assert!(got == written, "the output differs from what was written");
        assert_eq!(*state.borrow(), DRAINING);
    }

    #[test]
    fn a_resize_reaches_the_terminal_of_the_run() {
        assert!(!resize_terminal("e2e-no-terminal", 24, 80).unwrap());
        if !std::path::Path::new("/dev/ptmx").exists() {
            eprintln!("no /dev/ptmx here: not checked");
            return;
        }
        let (master, _slave) = crate::nsenter::host_pty(24, 80).unwrap();
        TERMINALS
            .lock()
            .unwrap()
            .push(("e2e-resize".to_string(), Arc::new(master)));
        assert!(resize_terminal("e2e-resize", 33, 99).unwrap());
        TERMINALS
            .lock()
            .unwrap()
            .retain(|(of, _)| of != "e2e-resize");
    }
}
