//! Commands started inside machines over the bus: org.nspawn.Process objects that say
//! what runs, let it be signalled and announce its end.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use zbus::object_server::SignalEmitter;
use zbus::zvariant::OwnedObjectPath;

use crate::daemon::State;
use crate::nsenter;

pub struct ProcessState {
    pub path: OwnedObjectPath,
    pub machine: String,
    pub argv: Vec<String>,
    /// The command's PID as the host sees it, 0 when unknown.
    pub pid: u32,
    pub state: Mutex<String>,
    pub exit_status: Mutex<i32>,
}

#[derive(Default)]
pub struct Processes {
    next: AtomicU64,
    running: AtomicUsize,
    all: Mutex<Vec<Arc<ProcessState>>>,
}

impl Processes {
    pub fn running(&self) -> usize {
        self.running.load(Ordering::SeqCst)
    }

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
}

#[zbus::interface(name = "org.nspawn.Process")]
impl Process {
    #[zbus(property)]
    fn machine(&self) -> String {
        self.process.machine.clone()
    }

    #[zbus(property)]
    fn argv(&self) -> Vec<String> {
        self.process.argv.clone()
    }

    /// The command's PID on the host; 0 when it could not be told.
    #[zbus(property)]
    fn pid(&self) -> u32 {
        self.process.pid
    }

    /// "running" or "exited".
    #[zbus(property)]
    fn state(&self) -> String {
        self.process.state.lock().unwrap().clone()
    }

    /// The exit code once exited; 128 plus the signal when it died of one.
    #[zbus(property)]
    fn exit_status(&self) -> i32 {
        *self.process.exit_status.lock().unwrap()
    }

    /// Sends a signal to the command.
    fn signal(&self, signal: i32) -> zbus::fdo::Result<()> {
        if self.process.pid == 0 {
            return Err(zbus::fdo::Error::Failed(
                "the command's PID is not known".to_string(),
            ));
        }
        let signal = nix::sys::signal::Signal::try_from(signal)
            .map_err(|e| zbus::fdo::Error::InvalidArgs(format!("signal {signal}: {e}")))?;
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(self.process.pid as i32), signal).map_err(
            |e| zbus::fdo::Error::Failed(format!("signalling PID {}: {e}", self.process.pid)),
        )
    }

    /// The command ended with this status.
    #[zbus(signal)]
    pub async fn exited(emitter: &SignalEmitter<'_>, status: i32) -> zbus::Result<()>;
}

/// Registers a started command as an object and reaps it in the background.
pub async fn register(
    state: &Arc<State>,
    machine: &str,
    argv: &[String],
    process: &nsenter::Process,
) -> zbus::Result<OwnedObjectPath> {
    let id = state.processes.next.fetch_add(1, Ordering::SeqCst) + 1;
    let path = OwnedObjectPath::try_from(format!("/org/nspawn/process/{id}"))?;
    let entry = Arc::new(ProcessState {
        path: path.clone(),
        machine: machine.to_string(),
        argv: argv.to_vec(),
        pid: process.pid.unwrap_or(0),
        state: Mutex::new("running".to_string()),
        exit_status: Mutex::new(0),
    });
    state.processes.all.lock().unwrap().push(entry.clone());
    state.processes.running.fetch_add(1, Ordering::SeqCst);
    state
        .connection()
        .object_server()
        .at(
            &path,
            Process {
                process: entry.clone(),
            },
        )
        .await?;
    let helper = process.helper;
    let state = state.clone();
    tokio::spawn(async move {
        let status = tokio::task::spawn_blocking(move || nsenter::wait(helper))
            .await
            .ok()
            .and_then(|r| r.ok())
            .unwrap_or(126);
        *entry.exit_status.lock().unwrap() = status;
        *entry.state.lock().unwrap() = "exited".to_string();
        state.processes.running.fetch_sub(1, Ordering::SeqCst);
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
    });
    Ok(path)
}
