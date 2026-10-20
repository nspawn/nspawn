//! Processes started for a client over the bus (a command inside a machine, journalctl
//! behind Logs): org.nspawn.Process objects that say what runs, let it be signalled and
//! announce its end.

use std::collections::HashMap;
use std::future::Future;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::OwnedObjectPath;

use crate::daemon::polkit;
use crate::daemon::State;
use crate::nsenter;

/// How many exited commands stay for whoever asks about them later. A client reads the
/// exit status once it has pumped the streams; the rest is history, and a service that
/// never goes idle must not keep every command it ever ran.
const EXITED_KEPT: usize = 100;

pub struct ProcessState {
    pub path: OwnedObjectPath,
    /// The user who started it: nobody else reads it or signals it.
    pub owner: u32,
    /// The bus name of the client that started it, which alone gets its signals.
    pub client: Option<String>,
    pub machine: String,
    pub argv: Vec<String>,
    /// The process's PID as the host sees it.
    pub pid: u32,
    /// The process itself, for signals: a PID may be given to someone else once the
    /// process is gone, a pidfd never is.
    pub pidfd: Option<OwnedFd>,
    pub state: Mutex<String>,
    pub exit_status: Mutex<i32>,
    pub signals: Signals,
    /// An Events stream, which a user who may only look holds a bounded number of.
    pub stream: bool,
    /// Its exit went out: only then may it be retired, its client still to read it.
    pub announced: AtomicBool,
}

/// Where the Signal method of an attached run goes, and what it remembers.
#[derive(Default)]
pub struct Signals {
    /// A booted machine: any signal asks it to power off, as Ctrl-C of an attached run
    /// stops it.
    pub poweroff: Option<String>,
    /// An app machine, and whether its program is PID 1: signals go to the program,
    /// whichever process it is by then.
    pub program: Option<(String, bool)>,
    /// The last signal sent, which the run's exit code may be made of.
    pub sent: Arc<Mutex<Option<i32>>>,
}

#[derive(Default)]
pub struct Processes {
    next: AtomicU64,
    all: Mutex<Vec<Arc<ProcessState>>>,
    /// Events streams being set up, by user: counted with the running ones.
    reserved: Mutex<HashMap<u32, usize>>,
}

/// A place among a user's Events streams, held while the stream is set up.
pub struct StreamSlot<'a> {
    processes: &'a Processes,
    uid: u32,
}

impl Drop for StreamSlot<'_> {
    fn drop(&mut self) {
        let mut reserved = self.processes.reserved.lock().unwrap();
        if let Some(n) = reserved.get_mut(&self.uid) {
            *n = n.saturating_sub(1);
        }
    }
}

impl Processes {
    pub fn paths(&self) -> Vec<OwnedObjectPath> {
        self.all
            .lock()
            .unwrap()
            .iter()
            .map(|p| p.path.clone())
            .collect()
    }

    /// A place for one more Events stream of `uid`, none past `most`: the streams that
    /// run and those being set up counted at once.
    pub fn reserve_stream(&self, uid: u32, most: usize) -> Option<StreamSlot<'_>> {
        let mut reserved = self.reserved.lock().unwrap();
        let running = self
            .all
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.owner == uid && p.stream && *p.state.lock().unwrap() == "running")
            .count();
        let taken = reserved.entry(uid).or_default();
        if running + *taken >= most {
            return None;
        }
        *taken += 1;
        Some(StreamSlot {
            processes: self,
            uid,
        })
    }

    /// Ends a command: its state, and its place after every other one, at once, so that
    /// retirement goes by the exit and never takes one at its starting place.
    fn exit(&self, process: &Arc<ProcessState>, status: i32) {
        let mut all = self.all.lock().unwrap();
        *process.exit_status.lock().unwrap() = status;
        *process.state.lock().unwrap() = "exited".to_string();
        if let Some(i) = all.iter().position(|p| Arc::ptr_eq(p, process)) {
            let process = all.remove(i);
            all.push(process);
        }
    }

    /// Forgets the commands that exited first beyond `EXITED_KEPT`; their paths, for the
    /// object server to drop. One whose exit has not gone out yet stays.
    fn retire(&self) -> Vec<OwnedObjectPath> {
        let mut all = self.all.lock().unwrap();
        let exited = |process: &ProcessState| {
            *process.state.lock().unwrap() != "running" && process.announced.load(Ordering::SeqCst)
        };
        let mut excess = all
            .iter()
            .filter(|p| exited(p))
            .count()
            .saturating_sub(EXITED_KEPT);
        let mut retired = Vec::new();
        all.retain(|process| {
            if excess > 0 && exited(process) {
                excess -= 1;
                retired.push(process.path.clone());
                false
            } else {
                true
            }
        });
        retired
    }
}

pub struct Process {
    process: Arc<ProcessState>,
    state: Arc<State>,
}

impl Process {
    /// Fails for anyone but the user who started the command, and root.
    async fn readable(&self, header: Option<&Header<'_>>) -> zbus::fdo::Result<()> {
        let uid = polkit::reader(self.state.connection(), header).await;
        if polkit::may_read(self.process.owner, uid) {
            return Ok(());
        }
        Err(zbus::fdo::Error::AccessDenied(format!(
            "this command belongs to another user ({})",
            self.process.owner
        )))
    }
}

#[zbus::interface(name = "org.nspawn.Process")]
impl Process {
    #[zbus(property)]
    async fn machine(&self, #[zbus(header)] hdr: Option<Header<'_>>) -> zbus::fdo::Result<String> {
        self.readable(hdr.as_ref()).await?;
        Ok(self.process.machine.clone())
    }

    #[zbus(property)]
    async fn argv(
        &self,
        #[zbus(header)] hdr: Option<Header<'_>>,
    ) -> zbus::fdo::Result<Vec<String>> {
        self.readable(hdr.as_ref()).await?;
        Ok(self.process.argv.clone())
    }

    /// The process's PID on the host.
    #[zbus(property)]
    async fn pid(&self, #[zbus(header)] hdr: Option<Header<'_>>) -> zbus::fdo::Result<u32> {
        self.readable(hdr.as_ref()).await?;
        Ok(self.process.pid)
    }

    /// "running" or "exited".
    #[zbus(property)]
    async fn state(&self, #[zbus(header)] hdr: Option<Header<'_>>) -> zbus::fdo::Result<String> {
        self.readable(hdr.as_ref()).await?;
        Ok(self.process.state.lock().unwrap().clone())
    }

    /// The exit code once exited; 128 plus the signal when it died of one.
    #[zbus(property)]
    async fn exit_status(&self, #[zbus(header)] hdr: Option<Header<'_>>) -> zbus::fdo::Result<i32> {
        self.readable(hdr.as_ref()).await?;
        Ok(*self.process.exit_status.lock().unwrap())
    }

    /// Sends a signal (a number; realtime ones included) to the process while it runs.
    async fn signal(&self, #[zbus(header)] hdr: Header<'_>, signal: i32) -> zbus::fdo::Result<()> {
        self.readable(Some(&hdr)).await?;
        if signal < 1 || signal > nix::libc::SIGRTMAX() {
            return Err(zbus::fdo::Error::InvalidArgs(format!(
                "signal {signal} is out of range"
            )));
        }
        if *self.process.state.lock().unwrap() != "running" {
            return Err(zbus::fdo::Error::Failed(
                "the process has exited".to_string(),
            ));
        }
        if let Some(machine) = &self.process.signals.poweroff {
            let sd = self
                .state
                .ctx
                .sd()
                .await
                .map_err(|e| zbus::fdo::Error::Failed(format!("{e:#}")))?;
            sd.poweroff_machine(machine)
                .await
                .map_err(|e| zbus::fdo::Error::Failed(format!("{e:#}")))?;
            *self.process.signals.sent.lock().unwrap() = Some(signal);
            return Ok(());
        }
        if let Some((machine, pid1)) = &self.process.signals.program {
            let sd = self
                .state
                .ctx
                .sd()
                .await
                .map_err(|e| zbus::fdo::Error::Failed(format!("{e:#}")))?;
            crate::api::machines::signal_program(sd, machine, *pid1, signal)
                .await
                .map_err(|e| zbus::fdo::Error::Failed(format!("{e:#}")))?;
            *self.process.signals.sent.lock().unwrap() = Some(signal);
            return Ok(());
        }
        let Some(pidfd) = &self.process.pidfd else {
            return Err(zbus::fdo::Error::Failed(
                "the process cannot be signalled".to_string(),
            ));
        };
        nsenter::pidfd_signal(pidfd, signal).map_err(|e| {
            zbus::fdo::Error::Failed(format!("signalling PID {}: {e}", self.process.pid))
        })?;
        *self.process.signals.sent.lock().unwrap() = Some(signal);
        Ok(())
    }

    /// The process ended with this status.
    #[zbus(signal)]
    pub async fn exited(emitter: &SignalEmitter<'_>, status: i32) -> zbus::Result<()>;
}

/// Registers a started process as an object; `wait` yields its exit status, and the
/// service counts as busy until that has been announced.
pub async fn register(
    state: &Arc<State>,
    owner: polkit::Caller,
    machine: &str,
    argv: &[String],
    pid: u32,
    pidfd: Option<OwnedFd>,
    wait: impl Future<Output = i32> + Send + 'static,
) -> zbus::Result<OwnedObjectPath> {
    register_with(
        state,
        owner,
        machine,
        argv,
        pid,
        pidfd,
        Signals::default(),
        false,
        wait,
    )
    .await
}

/// `register` of an Events stream.
pub async fn register_stream(
    state: &Arc<State>,
    owner: polkit::Caller,
    argv: &[String],
    pid: u32,
    pidfd: Option<OwnedFd>,
    wait: impl Future<Output = i32> + Send + 'static,
) -> zbus::Result<OwnedObjectPath> {
    register_with(
        state,
        owner,
        "",
        argv,
        pid,
        pidfd,
        Signals::default(),
        true,
        wait,
    )
    .await
}

/// `register`, with the Signal method of an attached run.
#[allow(clippy::too_many_arguments)]
pub async fn register_with(
    state: &Arc<State>,
    owner: polkit::Caller,
    machine: &str,
    argv: &[String],
    pid: u32,
    pidfd: Option<OwnedFd>,
    signals: Signals,
    stream: bool,
    wait: impl Future<Output = i32> + Send + 'static,
) -> zbus::Result<OwnedObjectPath> {
    let id = state.processes.next.fetch_add(1, Ordering::SeqCst) + 1;
    let path = OwnedObjectPath::try_from(format!("/org/nspawn/process/{id}"))?;
    let entry = Arc::new(ProcessState {
        path: path.clone(),
        owner: owner.uid,
        client: owner.name,
        machine: machine.to_string(),
        argv: argv.to_vec(),
        pid,
        pidfd,
        state: Mutex::new("running".to_string()),
        exit_status: Mutex::new(0),
        signals,
        stream,
        announced: AtomicBool::new(false),
    });
    state.processes.all.lock().unwrap().push(entry.clone());
    state
        .connection()
        .object_server()
        .at(
            &path,
            Process {
                process: entry.clone(),
                state: state.clone(),
            },
        )
        .await?;
    let state = state.clone();
    let busy = state.enter();
    tokio::spawn(async move {
        let status = wait.await;
        state.processes.exit(&entry, status);
        // Clients that cache properties learn of the change through PropertiesChanged.
        if let Ok(iface) = state
            .connection()
            .object_server()
            .interface::<_, Process>(&entry.path)
            .await
        {
            if let Ok(emitter) = state.emitter_to(entry.path.as_ref(), entry.client.as_deref()) {
                let _ = iface.get().await.state_changed(&emitter).await;
                let _ = iface.get().await.exit_status_changed(&emitter).await;
            }
        }
        if let Ok(emitter) = state.emitter_to(entry.path.as_ref(), entry.client.as_deref()) {
            let _ = Process::exited(&emitter, status).await;
        }
        entry.announced.store(true, Ordering::SeqCst);
        for path in state.processes.retire() {
            let _ = state
                .connection()
                .object_server()
                .remove::<Process, _>(&path)
                .await;
        }
        // Only now may the service go idle: a client is about to read the outcome.
        drop(busy);
    });
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(id: u64, state: &str) -> Arc<ProcessState> {
        Arc::new(ProcessState {
            path: OwnedObjectPath::try_from(format!("/org/nspawn/process/{id}")).unwrap(),
            owner: 0,
            client: None,
            machine: "web".to_string(),
            argv: vec!["true".to_string()],
            pid: id as u32,
            pidfd: None,
            state: Mutex::new(state.to_string()),
            exit_status: Mutex::new(0),
            signals: Signals::default(),
            stream: false,
            announced: AtomicBool::new(state != "running"),
        })
    }

    #[test]
    fn exited_commands_are_kept_up_to_a_point_and_running_ones_always() {
        let processes = Processes::default();
        for id in 1..=(EXITED_KEPT as u64 + 5) {
            let state = if id <= 3 { "running" } else { "exited" };
            processes.all.lock().unwrap().push(process(id, state));
        }
        let retired = processes.retire();
        assert_eq!(
            retired.iter().map(|p| p.as_str()).collect::<Vec<_>>(),
            ["/org/nspawn/process/4", "/org/nspawn/process/5"],
            "the oldest exited ones, never a running one"
        );
        let kept = processes.paths();
        assert_eq!(kept.len(), EXITED_KEPT + 3);
        assert!(kept.iter().any(|p| p.as_str() == "/org/nspawn/process/1"));
        assert!(kept.iter().any(|p| p.as_str() == "/org/nspawn/process/6"));
        assert!(processes.retire().is_empty());
        // One whose exit has not gone out yet is not retired, whatever its place.
        let unannounced = process(1000, "running");
        processes.all.lock().unwrap().insert(0, unannounced.clone());
        processes.exit(&unannounced, 3);
        processes.all.lock().unwrap().push(process(1001, "exited"));
        let retired = processes.retire();
        assert!(
            !retired
                .iter()
                .any(|p| p.as_str() == "/org/nspawn/process/1000"),
            "{retired:?}"
        );
        assert_eq!(*unannounced.exit_status.lock().unwrap(), 3);
    }

    #[test]
    fn only_events_streams_count_against_their_cap() {
        let processes = Processes::default();
        let of = |id: u64, stream: bool| {
            let mut p = Arc::into_inner(process(id, "running")).unwrap();
            p.owner = 1000;
            p.stream = stream;
            Arc::new(p)
        };
        for id in 1..=5 {
            processes.all.lock().unwrap().push(of(id, false));
        }
        processes.all.lock().unwrap().push(of(6, true));
        let first = processes.reserve_stream(1000, 2);
        assert!(first.is_some(), "an exec or a log follow is no stream");
        assert!(
            processes.reserve_stream(1000, 2).is_none(),
            "one runs, one is set up"
        );
        drop(first);
        assert!(processes.reserve_stream(1000, 2).is_some());
        assert!(
            processes.reserve_stream(0, 1).is_some(),
            "another user's own count"
        );
    }
}
