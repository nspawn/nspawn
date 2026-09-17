use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use nix::libc;

use crate::backend::MANAGED_NS_SOCKETS;
use crate::bridge;
use crate::cli::{BackendChoice, ExecArgs, LogsArgs, PsArgs, ShellArgs, StartArgs, StopArgs};
use crate::config::Config;
use crate::hostnet;
use crate::nsenter;
use crate::oci::Mode;
use crate::output::{human_duration, table};
use crate::pty;
use crate::reference::validate_machine_name;
use crate::settings::{self, BridgeMount, MachineSettings, Network};
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
            network_column(records.get(&m.name)),
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
                network_column(Some(r)),
                "-".into(),
            ]);
        }
    }
    println!(
        "{}",
        table(
            &["MACHINE", "IMAGE", "MODE", "COMMAND", "STATE", "UP", "PID", "NETWORK", "OS"],
            rows
        )
    );
    Ok(())
}

/// Address and published ports on the bridge, or the kind of network otherwise.
fn network_column(record: Option<&ImageRecord>) -> String {
    match record {
        Some(r) if r.network == Network::Bridge => {
            let mut parts = vec![r
                .address
                .map(|a| a.to_string())
                .unwrap_or_else(|| "bridge".to_string())];
            parts.extend(r.ports.iter().map(|p| p.to_string()));
            parts.join(" ")
        }
        Some(r) if r.network == Network::Host => "host".to_string(),
        Some(_) => "veth".to_string(),
        None => "-".to_string(),
    }
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

/// Everything a machine needs before its unit starts: checks, address and files on the
/// bridge, settings, unit hooks. Shared by `start` and the ExecStartPre hook, so it is
/// idempotent. The caller holds the store lock. Returns the network the machine uses.
pub async fn prepare(
    sd: &Systemd,
    store: &Store,
    config: &Config,
    name: &str,
    record: Option<ImageRecord>,
) -> Result<Network> {
    let Some(mut record) = record else {
        // Not ours: the stock systemd-nspawn@.service template uses --network-veth.
        hostnet::ensure_networkd(sd).await?;
        return Ok(Network::Veth);
    };
    if record.network == Network::Bridge
        && record.mode == Mode::App
        && record.backend == BackendChoice::Mstack
    {
        bail!(
            "{name} is an mstack app: managed user namespaces cannot join the network namespace prepared for the bridge; pull it again with --backend overlay, or start it with --network host"
        );
    }
    if !record.ports.is_empty() && record.network != Network::Bridge {
        bail!("ports are published through the bridge network; start {name} with --network bridge");
    }
    if record.network == Network::Veth && record.mode == Mode::App {
        bail!(
            "{name} is an app image, with nothing inside to configure a veth; use --network bridge or --network host"
        );
    }
    let files = if record.network == Network::Bridge {
        bridge::up(config, sd).await?;
        bridge::check_port_conflicts(store, sd, &record).await?;
        // Only now, past the checks, does the record keep what it was given.
        store.record_image(&record)?;
        let addr = bridge::prepare_machine(store, config, &mut record)?;
        if record.mode == Mode::App {
            bridge::create_netns(config, name, addr)?;
        }
        Some(store.machine_files_dir(name))
    } else {
        store.record_image(&record)?;
        None
    };
    // The settings file is regenerated every time: it carries the command and comes back
    // if it went missing.
    settings::write(&MachineSettings {
        name,
        managed_userns: record.backend == BackendChoice::Mstack,
        mode: record.mode,
        run: &record.run,
        command_override: if record.command.is_empty() {
            None
        } else {
            Some(&record.command)
        },
        network: record.network,
        bridge: files.as_deref().map(|files| BridgeMount {
            bridge: &config.bridge,
            files,
        }),
    })?;
    if write_hooks(name, config)? {
        sd.reload().await?;
    }
    if record.backend == BackendChoice::Mstack {
        // Managed user namespaces come from socket activated services that
        // distributions ship disabled.
        for unit in MANAGED_NS_SOCKETS {
            sd.start_unit(unit).await?;
        }
    }
    if record.network == Network::Veth {
        hostnet::ensure_networkd(sd).await?;
    }
    Ok(record.network)
}

/// The drop-in that makes the machine's unit call nspawn around its life: the network is
/// prepared before it starts, ports are published once it runs and everything is released
/// however it ends, whether it was started by nspawn, machinectl or at boot.
fn write_hooks(name: &str, config: &Config) -> Result<bool> {
    let exe = std::env::current_exe().context("locating the nspawn binary")?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    let mut command = settings::quote(&exe.to_string_lossy());
    if let Some(path) = &config.config_path {
        command.push_str(" --config ");
        command.push_str(&settings::quote(&path.to_string_lossy()));
    }
    let text = format!(
        "# Generated by nspawn; do not edit.\n[Service]\nExecStartPre={command} network prepare %i\nExecStartPost={command} network publish %i\nExecStopPost=-{command} network release %i\n"
    );
    let dir = crate::backend::dropin_dir(name);
    let path = dir.join("nspawn-hooks.conf");
    if std::fs::read_to_string(&path)
        .map(|current| current == text)
        .unwrap_or(false)
    {
        return Ok(false);
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(true)
}

pub async fn start(args: StartArgs, config: &Config) -> Result<()> {
    if args.name.contains(':') || args.name.contains('/') {
        bail!(
            "{} looks like an image reference; machines are started by name. Pull it (nspawn pull {}) or make a machine from a local image (nspawn create IMAGE NAME)",
            args.name,
            args.name
        );
    }
    validate_machine_name(&args.name)?;
    let sd = Systemd::connect().await?;
    if sd.machine_exists(&args.name).await? {
        bail!("machine {} is already running", args.name);
    }
    let store = Store::new(&config.machines_dir, &config.state_dir);
    let lock = store.lock()?;
    let mut record = store.load_image(&args.name)?;
    match record.as_mut() {
        Some(r) => {
            if let Some(network) = args.network {
                r.network = network;
            }
            if !args.publish.is_empty() {
                r.ports = bridge::parse_publish(&args.publish)?;
            }
            if args.image_command {
                r.command.clear();
            }
            if !args.command.is_empty() {
                if r.mode == Mode::Boot {
                    bail!(
                        "{} boots an init system; a command can only replace the entrypoint of an app image",
                        args.name
                    );
                }
                r.command = args.command.clone();
            }
        }
        None if !args.command.is_empty() || args.network.is_some() || !args.publish.is_empty() => {
            bail!(
                "{} is not an image managed by nspawn; a command, network or ports need one",
                args.name
            )
        }
        None => {
            if !sd.list_images().await?.iter().any(|i| i.name == args.name) {
                bail!(
                    "no image named {}; see nspawn images ls, or pull one",
                    args.name
                );
            }
        }
    }
    let booted = record.as_ref().is_none_or(|r| r.mode == Mode::Boot);
    let network = prepare(&sd, &store, config, &args.name, record).await?;
    // The unit's own hooks take the lock; it must be free while the unit starts.
    drop(lock);
    // firewalld only knows a veth once the machine is registered; published ports need
    // the machine to count as running.
    let firewalld = network == Network::Veth && hostnet::firewalld_running(&sd).await;
    sd.start_machine(&args.name).await?;
    if args.wait || firewalld || network == Network::Bridge {
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
    if network == Network::Bridge {
        let _lock = store.lock()?;
        bridge::sync_ports(&store, &sd).await?;
    }
    if args.wait && booted {
        wait_for_init(&sd, &args.name).await?;
    }
    println!("started {}", args.name);
    Ok(())
}

/// Registration comes early in a booted machine's life; a command run right after `start`
/// would find no service manager to talk to. Wait, within reason, until the machine's
/// systemd listens on its private socket.
async fn wait_for_init(sd: &Systemd, name: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let Ok(leader) = sd.machine_leader(name).await else {
            return Ok(());
        };
        if std::path::Path::new(&format!("/proc/{leader}/root/run/systemd/private")).exists()
            || Instant::now() > deadline
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// What must go when a machine is gone, however it went: its network namespace and its
/// published ports. Safe to repeat.
pub async fn release_machine(
    sd: &Systemd,
    store: &Store,
    name: &str,
    record: Option<&ImageRecord>,
) -> Result<()> {
    if record.map(|r| r.network) == Some(Network::Bridge) {
        bridge::delete_netns(name);
        bridge::sync_ports_except(store, sd, name).await?;
    }
    Ok(())
}

pub async fn stop(args: StopArgs, config: &Config) -> Result<()> {
    let sd = Systemd::connect().await?;
    let store = Store::new(&config.machines_dir, &config.state_dir);
    let record = store.load_image(&args.name)?;
    if !sd.machine_exists(&args.name).await? {
        match &record {
            // It ended on its own or elsewhere; leave nothing of it behind, like docker
            // stop on a stopped container.
            Some(r) => {
                release_machine(&sd, &store, &args.name, Some(r)).await?;
                println!("{} was not running", args.name);
                return Ok(());
            }
            None => bail!("machine {} is not running", args.name),
        }
    }
    let admitted = if hostnet::firewalld_running(&sd).await {
        hostnet::machine_interfaces(&sd, &args.name)
            .await
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    if args.force {
        // docker kill: no questions asked.
        sd.kill_machine(&args.name, "all", libc::SIGKILL).await?;
    } else {
        match record.as_ref().map(|r| r.mode) {
            // Like docker stop: the image's stop signal to every process, then the hammer.
            Some(Mode::App) => {
                let signal = record
                    .as_ref()
                    .and_then(|r| r.run.stop_signal.clone())
                    .unwrap_or_else(|| "SIGTERM".to_string());
                // To the program itself (PID 2, the stub init's child), never to the
                // whole cgroup: systemd-nspawn and its stub react to signals in their own
                // ways (the stub reboots the machine on SIGINT, nspawn dies of SIGQUIT).
                let leader = sd.machine_leader(&args.name).await?;
                // Right after start the stub init may not have forked the program yet.
                let mut payload = payload_pid(leader);
                let deadline = Instant::now() + Duration::from_secs(2);
                while payload.is_none() && Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    payload = payload_pid(leader);
                }
                match payload {
                    Some(payload) => {
                        if let Err(e) = nix::sys::signal::kill(
                            nix::unistd::Pid::from_raw(payload),
                            nix::sys::signal::Signal::try_from(signal_number(&signal)?)?,
                        ) {
                            eprintln!("note: could not send {signal} to PID {payload} of {}: {e}", args.name);
                        }
                    }
                    None => eprintln!(
                        "note: {} has no program running under its init (leader PID {leader}); waiting for it to end",
                        args.name
                    ),
                }
                if !wait_gone(&sd, &args.name, Duration::from_secs(args.timeout)).await? {
                    eprintln!(
                        "{} ignored {signal} for {} seconds; killing it",
                        args.name, args.timeout
                    );
                    sd.kill_machine(&args.name, "all", libc::SIGKILL).await?;
                }
            }
            _ => {
                if args.wait {
                    if !poweroff_until_gone(&sd, &args.name, Duration::from_secs(60)).await? {
                        bail!(
                            "machine {} is still running after 60 seconds; use --force",
                            args.name
                        );
                    }
                } else {
                    sd.poweroff_machine(&args.name).await?;
                }
            }
        }
    }
    if args.wait {
        if !wait_gone(&sd, &args.name, Duration::from_secs(60)).await? {
            bail!(
                "machine {} is still running after 60 seconds; use --force",
                args.name
            );
        }
        // Let the service finish its own teardown so that the image can be removed right
        // away, and clear the failure a signal-killed program leaves on the unit.
        let unit = format!("systemd-nspawn@{}.service", args.name);
        sd.stop_unit(&unit).await?;
        sd.reset_failed(&unit).await?;
        hostnet::release(&sd, &admitted).await;
        let _lock = store.lock()?;
        release_machine(&sd, &store, &args.name, record.as_ref()).await?;
    }
    println!("stopped {}", args.name);
    Ok(())
}

/// Asks a booted machine to power off and waits until it is gone, repeating the request
/// every couple of seconds: right after `start` the machine's init may not have installed
/// its signal handlers yet, and the kernel silently drops signals that PID 1 of a PID
/// namespace does not handle.
async fn poweroff_until_gone(sd: &Systemd, name: &str, timeout: Duration) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    if sd.poweroff_machine(name).await.is_err() && !sd.machine_exists(name).await? {
        return Ok(true);
    }
    let mut requested = Instant::now();
    while sd.machine_exists(name).await? {
        if Instant::now() > deadline {
            return Ok(false);
        }
        if requested.elapsed() > Duration::from_secs(2) {
            // The machine may vanish between the check and the signal; that is success.
            let _ = sd.poweroff_machine(name).await;
            requested = Instant::now();
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Ok(true)
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

/// Signal names as OCI configs write them: SIGTERM, TERM, 15, SIGRTMIN+3, RTMAX-1.
fn signal_number(name: &str) -> Result<i32> {
    if let Ok(number) = name.trim().parse::<i32>() {
        if number >= 1 && number <= libc::SIGRTMAX() {
            return Ok(number);
        }
        bail!("signal number {number} is out of range");
    }
    let upper = name.trim().to_ascii_uppercase();
    let full = if upper.starts_with("SIG") {
        upper
    } else {
        format!("SIG{upper}")
    };
    for (base, value) in [
        ("SIGRTMIN", libc::SIGRTMIN()),
        ("SIGRTMAX", libc::SIGRTMAX()),
    ] {
        if let Some(rest) = full.strip_prefix(base) {
            let offset: i32 = match rest {
                "" => 0,
                _ => rest
                    .parse()
                    .map_err(|_| anyhow::anyhow!("unknown signal {name}"))?,
            };
            let number = value + offset;
            if number < libc::SIGRTMIN() || number > libc::SIGRTMAX() {
                bail!("signal {name} is outside the realtime range");
            }
            return Ok(number);
        }
    }
    let signal: nix::sys::signal::Signal = full
        .parse()
        .map_err(|_| anyhow::anyhow!("unknown signal {name}"))?;
    Ok(signal as i32)
}

/// The program an app machine runs: the child of its stub init (the leader), found by
/// its parent PID (the /proc children file is optional in kernels).
fn payload_pid(leader: u32) -> Option<i32> {
    let entries = std::fs::read_dir("/proc").ok()?;
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<i32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // "pid (comm) state ppid ...": the comm may contain spaces and parentheses.
        let Some(rest) = stat.rfind(')').map(|i| &stat[i + 1..]) else {
            continue;
        };
        let ppid = rest
            .split_whitespace()
            .nth(1)
            .and_then(|p| p.parse::<u32>().ok());
        if ppid == Some(leader) {
            return Some(pid);
        }
    }
    None
}

pub async fn exec(args: ExecArgs, config: &Config) -> Result<()> {
    let sd = Systemd::connect().await?;
    if !sd.machine_exists(&args.machine).await? {
        bail!("machine {} is not running", args.machine);
    }
    let store = Store::new(&config.machines_dir, &config.state_dir);
    let record = store.load_image(&args.machine)?;
    // docker exec: through the namespaces for every kind of machine, so that the exit
    // code comes back and nothing inside (D-Bus, PAM) is needed. `shell` keeps the login
    // session machined offers for booted machines.
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
    let env: &[String] = record.map(|r| r.run.env.as_slice()).unwrap_or(&[]);
    tokio::task::block_in_place(|| nsenter::exec(leader, command, user, working_dir, env))
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
    fn stop_signal_spellings() {
        assert_eq!(signal_number("SIGTERM").unwrap(), 15);
        assert_eq!(signal_number("term").unwrap(), 15);
        assert_eq!(signal_number("15").unwrap(), 15);
        assert_eq!(signal_number("SIGRTMIN").unwrap(), libc::SIGRTMIN());
        assert_eq!(signal_number("SIGRTMIN+3").unwrap(), libc::SIGRTMIN() + 3);
        assert_eq!(signal_number("RTMAX-1").unwrap(), libc::SIGRTMAX() - 1);
        assert!(signal_number("SIGRTMIN+200").is_err());
        assert!(signal_number("0").is_err());
        assert!(signal_number("SIGBOGUS").is_err());
    }

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
