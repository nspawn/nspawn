//! The D-Bus service: org.nspawn on the system bus, started by the bus when a client
//! calls it and gone again after a while without work. It serves the same library the
//! command line uses (`api`), so both do exactly the same things.

pub mod install;
pub mod jobs;
pub mod manager;
pub mod polkit;
pub mod processes;
pub mod values;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use futures_util::StreamExt;
use zbus::object_server::SignalEmitter;
use zbus_systemd::machine1;

use crate::api::{require_root, Context};
use crate::config::Config;
use crate::daemon::manager::Manager;

pub const BUS_NAME: &str = "org.nspawn";
pub const MANAGER_PATH: &str = "/org/nspawn";

/// What the interfaces share: the library context, the jobs, and how busy the service is.
pub struct State {
    pub ctx: Arc<Context>,
    pub jobs: jobs::Jobs,
    pub processes: processes::Processes,
    connection: OnceLock<zbus::Connection>,
    /// Calls, jobs and processes in progress; the service does not exit while one runs.
    busy: AtomicUsize,
    last_activity: Mutex<Instant>,
}

impl State {
    pub fn new(ctx: Context) -> Self {
        State {
            ctx: Arc::new(ctx),
            jobs: jobs::Jobs::default(),
            processes: processes::Processes::default(),
            connection: OnceLock::new(),
            busy: AtomicUsize::new(0),
            last_activity: Mutex::new(Instant::now()),
        }
    }

    pub fn connection(&self) -> &zbus::Connection {
        self.connection
            .get()
            .expect("the connection is set before the service takes calls")
    }

    /// A signal emitter for the manager object.
    pub fn emitter(&self) -> zbus::Result<SignalEmitter<'_>> {
        SignalEmitter::new(self.connection(), MANAGER_PATH)
    }

    /// Marks a call in progress until the guard is dropped.
    pub fn enter(self: &Arc<Self>) -> Busy {
        self.busy.fetch_add(1, Ordering::SeqCst);
        *self.last_activity.lock().unwrap() = Instant::now();
        Busy(self.clone())
    }

    fn idle_for(&self) -> Duration {
        self.last_activity.lock().unwrap().elapsed()
    }
}

pub struct Busy(Arc<State>);

impl Drop for Busy {
    fn drop(&mut self) {
        *self.0.last_activity.lock().unwrap() = Instant::now();
        self.0.busy.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Serves org.nspawn until `idle_exit` passes without a call, a job or a process; None
/// serves forever.
pub async fn run(config: Config, idle_exit: Option<Duration>) -> Result<()> {
    require_root("daemon")?;
    let state = Arc::new(State::new(Context::new(config)));
    // Earlier versions left the state directory open to everyone.
    if let Err(e) = state.ctx.store.protect() {
        eprintln!("warning: {e:#}");
    }
    let connection = zbus::connection::Builder::system()?
        .serve_at(MANAGER_PATH, Manager::new(state.clone()))?
        .build()
        .await
        .context("connecting to the system bus")?;
    if state.connection.set(connection.clone()).is_err() {
        unreachable!("the connection is set once");
    }
    // The name comes last: nobody can call before it exists, and by then the objects
    // have their connection.
    connection
        .request_name(BUS_NAME)
        .await
        .with_context(|| format!("claiming {BUS_NAME} on the system bus"))?;
    tokio::spawn(relay_machine_signals(state.clone()));
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let Some(idle_exit) = idle_exit else {
            continue;
        };
        if state.busy.load(Ordering::SeqCst) == 0 && state.idle_for() >= idle_exit {
            break;
        }
    }
    // Dropping the connection releases the name; the bus starts the service again when
    // the next client calls.
    drop(connection);
    Ok(())
}

/// machined announces every machine; only the ones nspawn installed are relayed, so that
/// a client of org.nspawn never has to know machined.
async fn relay_machine_signals(state: Arc<State>) {
    let Ok(machined) = machine1::ManagerProxy::new(state.connection()).await else {
        return;
    };
    let (Ok(mut new), Ok(mut removed)) = (
        machined.receive_machine_new().await,
        machined.receive_machine_removed().await,
    ) else {
        return;
    };
    loop {
        let (name, started) = tokio::select! {
            Some(signal) = new.next() => match signal.args() {
                Ok(args) => (args.machine.to_string(), true),
                Err(_) => continue,
            },
            Some(signal) = removed.next() => match signal.args() {
                Ok(args) => (args.machine.to_string(), false),
                Err(_) => continue,
            },
            else => break,
        };
        if !state.ctx.store.load_image(&name).ok().flatten().is_some() {
            continue;
        }
        let Ok(emitter) = state.emitter() else {
            continue;
        };
        let _ = if started {
            Manager::machine_started(&emitter, &name).await
        } else {
            Manager::machine_stopped(&emitter, &name).await
        };
    }
}
