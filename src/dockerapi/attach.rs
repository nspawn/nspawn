//! POST /containers/{id}/attach: what a container writes from the attach on (from its
//! creation with `logs`), until its run ends. The output is the journal's, followed as
//! an attached run follows it, so that it comes whoever starts the machine; the end is a
//! WaitMachine taken before the answer, so that a start sent right after it is not
//! missed. A client that asks for an upgrade gets the connection with 101, as the
//! engine's command line wants it; others read the stream as the body of a 200. The
//! container's input is not taken.

use std::os::fd::{AsFd, OwnedFd};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use hyper::upgrade::OnUpgrade;
use hyper::{Response, StatusCode};
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt, Interest, Ready};
use tokio::net::UnixStream;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::watch;
use zbus::zvariant::{OwnedObjectPath, Value as Bus};

use super::containers::status_of;
use super::model::Snapshot;
use super::streams::{frame, LogsQuery};
use super::{error, lookup_status, Body, Query};
use crate::api::machines::{prepare_logs, LogsRequest};
use crate::api::run::{ensure_journal_namespace, journal_cursor, FollowFrom, Follower};
use crate::api::Context;
use crate::client::{self, Client, JobWatch, Options};

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

pub async fn attach(
    ctx: &Arc<Context>,
    key: &str,
    query: &Query,
    upgrade: Option<OnUpgrade>,
    version: Option<(u32, u32)>,
) -> Response<Body> {
    if query.flag("stdin", false) {
        return error(
            StatusCode::NOT_IMPLEMENTED,
            "attaching to a container's input is not supported by nspawn's Docker API",
        );
    }
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
    let shape = Shape::of(
        record.tty,
        query.flag("stdout", false),
        query.flag("stderr", false),
    );
    let kind = content_type(record.tty, upgrade.is_some(), version);
    let name = machine.name().to_string();
    let created = record.created;
    let namespace = record.tuning.journal_namespace();
    drop(snapshot);
    let (logs, stream) = (query.flag("logs", false), query.flag("stream", false));
    let Ok(permit) = super::STREAMS.try_acquire() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "too many streams are open on this socket",
        );
    };
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(64);
    if stream {
        let Ok(waiting) = super::WAITS.try_acquire() else {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "too many waits are open on this socket",
            );
        };
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
        return deliver(rx, upgrade, kind, Some(attachment));
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
    deliver(rx, upgrade, kind, None)
}

/// The stream to the client: on the connection it gave up, or as a body; the attach is
/// done once it is written.
fn deliver(
    rx: Receiver<Bytes>,
    upgrade: Option<OnUpgrade>,
    kind: &str,
    attachment: Option<Attachment>,
) -> Response<Body> {
    let Some(upgrade) = upgrade else {
        return super::streaming_rx(rx, kind, attachment);
    };
    // Counted from now: the connection leaves hyper's hands with the 101.
    let held = super::Upgraded::hold();
    tokio::spawn(async move {
        let _held = (held, attachment);
        let Ok(upgraded) = upgrade.await else { return };
        match upgraded.downcast::<hyper_util::rt::TokioIo<UnixStream>>() {
            Ok(parts) => {
                let stream = parts.io.into_inner();
                let hangup = hangup_watch(&stream);
                write_out(stream, rx, hangup).await;
            }
            Err(upgraded) => write_out(hyper_util::rt::TokioIo::new(upgraded), rx, None).await,
        }
    });
    let mut response = super::respond(StatusCode::SWITCHING_PROTOCOLS, kind, Bytes::new());
    let headers = response.headers_mut();
    headers.insert("Connection", "Upgrade".parse().unwrap());
    headers.insert("Upgrade", "tcp".parse().unwrap());
    response
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
        let _client = client;
        let Ok(mut output) = tokio::net::unix::pipe::Receiver::from_owned_fd(output) else {
            return;
        };
        // The follower closes the pipe once the output has been quiet for a moment.
        let end = async move {
            let _ = watch.finish(job).await;
            // Unless the client went meanwhile: done stays done.
            state.send_if_modified(|now| {
                let following = *now == FOLLOWING;
                if following {
                    *now = DRAINING;
                }
                following
            });
            follower.finish_within(DRAIN).await;
        };
        tokio::pin!(end);
        let mut ended = false;
        let mut buffer = vec![0u8; 16 * 1024];
        loop {
            tokio::select! {
                () = &mut end, if !ended => ended = true,
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
        state.send_if_modified(|now| {
            let following = *now == FOLLOWING;
            if following {
                *now = DRAINING;
            }
            following
        });
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
}
