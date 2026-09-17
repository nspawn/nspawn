use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::backend::MANAGED_NS_SOCKETS;
use crate::cli::{BackendChoice, ExecArgs, LogsArgs, PsArgs, ShellArgs, StartArgs, StopArgs};
use crate::config::Config;
use crate::hostnet;
use crate::nsenter;
use crate::oci::Mode;
use crate::output::{human_duration, table};
use crate::pty;
use crate::settings::{self, MachineSettings, Network};
use crate::store::{now_unix, ImageRecord, Store};
use crate::systemd::Systemd;

pub async fn ls(args: PsArgs, config: &Config) -> Result<()> {
    let sd = Systemd::connect().await?;
    let store = Store::new(&config.machines_dir, &config.state_dir);
    let records: std::collections::HashMap<String, ImageRecord> = store
        .list_images()?
        .into_iter()
        .map(|r| (r.name.clone(), r))
        .collect();
    let mut machines = sd.list_machines().await?;
    machines.retain(|m| !m.name.starts_with('.'));
    machines.sort_by(|a, b| a.name.cmp(&b.name));
    let now = now_unix();
    let mut rows = Vec::new();
    for m in &machines {
        let details = sd.machine_details(&m.name).await.ok();
        let (image, mode, command) = describe(records.get(&m.name));
        let os = sd
            .machine_os(&m.name)
            .await
            .unwrap_or_else(|| "-".to_string());
        rows.push(vec![
            m.name.clone(),
            image,
            mode,
            command,
            details
                .as_ref()
                .map(|d| d.state.clone())
                .unwrap_or_else(|| "-".to_string()),
            details
                .as_ref()
                .filter(|d| d.started > 0 && d.started <= now)
                .map(|d| human_duration(now - d.started))
                .unwrap_or_else(|| "-".to_string()),
            details
                .as_ref()
                .map(|d| d.leader.to_string())
                .unwrap_or_else(|| "-".to_string()),
            os,
        ]);
    }
    if args.all {
        let running: std::collections::HashSet<&str> =
            machines.iter().map(|m| m.name.as_str()).collect();
        let mut stopped: Vec<&ImageRecord> = records
            .values()
            .filter(|r| !running.contains(r.name.as_str()))
            .collect();
        stopped.sort_by(|a, b| a.name.cmp(&b.name));
        for r in stopped {
            let (image, mode, command) = describe(Some(r));
            rows.push(vec![
                r.name.clone(),
                image,
                mode,
                command,
                "stopped".into(),
                "-".into(),
                "-".into(),
                "-".into(),
            ]);
        }
    }
    println!(
        "{}",
        table(
            &["MACHINE", "IMAGE", "MODE", "COMMAND", "STATE", "UP", "PID", "OS"],
            rows
        )
    );
    Ok(())
}

/// Image reference, mode and command of a machine, when nspawn installed its image.
fn describe(record: Option<&ImageRecord>) -> (String, String, String) {
    match record {
        Some(r) => {
            let command = match r.mode {
                Mode::Boot => "init".to_string(),
                Mode::App => {
                    let joined = r.run.command.join(" ");
                    if joined.chars().count() > 40 {
                        format!("{}...", joined.chars().take(37).collect::<String>())
                    } else {
                        joined
                    }
                }
            };
            (r.reference.clone(), r.mode.name().to_string(), command)
        }
        None => ("-".to_string(), "-".to_string(), "-".to_string()),
    }
}

pub async fn start(args: StartArgs, config: &Config) -> Result<()> {
    let sd = Systemd::connect().await?;
    if sd.machine_exists(&args.name).await? {
        bail!("machine {} is already running", args.name);
    }
    let store = Store::new(&config.machines_dir, &config.state_dir);
    let network = match store.load_image(&args.name)? {
        Some(mut record) => {
            if let Some(network) = args.network {
                record.network = network;
                store.record_image(&record)?;
            }
            if !args.command.is_empty() && record.mode == Mode::Boot {
                bail!("{} boots an init system; a command can only replace the entrypoint of an app image", args.name);
            }
            // The settings file is regenerated every time: it carries the command override
            // and comes back if it went missing.
            settings::write(&MachineSettings {
                name: &args.name,
                managed_userns: record.backend == BackendChoice::Mstack,
                mode: record.mode,
                run: &record.run,
                command_override: if args.command.is_empty() {
                    None
                } else {
                    Some(&args.command)
                },
                network: record.network,
            })?;
            if record.backend == BackendChoice::Mstack {
                // Managed user namespaces come from socket activated services that
                // distributions ship disabled.
                for unit in MANAGED_NS_SOCKETS {
                    sd.start_unit(unit).await?;
                }
            }
            record.network
        }
        None if !args.command.is_empty() => {
            bail!(
                "{} is not an image managed by nspawn; a command override needs one",
                args.name
            )
        }
        // Not ours: the stock systemd-nspawn@.service template uses --network-veth.
        None => Network::Veth,
    };
    let veth = network == Network::Veth;
    if veth {
        hostnet::ensure_networkd(&sd).await?;
    }
    // firewalld only knows the interface once the machine is registered.
    let firewalld = veth && hostnet::firewalld_running(&sd).await;
    sd.start_machine(&args.name).await?;
    if args.wait || firewalld {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !sd.machine_exists(&args.name).await? {
            if Instant::now() > deadline {
                bail!(
                    "machine {} did not register within 30 seconds; see journalctl -u systemd-nspawn@{}",
                    args.name,
                    args.name
                );
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
    if firewalld {
        hostnet::admit(&sd, &args.name).await?;
    }
    println!("started {}", args.name);
    Ok(())
}

pub async fn stop(args: StopArgs, config: &Config) -> Result<()> {
    let sd = Systemd::connect().await?;
    if !sd.machine_exists(&args.name).await? {
        bail!("machine {} is not running", args.name);
    }
    let store = Store::new(&config.machines_dir, &config.state_dir);
    let record = store.load_image(&args.name)?;
    let admitted = if hostnet::firewalld_running(&sd).await {
        hostnet::machine_interfaces(&sd, &args.name)
            .await
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    if args.force {
        sd.terminate_machine(&args.name).await?;
    } else {
        match record.as_ref().map(|r| r.mode) {
            // Like docker stop: the image's stop signal to every process, then the hammer.
            Some(Mode::App) => {
                let signal = record
                    .as_ref()
                    .and_then(|r| r.run.stop_signal.clone())
                    .unwrap_or_else(|| "SIGTERM".to_string());
                sd.kill_machine(&args.name, "all", signal_number(&signal)?)
                    .await?;
                if !wait_gone(&sd, &args.name, Duration::from_secs(args.timeout)).await? {
                    eprintln!(
                        "{} ignored {signal} for {} seconds; terminating it",
                        args.name, args.timeout
                    );
                    sd.terminate_machine(&args.name).await?;
                }
            }
            _ => sd.poweroff_machine(&args.name).await?,
        }
    }
    if args.wait {
        if !wait_gone(&sd, &args.name, Duration::from_secs(60)).await? {
            bail!(
                "machine {} is still running after 60 seconds; use --force",
                args.name
            );
        }
        // Let the service finish its own teardown so that the image can be removed right away.
        sd.stop_unit(&format!("systemd-nspawn@{}.service", args.name))
            .await?;
        hostnet::release(&sd, &admitted).await;
    }
    println!("stopped {}", args.name);
    Ok(())
}

async fn wait_gone(sd: &Systemd, name: &str, timeout: Duration) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    while sd.machine_exists(name).await? {
        if Instant::now() > deadline {
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Ok(true)
}

fn signal_number(name: &str) -> Result<i32> {
    let upper = name.to_ascii_uppercase();
    let full = if upper.starts_with("SIG") {
        upper
    } else {
        format!("SIG{upper}")
    };
    let signal: nix::sys::signal::Signal = full
        .parse()
        .map_err(|_| anyhow::anyhow!("unknown signal {name}"))?;
    Ok(signal as i32)
}

pub async fn exec(args: ExecArgs, config: &Config) -> Result<()> {
    let sd = Systemd::connect().await?;
    if !sd.machine_exists(&args.machine).await? {
        bail!("machine {} is not running", args.machine);
    }
    let store = Store::new(&config.machines_dir, &config.state_dir);
    let record = store.load_image(&args.machine)?;
    if args.nsenter || record.as_ref().is_some_and(|r| r.mode == Mode::App) {
        let code = exec_in_namespaces(
            &sd,
            &args.machine,
            &args.command,
            &args.user,
            record.as_ref(),
        )
        .await?;
        std::process::exit(code);
    }
    let path = args.command[0].clone();
    if !path.starts_with('/') {
        bail!("the command must be an absolute path inside the machine, for example /usr/bin/{path} (or use --nsenter)");
    }
    let (fd, _pty) =
        open_shell_when_ready(&sd, &args.machine, &args.user, &path, args.command.clone()).await?;
    pty::run_session(fd)
}

pub async fn shell(args: ShellArgs, config: &Config) -> Result<()> {
    let sd = Systemd::connect().await?;
    if !sd.machine_exists(&args.machine).await? {
        bail!("machine {} is not running", args.machine);
    }
    let store = Store::new(&config.machines_dir, &config.state_dir);
    let record = store.load_image(&args.machine)?;
    if record.as_ref().is_some_and(|r| r.mode == Mode::App) {
        let shell = vec!["/bin/sh".to_string()];
        let code =
            exec_in_namespaces(&sd, &args.machine, &shell, &args.user, record.as_ref()).await?;
        std::process::exit(code);
    }
    let (fd, _pty) = open_shell_when_ready(&sd, &args.machine, &args.user, "", Vec::new()).await?;
    pty::run_session(fd)
}

/// docker exec: no D-Bus needed inside the machine. Returns the command's exit code.
async fn exec_in_namespaces(
    sd: &Systemd,
    machine: &str,
    command: &[String],
    user: &str,
    record: Option<&ImageRecord>,
) -> Result<i32> {
    let leader = sd.machine_leader(machine).await?;
    let user = if user == "root" { None } else { Some(user) };
    let working_dir = record.and_then(|r| r.run.working_dir.as_deref());
    tokio::task::block_in_place(|| nsenter::exec(leader, command, user, working_dir))
        .with_context(|| format!("running a command inside {machine}"))
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

/// docker logs: the machine's console output lives in the journal of its service (nspawn
/// pipes the payload's stdout and stderr there); boot machines also have a journal of
/// their own. journalctl is the journal's reader, so it does the work. Everything the unit
/// ever logged is shown, earlier runs included; --follow starts from the last lines.
pub fn logs(args: LogsArgs) -> Result<()> {
    let argv = journalctl_arguments(&args);
    let status = std::process::Command::new("journalctl")
        .args(&argv)
        .status()
        .context("running journalctl")?;
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

/// Lines shown before following when --lines is not given.
const FOLLOW_TAIL: u32 = 10;

pub fn journalctl_arguments(args: &LogsArgs) -> Vec<String> {
    let mut argv = vec!["--no-pager".to_string(), "--quiet".to_string()];
    let output = if args.timestamps { "short-iso" } else { "cat" };
    if args.inside {
        argv.push(format!("--machine={}", args.machine));
        argv.push(format!("--output={output}"));
    } else {
        argv.push(format!("--unit=systemd-nspawn@{}.service", args.machine));
        argv.push(format!("--output={output}"));
        if !args.all {
            // Only what the machine itself wrote, not systemd's messages about the unit.
            argv.push("_TRANSPORT=stdout".to_string());
        }
    }
    match (args.lines, args.follow) {
        (Some(n), _) => argv.push(format!("--lines={n}")),
        (None, true) => argv.push(format!("--lines={FOLLOW_TAIL}")),
        (None, false) => {}
    }
    if let Some(since) = &args.since {
        argv.push(format!("--since={since}"));
    }
    if args.follow {
        argv.push("--follow".to_string());
    }
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journalctl_command_lines() {
        let base = LogsArgs {
            machine: "web".into(),
            ..LogsArgs::default()
        };
        assert_eq!(
            journalctl_arguments(&base),
            vec![
                "--no-pager",
                "--quiet",
                "--unit=systemd-nspawn@web.service",
                "--output=cat",
                "_TRANSPORT=stdout"
            ]
        );
        let follow = LogsArgs {
            machine: "web".into(),
            follow: true,
            ..LogsArgs::default()
        };
        let argv = journalctl_arguments(&follow);
        assert!(argv.contains(&"--lines=10".to_string()) && argv.last().unwrap() == "--follow");
        let full = LogsArgs {
            machine: "web".into(),
            follow: true,
            lines: Some(50),
            since: Some("10 min ago".into()),
            timestamps: true,
            all: true,
            inside: false,
        };
        assert_eq!(
            journalctl_arguments(&full),
            vec![
                "--no-pager",
                "--quiet",
                "--unit=systemd-nspawn@web.service",
                "--output=short-iso",
                "--lines=50",
                "--since=10 min ago",
                "--follow"
            ]
        );
        let inside = LogsArgs {
            machine: "fedora-44".into(),
            inside: true,
            ..LogsArgs::default()
        };
        assert_eq!(
            journalctl_arguments(&inside),
            vec![
                "--no-pager",
                "--quiet",
                "--machine=fedora-44",
                "--output=cat"
            ]
        );
    }
}
