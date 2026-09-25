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
/// whatever its Restart= says. Its end is watched by a task of its own from the start:
/// systemd announces the end of every job on the host, and a signal stream nobody reads
/// fills up and then holds up every reply on the connection.
pub struct StopJob {
    ended: Option<tokio::task::JoinHandle<String>>,
    unit: String,
}

impl StopJob {
    /// Waits for the job to finish (at once when the unit was unknown).
    pub async fn wait(mut self) -> Result<()> {
        let Some(ended) = self.ended.as_mut() else {
            return Ok(());
        };
        match tokio::time::timeout(JOB_TIMEOUT, ended).await {
            Ok(Ok(result)) => job_outcome(&result, &self.unit, "stopping"),
            Ok(Err(e)) => bail!("watching the stop of {}: {e}", self.unit),
            Err(_) => bail!("timed out while stopping {}", self.unit),
        }
    }
}

impl Drop for StopJob {
    fn drop(&mut self) {
        if let Some(ended) = self.ended.take() {
            ended.abort();
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
            .await
            .with_context(|| format!("querying the state of {unit}"))?
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

    /// The containers machined knows. It also registers the host (".host") and the
    /// virtual machines of libvirt and others, which nspawn can neither enter nor stop.
    pub async fn list_machines(&self) -> Result<Vec<MachineInfo>> {
        Ok(containers(self.all_machines().await?))
    }

    async fn all_machines(
        &self,
    ) -> Result<Vec<(String, String, String, zbus::zvariant::OwnedObjectPath)>> {
        self.machined
            .list_machines()
            .await
            .context("listing machines through systemd-machined")
    }

    /// The service that runs `name` when it is a machine of machined but not a
    /// container (a virtual machine of libvirt-qemu, say).
    pub async fn foreign_machine(&self, name: &str) -> Result<Option<String>> {
        Ok(self
            .all_machines()
            .await?
            .into_iter()
            .find(|(n, class, _, _)| n == name && class != "container")
            .map(|(_, class, service, _)| format!("{service} ({class})")))
    }

    /// Whether machined runs a machine of that name. Virtual machines count: their
    /// names are taken, and their images are in use.
    pub async fn machine_exists(&self, name: &str) -> Result<bool> {
        Ok(self.all_machines().await?.iter().any(|m| m.0 == name))
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
        let deadline = tokio::time::Instant::now() + JOB_TIMEOUT;
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

    /// State ("opening", "running", "closing"), leader PID and start time (unix seconds)
    /// of a machine.
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

    /// The unit machined has the machine in (systemd-nspawn@NAME.service for nspawn's).
    pub async fn machine_unit(&self, name: &str) -> Result<String> {
        self.machine(name)
            .await?
            .unit()
            .await
            .with_context(|| format!("reading the unit of {name}"))
    }

    /// The cgroup of a service or scope unit, as a path below /sys/fs/cgroup.
    pub async fn control_group(&self, unit: &str) -> Result<String> {
        let path = self
            .manager
            .get_unit(unit.to_string())
            .await
            .with_context(|| format!("looking up {unit}"))?;
        let interface = if unit.ends_with(".scope") {
            "org.freedesktop.systemd1.Scope"
        } else {
            "org.freedesktop.systemd1.Service"
        };
        let proxy = zbus::Proxy::new(&self.conn, "org.freedesktop.systemd1", path, interface)
            .await
            .with_context(|| format!("connecting to {unit}"))?;
        proxy
            .get_property::<String>("ControlGroup")
            .await
            .with_context(|| format!("reading the cgroup of {unit}"))
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

    /// Clears the "failed" state a signal leaves on a unit, so that a stopped app machine
    /// is not listed by systemctl --failed.
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
        let mut jobs = self
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
                ended: Some(tokio::spawn(
                    async move { job_result(&mut jobs, &job).await },
                )),
                unit: unit.to_string(),
            }),
            Err(zbus::Error::MethodError(name, _, _))
                if name.as_str() == "org.freedesktop.systemd1.NoSuchUnit" =>
            {
                Ok(StopJob {
                    ended: None,
                    unit: unit.to_string(),
                })
            }
            Err(e) => Err(e).with_context(|| format!("stopping {unit}")),
        }
    }

    /// Asks systemd for its unit signals (PropertiesChanged of every unit), which it only
    /// sends while a client subscribed; the subscription ends with the connection.
    pub async fn subscribe(&self) -> Result<()> {
        match self.manager.subscribe().await {
            Ok(()) => Ok(()),
            Err(zbus::Error::MethodError(name, _, _))
                if name.as_str() == "org.freedesktop.systemd1.AlreadySubscribed" =>
            {
                Ok(())
            }
            Err(e) => Err(e).context("subscribing to the signals of systemd"),
        }
    }

    /// The object path of a unit, loading it if needed.
    pub async fn unit_path(&self, unit: &str) -> Result<zbus::zvariant::OwnedObjectPath> {
        self.manager
            .load_unit(unit.to_string())
            .await
            .with_context(|| format!("loading {unit}"))
    }

    async fn service(&self, unit: &str) -> Result<systemd1::ServiceProxy<'static>> {
        systemd1::ServiceProxy::builder(&self.conn)
            .path(self.unit_path(unit).await?)?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .with_context(|| format!("connecting to {unit}"))
    }

    /// The PID of the main process of the unit's current or last run (0 before any).
    pub async fn exec_main_pid(&self, unit: &str) -> Result<u32> {
        self.service(unit)
            .await?
            .exec_main_pid()
            .await
            .with_context(|| format!("reading the main process of {unit}"))
    }

    /// The invocation ID of the unit's current or last run, in hex.
    pub async fn invocation_id(&self, unit: &str) -> Result<String> {
        let proxy = systemd1::UnitProxy::builder(&self.conn)
            .path(self.unit_path(unit).await?)?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .with_context(|| format!("connecting to {unit}"))?;
        let id = proxy
            .invocation_id()
            .await
            .with_context(|| format!("reading the invocation of {unit}"))?;
        Ok(hex::encode(id))
    }

    /// Starts a transient unit with these properties and returns without waiting.
    /// `mode` as StartTransientUnit takes it: "fail" when a job for the unit is pending,
    /// "replace" to take over.
    pub async fn start_transient(
        &self,
        unit: &str,
        properties: Vec<(String, zbus::zvariant::OwnedValue)>,
        mode: &str,
    ) -> Result<()> {
        self.manager
            .start_transient_unit(unit.to_string(), mode.to_string(), properties, Vec::new())
            .await
            .with_context(|| format!("starting {unit}"))?;
        Ok(())
    }

    /// Queues a stop job for a unit and returns at once, without watching the job.
    pub async fn queue_stop(&self, unit: &str) -> Result<()> {
        self.manager
            .stop_unit(unit.to_string(), "replace".to_string())
            .await
            .with_context(|| format!("stopping {unit}"))?;
        Ok(())
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

fn containers(
    raw: Vec<(String, String, String, zbus::zvariant::OwnedObjectPath)>,
) -> Vec<MachineInfo> {
    raw.into_iter()
        .filter(|(_, class, _, _)| class == "container")
        .map(|(name, _, _, _)| MachineInfo { name })
        .collect()
}

/// How long a start or stop job of a machine may take.
const JOB_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);

/// The result systemd gives `job` when it ends ("done", "failed", ...).
async fn job_result(
    jobs: &mut zbus_systemd::systemd1::JobRemovedStream,
    job: &zbus::zvariant::OwnedObjectPath,
) -> String {
    while let Some(event) = jobs.next().await {
        if let Ok(args) = event.args() {
            if args.job() == job {
                return args.result().to_string();
            }
        }
    }
    "lost".to_string()
}

fn job_outcome(result: &str, unit: &str, verb: &str) -> Result<()> {
    match result {
        "done" | "skipped" => Ok(()),
        _ => bail!("{verb} {unit} ended with result {result}"),
    }
}

async fn wait_for_job(
    jobs: &mut zbus_systemd::systemd1::JobRemovedStream,
    job: &zbus::zvariant::OwnedObjectPath,
    unit: &str,
    verb: &str,
) -> Result<()> {
    match tokio::time::timeout(JOB_TIMEOUT, job_result(jobs, job)).await {
        Ok(result) => job_outcome(&result, unit, verb),
        Err(_) => bail!("timed out while {verb} {unit}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_job_that_did_not_run_is_fine_one_that_failed_is_not() {
        assert!(job_outcome("done", "u.service", "stopping").is_ok());
        assert!(job_outcome("skipped", "u.service", "stopping").is_ok());
        for bad in ["failed", "canceled", "timeout", "lost"] {
            let e = job_outcome(bad, "u.service", "stopping").unwrap_err();
            assert_eq!(
                e.to_string(),
                format!("stopping u.service ended with result {bad}")
            );
        }
    }

    #[test]
    fn only_containers_are_machines_here() {
        let path = |n: &str| {
            zbus::zvariant::OwnedObjectPath::try_from(format!(
                "/org/freedesktop/machine1/machine/{n}"
            ))
            .unwrap()
        };
        let raw = vec![
            (
                ".host".to_string(),
                "host".to_string(),
                "".to_string(),
                path("_2ehost"),
            ),
            (
                "web".to_string(),
                "container".to_string(),
                "systemd-nspawn".to_string(),
                path("web"),
            ),
            (
                "qemu-1-vm".to_string(),
                "vm".to_string(),
                "libvirt-qemu".to_string(),
                path("qemu"),
            ),
        ];
        let names: Vec<String> = containers(raw).into_iter().map(|m| m.name).collect();
        assert_eq!(names, ["web"]);
    }
}
