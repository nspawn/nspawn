//! Machines: what runs, and how one is started, stopped, entered and read.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::time::{Duration, Instant};

use anyhow::{bail, Context as _, Result};
use nix::libc;

use crate::api::{note, Context, Report};
use crate::backend::{BackendChoice, MANAGED_NS_SOCKETS};
use crate::bridge;
use crate::config::Config;
use crate::hostnet;
use crate::nsenter;
use crate::oci::Mode;
use crate::reference::validate_machine_name;
use crate::settings::{self, Bind, BridgeMount, MachineSettings, Network};
use crate::store::{now_unix, ImageRecord, Store};
use crate::systemd::Systemd;
use crate::volume;

/// One machine as `ps` shows it: machined's state plus nspawn's record when it installed
/// the image.
#[derive(Debug, Clone)]
pub struct MachineSummary {
    pub name: String,
    pub record: Option<ImageRecord>,
    /// machined's state ("opening", "running", "closing"), or "stopped".
    pub state: String,
    /// Unix seconds, for running machines.
    pub started: Option<u64>,
    /// PID of the machine's init as seen from the host.
    pub leader: Option<u32>,
    /// PRETTY_NAME of the machine's os-release.
    pub os: Option<String>,
}

/// The machines machined knows, and with `all` also the images of nspawn that are not
/// running, as stopped.
pub async fn list(ctx: &Context, all: bool) -> Result<Vec<MachineSummary>> {
    let sd = ctx.sd().await?;
    let records: HashMap<String, ImageRecord> = ctx
        .store
        .list_images()?
        .into_iter()
        .map(|r| (r.name.clone(), r))
        .collect();
    let mut machines = sd.list_machines().await?;
    machines.retain(|m| !m.name.starts_with('.'));
    machines.sort_by(|a, b| a.name.cmp(&b.name));
    let now = now_unix();
    let mut summaries = Vec::new();
    for m in &machines {
        let details = sd.machine_details(&m.name).await.ok();
        summaries.push(MachineSummary {
            name: m.name.clone(),
            record: records.get(&m.name).cloned(),
            state: details
                .as_ref()
                .map(|d| d.state.clone())
                .unwrap_or_else(|| "-".to_string()),
            started: details
                .as_ref()
                .filter(|d| d.started > 0 && d.started <= now)
                .map(|d| d.started),
            leader: details.as_ref().map(|d| d.leader),
            os: sd.machine_os(&m.name).await,
        });
    }
    if all {
        let running: std::collections::HashSet<&str> =
            machines.iter().map(|m| m.name.as_str()).collect();
        let mut stopped: Vec<&ImageRecord> = records
            .values()
            .filter(|r| !running.contains(r.name.as_str()))
            .collect();
        stopped.sort_by(|a, b| a.name.cmp(&b.name));
        for r in stopped {
            summaries.push(MachineSummary {
                name: r.name.clone(),
                record: Some(r.clone()),
                state: "stopped".to_string(),
                started: None,
                leader: None,
                os: None,
            });
        }
    }
    Ok(summaries)
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
    // Named volumes live under the state directory; a missing host directory is created,
    // as docker does.
    let mut binds = Vec::new();
    for volume in &record.volumes {
        let source = volume.host_path(&store.volumes_dir());
        if !source.exists() {
            std::fs::create_dir_all(&source)
                .with_context(|| format!("creating volume {}", source.display()))?;
        }
        binds.push(Bind {
            source,
            target: volume.target.clone(),
            read_only: volume.read_only,
        });
    }
    // Every booted machine with volumes waits for them before local-fs.target through the
    // units mounted here (mstack machines get them from the host after their init started).
    let managed_userns = record.backend == BackendChoice::Mstack;
    let volume_units = if record.mode == Mode::Boot && !binds.is_empty() {
        let dir = store.machine_files_dir(name).join("units");
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let targets: Vec<String> = binds.iter().map(|b| b.target.clone()).collect();
        let (service, dropin) = settings::volume_wait_units(&targets);
        std::fs::write(dir.join("nspawn-volumes.service"), service)?;
        std::fs::write(dir.join("nspawn-volumes.conf"), dropin)?;
        Some(dir)
    } else {
        None
    };
    // The settings file is regenerated every time: it carries the command and comes back
    // if it went missing.
    let route = settings::namespace_route(sd, name, record.mode, record.network).await?;
    settings::write(
        &MachineSettings {
            name,
            managed_userns,
            mode: record.mode,
            run: &record.run,
            command: &record.effective_command(),
            extra_env: &record.env,
            binds: &binds,
            volume_units: volume_units.as_deref(),
            network: record.network,
            bridge: files.as_deref().map(|files| BridgeMount {
                bridge: &config.bridge,
                files,
            }),
        },
        &route,
    )?;
    if settings::write_hooks(name, config, &route)? {
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

/// What `start` may be asked for. Everything but the name is remembered for the image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartRequest {
    pub name: String,
    /// Wait for a booted machine's init to be up before returning.
    pub wait: bool,
    pub network: Option<Network>,
    /// HOST:CONTAINER[/udp]; "none" forgets them all.
    pub publish: Vec<String>,
    /// Replaces the image's entrypoint; an empty string runs the arguments alone.
    pub entrypoint: Option<String>,
    /// VAR=value or VAR; "none" forgets them.
    pub env: Vec<String>,
    /// SOURCE:TARGET[:ro]; "none" forgets them.
    pub volume: Vec<String>,
    /// Forget the remembered entrypoint and arguments and run the image's own again.
    pub image_command: bool,
    /// App images: replaces the image's cmd and follows its entrypoint.
    pub command: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartOutcome {
    /// The machine is registered (and, when asked, its init is up).
    Started,
    /// A short program ran and returned before the machine registered.
    Ended,
}

pub async fn start(
    ctx: &Context,
    args: &StartRequest,
    _report: Report<'_>,
) -> Result<StartOutcome> {
    let config = &ctx.config;
    if args.name.contains(':') || args.name.contains('/') {
        bail!(
            "{} looks like an image reference; machines are started by name. Pull it (nspawn pull {}) or make a machine from a local image (nspawn create IMAGE NAME)",
            args.name,
            args.name
        );
    }
    validate_machine_name(&args.name)?;
    let sd = ctx.sd().await?;
    let unit = format!("systemd-nspawn@{}.service", args.name);
    let store = &ctx.store;
    let mode = store.load_image(&args.name)?.map(|r| r.mode);
    wait_for_previous(sd, &args.name, &unit, mode).await?;
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
                r.entrypoint = None;
                r.cmd = None;
            }
            if r.mode == Mode::Boot
                && (!args.command.is_empty() || args.entrypoint.is_some() || !args.env.is_empty())
            {
                bail!(
                    "{} boots an init system; a command, an entrypoint and variables only apply to the program of an app image",
                    args.name
                );
            }
            if let Some(entrypoint) = &args.entrypoint {
                r.entrypoint = Some(if entrypoint.is_empty() {
                    Vec::new()
                } else {
                    vec![entrypoint.clone()]
                });
            }
            if !args.command.is_empty() {
                r.cmd = Some(args.command.clone());
            }
            if !args.env.is_empty() {
                r.env = volume::parse_env(&args.env)?;
            }
            if !args.volume.is_empty() {
                r.volumes = volume::parse_volumes(&args.volume)?;
            }
        }
        None if !args.command.is_empty()
            || args.network.is_some()
            || !args.publish.is_empty()
            || args.entrypoint.is_some()
            || !args.env.is_empty()
            || !args.volume.is_empty() =>
        {
            bail!(
                "{} is not an image managed by nspawn; a command, network, ports, variables or volumes need one",
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
    let network = prepare(sd, store, config, &args.name, record).await?;
    // The unit's own hooks take the lock; it must be free while the unit starts.
    drop(lock);
    // firewalld only knows a veth once the machine is registered; published ports need
    // the machine to count as running.
    let firewalld = network == Network::Veth && hostnet::firewalld_running(sd).await;
    sd.start_machine(&args.name).await?;
    if args.wait || firewalld || network == Network::Bridge {
        let unit = format!("systemd-nspawn@{}.service", args.name);
        let deadline = Instant::now() + Duration::from_secs(30);
        while !sd.machine_exists(&args.name).await? {
            // A short program may have run and returned already: not a failure.
            let (_, active) = sd.unit_state(&unit).await?;
            match active.as_str() {
                "active" | "activating" | "reloading" | "deactivating" => {}
                "failed" => bail!(
                    "{} ended right after starting with an error; see journalctl -u {unit}",
                    args.name
                ),
                _ => return Ok(StartOutcome::Ended),
            }
            if Instant::now() > deadline {
                bail!(
                    "machine {} did not register within 30 seconds; see journalctl -u {unit}",
                    args.name
                );
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
    if firewalld {
        hostnet::admit(sd, &args.name).await?;
    }
    if network == Network::Bridge {
        let _lock = store.lock()?;
        bridge::sync_ports(store, sd).await?;
    }
    if args.wait && booted {
        wait_for_init(sd, &args.name).await?;
    }
    Ok(StartOutcome::Started)
}

/// Registration comes early in a booted machine's life; a command run right after `start`
/// would find no service manager to talk to. Wait, within reason, until the machine's
/// systemd listens on its private socket.
async fn wait_for_init(sd: &Systemd, name: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let Ok(leader) = sd.machine_leader(name).await else {
            // Gone while booting: say so instead of "started".
            let (_, active) = sd
                .unit_state(&format!("systemd-nspawn@{name}.service"))
                .await?;
            if active == "failed" {
                bail!("{name} died while booting; see journalctl -u systemd-nspawn@{name}.service");
            }
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
pub fn release_machine(name: &str, record: Option<&ImageRecord>) -> Result<()> {
    if record.map(|r| r.network) == Some(Network::Bridge) {
        bridge::delete_netns(name);
        // Only this machine's entries: release runs without the store lock, next to
        // another machine's publish.
        record.map(bridge::withdraw_ports).transpose()?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopRequest {
    pub name: String,
    /// SIGKILL right away, like docker kill.
    pub force: bool,
    /// Wait until the machine is gone; otherwise return after the stop request.
    pub wait: bool,
    /// App images: seconds between the stop signal and SIGKILL.
    pub timeout: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    Stopped,
    /// It had ended on its own or elsewhere; what it left behind is gone now.
    WasNotRunning,
}

pub async fn stop(ctx: &Context, args: &StopRequest, report: Report<'_>) -> Result<StopOutcome> {
    let sd = ctx.sd().await?;
    let store = &ctx.store;
    let record = store.load_image(&args.name)?;
    if !sd.machine_exists(&args.name).await? {
        match &record {
            // It ended on its own or elsewhere; leave nothing of it behind, like docker
            // stop on a stopped container. The unit may still be running its release
            // hook: let that finish rather than work beside it.
            Some(r) => {
                let unit = format!("systemd-nspawn@{}.service", args.name);
                settled_unit_state(sd, &unit, Duration::from_secs(30)).await?;
                release_machine(&args.name, Some(r))?;
                sd.reset_failed(&unit).await?;
                return Ok(StopOutcome::WasNotRunning);
            }
            None => bail!("machine {} is not running", args.name),
        }
    }
    let admitted = if hostnet::firewalld_running(sd).await {
        hostnet::machine_interfaces(sd, &args.name)
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
                        if let Err(e) = send_signal(payload, signal_number(&signal)?) {
                            note(report, format!("note: could not send {signal} to PID {payload} of {}: {e}", args.name));
                        }
                    }
                    None => note(
                        report,
                        format!(
                            "note: {} has no program running under its init (leader PID {leader}); waiting for it to end",
                            args.name
                        ),
                    ),
                }
                if args.wait
                    && !wait_gone(sd, &args.name, Duration::from_secs(args.timeout)).await?
                {
                    note(
                        report,
                        format!(
                            "{} ignored {signal} for {} seconds; killing it",
                            args.name, args.timeout
                        ),
                    );
                    sd.kill_machine(&args.name, "all", libc::SIGKILL).await?;
                }
            }
            _ => {
                if args.wait {
                    if !poweroff_until_gone(sd, &args.name, Duration::from_secs(60)).await? {
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
        if !wait_gone(sd, &args.name, Duration::from_secs(60)).await? {
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
        hostnet::release(sd, &admitted).await;
        let _lock = store.lock()?;
        release_machine(&args.name, record.as_ref())?;
    }
    Ok(StopOutcome::Stopped)
}

/// Asks a booted machine to power off and waits until it is gone, repeating the request
/// every couple of seconds: right after `start` the machine's init may not have installed
/// its signal handlers yet, and the kernel silently drops signals that PID 1 of a PID
/// namespace does not handle.
/// Waits, within reason, until nothing of a previous instance stands in the way of a new
/// start: the unit still up with its program gone (nspawn shutting down), the machine
/// not yet dropped by machined, or the release hook running while the unit deactivates,
/// which would undo what the start prepares. A machine that is really running, or one
/// starting elsewhere, is an error.
async fn wait_for_previous(sd: &Systemd, name: &str, unit: &str, mode: Option<Mode>) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let (_, active) = sd.unit_state(unit).await?;
        let registered = sd.machine_exists(name).await?;
        match active.as_str() {
            "active" | "activating" if registered && machine_alive(sd, name, mode).await => {
                if active == "activating" {
                    bail!("machine {name} is already starting");
                }
                bail!("machine {name} is already running");
            }
            // Up but not registered yet, or registered with nothing running inside, or
            // on its way down: the next poll tells more.
            "active" | "activating" | "reloading" | "deactivating" => {}
            // Down, but machined has yet to drop it.
            _ if registered => {}
            _ => return Ok(()),
        }
        if Instant::now() > deadline {
            bail!("machine {name} is still going away; see journalctl -u {unit}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Whether a registered machine still has something running: its leader, and for an app
/// the program under the stub init (the leader outlives it for a moment).
async fn machine_alive(sd: &Systemd, name: &str, mode: Option<Mode>) -> bool {
    let Ok(leader) = sd.machine_leader(name).await else {
        return false;
    };
    if !std::path::Path::new(&format!("/proc/{leader}")).exists() {
        return false;
    }
    mode != Some(Mode::App) || payload_pid(leader).is_some()
}

/// The active state of a unit once it is no longer on its way down. ExecStopPost=, the
/// release hook, runs while the unit is "deactivating"; nothing may be prepared for the
/// next start until it is done. After `timeout` the state is returned as it is.
async fn settled_unit_state(sd: &Systemd, unit: &str, timeout: Duration) -> Result<String> {
    let deadline = Instant::now() + timeout;
    loop {
        let (_, active) = sd.unit_state(unit).await?;
        if active != "deactivating" || Instant::now() >= deadline {
            return Ok(active);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

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

/// The program an app machine runs: the child of its stub init (the leader). The kernel's
/// children list gives it directly when available; otherwise the oldest child by start
/// time, since anything re-parented to the stub came later.
fn payload_pid(leader: u32) -> Option<i32> {
    if let Ok(children) = std::fs::read_to_string(format!("/proc/{leader}/task/{leader}/children"))
    {
        if let Some(first) = children
            .split_whitespace()
            .next()
            .and_then(|p| p.parse().ok())
        {
            return Some(first);
        }
    }
    let mut oldest: Option<(u64, i32)> = None;
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
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
        if let Some((ppid, started)) = stat_ppid_and_start(&stat) {
            if ppid == leader && oldest.is_none_or(|(s, _)| started < s) {
                oldest = Some((started, pid));
            }
        }
    }
    oldest.map(|(_, pid)| pid)
}

/// Parent PID and start time from a /proc/PID/stat line, whose comm may contain spaces
/// and parentheses: "pid (comm) state ppid ... starttime" (field 22).
fn stat_ppid_and_start(stat: &str) -> Option<(u32, u64)> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // rest starts at field 3 (state), so ppid is index 1 and starttime index 19.
    Some((fields.get(1)?.parse().ok()?, fields.get(19)?.parse().ok()?))
}

/// kill(2) with a raw number: nix's Signal enum stops at the standard signals and images
/// may ask for realtime ones (SIGRTMIN+3 is common for systemd-based images).
fn send_signal(pid: i32, signal: i32) -> nix::Result<()> {
    nix::errno::Errno::result(unsafe { libc::kill(pid, signal) }).map(drop)
}

/// docker exec: through the namespaces of the machine's leader, for every kind of
/// machine, so that the exit code comes back and nothing inside (D-Bus, PAM) is needed.
/// The command runs with the terminal of this process; see `nsenter::exec`.
pub async fn exec_in_namespaces(
    ctx: &Context,
    machine: &str,
    command: &[String],
    user: &str,
) -> Result<i32> {
    let sd = ctx.sd().await?;
    if !sd.machine_exists(machine).await? {
        bail!("machine {machine} is not running");
    }
    let record = ctx.store.load_image(machine)?;
    let record = record.as_ref();
    let leader = sd.machine_leader(machine).await?;
    let user = if user == "root" { None } else { Some(user) };
    let working_dir = record.and_then(|r| r.run.working_dir.as_deref());
    // The image's environment plus what -e added, like the program itself sees: one entry
    // per variable, the later one winning.
    let env: Vec<String> = record
        .map(|r| volume::merge_env(&r.run.env, &r.env))
        .unwrap_or_default();
    tokio::task::block_in_place(|| nsenter::exec(leader, command, user, working_dir, &env))
        .with_context(|| format!("running a command inside {machine}"))
}

/// The login session machined offers for a booted machine: a PTY running `path` (the
/// user's shell when empty) with `args`. A machine that has just been started has no
/// D-Bus yet for a few seconds; OpenMachineShell is retried for a while.
pub async fn open_shell(
    ctx: &Context,
    machine: &str,
    user: &str,
    path: &str,
    args: Vec<String>,
) -> Result<(OwnedFd, String)> {
    let sd = ctx.sd().await?;
    if !sd.machine_exists(machine).await? {
        bail!("machine {machine} is not running");
    }
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

/// What `logs` may be asked for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogsRequest {
    pub machine: String,
    /// Keep reading; starts from the last lines unless `lines` says otherwise.
    pub follow: bool,
    pub lines: Option<u32>,
    /// journalctl --since syntax.
    pub since: Option<String>,
    pub timestamps: bool,
    /// Also what systemd says about the unit, not only what the machine wrote.
    pub all: bool,
    /// Booted machines: the machine's own journal instead of its console output.
    pub inside: bool,
}

/// Lines shown before following when --lines is not given.
const FOLLOW_TAIL: u32 = 10;

/// docker logs: the machine's console output lives in the journal of its service (nspawn
/// pipes the payload's stdout and stderr there); boot machines also have a journal of
/// their own. journalctl is the journal's reader, so it does the work: these are its
/// arguments. Everything the unit ever logged is shown, earlier runs included.
pub fn journalctl_arguments(args: &LogsRequest) -> Vec<String> {
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
    fn realtime_signals_reach_kill() {
        // A PID that cannot exist: the kernel validates the signal number first, so a
        // realtime number must come back as ESRCH, never EINVAL.
        let rt = libc::SIGRTMIN() + 3;
        assert_eq!(send_signal(i32::MAX, rt), Err(nix::errno::Errno::ESRCH));
        assert_eq!(send_signal(std::process::id() as i32, 0), Ok(()));
    }

    #[test]
    fn stat_lines_with_odd_comms() {
        let line = "4242 (a (weird) name) S 17 4242 4242 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 987654 12345 0 18446744073709551615";
        assert_eq!(stat_ppid_and_start(line), Some((17, 987654)));
        assert_eq!(stat_ppid_and_start("garbage"), None);
    }

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
        let base = LogsRequest {
            machine: "web".into(),
            ..LogsRequest::default()
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
        let follow = LogsRequest {
            machine: "web".into(),
            follow: true,
            ..LogsRequest::default()
        };
        let argv = journalctl_arguments(&follow);
        assert!(argv.contains(&"--lines=10".to_string()) && argv.last().unwrap() == "--follow");
        let full = LogsRequest {
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
        let inside = LogsRequest {
            machine: "fedora-44".into(),
            inside: true,
            ..LogsRequest::default()
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
