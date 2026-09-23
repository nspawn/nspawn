//! Processes started for a client over the bus (a command inside a machine, journalctl
//! behind Logs): org.nspawn.Process objects that say what runs, let it be signalled and
//! announce its end.

use std::future::Future;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::OwnedObjectPath;

use crate::daemon::polkit;
use crate::daemon::State;
use crate::nsenter;

pub struct ProcessState {
    pub path: OwnedObjectPath,
    /// The user who started it: nobody else reads it or signals it.
    pub owner: u32,
    pub machine: String,
    pub argv: Vec<String>,
    /// The process's PID as the host sees it.
    pub pid: u32,
    /// The process itself, for signals: a PID may be given to someone else once the
    /// process is gone, a pidfd never is.
    pub pidfd: Option<OwnedFd>,
    pub state: Mutex<String>,
    pub exit_status: Mutex<i32>,
}

#[derive(Default)]
pub struct Processes {
    next: AtomicU64,
    all: Mutex<Vec<Arc<ProcessState>>>,
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
}

pub struct Process {
    process: Arc<ProcessState>,
    state: Arc<State>,
}

impl Process {
    /// Fails for anyone but the user who started the command, and root.
    async fn readable(&self, header: Option<&Header<'_>>) -> zbus::fdo::Result<()> {
        let uid = polkit::header_uid(self.state.connection(), header).await;
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
        let Some(pidfd) = &self.process.pidfd else {
            return Err(zbus::fdo::Error::Failed(
                "the process cannot be signalled".to_string(),
            ));
        };
        nsenter::pidfd_signal(pidfd, signal).map_err(|e| {
            zbus::fdo::Error::Failed(format!("signalling PID {}: {e}", self.process.pid))
        })
    }

    /// The process ended with this status.
    #[zbus(signal)]
    pub async fn exited(emitter: &SignalEmitter<'_>, status: i32) -> zbus::Result<()>;
}

/// Registers a started process as an object; `wait` yields its exit status, and the
/// service counts as busy until that has been announced.
pub async fn register(
    state: &Arc<State>,
    owner: u32,
    machine: &str,
    argv: &[String],
    pid: u32,
    pidfd: Option<OwnedFd>,
    wait: impl Future<Output = i32> + Send + 'static,
) -> zbus::Result<OwnedObjectPath> {
    let id = state.processes.next.fetch_add(1, Ordering::SeqCst) + 1;
    let path = OwnedObjectPath::try_from(format!("/org/nspawn/process/{id}"))?;
    let entry = Arc::new(ProcessState {
        path: path.clone(),
        owner,
        machine: machine.to_string(),
        argv: argv.to_vec(),
        pid,
        pidfd,
        state: Mutex::new("running".to_string()),
        exit_status: Mutex::new(0),
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
        *entry.exit_status.lock().unwrap() = status;
        *entry.state.lock().unwrap() = "exited".to_string();
        // Clients that cache properties learn of the change through PropertiesChanged.
        if let Ok(iface) = state
            .connection()
            .object_server()
            .interface::<_, Process>(&entry.path)
            .await
        {
            let emitter = iface.signal_emitter();
            let _ = iface.get().await.state_changed(emitter).await;
            let _ = iface.get().await.exit_status_changed(emitter).await;
        }
        if let Ok(emitter) = SignalEmitter::new(state.connection(), entry.path.clone()) {
            let _ = Process::exited(&emitter, status).await;
        }
        // Only now may the service go idle: a client is about to read the outcome.
        drop(busy);
    });
    Ok(path)
}
