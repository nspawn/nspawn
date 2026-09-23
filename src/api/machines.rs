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
use crate::policy::{Limits, Restart};
use crate::reference::validate_machine_name;
use crate::settings::{self, Bind, BridgeMount, MachineSettings, Network};
use crate::store::{now_unix, ImageRecord, Store};
use crate::systemd::{Systemd, UnitState};
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

/// One machine or image by name, as `list` shows it: machined's view when it runs, the
/// record as stopped otherwise.
pub async fn get(ctx: &Context, name: &str) -> Result<MachineSummary> {
    let sd = ctx.sd().await?;
    let record = ctx.store.load_image(name)?;
    if sd.machine_exists(name).await? {
        let details = sd.machine_details(name).await.ok();
        let now = now_unix();
        return Ok(MachineSummary {
            name: name.to_string(),
            record,
            state: details
                .as_ref()
                .map(|d| d.state.clone())
                .unwrap_or_else(|| "-".to_string()),
            started: details
                .as_ref()
                .filter(|d| d.started > 0 && d.started <= now)
                .map(|d| d.started),
            leader: details.as_ref().map(|d| d.leader),
            os: sd.machine_os(name).await,
        });
    }
    let unit = sd
        .unit_status(&format!("systemd-nspawn@{name}.service"))
        .await?;
    match record {
        Some(record) => Ok(MachineSummary {
            name: name.to_string(),
            record: Some(record),
            state: unit_word(&unit).to_string(),
            started: None,
            leader: None,
            os: None,
        }),
        None => bail!("no machine or image named {name}"),
    }
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
    // Records that machined does not list: stopped, or between two runs of a restart
    // policy, which ps shows without -a since they are not stopped.
    let running: std::collections::HashSet<&str> =
        machines.iter().map(|m| m.name.as_str()).collect();
    let mut others: Vec<&ImageRecord> = records
        .values()
        .filter(|r| !running.contains(r.name.as_str()))
        .collect();
    others.sort_by(|a, b| a.name.cmp(&b.name));
    let units: Vec<String> = others
        .iter()
        .map(|r| format!("systemd-nspawn@{}.service", r.name))
        .collect();
    let states = sd.unit_states(&units).await?;
    for (r, unit) in others.into_iter().zip(&units) {
        let state = states
            .get(unit)
            .map(unit_word)
            .unwrap_or("stopped")
            .to_string();
        if all || state != "stopped" {
            summaries.push(MachineSummary {
                name: r.name.clone(),
                record: Some(r.clone()),
                state,
                started: None,
                leader: None,
                os: None,
            });
        }
    }
    Ok(summaries)
}

/// How ps names a machine machined does not list, from its unit's state.
pub fn unit_word(state: &UnitState) -> &'static str {
    if state.restarting() {
        "restarting"
    } else {
        match state.active.as_str() {
            "activating" | "active" | "reloading" => "starting",
            "deactivating" => "closing",
            _ => "stopped",
        }
    }
}

/// Why a machine that machined does not list cannot be removed or replaced: its unit is
/// still up, starting, or waiting to restart it. Removing its record then would leave a
/// unit restarting forever with nothing to start.
pub async fn unit_busy(sd: &Systemd, name: &str) -> Result<Option<String>> {
    let state = sd
        .unit_status(&format!("systemd-nspawn@{name}.service"))
        .await?;
    Ok(if state.restarting() {
        Some(format!("machine {name} is restarting; stop it first"))
    } else if state.busy() {
        Some(format!("machine {name} is starting; stop it first"))
    } else {
        None
    })
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
    report: Report<'_>,
) -> Result<Network> {
    let Some(mut record) = record else {
        // Not ours: the stock systemd-nspawn@.service template uses --network-veth.
        hostnet::ensure_networkd(sd, report).await?;
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
        bridge::up(config, sd, report).await?;
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
    // Named volumes live under the state directory and are created on first use; a
    // host path has to exist, as with podman (the service cannot make directories
    // anywhere on the host, nor should it).
    let mut binds = Vec::new();
    for volume in &record.volumes {
        let source = volume.host_path(&store.volumes_dir());
        if !source.exists() {
            if volume.source.starts_with('/') {
                bail!(
                    "volume {volume}: {} does not exist on the host",
                    source.display()
                );
            }
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
        for (file, text) in [
            ("nspawn-volumes.service", service),
            ("nspawn-volumes.conf", dropin),
        ] {
            let path = dir.join(file);
            std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        }
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
    if settings::write_hooks(name, config, &route, record.restart, &record.limits)? {
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
        hostnet::ensure_networkd(sd, report).await?;
    }
    Ok(record.network)
}

/// What `start` may be asked for. Everything but the name is remembered for the image.
#[derive(Debug, Clone, PartialEq)]
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
    /// KEY=VALUE labels on top of the image's; "none" forgets them.
    pub label: Vec<String>,
    /// docker's --restart; None keeps the remembered one.
    pub restart: Option<Restart>,
    /// Bytes, 0 for none; None keeps the remembered limit.
    pub memory: Option<u64>,
    /// CPUs (0.5), 0 for none; None keeps the remembered limit.
    pub cpus: Option<f64>,
    /// Processes, 0 for none; None keeps the remembered limit.
    pub pids_limit: Option<u64>,
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
    /// Its program ended right away and its restart policy is bringing it back.
    Restarting,
}

pub async fn start(ctx: &Context, args: &StartRequest, report: Report<'_>) -> Result<StartOutcome> {
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
    let lock = store.lock().await?;
    // Under the lock, what wait_for_previous saw may have changed: another start may be
    // preparing this machine (its files and network namespace would be redone under it),
    // or it may be up already.
    if store.is_starting(&args.name) {
        bail!("machine {} is starting already", args.name);
    }
    if sd.machine_exists(&args.name).await? {
        bail!("machine {} is already running", args.name);
    }
    let _starting = store.mark_starting(&args.name)?;
    let mut record = store.load_image(&args.name)?;
    let previous_policy = record.as_ref().map(|r| r.restart).unwrap_or_default();
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
            if !args.label.is_empty() {
                r.labels = volume::parse_labels(&args.label)?;
            }
            if let Some(restart) = args.restart {
                r.restart = restart;
            }
            apply_limits(&mut r.limits, args.memory, args.cpus, args.pids_limit)?;
            r.limits.check(r.mode)?;
        }
        None if !args.command.is_empty()
            || args.network.is_some()
            || !args.publish.is_empty()
            || args.entrypoint.is_some()
            || !args.env.is_empty()
            || !args.volume.is_empty()
            || !args.label.is_empty()
            || args.restart.is_some()
            || args.memory.is_some()
            || args.cpus.is_some()
            || args.pids_limit.is_some() =>
        {
            bail!(
                "{} is not an image managed by nspawn; a command, network, ports, variables, volumes, labels, a restart policy or limits need one",
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
    let policy = record.as_ref().map(|r| r.restart);
    let network = prepare(sd, store, config, &args.name, record, report).await?;
    // The unit's own hooks take the lock; it must be free while the unit starts.
    drop(lock);
    // always and unless-stopped start the machine at boot too, as machinectl enable
    // would. A unit is only disabled when its policy was changed away from those, so
    // that one an administrator enabled by hand stays enabled.
    if let Some(policy) = policy {
        let changed = if policy.enabled_at_boot() {
            sd.enable_unit(&unit).await?
        } else if args.restart.is_some() && previous_policy.enabled_at_boot() {
            sd.disable_unit(&unit).await?
        } else {
            false
        };
        if changed {
            sd.reload().await?;
        }
    }
    // firewalld only knows a veth once the machine is registered; published ports need
    // the machine to count as running.
    let firewalld = network == Network::Veth && hostnet::firewalld_running(sd).await;
    if sd.start_machine(&args.name).await? {
        return Ok(StartOutcome::Restarting);
    }
    if args.wait || firewalld || network == Network::Bridge {
        let unit = format!("systemd-nspawn@{}.service", args.name);
        let deadline = Instant::now() + Duration::from_secs(30);
        while !sd.machine_exists(&args.name).await? {
            // A short program may have run and returned already: not a failure.
            let state = sd.unit_status(&unit).await?;
            if state.restarting() {
                return Ok(StartOutcome::Restarting);
            }
            match state.active.as_str() {
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
        hostnet::admit(sd, &args.name, report).await?;
    }
    if network == Network::Bridge {
        let _lock = store.lock().await?;
        bridge::sync_ports(store, sd).await?;
    }
    if args.wait && booted {
        return wait_for_init(sd, &args.name).await;
    }
    Ok(StartOutcome::Started)
}

/// Sets the limits given on this call (0 removes one), keeping the others.
fn apply_limits(
    limits: &mut Limits,
    memory: Option<u64>,
    cpus: Option<f64>,
    pids: Option<u64>,
) -> Result<()> {
    if let Some(memory) = memory {
        limits.memory = memory;
    }
    if let Some(cpus) = cpus {
        limits.milli_cpus = crate::policy::milli_cpus_from(cpus)?;
    }
    if let Some(pids) = pids {
        limits.pids = pids;
    }
    Ok(())
}

/// Registration comes early in a booted machine's life; a command run right after `start`
/// would find no service manager to talk to. Wait, within reason, until the machine's
/// systemd listens on its private socket.
async fn wait_for_init(sd: &Systemd, name: &str) -> Result<StartOutcome> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let Ok(leader) = sd.machine_leader(name).await else {
            // Gone while booting: say so instead of "started".
            let state = sd
                .unit_status(&format!("systemd-nspawn@{name}.service"))
                .await?;
            if state.restarting() {
                return Ok(StartOutcome::Restarting);
            }
            if state.active == "failed" {
                bail!("{name} died while booting; see journalctl -u systemd-nspawn@{name}.service");
            }
            return Ok(StartOutcome::Started);
        };
        if std::path::Path::new(&format!("/proc/{leader}/root/run/systemd/private")).exists()
            || Instant::now() > deadline
        {
            return Ok(StartOutcome::Started);
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

/// Where `stop` queues the unit's stop job, which is what keeps a restart policy from
/// bringing the machine back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Latch {
    /// No policy: nothing restarts it, stop as before.
    NotNeeded,
    /// Before the first signal: a booted machine's init shuts down the same way either
    /// way.
    BeforeSignal,
    /// Right after the signal: SIGKILL with `--force` (a stop job first would have
    /// machined refuse the kill of a machine it already closes), or an app's stop
    /// signal, after its time to act on it when `wait`, so that the stub init does not
    /// add SIGTERM and SIGHUP while the program handles its own. Should the machine end
    /// in between, the stop job also cancels the restart systemd has scheduled.
    AfterSignal,
}

pub fn latch(restart: Restart, force: bool, mode: Option<Mode>) -> Latch {
    if restart == Restart::No {
        Latch::NotNeeded
    } else if force || mode == Some(Mode::App) {
        Latch::AfterSignal
    } else {
        Latch::BeforeSignal
    }
}

pub async fn stop(ctx: &Context, args: &StopRequest, report: Report<'_>) -> Result<StopOutcome> {
    let sd = ctx.sd().await?;
    let store = &ctx.store;
    let record = store.load_image(&args.name)?;
    let unit = format!("systemd-nspawn@{}.service", args.name);
    let policy = record.as_ref().map(|r| r.restart).unwrap_or_default();
    // unless-stopped: stopped by hand means not at boot either, until the next start.
    if policy == Restart::UnlessStopped && sd.disable_unit(&unit).await? {
        sd.reload().await?;
    }
    if !sd.machine_exists(&args.name).await? {
        match &record {
            // It ended on its own or elsewhere, or it is between two runs of its restart
            // policy; leave nothing of it behind, like docker stop on a stopped container.
            // The stop job ends a pending restart, and it completes once the release hook
            // of the last run is done, so nothing is released beside it.
            Some(r) => {
                let state = sd.unit_status(&unit).await?;
                sd.stop_unit_job(&unit).await?.wait().await?;
                release_machine(&args.name, Some(r))?;
                sd.reset_failed(&unit).await?;
                return Ok(if state.busy() {
                    StopOutcome::Stopped
                } else {
                    StopOutcome::WasNotRunning
                });
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
    let mode = record.as_ref().map(|r| r.mode);
    let latch = latch(policy, args.force, mode);
    let mut job = None;
    if latch == Latch::BeforeSignal {
        job = Some(sd.stop_unit_job(&unit).await?);
    }
    if args.force {
        // docker kill: no questions asked. systemd 255 answers "Invalid argument" when it
        // cannot signal some process of the unit although the machine got its SIGKILL,
        // and machined may keep it listed for a while: the stop job goes in whatever the
        // answer, so that a restart policy does not bring it back meanwhile, and what
        // counts is that the machine goes (awaited below when `wait`).
        let killed = sd.kill_machine(&args.name, "all", libc::SIGKILL).await;
        if latch == Latch::AfterSignal {
            job = Some(sd.stop_unit_job(&unit).await?);
        }
        if let Err(e) = killed {
            note(
                report,
                format!("note: {e:#}; waiting for {} to go", args.name),
            );
        }
    } else {
        match mode {
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
                if latch == Latch::AfterSignal && !args.wait {
                    job = Some(sd.stop_unit_job(&unit).await?);
                }
                if args.wait {
                    let gone = wait_gone(sd, &args.name, Duration::from_secs(args.timeout)).await?;
                    if latch == Latch::AfterSignal {
                        job = Some(sd.stop_unit_job(&unit).await?);
                    }
                    if !gone {
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
            }
            _ => {
                if args.wait {
                    if !poweroff_until_gone(sd, &args.name, Duration::from_secs(60)).await? {
                        bail!(
                            "machine {} is still running after 60 seconds; use --force",
                            args.name
                        );
                    }
                } else if let Err(e) = sd.poweroff_machine(&args.name).await {
                    if sd.machine_exists(&args.name).await? {
                        return Err(e);
                    }
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
        match job {
            Some(job) => job.wait().await?,
            None => sd.stop_unit(&unit).await?,
        }
        sd.reset_failed(&unit).await?;
        hostnet::release(sd, &admitted).await;
        let _lock = store.lock().await?;
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
        let state = sd.unit_status(unit).await?;
        if state.restarting() {
            bail!("machine {name} is restarting after its program ended; stop it first, or see journalctl -u {unit}");
        }
        let active = state.active;
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

/// Starts a command inside a machine without waiting for it: what the bus service hands
/// out with its streams. `extra_env` comes after the image's and the remembered one.
pub async fn spawn_in_namespaces(
    ctx: &Context,
    machine: &str,
    command: &[String],
    user: &str,
    extra_env: &[String],
    stdio: nsenter::Stdio,
) -> Result<nsenter::Process> {
    let sd = ctx.sd().await?;
    if !sd.machine_exists(machine).await? {
        bail!("machine {machine} is not running");
    }
    let record = ctx.store.load_image(machine)?;
    let record = record.as_ref();
    let leader = sd.machine_leader(machine).await?;
    let user = if user == "root" || user.is_empty() {
        None
    } else {
        Some(user)
    };
    let working_dir = record.and_then(|r| r.run.working_dir.as_deref());
    let env = record
        .map(|r| volume::merge_env(&r.run.env, &r.env))
        .unwrap_or_default();
    let env = volume::merge_env(&env, extra_env);
    tokio::task::block_in_place(|| nsenter::spawn(leader, command, user, working_dir, &env, stdio))
        .with_context(|| format!("running a command inside {machine}"))
}

/// The login session machined offers for a booted machine: a PTY running `path` (the
/// user's shell when empty) with `args` and `env`. A machine that has just been started
/// has no D-Bus yet for a few seconds; OpenMachineShell is retried for a while.
pub async fn open_shell(
    ctx: &Context,
    machine: &str,
    user: &str,
    path: &str,
    args: Vec<String>,
    env: Vec<String>,
) -> Result<(OwnedFd, String)> {
    let sd = ctx.sd().await?;
    if !sd.machine_exists(machine).await? {
        bail!("machine {machine} is not running");
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match sd
            .open_shell(machine, user, path, args.clone(), env.clone())
            .await
        {
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
    fn the_stop_job_goes_where_nothing_is_lost() {
        use Latch::*;
        for mode in [Some(Mode::App), Some(Mode::Boot), None] {
            for force in [false, true] {
                assert_eq!(latch(Restart::No, force, mode), NotNeeded);
            }
        }
        for policy in [Restart::OnFailure, Restart::Always, Restart::UnlessStopped] {
            assert_eq!(latch(policy, false, Some(Mode::App)), AfterSignal);
            assert_eq!(latch(policy, true, Some(Mode::App)), AfterSignal);
            assert_eq!(latch(policy, true, Some(Mode::Boot)), AfterSignal);
            assert_eq!(latch(policy, false, Some(Mode::Boot)), BeforeSignal);
            assert_eq!(latch(policy, false, None), BeforeSignal);
        }
    }

    #[test]
    fn machines_machined_does_not_list_are_named_by_their_unit() {
        let state = |active: &str, sub: &str| UnitState {
            load: "loaded".to_string(),
            active: active.to_string(),
            sub: sub.to_string(),
        };
        assert_eq!(
            unit_word(&state("activating", "auto-restart")),
            "restarting"
        );
        assert_eq!(
            unit_word(&state("activating", "auto-restart-queued")),
            "restarting"
        );
        assert_eq!(unit_word(&state("activating", "start-pre")), "starting");
        assert_eq!(unit_word(&state("active", "running")), "starting");
        assert_eq!(unit_word(&state("deactivating", "stop-post")), "closing");
        assert_eq!(unit_word(&state("inactive", "dead")), "stopped");
        assert_eq!(unit_word(&state("failed", "failed")), "stopped");
    }

    #[test]
    fn limits_given_on_a_start_replace_only_themselves() {
        let mut limits = Limits {
            memory: 64 << 20,
            milli_cpus: 500,
            pids: 100,
        };
        apply_limits(&mut limits, None, Some(2.0), Some(0)).unwrap();
        assert_eq!(
            limits,
            Limits {
                memory: 64 << 20,
                milli_cpus: 2000,
                pids: 0,
            }
        );
        apply_limits(&mut limits, Some(0), None, None).unwrap();
        assert_eq!(limits.memory, 0);
        assert!(apply_limits(&mut limits, None, Some(-1.0), None).is_err());
    }

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
