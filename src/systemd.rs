//! Thin typed layer over the D-Bus APIs of systemd-machined and the service manager.

use std::os::fd::OwnedFd;

use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use nix::libc;
use zbus::Connection;
use zbus_systemd::{machine1, systemd1};

pub struct Systemd {
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
    pub class: String,
    pub service: String,
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
        Ok(Systemd { machined, manager })
    }

    pub async fn version(&self) -> Result<String> {
        self.manager
            .version()
            .await
            .context("reading the systemd version")
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
            .map(|(name, class, service, _path)| MachineInfo {
                name,
                class,
                service,
            })
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

    pub async fn start_machine(&self, name: &str) -> Result<()> {
        let unit = format!("systemd-nspawn@{name}.service");
        self.start_unit(&unit)
            .await
            .with_context(|| format!("machine {name} failed to start; see journalctl -u {unit}"))
    }

    /// Asks the machine to power off cleanly (SIGRTMIN+4 to its init, like machinectl poweroff).
    pub async fn poweroff_machine(&self, name: &str) -> Result<()> {
        self.machined
            .kill_machine(name.to_string(), "leader".to_string(), libc::SIGRTMIN() + 4)
            .await
            .with_context(|| format!("powering off {name}"))
    }

    pub async fn terminate_machine(&self, name: &str) -> Result<()> {
        self.machined
            .terminate_machine(name.to_string())
            .await
            .with_context(|| format!("terminating {name}"))
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
        let mut jobs = self
            .manager
            .receive_job_removed()
            .await
            .context("subscribing to job events")?;
        let job = match self
            .manager
            .stop_unit(unit.to_string(), "replace".to_string())
            .await
        {
            Ok(job) => job,
            Err(zbus::Error::MethodError(name, _, _))
                if name.as_str() == "org.freedesktop.systemd1.NoSuchUnit" =>
            {
                return Ok(())
            }
            Err(e) => return Err(e).with_context(|| format!("stopping {unit}")),
        };
        wait_for_job(&mut jobs, &job, unit, "stopping").await
    }

    /// Opens a PTY inside the machine running `path` with `args` (argv including argv[0]).
    /// An empty path means the user's login shell.
    pub async fn open_shell(
        &self,
        name: &str,
        user: &str,
        path: &str,
        args: Vec<String>,
    ) -> Result<(OwnedFd, String)> {
        let mut env = Vec::new();
        if let Ok(term) = std::env::var("TERM") {
            env.push(format!("TERM={term}"));
        }
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
