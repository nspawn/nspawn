use std::time::{Duration, Instant};

use anyhow::{bail, Result};

use crate::cli::{ExecArgs, ShellArgs, StartArgs, StopArgs};
use crate::output::table;
use crate::pty;
use crate::systemd::Systemd;

pub async fn ls() -> Result<()> {
    let sd = Systemd::connect().await?;
    let mut machines = sd.list_machines().await?;
    machines.retain(|m| !m.name.starts_with('.'));
    machines.sort_by(|a, b| a.name.cmp(&b.name));
    let mut rows = Vec::new();
    for m in machines {
        let os = sd
            .machine_os(&m.name)
            .await
            .unwrap_or_else(|| "-".to_string());
        rows.push(vec![m.name, m.class, m.service, os]);
    }
    println!("{}", table(&["MACHINE", "CLASS", "SERVICE", "OS"], rows));
    Ok(())
}

pub async fn start(args: StartArgs) -> Result<()> {
    let sd = Systemd::connect().await?;
    if sd.machine_exists(&args.name).await? {
        bail!("machine {} is already running", args.name);
    }
    sd.start_machine(&args.name).await?;
    if args.wait {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !sd.machine_exists(&args.name).await? {
            if Instant::now() > deadline {
                bail!("machine {} did not register within 30 seconds; see journalctl -u systemd-nspawn@{}", args.name, args.name);
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
    println!("started {}", args.name);
    Ok(())
}

pub async fn stop(args: StopArgs) -> Result<()> {
    let sd = Systemd::connect().await?;
    if !sd.machine_exists(&args.name).await? {
        bail!("machine {} is not running", args.name);
    }
    if args.force {
        sd.terminate_machine(&args.name).await?;
    } else {
        sd.poweroff_machine(&args.name).await?;
    }
    if args.wait {
        let deadline = Instant::now() + Duration::from_secs(60);
        while sd.machine_exists(&args.name).await? {
            if Instant::now() > deadline {
                bail!(
                    "machine {} is still running after 60 seconds; use --force",
                    args.name
                );
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        // Let the service finish its own teardown so that the image can be removed right away.
        sd.stop_unit(&format!("systemd-nspawn@{}.service", args.name))
            .await?;
    }
    println!("stopped {}", args.name);
    Ok(())
}

pub async fn exec(args: ExecArgs) -> Result<()> {
    let sd = Systemd::connect().await?;
    if !sd.machine_exists(&args.machine).await? {
        bail!("machine {} is not running", args.machine);
    }
    let path = args.command[0].clone();
    if !path.starts_with('/') {
        bail!(
            "the command must be an absolute path inside the machine, for example /usr/bin/{path}"
        );
    }
    let (fd, _pty) =
        open_shell_when_ready(&sd, &args.machine, &args.user, &path, args.command.clone()).await?;
    pty::run_session(fd)
}

pub async fn shell(args: ShellArgs) -> Result<()> {
    let sd = Systemd::connect().await?;
    if !sd.machine_exists(&args.machine).await? {
        bail!("machine {} is not running", args.machine);
    }
    let (fd, _pty) = open_shell_when_ready(&sd, &args.machine, &args.user, "", Vec::new()).await?;
    pty::run_session(fd)
}

/// A machine that has just been started has no D-Bus yet for a few seconds; retry
/// OpenMachineShell for a while instead of failing right away.
async fn open_shell_when_ready(
    sd: &Systemd,
    machine: &str,
    user: &str,
    path: &str,
    args: Vec<String>,
) -> Result<(std::os::fd::OwnedFd, String)> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match sd.open_shell(machine, user, path, args.clone()).await {
            Ok(session) => return Ok(session),
            Err(e) if Instant::now() < deadline && format!("{e:#}").contains("no system bus") => {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(e) => return Err(e),
        }
    }
}
