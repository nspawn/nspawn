//! Thin typed layer over the D-Bus APIs of systemd-machined and the service manager.

use std::os::fd::OwnedFd;

use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use nix::libc;
use zbus::Connection;
use zbus_systemd::{machine1, systemd1};

#[derive(Clone)]
pub struct Systemd {
    conn: Connection,
    machined: machine1::ManagerProxy<'static>,
    manager: systemd1::ManagerProxy<'static>,
}

#[derive(Debug, Clone)]
pub struct ImageInfo {
    pub name: String,
    pub kind: String,
    pub read_only: bool,
    pub usage: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct MachineInfo {
    pub name: String,
}

/// Load, active and sub state of a unit, as `systemctl status` shows them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitState {
    pub load: String,
    pub active: String,
    pub sub: String,
}

impl UnitState {
    fn unknown() -> Self {
        UnitState {
            load: "not-found".to_string(),
            active: "inactive".to_string(),
            sub: "dead".to_string(),
        }
    }

    /// Between two runs of a unit with Restart=: its program ended and systemd waits
    /// before starting it again.
    pub fn restarting(&self) -> bool {
        self.active == "activating" && self.sub.starts_with("auto-restart")
    }

    /// Running, or on its way up (a restart included).
    pub fn busy(&self) -> bool {
        matches!(self.active.as_str(), "active" | "activating" | "reloading")
    }
}

/// A stop job systemd accepted; once it is queued the unit is not restarted any more,
/// whatever its Restart= says.
pub struct StopJob {
    jobs: Option<zbus_systemd::systemd1::JobRemovedStream>,
    job: Option<zbus::zvariant::OwnedObjectPath>,
    unit: String,
}

impl StopJob {
    /// Waits for the job to finish (at once when the unit was unknown).
    pub async fn wait(self) -> Result<()> {
        match (self.jobs, self.job) {
            (Some(mut jobs), Some(job)) => {
                wait_for_job(&mut jobs, &job, &self.unit, "stopping").await
            }
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MachineDetails {
    pub state: String,
    pub leader: u32,
    pub started: u64,
}

impl Systemd {
    pub async fn connect() -> Result<Self> {
        let conn = Connection::system()
            .await
            .context("connecting to the system bus")?;
        let machined = machine1::ManagerProxy::new(&conn)
            .await
            .context("connecting to systemd-machined")?;
        let manager = systemd1::ManagerProxy::new(&conn)
            .await
            .context("connecting to systemd")?;
        Ok(Systemd {
            conn,
            machined,
            manager,
        })
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Load state and active state of a unit; unknown units read as not-found/inactive.
    pub async fn unit_state(&self, unit: &str) -> Result<(String, String)> {
        let units = self
            .manager
            .list_units_by_names(vec![unit.to_string()])
            .await
            .with_context(|| format!("querying the state of {unit}"))?;
        Ok(units
            .into_iter()
            .next()
            .map(|u| (u.2, u.3))
            .unwrap_or_else(|| ("not-found".to_string(), "inactive".to_string())))
    }

    /// Load, active and sub state of several units in one call; unknown units read as
    /// not-found/inactive/dead.
    pub async fn unit_states(
        &self,
        units: &[String],
    ) -> Result<std::collections::HashMap<String, UnitState>> {
        let mut states: std::collections::HashMap<String, UnitState> = units
            .iter()
            .map(|u| (u.clone(), UnitState::unknown()))
            .collect();
        if units.is_empty() {
            return Ok(states);
        }
        let listed = self
            .manager
            .list_units_by_names(units.to_vec())
            .await
            .context("querying the state of the machines' units")?;
        for u in listed {
            states.insert(
                u.0,
                UnitState {
                    load: u.2,
                    active: u.3,
                    sub: u.4,
                },
            );
        }
        Ok(states)
    }

    /// The state of one unit, sub state included.
    pub async fn unit_status(&self, unit: &str) -> Result<UnitState> {
        Ok(self
            .unit_states(&[unit.to_string()])
            .await?
            .remove(unit)
            .unwrap_or_else(UnitState::unknown))
    }

    /// Enables a machine's unit at boot the way `machinectl enable` does: the unit and
    /// machines.target, which pulls it in. Returns whether anything changed, i.e. whether
    /// a daemon-reload is due.
    pub async fn enable_unit(&self, unit: &str) -> Result<bool> {
        let (_, changes) = self
            .manager
            .enable_unit_files(
                vec![unit.to_string(), "machines.target".to_string()],
                false,
                false,
            )
            .await
            .with_context(|| format!("enabling {unit} at boot"))?;
        Ok(!changes.is_empty())
    }

    /// Undoes `enable_unit` for the machine's unit (machines.target stays, as machinectl
    /// leaves it). Unknown units are not an error. Returns whether anything changed.
    pub async fn disable_unit(&self, unit: &str) -> Result<bool> {
        match self
            .manager
            .disable_unit_files(vec![unit.to_string()], false)
            .await
        {
            Ok(changes) => Ok(!changes.is_empty()),
            Err(zbus::Error::MethodError(name, _, _))
                if name.as_str() == "org.freedesktop.systemd1.NoSuchUnit" =>
            {
                Ok(false)
            }
            Err(e) => Err(e).with_context(|| format!("disabling {unit} at boot")),
        }
    }

    /// The first host UID of a running machine's user namespace (0 without one).
    pub async fn machine_uid_shift(&self, name: &str) -> Result<u32> {
        self.machined
            .get_machine_uid_shift(name.to_string())
            .await
            .with_context(|| format!("reading the UID shift of {name}"))
    }

    /// True when `name` is owned on the system bus, i.e. that service is running.
    pub async fn name_has_owner(&self, name: &str) -> bool {
        let Ok(dbus) = zbus::fdo::DBusProxy::new(&self.conn).await else {
            return false;
        };
        let Ok(name) = zbus::names::BusName::try_from(name) else {
            return false;
        };
        dbus.name_has_owner(name).await.unwrap_or(false)
    }

    pub async fn version(&self) -> Result<String> {
        self.manager
            .version()
            .await
            .context("reading the systemd version")
    }

    /// Major version of the running systemd, when it can be read.
    pub async fn major(&self) -> Option<u32> {
        self.version()
            .await
            .ok()
            .and_then(|v| crate::backend::systemd_major(&v))
    }

    /// argv of the first ExecStart= of a unit as systemd has it loaded: specifiers
    /// expanded, drop-ins applied.
    pub async fn exec_start(&self, unit: &str) -> Result<Vec<String>> {
        let path = self
            .manager
            .load_unit(unit.to_string())
            .await
            .with_context(|| format!("loading {unit}"))?;
        let service = systemd1::ServiceProxy::builder(&self.conn)
            .path(path)?
            .build()
            .await
            .with_context(|| format!("connecting to {unit}"))?;
        let execs = service
            .exec_start()
            .await
            .with_context(|| format!("reading ExecStart= of {unit}"))?;
        execs
            .into_iter()
            .next()
            .map(|exec| exec.1)
            .with_context(|| format!("{unit} has no ExecStart="))
    }

    pub async fn list_images(&self) -> Result<Vec<ImageInfo>> {
        let raw = self
            .machined
            .list_images()
            .await
            .context("listing images through systemd-machined")?;
        Ok(raw
            .into_iter()
            .map(
                |(name, kind, read_only, _created, _modified, usage, _path)| ImageInfo {
                    name,
                    kind,
                    read_only,
                    usage: if usage == u64::MAX { None } else { Some(usage) },
                },
            )
            .collect())
    }

    pub async fn list_machines(&self) -> Result<Vec<MachineInfo>> {
        let raw = self
            .machined
            .list_machines()
            .await
            .context("listing machines through systemd-machined")?;
        Ok(raw
            .into_iter()
            .map(|(name, _class, _service, _path)| MachineInfo { name })
            .collect())
    }

    pub async fn machine_exists(&self, name: &str) -> Result<bool> {
        Ok(self.list_machines().await?.iter().any(|m| m.name == name))
    }

    pub async fn machine_os(&self, name: &str) -> Option<String> {
        let pairs = self
            .machined
            .get_machine_os_release(name.to_string())
            .await
            .ok()?;
        pairs
            .into_iter()
            .find(|(k, _)| k == "PRETTY_NAME")
            .map(|(_, v)| v)
    }

    /// Starts a machine's unit. Returns true when its program ended during the start
    /// and the unit is waiting to start it again (Restart=): systemd keeps the start job
    /// open across restarts, so it is not waited for then.
    pub async fn start_machine(&self, name: &str) -> Result<bool> {
        let unit = format!("systemd-nspawn@{name}.service");
        self.start_unit_or_restart(&unit)
            .await
            .with_context(|| format!("machine {name} failed to start; see journalctl -u {unit}"))
    }

    async fn start_unit_or_restart(&self, unit: &str) -> Result<bool> {
        let mut jobs = self
            .manager
            .receive_job_removed()
            .await
            .context("subscribing to job events")?;
        let job = self
            .manager
            .start_unit(unit.to_string(), "replace".to_string())
            .await
            .with_context(|| format!("starting {unit}"))?;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(90);
        let mut poll = tokio::time::interval(std::time::Duration::from_millis(250));
        loop {
            tokio::select! {
                event = jobs.next() => {
                    let Some(event) = event else { bail!("lost the job events while starting {unit}") };
                    let Ok(args) = event.args() else { continue };
                    if args.job() != &job { continue }
                    let result = args.result().to_string();
                    if result == "done" || result == "skipped" {
                        return Ok(false);
                    }
                    bail!("starting {unit} ended with result {result}");
                }
                _ = poll.tick() => {
                    if self.unit_status(unit).await?.restarting() {
                        return Ok(true);
                    }
                }
                _ = tokio::time::sleep_until(deadline) => bail!("timed out while starting {unit}"),
            }
        }
    }

    /// State ("opening", "running", "closing"), leader PID and start time (unix seconds)
    /// of a machine.
    async fn machine(&self, name: &str) -> Result<machine1::MachineProxy<'static>> {
        let path = self
            .machined
            .get_machine(name.to_string())
            .await
            .with_context(|| format!("looking up machine {name}"))?;
        machine1::MachineProxy::builder(&self.conn)
            .path(path)?
            .build()
            .await
            .with_context(|| format!("connecting to machine {name}"))
    }

    pub async fn machine_details(&self, name: &str) -> Result<MachineDetails> {
        let machine = self.machine(name).await?;
        Ok(MachineDetails {
            state: machine.state().await.unwrap_or_else(|_| "-".to_string()),
            leader: machine.leader().await.unwrap_or(0),
            started: machine
                .timestamp()
                .await
                .map(|usec| usec / 1_000_000)
                .unwrap_or(0),
        })
    }

    /// PID of the machine's leader (its PID 1 as seen from the host).
    pub async fn machine_leader(&self, name: &str) -> Result<u32> {
        self.machine(name)
            .await?
            .leader()
            .await
            .with_context(|| format!("reading the leader of {name}"))
    }

    /// Interface indices of the machine's host-side network interfaces.
    pub async fn machine_interfaces(&self, name: &str) -> Result<Vec<i32>> {
        self.machine(name)
            .await?
            .network_interfaces()
            .await
            .with_context(|| format!("reading the network interfaces of {name}"))
    }

    /// Sends `signal` to the leader or to all processes ("all") of a machine.
    pub async fn kill_machine(&self, name: &str, who: &str, signal: i32) -> Result<()> {
        self.machined
            .kill_machine(name.to_string(), who.to_string(), signal)
            .await
            .with_context(|| format!("signalling {name}"))
    }

    /// Asks the machine to power off cleanly (SIGRTMIN+4 to its init, like machinectl poweroff).
    pub async fn poweroff_machine(&self, name: &str) -> Result<()> {
        self.machined
            .kill_machine(name.to_string(), "leader".to_string(), libc::SIGRTMIN() + 4)
            .await
            .with_context(|| format!("powering off {name}"))
    }

    /// Clears the "failed" state a unit keeps after its process died of a signal, so that
    /// a docker-style stop does not leave every app machine listed by systemctl --failed.
    pub async fn reset_failed(&self, unit: &str) -> Result<()> {
        match self.manager.reset_failed_unit(unit.to_string()).await {
            Ok(()) => Ok(()),
            Err(zbus::Error::MethodError(name, _, _))
                if name.as_str() == "org.freedesktop.systemd1.NoSuchUnit" =>
            {
                Ok(())
            }
            Err(e) => Err(e).with_context(|| format!("resetting {unit}")),
        }
    }

    pub async fn remove_image(&self, name: &str) -> Result<()> {
        self.machined
            .remove_image(name.to_string())
            .await
            .with_context(|| format!("removing image {name} through systemd-machined"))
    }

    pub async fn reload(&self) -> Result<()> {
        self.manager
            .reload()
            .await
            .context("reloading systemd units")
    }

    /// Starts a unit and waits until systemd reports the start job finished.
    pub async fn start_unit(&self, unit: &str) -> Result<()> {
        let mut jobs = self
            .manager
            .receive_job_removed()
            .await
            .context("subscribing to job events")?;
        let job = self
            .manager
            .start_unit(unit.to_string(), "replace".to_string())
            .await
            .with_context(|| format!("starting {unit}"))?;
        wait_for_job(&mut jobs, &job, unit, "starting").await
    }

    /// Stops a unit and waits until systemd reports the stop job finished. Unknown units
    /// (never loaded) are treated as already stopped.
    pub async fn stop_unit(&self, unit: &str) -> Result<()> {
        self.stop_unit_job(unit).await?.wait().await
    }

    /// Queues a stop job for a unit without waiting for it. From the moment systemd
    /// accepts it the unit is not restarted any more, whatever its Restart= says.
    pub async fn stop_unit_job(&self, unit: &str) -> Result<StopJob> {
        let jobs = self
            .manager
            .receive_job_removed()
            .await
            .context("subscribing to job events")?;
        match self
            .manager
            .stop_unit(unit.to_string(), "replace".to_string())
            .await
        {
            Ok(job) => Ok(StopJob {
                jobs: Some(jobs),
                job: Some(job),
                unit: unit.to_string(),
            }),
            Err(zbus::Error::MethodError(name, _, _))
                if name.as_str() == "org.freedesktop.systemd1.NoSuchUnit" =>
            {
                Ok(StopJob {
                    jobs: None,
                    job: None,
                    unit: unit.to_string(),
                })
            }
            Err(e) => Err(e).with_context(|| format!("stopping {unit}")),
        }
    }

    /// Opens a PTY inside the machine running `path` with `args` (argv including argv[0])
    /// and `env`. An empty path means the user's login shell.
    pub async fn open_shell(
        &self,
        name: &str,
        user: &str,
        path: &str,
        args: Vec<String>,
        env: Vec<String>,
    ) -> Result<(OwnedFd, String)> {
        let (fd, pty) = self
            .machined
            .open_machine_shell(
                name.to_string(),
                user.to_string(),
                path.to_string(),
                args,
                env,
            )
            .await
            .with_context(|| format!("opening a shell in {name}"))?;
        Ok((fd.into(), pty))
    }
}

async fn wait_for_job(
    jobs: &mut zbus_systemd::systemd1::JobRemovedStream,
    job: &zbus::zvariant::OwnedObjectPath,
    unit: &str,
    verb: &str,
) -> Result<()> {
    let wait = async {
        while let Some(event) = jobs.next().await {
            if let Ok(args) = event.args() {
                if args.job() == job {
                    return args.result().to_string();
                }
            }
        }
        "lost".to_string()
    };
    match tokio::time::timeout(std::time::Duration::from_secs(90), wait).await {
        Ok(result) if result == "done" || result == "skipped" => Ok(()),
        Ok(result) => bail!("{verb} {unit} ended with result {result}"),
        Err(_) => bail!("timed out while {verb} {unit}"),
    }
}
