//! Machines: listing, start, stop, kill, update, exec and logs.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
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
use crate::reference::{validate_entry_name, validate_machine_name};
use crate::settings::{self, Bind, BridgeMount, MachineSettings, Network};
use crate::store::{now_unix, ImageRecord, Store};
use crate::systemd::{Systemd, UnitState};
use crate::volume;

/// A machine as `ps` shows it: machined's state and nspawn's record when there is one.
#[derive(Debug, Clone)]
pub struct MachineSummary {
    pub name: String,
    pub record: Option<ImageRecord>,
    /// machined's state ("opening", "running", "closing"), or "stopped".
    pub state: String,
    /// Unix seconds, for running machines.
    pub started: Option<u64>,
    /// The machine's init, as the host sees it.
    pub leader: Option<u32>,
    /// PRETTY_NAME of the machine's os-release.
    pub os: Option<String>,
    /// The verdict of its healthcheck, while it runs and has one.
    pub health: Option<crate::health::Status>,
    /// How its last run ended, for a stopped machine: its exit code as `run` reports it.
    pub exit_code: Option<i32>,
    /// When that run ended, unix seconds, when that is known.
    pub finished: Option<u64>,
}

/// The exit code of the machine's last run, as an attached run would have given it.
pub(crate) async fn last_exit_code(
    sd: &Systemd,
    store: &Store,
    record: &ImageRecord,
) -> Option<i32> {
    last_exit(sd, store, record).await.map(|(code, _)| code)
}

/// `last_exit_code`, and when that run ended (unix seconds), when that is known.
pub(crate) async fn last_exit(
    sd: &Systemd,
    store: &Store,
    record: &ImageRecord,
) -> Option<(i32, Option<u64>)> {
    // Only this machine's runs: the unit and its journal are those of every machine
    // that had the name. A record of a release that did not keep the first start says
    // nothing of it, and its runs are counted from its making.
    let since = match record.started {
        Some(started) => started,
        None if store
            .starts_kept_since()
            .is_none_or(|kept| record.created < kept) =>
        {
            record.created
        }
        None => return None,
    };
    let unit = format!("systemd-nspawn@{}.service", record.name);
    // The unit's own ExecMainCode= goes with it when it is unloaded; the journal keeps
    // what systemd logged when it reaped the process.
    let (code, status, at_usec) = match sd.exec_main_exit(&unit).await {
        Ok(Some((code, status))) => (
            code,
            status,
            sd.exec_main_exit_usec(&unit).await.unwrap_or(0),
        ),
        _ => crate::journal::last_main_exit(&unit, since).await?,
    };
    let ending = crate::api::run::Ending {
        code,
        status,
        result: String::new(),
    };
    let code = crate::api::run::exit_code(
        &ending,
        store.last_signal(&record.name),
        record.under_stub_init(),
    );
    Some((code, (at_usec > 0).then_some(at_usec / 1_000_000)))
}

/// machined's state, or "paused" while the unit's cgroup is frozen.
async fn running_state(sd: &Systemd, name: &str, state: String) -> String {
    let unit = format!("systemd-nspawn@{name}.service");
    if state == "running" && sd.freezer_state(&unit).await.ok().as_deref() == Some("frozen") {
        return "paused".to_string();
    }
    state
}

/// Refuses machined's non-containers (libvirt's virtual machines), which nspawn can neither
/// enter nor stop.
pub async fn refuse_foreign(sd: &Systemd, name: &str) -> Result<()> {
    if let Some(runner) = sd.foreign_machine(name).await? {
        bail!("{name} is a machine of {runner}, not a container; nspawn manages systemd-nspawn machines only");
    }
    Ok(())
}

pub async fn get(ctx: &Context, name: &str) -> Result<MachineSummary> {
    validate_entry_name(name)?;
    let sd = ctx.sd().await?;
    refuse_foreign(sd, name).await?;
    let record = ctx.store.load_image(name)?;
    if sd.machine_exists(name).await? {
        let details = sd.machine_details(name).await.ok();
        let now = now_unix();
        let state = details
            .as_ref()
            .map(|d| d.state.clone())
            .unwrap_or_else(|| "-".to_string());
        return Ok(MachineSummary {
            name: name.to_string(),
            record,
            state: running_state(sd, name, state).await,
            started: details
                .as_ref()
                .filter(|d| d.started > 0 && d.started <= now)
                .map(|d| d.started),
            leader: details.as_ref().map(|d| d.leader),
            os: sd.machine_os(name).await,
            health: crate::health::read_status(name),
            exit_code: None,
            finished: None,
        });
    }
    let unit = sd
        .unit_status(&format!("systemd-nspawn@{name}.service"))
        .await?;
    match record {
        Some(record) => {
            let state = unit_word(&unit).to_string();
            let (exit_code, finished) = if state == "stopped" || state == "restarting" {
                match last_exit(sd, &ctx.store, &record).await {
                    Some((code, finished)) => (Some(code), finished),
                    None => (None, None),
                }
            } else {
                (None, None)
            };
            Ok(MachineSummary {
                name: name.to_string(),
                record: Some(record),
                state,
                started: None,
                leader: None,
                os: None,
                health: None,
                exit_code,
                finished,
            })
        }
        None => bail!("no machine or image named {name}"),
    }
}

/// With `all`, nspawn's machines that do not run too.
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
        let state = details
            .as_ref()
            .map(|d| d.state.clone())
            .unwrap_or_else(|| "-".to_string());
        summaries.push(MachineSummary {
            name: m.name.clone(),
            record: records.get(&m.name).cloned(),
            state: running_state(sd, &m.name, state).await,
            started: details
                .as_ref()
                .filter(|d| d.started > 0 && d.started <= now)
                .map(|d| d.started),
            leader: details.as_ref().map(|d| d.leader),
            os: sd.machine_os(&m.name).await,
            health: crate::health::read_status(&m.name),
            exit_code: None,
            finished: None,
        });
    }
    // Records machined does not list. Those between two runs of a restart policy show
    // without -a; starting, closing and stopped ones only with -a.
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
        if listed(all, &state) {
            // Between two runs, the code of the run that ended.
            let (exit_code, finished) = if state == "stopped" || state == "restarting" {
                match last_exit(sd, &ctx.store, r).await {
                    Some((code, finished)) => (Some(code), finished),
                    None => (None, None),
                }
            } else {
                (None, None)
            };
            summaries.push(MachineSummary {
                name: r.name.clone(),
                record: Some(r.clone()),
                state,
                started: None,
                leader: None,
                os: None,
                health: None,
                exit_code,
                finished,
            });
        }
    }
    Ok(summaries)
}

fn listed(all: bool, state: &str) -> bool {
    all || state == "restarting"
}

/// Whether SIGKILL to a machine machined does not list ends something: its restarts, or a
/// start still under way.
fn kill_ends(state: &UnitState) -> bool {
    matches!(unit_word(state), "restarting" | "starting")
}

/// ps's state for a machine machined does not list. systemd-nspawn registers the machine
/// before it is ready, so an active unit without a machine is one whose machine ended.
pub fn unit_word(state: &UnitState) -> &'static str {
    if state.restarting() {
        "restarting"
    } else {
        match state.active.as_str() {
            "activating" => "starting",
            "active" | "reloading" | "deactivating" => "closing",
            _ => "stopped",
        }
    }
}

/// Why a machine machined does not list cannot be removed or replaced yet: its unit is up
/// or restarting, and would go on restarting with nothing to start.
pub async fn unit_busy(sd: &Systemd, name: &str) -> Result<Option<String>> {
    let unit = format!("systemd-nspawn@{name}.service");
    let state = sd.unit_status(&unit).await?;
    // Its release hook runs while it shuts down, reading the record and the files a
    // removal or a replacement takes.
    if state.shutting_down() {
        return Ok(Some(format!(
            "machine {name} is shutting down; wait for it or stop it first"
        )));
    }
    // A start held in systemd's queue behind the machines it depends on runs later
    // from what is loaded now, files gone or not.
    if !state.busy() && sd.start_queued(&unit).await? {
        return Ok(Some(format!(
            "machine {name} is about to start, once the machines it depends on are; stop it first"
        )));
    }
    Ok(if !state.busy() {
        None
    } else if state.restarting() {
        Some(format!("machine {name} is restarting; stop it first"))
    } else if unit_word(&state) == "starting" {
        Some(format!("machine {name} is starting; stop it first"))
    } else {
        Some(format!(
            "machine {name} is shutting down; wait for it or stop it first"
        ))
    })
}

/// Where the host interfaces of --interface go: into the settings file, for
/// systemd-nspawn to move where it makes the machine's network namespace, or moved by
/// the hooks into the namespace nspawn prepares for an app on the bridge, which
/// NamespacePath= hands over with no room for Interface= beside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterfaceRoute {
    Settings,
    Hooks,
}

pub fn interface_route(record: &ImageRecord) -> InterfaceRoute {
    if record.mode == Mode::App && bridge::bridge_kind(record) {
        InterfaceRoute::Hooks
    } else {
        InterfaceRoute::Settings
    }
}

/// The interfaces the hooks move for a machine: none where systemd-nspawn moves them.
pub fn hook_interfaces(record: &ImageRecord) -> &[String] {
    match interface_route(record) {
        InterfaceRoute::Hooks => &record.tuning.interfaces,
        InterfaceRoute::Settings => &[],
    }
}

/// Moves the machine's interfaces into its namespace; on a failure what was moved by
/// then comes back.
fn move_interfaces(name: &str, interfaces: &[String]) -> Result<()> {
    for (i, iface) in interfaces.iter().enumerate() {
        if let Err(e) = bridge::move_interface(iface, name) {
            return_interfaces(name, &interfaces[..i]);
            return Err(e);
        }
    }
    Ok(())
}

/// Gives the machine's interfaces back to the host, best effort: one it holds no more
/// is skipped, a failure is a warning, so that the rest of a release goes on.
pub fn return_interfaces(name: &str, interfaces: &[String]) {
    for iface in interfaces {
        if let Err(e) = bridge::return_interface(iface, name) {
            eprintln!("warning: {e:#}");
        }
    }
}

/// What --interface asks of the host, checked before anything is made: the machine has
/// a network namespace of its own, each interface is on the host and nspawn's to give,
/// no running machine names it, and the host can move it.
async fn check_interfaces(
    sd: &Systemd,
    store: &Store,
    config: &Config,
    name: &str,
    record: &ImageRecord,
    report: Report<'_>,
) -> Result<()> {
    let interfaces = &record.tuning.interfaces;
    let Some(first) = interfaces.first() else {
        return Ok(());
    };
    if record.network == Network::Host {
        bail!("{name} shares the host's network, where {first} is already; --interface is for a machine with a network namespace of its own");
    }
    if let Some(owner) = bridge::shares_network(record) {
        bail!("{name} shares the network namespace of {owner}; give the interface to {owner} (nspawn start {owner} --interface {first})");
    }
    if record.backend == BackendChoice::Mstack {
        bail!("{name} runs under managed user namespaces (mstack), where a host interface cannot be moved in; pull it again with --backend overlay");
    }
    let route = interface_route(record);
    let sys_net = Path::new(bridge::SYS_NET);
    let others = store.list_images()?;
    for iface in interfaces {
        for other in others
            .iter()
            .filter(|o| o.name != name && o.tuning.interfaces.contains(iface))
        {
            if store.is_starting(&other.name) || sd.machine_exists(&other.name).await? {
                bail!(
                    "{iface} is set for {}, which is running; stop it first, or start it with --interface none",
                    other.name
                );
            }
        }
        // A namespace that just died gives its interfaces back a moment later.
        let deadline = Instant::now() + Duration::from_secs(3);
        while !sys_net.join(iface).exists() {
            if Instant::now() > deadline {
                bail!("{iface} is not on the host (ip link lists what is); a machine that ended a moment ago may still have it");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if bridge::nspawn_owned(iface, &config.bridge) {
            bail!("{iface} is a bridge or a veth end of nspawn's own; it stays on the host");
        }
        if let Some(master) = bridge::master_of(sys_net, iface) {
            bail!("{iface} is a port of {master}; take it off it first (ip link set {iface} nomaster)");
        }
        let v4 = std::fs::read_to_string("/proc/net/route").unwrap_or_default();
        let v6 = std::fs::read_to_string("/proc/net/ipv6_route").unwrap_or_default();
        if bridge::default_route_interfaces(&v4).contains(iface)
            || bridge::default_route_interfaces_v6(&v6).contains(iface)
        {
            note(
                report,
                format!("warning: {iface} carries the host's default route; the host loses it while {name} runs"),
            );
        }
        if let Some(phy) = bridge::wireless_phy(sys_net, iface) {
            match route {
                InterfaceRoute::Hooks => {
                    if !bridge::iw_available() {
                        bail!("iw is needed to move a wireless interface ({iface} is one); install it");
                    }
                }
                InterfaceRoute::Settings => {
                    let version = sd.version().await?;
                    if crate::backend::systemd_major(&version)
                        .is_none_or(|major| major < settings::WIRELESS_INTERFACE_SINCE)
                    {
                        bail!(
                            "moving a wireless interface into a booted machine, or one without a network, takes systemd {} or newer; this host runs {version}; an app machine on the bridge can take {iface} on any version",
                            settings::WIRELESS_INTERFACE_SINCE
                        );
                    }
                }
            }
            let others: Vec<String> = bridge::phy_interfaces(sys_net, &phy)
                .into_iter()
                .filter(|i| i != iface && !interfaces.contains(i))
                .collect();
            if !others.is_empty() {
                note(
                    report,
                    format!(
                        "note: {phy} carries {} too, which move with {iface}: a phy moves whole",
                        others.join(", ")
                    ),
                );
            }
        }
    }
    Ok(())
}

/// Where the CDI device nodes the host lacks are made for a machine.
/// Whether the running kernel is MAJOR.MINOR or later.
fn kernel_at_least(major: u32, minor: u32) -> bool {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .is_ok_and(|text| release_at_least(&text, major, minor))
}

fn release_at_least(release: &str, major: u32, minor: u32) -> bool {
    let mut parts = release
        .trim()
        .split(|c: char| !c.is_ascii_digit())
        .map(|n| n.parse::<u32>().unwrap_or(0));
    let (have_major, have_minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    (have_major, have_minor) >= (major, minor)
}

fn cdi_nodes_dir(store: &Store, name: &str) -> PathBuf {
    store.machine_files_dir(name).join("cdi").join("nodes")
}

/// Who calls `prepare`: both passes do the same checks and write the same files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pass {
    /// `start`, ahead of the unit, so that a refusal reaches the caller.
    Ahead,
    /// The unit's ExecStartPre, right before systemd-nspawn: the only pass that moves
    /// host interfaces in, so that their device units are still active when the unit's
    /// start job is queued (it waits for them), and so that they move once per start.
    Hook,
}

/// Writes a machine's unit drop-in from its record: what systemd reads when it starts the
/// unit itself (machinectl, a dependent's Wants=, boot) before any hook rewrites it.
pub async fn rewrite_hooks(ctx: &Context, sd: &Systemd, record: &ImageRecord) -> Result<()> {
    let route =
        settings::namespace_route(sd, &record.name, record.mode, bridge::namespaced(record))
            .await?;
    // The CDI nodes' rules, for a first start systemd makes itself: its hook pass makes
    // them under the unit's device policy. Specs that do not resolve now are the start's
    // to report.
    let mut tuning = record.tuning.clone();
    if let Some(plan) = crate::cdi::plan(&record.tuning).ok().flatten() {
        tuning.devices.extend(crate::cdi::device_paths(
            &plan,
            &cdi_nodes_dir(&ctx.store, &record.name),
        ));
    }
    let app_argv =
        settings::wrapped_argv(sd, &record.name, record.mode, &route, &record.tuning).await?;
    let reload = settings::write_hooks(
        &record.name,
        &ctx.config,
        &route,
        app_argv.as_deref(),
        &settings::HookSpec {
            restart: record.restart,
            limits: &record.limits,
            remove_on_exit: record.remove_on_exit,
            tuning: &tuning,
            api_socket: record.takes_api_socket(),
        },
    )?;
    reload_if_needed(
        sd,
        &format!("systemd-nspawn@{}.service", record.name),
        reload,
    )
    .await
}

/// Whether an update leaves an unless-stopped machine off the boot list: stopped by hand,
/// it stays so until its next start, whatever else changes, unless-stopped given again
/// too.
fn stopped_by_hand(now: Restart, before: Restart, running: bool) -> bool {
    now == Restart::UnlessStopped && before == Restart::UnlessStopped && !running
}

/// Reloads systemd when a drop-in was written now, or when the reload after an earlier
/// write failed and left the unit on its old one, which systemd says.
async fn reload_if_needed(sd: &Systemd, unit: &str, written: bool) -> Result<()> {
    if written || sd.needs_reload(unit).await.unwrap_or(false) {
        sd.reload().await?;
    }
    Ok(())
}

/// A path that two mounts take, one of them a volume, a secret or a device: one would
/// hide the other, which one depending on how systemd-nspawn orders them. A volume on
/// the path of another or of a tmpfs is refused; two tmpfs on one path are one tmpfs.
pub(crate) fn duplicate_mount_points(record: &ImageRecord) -> Result<()> {
    let mut taken: std::collections::HashSet<String> = record
        .tuning
        .tmpfs
        .iter()
        .map(|mount| volume::mount_point(mount.split_once(':').map_or(mount.as_str(), |(p, _)| p)))
        .collect();
    let others = record
        .volumes
        .iter()
        .map(|v| v.target.as_str())
        .chain(record.tuning.secrets.iter().map(|s| s.target.as_str()))
        .chain(record.tuning.devices.iter().map(|d| d.container.as_str()));
    for target in others {
        let target = volume::mount_point(target);
        if !taken.insert(target.clone()) {
            bail!("Duplicate mount point: {target}");
        }
    }
    Ok(())
}

/// Whether a volume is filled from the image: a named one made now, or one `volume create`
/// or compose made ahead and still empty when a new machine first takes it.
fn seeds(
    volume: &crate::volume::Volume,
    made: bool,
    first_start: bool,
    empty: impl FnOnce() -> bool,
) -> bool {
    !volume.source.starts_with('/') && (made || (first_start && empty()))
}

/// A machine other than `name` that mounts the named volume `volume` and runs, or is on
/// its way to.
async fn running_user_of(
    sd: &Systemd,
    store: &Store,
    volume: &str,
    name: &str,
) -> Result<Option<String>> {
    for record in store.list_images()? {
        if record.name == name
            || !record
                .volumes
                .iter()
                .any(|v| v.is_named() && v.source == volume)
        {
            continue;
        }
        let unit = format!("systemd-nspawn@{}.service", record.name);
        if sd.machine_exists(&record.name).await? || mounts_volumes(&sd.unit_status(&unit).await?) {
            return Ok(Some(record.name));
        }
    }
    Ok(None)
}

/// Whether a unit's machine has its volumes mounted, or is about to mount them again: one
/// still in its ExecStartPre hooks has mounted nothing, and waits for the lock of the
/// preparation that asks.
fn mounts_volumes(state: &UnitState) -> bool {
    state.busy() && !matches!(state.sub.as_str(), "condition" | "start-pre")
}

/// Everything a machine needs before its unit starts. Shared by `start` and the
/// ExecStartPre hook, so idempotent; the caller holds the store lock.
pub async fn prepare(
    sd: &Systemd,
    store: &Store,
    config: &Config,
    name: &str,
    record: Option<ImageRecord>,
    pass: Pass,
    report: Report<'_>,
) -> Result<Network> {
    let Some(mut record) = record else {
        // Not ours: the stock template uses --network-veth.
        hostnet::ensure_networkd(sd, report).await?;
        return Ok(Network::Veth);
    };
    // A start systemd makes (a dependent's Wants=, machinectl) while an update swaps the
    // machine's image would run it from a half-made record.
    if pass == Pass::Hook && store.rollback_dir(name).exists() {
        use crate::api::recreate::Phase;
        let phase = crate::api::recreate::progress(store, name).phase;
        if matches!(
            phase,
            Phase::Saved | Phase::Aside | Phase::Restoring | Phase::Rejected
        ) {
            bail!("{name} is being updated, or an update of it was cut short, which nspawn auto-update puts back; it does not start meanwhile");
        }
    }
    // From its first start on it is a machine, not an image only.
    let first_start = record.started.is_none();
    if first_start {
        record.started = Some(now_unix());
    }
    // A new run: the marks `kill` and `stop` left for the last one go.
    store.take_exit_on_next(name)?;
    store.forget_signal(name)?;
    // A run that ended without its release hook may have left them in a namespace
    // this pass makes anew.
    return_interfaces(name, hook_interfaces(&record));
    let bridged = bridge::bridge_kind(&record);
    if bridged && record.mode == Mode::App && record.backend == BackendChoice::Mstack {
        bail!(
            "{name} is an mstack app: managed user namespaces cannot join the network namespace prepared for the bridge; pull it again with --backend overlay, or start it with --network host"
        );
    }
    let shared = bridge::shares_network(&record).map(str::to_string);
    if let Some(owner) = &shared {
        if record.mode != Mode::App {
            bail!("{name} boots an init system, whose systemd would configure the interfaces of {owner} again; --network container: is for app images");
        }
        if !record.ports.is_empty() {
            bail!("{name} shares the network of {owner}: ports are published there, with -p on {owner}");
        }
        if !record.tuning.sysctls.is_empty() {
            bail!("--sysctl values are set in the network namespace nspawn makes for a machine; {name} shares the one of {owner}");
        }
        if !record.tuning.dns.is_empty()
            || !record.tuning.dns_search.is_empty()
            || !record.tuning.extra_hosts.is_empty()
        {
            bail!("--dns, --dns-search and --add-host shape the resolv.conf and hosts files of a machine with a network of its own; {name} takes the ones of {owner}");
        }
    }
    if !record.ports.is_empty() && !bridged {
        if record.no_network {
            bail!("ports are published through a bridge network; {name} has no network (--network none)");
        }
        bail!("ports are published through the bridge network; start {name} with --network bridge");
    }
    if record.network == Network::Veth && record.mode == Mode::App {
        bail!(
            "{name} is an app image, with nothing inside to configure a veth; use --network bridge or --network host"
        );
    }
    if !record.tuning.sysctls.is_empty() && (record.mode != Mode::App || !bridged) {
        bail!("--sysctl values are set in the network namespace nspawn makes for an app machine on a bridge network; {name} has none");
    }
    if record.backend == BackendChoice::Mstack && !record.tuning.secrets.is_empty() {
        bail!("{name} runs under managed user namespaces (mstack), where secrets cannot be attached yet; pull it again with --backend overlay");
    }
    check_interfaces(sd, store, config, name, &record, report).await?;
    let files = if bridged {
        let all = crate::api::network::all(store, config)?;
        let nets = crate::api::network::nets_of(store, config, &record)?;
        if nets[0].internal && !record.ports.is_empty() {
            bail!(
                "network {} is internal: nothing is published from it; start {name} with -p none or on another network",
                nets[0].name
            );
        }
        for net in &nets {
            bridge::up(net, &all, sd, report).await?;
        }
        bridge::check_port_conflicts(store, sd, &record).await?;
        store.record_image(&record)?;
        let addrs = bridge::prepare_machine(store, config, &nets, &all, &mut record)?;
        if record.mode == Mode::App {
            let attached: Vec<(&crate::bridge::NetSpec, std::net::Ipv4Addr)> =
                nets.iter().zip(addrs).collect();
            let gateway = nets[bridge::gateway_index(&nets)].subnet.gateway();
            bridge::create_netns(&attached, name, gateway, &record.tuning.sysctls)?;
        }
        let extras: Vec<String> = (1..nets.len())
            .map(|i| bridge::host_end_name_at(name, i))
            .collect();
        Some((
            store.machine_files_dir(name),
            nets[0].interface.clone(),
            extras,
        ))
    } else if let Some(owner) = &shared {
        // The other machine's namespace under this machine's name, and its generated
        // hosts and resolv.conf files, which follow that network's members.
        let owner_record = store.load_image(owner)?.with_context(|| {
            format!("{name} shares the network of {owner}, which is not a machine of nspawn's")
        })?;
        if !bridge::bridge_kind(&owner_record) {
            bail!("{name} shares the network of {owner}, which has no network of its own to share");
        }
        // A machine it depends on is started by systemd before its unit's hooks run,
        // the second pass of a start, which attaches the namespace then.
        let later =
            pass == Pass::Ahead && record.tuning.depends_on.iter().any(|d| d.name == *owner);
        if !sd.machine_exists(owner).await? && !later {
            if record.tuning.depends_on.iter().any(|d| d.name == *owner) {
                bail!("{name} shares the network of {owner}, which is not running; see journalctl -u systemd-nspawn@{owner}.service");
            }
            bail!(
                "{name} shares the network of {owner}, which is not running; start {owner} first, or give {name} --depends-on {owner}"
            );
        }
        let nets = crate::api::network::nets_of(store, config, &owner_record)?;
        if sd.machine_exists(owner).await? {
            let leader = sd.machine_leader(owner).await?;
            bridge::attach_netns(name, leader)?;
        }
        store.record_image(&record)?;
        Some((
            store.machine_files_dir(owner),
            nets[0].interface.clone(),
            Vec::new(),
        ))
    } else {
        store.record_image(&record)?;
        None
    };
    // nspawn has no anonymous volumes: what the image expects a volume at lives in the
    // machine, and a recreate loses it.
    for path in &record.run.volumes {
        if !record.volumes.iter().any(|v| v.target == *path) {
            note(
                report,
                format!("note: {path} is a volume of the image and nothing is mounted there: what {name} writes there goes with the machine; -v NAME:{path} keeps it"),
            );
        }
    }
    // Named volumes are made on first use; a host path must exist, as with podman: the
    // service does not make directories anywhere on the host.
    if record.backend == BackendChoice::Overlay {
        crate::backend::mount_root(sd, store, name).await?;
    }
    if record.takes_api_socket() && record.backend == BackendChoice::Mstack {
        bail!("{name} mounts docker's socket, which nspawn's Docker API cannot reach into an mstack machine yet; make it with --backend overlay");
    }
    let mut binds = Vec::new();
    duplicate_mount_points(&record)?;
    for volume in &record.volumes {
        if volume.is_api_socket() {
            let socket = crate::dockerapi::socket_path(name);
            // A machine whose unit predates the socket's Wants= opens it here.
            if pass == Pass::Hook && !socket.exists() {
                let _ = sd.start_unit(&format!("nspawn-api@{name}.socket")).await;
            }
            if pass == Pass::Hook && !socket.exists() {
                bail!(
                    "{} is missing: nspawn-api@{name}.socket did not open it (the packages ship nspawn-api@.socket, nspawn daemon --install writes it); see systemctl status nspawn-api@{name}.socket",
                    socket.display()
                );
            }
            // In the journal too for a start by systemd: a tool that worked with what
            // this host serves there gets another answer.
            if std::path::Path::new(&volume.source).exists() {
                note(
                    report,
                    format!(
                        "note: {} in {name} is nspawn's Docker API, not this host's docker",
                        volume.target
                    ),
                );
            }
            // In a user namespace the socket is bound idmapped, which a tmpfs such as
            // /run takes from Linux 6.3 on. As the settings file has it: an app on a
            // bridge or in another machine's network runs without one, --network none
            // and a booted machine with one.
            let private_users =
                !crate::settings::app_without_userns(record.mode, record.network, files.is_some());
            if private_users && !kernel_at_least(6, 3) {
                bail!(
                    "{name} runs in a user namespace, where the Docker API socket is bound idmapped, which needs Linux 6.3 or later; run it as an app on a bridge network, or on a newer kernel"
                );
            }
            binds.push(Bind {
                source: socket,
                target: volume.target.clone(),
                read_only: volume.read_only,
            });
            continue;
        }
        let source = volume.host_path(&store.volumes_dir());
        let made = !source.exists();
        if made {
            if volume.source.starts_with('/') {
                bail!(
                    "volume {volume}: {} does not exist on the host",
                    source.display()
                );
            }
            std::fs::create_dir_all(&source)
                .with_context(|| format!("creating volume {}", source.display()))?;
        }
        let empty = || std::fs::read_dir(&source).is_ok_and(|mut d| d.next().is_none());
        // Another machine that runs with the volume could change it while the service
        // fills it, its links leading out of the volume among those changes: it is
        // filled only when none does.
        let running_user = if !made && seeds(volume, made, first_start, empty) {
            running_user_of(sd, store, &volume.source, name).await?
        } else {
            None
        };
        if let Some(other) = &running_user {
            note(
                report,
                format!(
                    "note: volume {} is in use by {other}, which runs: it is not filled from the image",
                    volume.source
                ),
            );
        } else if seeds(volume, made, first_start, empty) {
            // What the image has there, owner included: from its layers, which earlier
            // runs have not touched, merged as the image shows them, or from the tree
            // itself where there are no layers.
            let target = volume.target.trim_start_matches('/');
            let layers = crate::backend::layer_dirs(store, &record.layers, record.backend);
            let seeded = if !layers.is_empty() {
                crate::volume::seed_from_layers(&layers, target, &source).map(|_| ())
            } else if let Some(path) = (record.backend == BackendChoice::Flat)
                .then(|| crate::backend::root_path(store, name, record.backend, target))
                .flatten()
            {
                crate::volume::seed(&path, &source)
            } else {
                Ok(())
            };
            // A seed cut short is taken back, so that the next start seeds it whole
            // instead of finding a volume that is there.
            if let Err(e) = seeded {
                if made {
                    let _ = crate::store::remove_tree(&source);
                } else {
                    crate::volume::empty(&source);
                }
                return Err(e).with_context(|| format!("filling volume {volume}"));
            }
        }
        if made {
            crate::api::events::emit(
                "volume",
                "create",
                &volume.source,
                &[("path", &source.to_string_lossy())],
            );
        }
        binds.push(Bind {
            source,
            target: volume.target.clone(),
            read_only: volume.read_only,
        });
    }
    // /run is a tmpfs of every machine already, and one over it would hide what
    // systemd-nspawn keeps there; --tmpfs /var/run, common for a read-only
    // nginx, lands on it through the image's symlink.
    let mut tuning = record.tuning.clone();
    tuning.tmpfs.retain(|mount| {
        let target = mount.split_once(':').map_or(mount.as_str(), |(p, _)| p);
        if crate::backend::lands_on_run(store, name, record.backend, target) {
            note(
                report,
                format!("note: {target} is /run in {name}, a tmpfs of every machine already; --tmpfs there is left out"),
            );
            return false;
        }
        true
    });
    // CDI devices, looked up in the host's specs at every start: a driver update
    // rewrites them.
    let cdi = crate::cdi::plan(&record.tuning)?;
    let mut extra_env = record.env.clone();
    if let Some(plan) = &cdi {
        if record.backend == BackendChoice::Mstack {
            bail!("{name} runs under managed user namespaces (mstack), where CDI devices cannot be applied yet; pull it again with --backend overlay");
        }
        if pass == Pass::Ahead {
            for text in &plan.notes {
                note(report, format!("note: {name}: {text}"));
            }
        }
        let nodes = cdi_nodes_dir(store, name);
        // Under the unit's device policy, a node to make again (other numbers since the
        // drop-in was written) needs its rule first.
        if pass == Pass::Hook {
            let rules = crate::cdi::rules_to_make(plan, &nodes);
            if !rules.is_empty() {
                sd.allow_devices(&format!("systemd-nspawn@{name}.service"), &rules)
                    .await?;
            }
        }
        tuning.devices.extend(crate::cdi::devices(plan, &nodes)?);
        binds.extend(plan.mounts.iter().cloned());
        extra_env.extend(plan.env.iter().cloned());
    }
    // A booted machine's systemd sets the hostname from /etc/hostname, over the one
    // systemd-nspawn set: --hostname goes in as that file. A machine that takes the
    // Docker API gets it from a directory named after its container ID, as docker binds
    // it: the tools reading the socket find their own container by that line of their
    // mountinfo.
    let hostname_file = if record.takes_api_socket() {
        let id = crate::dockerapi::util::container_id(name, record.created);
        let ids = store.machine_files_dir(name).join("docker");
        let _ = crate::store::remove_tree(&ids);
        let dir = ids.join("containers").join(id);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join("hostname");
        let hostname = record.tuning.hostname.as_deref().unwrap_or(name);
        std::fs::write(&path, format!("{hostname}\n"))
            .with_context(|| format!("writing {}", path.display()))?;
        Some(path)
    } else {
        match (&record.tuning.hostname, record.mode) {
            (Some(hostname), Mode::Boot) => {
                let dir = store.machine_files_dir(name);
                std::fs::create_dir_all(&dir)
                    .with_context(|| format!("creating {}", dir.display()))?;
                let path = dir.join("hostname");
                std::fs::write(&path, format!("{hostname}\n"))
                    .with_context(|| format!("writing {}", path.display()))?;
                Some(path)
            }
            _ => None,
        }
    };
    let managed_userns = record.backend == BackendChoice::Mstack;
    binds.extend(crate::api::secrets::materialize(store, &record)?);
    if record.mode == Mode::App {
        binds.extend(crate::getent::shim(store, name, &record)?);
        crate::backend::ensure_usr(store, name, record.backend)?;
    }
    if record.tuning.read_only && record.backend == BackendChoice::Mstack {
        // Its mount points would be made in the writable layer over the image's own
        // entries, owned by an id the machine does not map.
        bail!("{name} is an mstack machine, which cannot run read-only yet; start it with --read-only=false");
    }
    if record.tuning.read_only {
        // systemd-nspawn makes mount points as it goes, which a read-only root refuses.
        let mut points: Vec<(String, bool)> = tuning
            .tmpfs
            .iter()
            .map(|t| {
                (
                    t.split_once(':').map_or(t.as_str(), |(p, _)| p).to_string(),
                    true,
                )
            })
            .collect();
        points.extend(binds.iter().map(|b| (b.target.clone(), b.source.is_dir())));
        points.extend(tuning.devices.iter().map(|d| (d.container.clone(), false)));
        if hostname_file.is_some() {
            points.push(("/etc/hostname".to_string(), false));
        }
        // The generated hosts and resolv.conf go over files an image may lack.
        if files.is_some() {
            points.push(("/etc/hosts".to_string(), false));
            if record.mode == Mode::App {
                points.push(("/etc/resolv.conf".to_string(), false));
            }
        }
        crate::backend::ensure_mount_points(
            &crate::backend::mount_point_root(store, name, record.backend)?,
            &points,
        )?;
    }
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
    let route =
        settings::namespace_route(sd, name, record.mode, bridge::namespaced(&record)).await?;
    settings::write(
        &MachineSettings {
            name,
            managed_userns,
            mode: record.mode,
            pid1: record.program_is_pid1(),
            run: &record.run,
            command: &record.effective_command(),
            extra_env: &extra_env,
            binds: &binds,
            volume_units: volume_units.as_deref(),
            network: record.network,
            no_network: record.no_network,
            hostname_file: hostname_file.as_deref(),
            bridge: files.as_ref().map(|(files, bridge, extras)| BridgeMount {
                bridge: bridge.as_str(),
                files: files.as_path(),
                extras,
            }),
            tuning: &tuning,
        },
        &route,
    )?;
    let app_argv = settings::wrapped_argv(sd, name, record.mode, &route, &record.tuning).await?;
    let changed = settings::write_hooks(
        name,
        config,
        &route,
        app_argv.as_deref(),
        &settings::HookSpec {
            restart: record.restart,
            limits: &record.limits,
            remove_on_exit: record.remove_on_exit,
            tuning: &tuning,
            api_socket: record.takes_api_socket(),
        },
    )?;
    // A reload that failed after an earlier write left the unit on its old drop-in,
    // which systemd says; before the start, not from inside it.
    let stale = !changed
        && pass == Pass::Ahead
        && sd
            .needs_reload(&format!("systemd-nspawn@{name}.service"))
            .await
            .unwrap_or(false);
    if changed || stale {
        sd.reload().await?;
    }
    if record.backend == BackendChoice::Mstack {
        // Socket-activated services that distributions ship disabled.
        for unit in MANAGED_NS_SOCKETS {
            sd.start_unit(unit).await?;
        }
    }
    if record.network == Network::Veth {
        hostnet::ensure_networkd(sd, report).await?;
    }
    if pass == Pass::Hook {
        if let Some(plan) = &cdi {
            let root = crate::backend::mount_point_root(store, name, record.backend)?;
            let env: Vec<String> = record.run.env.iter().chain(&extra_env).cloned().collect();
            crate::cdi::run_hooks(
                plan,
                &crate::cdi::HookMachine {
                    name,
                    root: &root,
                    bundle: &store.machine_files_dir(name).join("cdi").join("bundle"),
                    env: &env,
                    command: &record.effective_command(),
                    nodes: &cdi_nodes_dir(store, name),
                },
            )
            .await?;
        }
        // Last: nothing that could fail is left to leave an interface in the namespace.
        move_interfaces(name, hook_interfaces(&record))?;
    }
    Ok(record.network)
}

/// Everything but the name is remembered.
#[derive(Debug, Clone, PartialEq)]
pub struct StartRequest {
    pub name: String,
    /// Wait for a booted machine's init to be up before returning.
    pub wait: bool,
    /// bridge, veth, host, none, or networks' names, the first one primary; empty keeps
    /// the remembered ones.
    pub network: Vec<String>,
    /// NAME or NETWORK=NAME; "none" forgets them.
    pub aliases: Vec<String>,
    /// Fixed addresses: IP on the primary network or NETWORK=IP (--ip).
    pub addresses: Vec<String>,
    /// HOST:CONTAINER[/udp]; "none" forgets them all.
    pub publish: Vec<String>,
    /// An empty string runs the arguments alone.
    pub entrypoint: Option<String>,
    /// VAR=value or VAR; "none" forgets them.
    pub env: Vec<String>,
    /// SOURCE:TARGET[:ro]; "none" forgets them.
    pub volume: Vec<String>,
    /// On top of the image's; "none" forgets them.
    pub label: Vec<String>,
    /// None keeps the remembered one.
    pub restart: Option<Restart>,
    /// Bytes, 0 for none; None keeps the remembered limit.
    pub memory: Option<u64>,
    /// Memory and swap together in bytes, -1 for no bound on swap, 0 for as much swap
    /// again as memory; None keeps the remembered one.
    pub memory_swap: Option<i64>,
    /// CPUs (0.5), 0 for none; None keeps the remembered limit.
    pub cpus: Option<f64>,
    /// Processes, 0 for none; None keeps the remembered limit.
    pub pids_limit: Option<u64>,
    pub image_command: bool,
    /// Replaces an app image's cmd.
    pub command: Vec<String>,
    /// run --rm.
    pub remove: bool,
    /// The --health-* flags.
    pub health: crate::health::Overrides,
    /// hostname, user, capabilities and the rest.
    pub tuning: crate::tuning::Overrides,
}

impl StartRequest {
    /// A start that changes nothing the machine has.
    pub fn keep(name: &str) -> StartRequest {
        StartRequest {
            name: name.to_string(),
            wait: true,
            network: Vec::new(),
            aliases: Vec::new(),
            addresses: Vec::new(),
            publish: Vec::new(),
            entrypoint: None,
            env: Vec::new(),
            volume: Vec::new(),
            label: Vec::new(),
            restart: None,
            memory: None,
            memory_swap: None,
            cpus: None,
            pids_limit: None,
            image_command: false,
            command: Vec::new(),
            remove: false,
            health: Default::default(),
            tuning: Default::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartOutcome {
    /// Registered (and, when asked, its init is up).
    Started,
    /// A short program returned before the machine registered.
    Ended,
    /// It ended right away and its restart policy brings it back.
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
    let stub = store
        .load_image(&args.name)?
        .is_some_and(|r| r.under_stub_init());
    wait_for_previous(sd, &args.name, &unit, stub).await?;
    let lock = store.lock().await?;
    // Again under the lock: another start may be preparing it, or it may be up by now.
    if store.is_starting(&args.name) {
        bail!("machine {} is starting already", args.name);
    }
    if sd.machine_exists(&args.name).await? {
        bail!("machine {} is already running", args.name);
    }
    // systemd may have begun it meanwhile (machinectl start, the Wants= of a machine
    // that depends on it): its own hooks prepare it, and this pass would tear that down.
    if sd.unit_status(&unit).await?.busy() || sd.start_queued(&unit).await? {
        bail!("machine {} is starting already", args.name);
    }
    let _starting = store.mark_starting(&args.name)?;
    let mut record = store.load_image(&args.name)?;
    let before = record.clone();
    let previous_policy = record.as_ref().map(|r| r.restart).unwrap_or_default();
    match record.as_mut() {
        Some(r) => {
            apply_flags(r, args)?;
            apply_addresses(store, config, r, &args.addresses)?;
            crate::api::dependencies::check(
                store,
                &args.name,
                &r.tuning.depends_on,
                r.network_container.as_deref(),
            )?;
        }
        None if !args.command.is_empty()
            || !args.network.is_empty()
            || !args.aliases.is_empty()
            || !args.addresses.is_empty()
            || !args.publish.is_empty()
            || args.entrypoint.is_some()
            || !args.env.is_empty()
            || !args.volume.is_empty()
            || !args.label.is_empty()
            || args.restart.is_some()
            || args.memory.is_some()
            || args.memory_swap.is_some()
            || args.cpus.is_some()
            || args.pids_limit.is_some()
            || !args.health.is_empty()
            || !args.tuning.is_empty()
            || args.remove =>
        {
            bail!(
                "{} is not an image managed by nspawn; a command, network, ports, variables, volumes, labels, a restart policy, limits or a healthcheck need one",
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
    let deps = record
        .as_ref()
        .map(|r| r.tuning.depends_on.clone())
        .unwrap_or_default();
    let network = match prepare(sd, store, config, &args.name, record, Pass::Ahead, report).await {
        Ok(network) => network,
        Err(e) => {
            // Nothing runs, under the lock and the start mark: what the pass made for a
            // run that never came goes (its namespace and veths, decrypted secrets), by
            // the record it made them from.
            let prepared = store.load_image(&args.name).ok().flatten();
            let _ = release_machine(&args.name, prepared.as_ref().or(before.as_ref()));
            // prepare writes the record with the start's flags before all of its checks:
            // a start refused keeps what the machine had, its restart policy among them.
            if let Some(before) = &before {
                let _ = store.record_image(before);
            }
            return Err(e);
        }
    };
    // Read with the records under the lock, prepare's included.
    let patient = crate::api::dependencies::waits(&store.list_images()?, &args.name);
    // The unit's own hooks take the lock; it must be free while the unit starts.
    drop(lock);
    // always and unless-stopped enable the unit, as machinectl enable does. It is only
    // disabled when the policy changes away from those, so that a unit an administrator
    // enabled stays enabled.
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
    // firewalld only knows a veth once the machine is registered, and published ports
    // need it registered too.
    let firewalld = network == Network::Veth && hostnet::firewalld_running(sd).await;
    // systemd starts the machines this one depends on, the ones down now said afterwards;
    // the hook starts one that completes when no run of it ended well, which a new run
    // of its program tells.
    let mut down = Vec::new();
    for dep in &deps {
        let unit = format!("systemd-nspawn@{}.service", dep.name);
        if !sd.unit_status(&unit).await?.busy() {
            let ran = if dep.condition == crate::tuning::Condition::Completed {
                Some(sd.exec_main_start_usec(&unit).await.unwrap_or(0))
            } else {
                None
            };
            down.push((dep, ran));
        }
    }
    let restarting = match sd.start_machine(&args.name, patient).await {
        Ok(restarting) => restarting,
        Err(e) => {
            // A start job cancelled before its unit ran (a stop while it waited in the
            // queue) leaves what the preparation made, which no release hook takes.
            {
                let _lock = store.lock().await?;
                if !sd.machine_exists(&args.name).await?
                    && !sd.unit_status(&unit).await?.busy()
                    && !sd.start_queued(&unit).await?
                {
                    hostnet::release_machine(sd, &args.name).await;
                    release_machine(&args.name, store.load_image(&args.name)?.as_ref())?;
                }
            }
            // The hook's reason, which the job's result does not carry.
            match dependency_failure(store, sd, &args.name).await {
                Some(why) => bail!("{} did not start: {why}", args.name),
                None => return Err(e),
            }
        }
    };
    for (dep, ran) in down {
        let unit = format!("systemd-nspawn@{}.service", dep.name);
        if sd.unit_status(&unit).await?.active != "failed" {
            match ran {
                None => note(
                    report,
                    format!("{} started first: {} depends on it", dep.name, args.name),
                ),
                Some(before) if sd.exec_main_start_usec(&unit).await.unwrap_or(0) != before => {
                    note(
                        report,
                        format!(
                            "{} ran first: {} waits for it to complete",
                            dep.name, args.name
                        ),
                    )
                }
                Some(_) => {}
            }
        } else if dep.optional {
            note(
                report,
                format!("warning: {} failed to start; it is optional, so {} started anyway (journalctl -u {unit} says why)", dep.name, args.name),
            );
        }
    }
    if restarting {
        if let Some(why) = dependency_failure(store, sd, &args.name).await {
            note(report, format!("warning: {why}"));
        }
        return Ok(StartOutcome::Restarting);
    }
    if args.wait || firewalld || network == Network::Bridge {
        let unit = format!("systemd-nspawn@{}.service", args.name);
        let deadline = Instant::now() + Duration::from_secs(30);
        while !sd.machine_exists(&args.name).await? {
            let state = sd.unit_status(&unit).await?;
            if state.restarting() {
                return Ok(StartOutcome::Restarting);
            }
            match state.active.as_str() {
                "active" | "activating" | "reloading" | "deactivating" => {}
                // The program ran and failed: an ending, with its code for whoever
                // attached; a unit that failed before it ran is the start's error.
                "failed" if sd.exec_main_exit(&unit).await?.is_some() => {
                    return Ok(StartOutcome::Ended)
                }
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

/// What a start's flags make of the machine's record: each one given replaces what the
/// machine had. Shared with the round trip of `generate`.
/// --ip: an address on the primary network, or NETWORK=IP on one the machine joins,
/// checked as `bridge::check_requested` says, and kept as the machine's address there.
pub fn apply_addresses(
    store: &Store,
    config: &Config,
    r: &mut ImageRecord,
    addresses: &[String],
) -> Result<()> {
    for text in addresses {
        if !bridge::bridge_kind(r) {
            bail!(
                "a fixed address is one on a bridge network; {} joins none",
                r.name
            );
        }
        let networks: Vec<String> = bridge::networks_of(r)
            .into_iter()
            .map(str::to_string)
            .collect();
        let (network, ip) = match text.split_once('=') {
            Some((network, ip)) => (network.to_string(), ip),
            None => (networks[0].clone(), text.as_str()),
        };
        if !networks.contains(&network) {
            bail!("--ip {text}: {} does not join network {network}", r.name);
        }
        let addr: std::net::Ipv4Addr = ip
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid IPv4 address: {ip}"))?;
        let net = crate::api::network::find(store, config, &network)?;
        bridge::check_requested(store, &net, &r.name, addr)?;
        bridge::set_address(r, &network, addr);
        r.fixed_addresses.insert(network);
    }
    Ok(())
}

pub fn apply_flags(r: &mut ImageRecord, args: &StartRequest) -> Result<()> {
    if !args.network.is_empty() {
        crate::api::network::apply(r, &crate::api::network::choices(&args.network)?);
    }
    if !args.aliases.is_empty() {
        if !bridge::bridge_kind(r) {
            bail!(
                "aliases are names on a bridge network; {} joins none",
                args.name
            );
        }
        r.aliases = crate::api::network::parse_aliases(&args.aliases, &bridge::networks_of(r))?;
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
        volume::reject_control_characters_but_tab(entrypoint)?;
        r.entrypoint = Some(if entrypoint.is_empty() {
            Vec::new()
        } else {
            vec![entrypoint.clone()]
        });
        // Only the arguments given with it follow it: none, the image's command dropped.
        r.cmd = Some(Vec::new());
    }
    if !args.command.is_empty() {
        for arg in &args.command {
            volume::reject_control_characters_but_tab(arg)?;
        }
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
    apply_limits(
        &mut r.limits,
        args.memory,
        args.memory_swap,
        args.cpus,
        args.pids_limit,
    )?;
    r.limits.check(r.mode)?;
    if !args.health.is_empty() {
        let hc = args.health.apply(r.effective_healthcheck())?;
        r.healthcheck = Some(hc);
    }
    args.tuning.apply(&mut r.tuning)?;
    duplicate_mount_points(r)?;
    if r.mode == Mode::Boot
        && (r.tuning.user.is_some() || r.tuning.working_dir.is_some() || r.tuning.init.is_some())
    {
        bail!(
            "{} boots an init system; --user, --workdir and --init only apply to the program of an app image",
            args.name
        );
    }
    if r.mode == Mode::App && r.effective_command().is_empty() {
        bail!("{}", no_command(&args.name));
    }
    // Set on every start, so that a start without --rm keeps the machine.
    if args.remove && r.restart != Restart::No {
        bail!("--rm and a restart policy exclude each other: a machine removed when it ends cannot be restarted (--restart no)");
    }
    r.remove_on_exit = args.remove;
    Ok(())
}

/// The refusal of an app with nothing to run, which systemd-nspawn would give a shell.
pub(crate) fn no_command(name: &str) -> String {
    format!("no command specified: {name} has neither an entrypoint nor a command to run; give one after the image")
}

/// Why the machine's start failed because of one it depends on, if that is why.
async fn dependency_failure(store: &Store, sd: &Systemd, name: &str) -> Option<String> {
    let record = store.load_image(name).ok()??;
    crate::api::dependencies::failure(sd, store, &record).await
}

/// 0 removes a limit; None keeps it. Removing the memory limit takes a total of memory
/// and swap with it, which means nothing without one.
fn apply_limits(
    limits: &mut Limits,
    memory: Option<u64>,
    memory_swap: Option<i64>,
    cpus: Option<f64>,
    pids: Option<u64>,
) -> Result<()> {
    if let Some(memory) = memory {
        limits.memory = memory;
        if memory == 0 && memory_swap.is_none() && limits.memory_swap > 0 {
            limits.memory_swap = 0;
        }
    }
    if let Some(memory_swap) = memory_swap {
        limits.memory_swap = memory_swap;
    }
    if let Some(cpus) = cpus {
        limits.milli_cpus = crate::policy::milli_cpus_from(cpus)?;
    }
    if let Some(pids) = pids {
        limits.pids = pids;
    }
    Ok(())
}

/// Registration comes early in a boot: waits, within reason, until the machine's systemd
/// listens, so that a command run right after `start` finds it.
async fn wait_for_init(sd: &Systemd, name: &str) -> Result<StartOutcome> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let Ok(leader) = sd.machine_leader(name).await else {
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

/// The machine's cgroup frozen, every process in it with it, or thawed.
pub async fn pause(ctx: &Context, name: &str, on: bool) -> Result<()> {
    validate_entry_name(name)?;
    let sd = ctx.sd().await?;
    refuse_foreign(sd, name).await?;
    if !sd.machine_exists(name).await? {
        bail!("machine {name} is not running");
    }
    let unit = format!("systemd-nspawn@{name}.service");
    if on {
        sd.freeze_unit(&unit).await?;
    } else {
        sd.thaw_unit(&unit).await?;
    }
    let image = ctx
        .store
        .load_image(name)?
        .map(|r| r.reference)
        .unwrap_or_default();
    crate::api::events::emit_machine(
        &ctx.store,
        if on { "pause" } else { "unpause" },
        name,
        &[("image", &image)],
    );
    Ok(())
}

/// A process of a running machine, as top shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Process {
    pub pid: u32,
    /// The uid as the machine sees it, "root" for 0.
    pub user: String,
    /// CPU time, HH:MM:SS.
    pub time: String,
    pub command: String,
}

/// The processes in the machine's PID namespace, from its cgroup.
pub async fn processes(ctx: &Context, name: &str) -> Result<Vec<Process>> {
    validate_entry_name(name)?;
    let sd = ctx.sd().await?;
    refuse_foreign(sd, name).await?;
    if !sd.machine_exists(name).await? {
        bail!("machine {name} is not running");
    }
    let leader = sd.machine_leader(name).await?;
    let shift = sd.machine_uid_shift(name).await.unwrap_or(0);
    let cgroup = sd
        .control_group(&format!("systemd-nspawn@{name}.service"))
        .await?;
    let unit_cgroup = PathBuf::from(format!("/sys/fs/cgroup{cgroup}"));
    let mut pids = Vec::new();
    // systemd-nspawn keeps itself in supervisor/ and the machine in payload/, which it
    // makes a moment after the machine registers: right after a restart the old tree may
    // still be going while the new one is not there yet, and for a moment more payload/
    // holds nspawn's own child, until it has become the machine's init. Without that
    // split (another supervisor), what is in the machine's PID namespace is its, and
    // never what is in the host's.
    let payload = unit_cgroup.join("payload");
    for _ in 0..20 {
        if payload.is_dir() {
            cgroup_pids(&payload, &mut pids);
            pids.retain(|pid| comm(*pid).as_deref() != Some("systemd-nspawn"));
            if !pids.is_empty() {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if !payload.is_dir() {
        let host = std::fs::read_link("/proc/self/ns/pid").ok();
        let inside = std::fs::read_link(format!("/proc/{leader}/ns/pid")).ok();
        cgroup_pids(&unit_cgroup, &mut pids);
        pids.retain(|pid| {
            let ns = std::fs::read_link(format!("/proc/{pid}/ns/pid")).ok();
            ns.is_some() && ns != host && ns == inside
        });
    }
    if pids.is_empty() {
        bail!("reading the processes of {name}: none in its cgroup {cgroup}");
    }
    pids.sort_unstable();
    Ok(pids
        .into_iter()
        .filter_map(|pid| read_process(pid, shift))
        .collect())
}

/// The command name of a process, as /proc has it.
fn comm(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|c| c.trim().to_string())
}

/// The PIDs of a cgroup and everything below it, where a booted machine's systemd
/// makes a tree of its own.
fn cgroup_pids(dir: &Path, out: &mut Vec<u32>) {
    if let Ok(procs) = std::fs::read_to_string(dir.join("cgroup.procs")) {
        out.extend(procs.lines().filter_map(|l| l.trim().parse::<u32>().ok()));
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            cgroup_pids(&entry.path(), out);
        }
    }
}

fn read_process(pid: u32, shift: u32) -> Option<Process> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let uid: u32 = status
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    let uid = uid.checked_sub(shift).unwrap_or(uid);
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // rest starts at field 3 (state): utime is field 14, stime 15.
    let ticks: u64 = fields.get(11)?.parse::<u64>().ok()? + fields.get(12)?.parse::<u64>().ok()?;
    let seconds = ticks
        / nix::unistd::sysconf(nix::unistd::SysconfVar::CLK_TCK)
            .ok()
            .flatten()
            .unwrap_or(100) as u64;
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let command = if cmdline.is_empty() {
        let comm = status
            .lines()
            .find_map(|l| l.strip_prefix("Name:"))
            .unwrap_or("")
            .trim();
        format!("[{comm}]")
    } else {
        cmdline
            .split(|b| *b == 0)
            .filter(|a| !a.is_empty())
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    };
    Some(Process {
        pid,
        user: if uid == 0 {
            "root".to_string()
        } else {
            uid.to_string()
        },
        time: format!(
            "{:02}:{:02}:{:02}",
            seconds / 3600,
            (seconds % 3600) / 60,
            seconds % 60
        ),
        command,
    })
}

/// The host interfaces, network namespace, published ports and decrypted secrets of a
/// machine that ended. Safe to repeat.
pub fn release_machine(name: &str, record: Option<&ImageRecord>) -> Result<()> {
    if let Some(record) = record {
        // Before the namespace goes: the kernel would give them back on its own, a
        // moment later and renamed when their names are taken by then.
        return_interfaces(name, hook_interfaces(record));
    }
    match record {
        Some(record) if record.network == Network::Bridge => {
            bridge::delete_netns(name);
            bridge::withdraw_ports(record)?;
        }
        Some(_) => {}
        // Nothing says which network it had; the namespace has the machine's name.
        None => bridge::delete_netns(name),
    }
    // A start that failed after decrypting them never ran the release hook.
    crate::api::secrets::clear(name);
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopRequest {
    pub name: String,
    pub force: bool,
    pub wait: bool,
    /// Seconds between an app's stop signal and SIGKILL; None for the machine's own
    /// --stop-timeout, 10 without one.
    pub timeout: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    Stopped,
    /// It had ended already; what it left behind is gone now.
    WasNotRunning,
}

/// Where `stop` queues the unit's stop job, which keeps a restart policy from bringing the
/// machine back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Latch {
    /// No policy, nothing to keep from restarting.
    NotNeeded,
    /// Before the first signal: a booted init shuts down the same way either way.
    BeforeSignal,
    /// After the signal: SIGKILL with `--force` (machined refuses to kill a machine it
    /// already closes), or an app's stop signal, after its timeout when waiting (a stop
    /// job would have the stub init add SIGTERM and SIGHUP meanwhile). The stop job also
    /// cancels a restart already scheduled.
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

/// For a machine machined does not list: nspawn's leaves files and a network behind, and
/// anybody's unit may be restarting.
fn stoppable_when_gone(recorded: bool, unit: &UnitState) -> bool {
    recorded || unit.busy() || unit.shutting_down()
}

/// After SIGKILL the stop job goes in for the latch, and whenever the kill failed, since
/// the job is then what makes the machine go.
fn stop_job_after_kill(latch: Latch, killed: bool) -> bool {
    latch == Latch::AfterSignal || !killed
}

/// stop of a machine machined does not list: ended, between two runs of its restart
/// policy, or waiting to start.
async fn stop_gone(
    sd: &Systemd,
    store: &Store,
    args: &StopRequest,
    record: Option<&ImageRecord>,
    unit: &str,
) -> Result<StopOutcome> {
    // A start is preparing it: its network must not be released under it. One that
    // waits for the machines it depends on, in its unit or in systemd's queue, is
    // systemd's to stop.
    let queued = sd.start_queued(unit).await?;
    // Records read without the lock: one that goes meanwhile only means no wait.
    let records = store.list_images().unwrap_or_default();
    let waiting = crate::api::dependencies::waits(&records, &args.name)
        && (sd.unit_status(unit).await?.busy() || queued);
    if store.is_starting(&args.name) && !waiting {
        bail!("machine {} is starting; stop it once it runs", args.name);
    }
    // Ended, or between two runs of a restart policy (nspawn's or an administrator's):
    // the stop job ends a pending restart and completes once the last release hook
    // is done, so nothing is released beside it.
    let state = sd.unit_status(unit).await?;
    if !stoppable_when_gone(record.is_some(), &state) {
        bail!("machine {} is not running", args.name);
    }
    sd.stop_unit_job(unit).await?.wait().await?;
    {
        let _lock = store.lock().await?;
        if still_down(sd, store, &args.name, unit).await? {
            hostnet::release_machine(sd, &args.name).await;
            release_machine(&args.name, record)?;
        }
    }
    sd.reset_failed(unit).await?;
    // A start held in systemd's queue was going to run it: it stopped.
    Ok(if state.busy() || queued {
        StopOutcome::Stopped
    } else {
        StopOutcome::WasNotRunning
    })
}

pub async fn stop(ctx: &Context, args: &StopRequest, report: Report<'_>) -> Result<StopOutcome> {
    validate_entry_name(&args.name)?;
    let sd = ctx.sd().await?;
    refuse_foreign(sd, &args.name).await?;
    let store = &ctx.store;
    let record = store.load_image(&args.name)?;
    let unit = format!("systemd-nspawn@{}.service", args.name);
    let policy = record.as_ref().map(|r| r.restart).unwrap_or_default();
    // A start nspawn is preparing is refused before anything about the machine changes.
    if !sd.machine_exists(&args.name).await? && store.is_starting(&args.name) {
        let records = store.list_images().unwrap_or_default();
        let waiting = crate::api::dependencies::waits(&records, &args.name)
            && (sd.unit_status(&unit).await?.busy() || sd.start_queued(&unit).await?);
        if !waiting {
            bail!("machine {} is starting; stop it once it runs", args.name);
        }
    }
    // A paused machine cannot act on a signal.
    let _ = sd.thaw_unit(&unit).await;
    // unless-stopped: stopped by hand means not at boot either, until the next start.
    if policy == Restart::UnlessStopped && sd.disable_unit(&unit).await? {
        sd.reload().await?;
    }
    if !sd.machine_exists(&args.name).await? {
        return stop_gone(sd, store, args, record.as_ref(), &unit).await;
    }
    let mode = record.as_ref().map(|r| r.mode);
    let latch = latch(policy, args.force, mode);
    let mut job = None;
    if latch == Latch::BeforeSignal {
        job = Some(sd.stop_unit_job(&unit).await?);
    }
    if args.force {
        // systemd 255 answers "Invalid argument" when some process of the unit cannot be
        // signalled although the machine got its SIGKILL. To workaround this, the stop job
        // goes in either way, and what counts is that the machine goes off.
        if record.is_some() {
            mark_signal(sd, store, &args.name, libc::SIGKILL).await?;
        }
        let killed = sd.kill_machine(&args.name, "all", libc::SIGKILL).await;
        if stop_job_after_kill(latch, killed.is_ok()) {
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
            // The image's stop signal, then SIGKILL after the timeout.
            Some(Mode::App) => {
                // One nspawn cannot read is SIGTERM, as the settings make it.
                let number = stop_signals(record.as_ref())?
                    .first()
                    .copied()
                    .unwrap_or(libc::SIGTERM);
                let signal =
                    signal_name(&number.to_string()).unwrap_or_else(|_| number.to_string());
                let timeout = args
                    .timeout
                    .or_else(|| record.as_ref().and_then(|r| r.tuning.stop_timeout))
                    .unwrap_or(10);
                let pid1 = record.as_ref().is_some_and(ImageRecord::program_is_pid1);
                let (leader, payload) = match wait_for_payload(sd, &args.name, pid1).await {
                    Ok(found) => found,
                    // It ended since it was looked at: nothing is left to signal.
                    Err(_) if !sd.machine_exists(&args.name).await? => {
                        return stop_gone(sd, store, args, record.as_ref(), &unit).await;
                    }
                    Err(e) => return Err(e),
                };
                mark_signal(sd, store, &args.name, number).await?;
                match payload {
                    Some(payload) => {
                        if let Err(e) =
                            signal_payload(sd, &args.name, leader, payload, pid1, number).await
                        {
                            note(report, format!("note: could not send {signal} to PID {payload} of {}: {e:#}", args.name));
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
                    let gone = wait_gone(sd, &args.name, Duration::from_secs(timeout)).await?;
                    // SIGKILL first, as --force sends it: a stop job queued before would
                    // have the stub init signal everything meanwhile. The job then
                    // cancels the restart that the policy schedules after the kill.
                    if !gone {
                        note(
                            report,
                            format!(
                                "{} ignored {signal} for {timeout} seconds; killing it",
                                args.name
                            ),
                        );
                        mark_signal(sd, store, &args.name, libc::SIGKILL).await?;
                        // It may have ended meanwhile; the stop job sees it go either way.
                        if let Err(e) = sd.kill_machine(&args.name, "all", libc::SIGKILL).await {
                            note(
                                report,
                                format!("note: {e:#}; waiting for {} to go", args.name),
                            );
                        }
                    }
                    if latch == Latch::AfterSignal {
                        job = Some(sd.stop_unit_job(&unit).await?);
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
            if args.force {
                bail!(
                    "machine {} is still running 60 seconds after SIGKILL; see journalctl -u {unit}",
                    args.name
                );
            }
            bail!(
                "machine {} is still running after 60 seconds; use --force",
                args.name
            );
        }
        // The unit's teardown finishes, so that the image can be removed right away, and
        // the failure a killed program leaves on the unit is cleared.
        match job {
            Some(job) => job.wait().await?,
            None => sd.stop_unit(&unit).await?,
        }
        sd.reset_failed(&unit).await?;
        let _lock = store.lock().await?;
        // The unit's own release ran; this one only finishes what it left, unless a new
        // run began meanwhile, whose network and secrets are its own.
        if still_down(sd, store, &args.name, &unit).await? {
            hostnet::release_machine(sd, &args.name).await;
            release_machine(&args.name, record.as_ref())?;
        }
    }
    Ok(StopOutcome::Stopped)
}

/// Whether a machine stopped is still down: not registered, no start preparing it, and
/// its unit neither running nor about to.
async fn still_down(sd: &Systemd, store: &Store, name: &str, unit: &str) -> Result<bool> {
    Ok(!sd.machine_exists(name).await?
        && !store.is_starting(name)
        && !sd.unit_status(unit).await?.busy()
        && !sd.start_queued(unit).await?)
}

/// An app's program, which signals go to, never the whole cgroup: the stub init reboots
/// on SIGINT and systemd-nspawn dies of SIGQUIT. The program is the stub init's child,
/// or the leader itself when it runs as PID 1. Right after a start the process may still
/// be systemd-nspawn's own, which a signal it does not handle ends before the program
/// runs, and the machine with it: waits a few seconds at most for the program to be
/// executed, then takes the process as it is. Returns the leader too.
async fn wait_for_payload(sd: &Systemd, name: &str, pid1: bool) -> Result<(u32, Option<i32>)> {
    let leader = sd.machine_leader(name).await?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        // A program that is PID 1 and gone has nothing left to signal: its PID may be
        // another process's by now.
        if pid1 && comm(leader).is_none() {
            return Ok((leader, None));
        }
        let payload = program_pid(leader, pid1);
        if running_program(payload, |pid| comm(pid as u32)).is_some() || Instant::now() > deadline {
            return Ok((leader, payload));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The program once it has been executed; `comm` names a PID.
fn running_program(payload: Option<i32>, comm: impl Fn(i32) -> Option<String>) -> Option<i32> {
    payload.filter(|&pid| comm(pid).is_some_and(|name| !in_nspawn_setup(&name)))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KillRequest {
    pub name: String,
    pub signal: String,
}

/// kill. SIGKILL is `stop --force`. Other signals go to an app's program or a
/// booted machine's init, and a machine they end is restarted by its policy, unless it
/// was its stop signal (through the mark the release hook reads).
pub async fn kill(ctx: &Context, args: &KillRequest, report: Report<'_>) -> Result<()> {
    validate_entry_name(&args.name)?;
    let signal = signal_number(&args.signal)?;
    let sd = ctx.sd().await?;
    refuse_foreign(sd, &args.name).await?;
    let unit = format!("systemd-nspawn@{}.service", args.name);
    let registered = sd.machine_exists(&args.name).await?;
    // Between two runs of a restart policy, or still starting (waiting for what it
    // depends on), only SIGKILL means something: it ends the restarts or the start.
    if !registered && !(signal == libc::SIGKILL && kill_ends(&sd.unit_status(&unit).await?)) {
        bail!("machine {} is not running", args.name);
    }
    if signal == libc::SIGKILL {
        let request = StopRequest {
            name: args.name.clone(),
            force: true,
            wait: true,
            timeout: Some(0),
        };
        stop(ctx, &request, report).await?;
        // Between two runs no process got it, and nothing is said.
        if registered {
            let image = ctx
                .store
                .load_image(&args.name)?
                .map(|r| r.reference)
                .unwrap_or_default();
            crate::api::events::emit_machine(
                &ctx.store,
                "kill",
                &args.name,
                &[("image", &image), ("signal", "9")],
            );
        }
        return Ok(());
    }
    let record = ctx.store.load_image(&args.name)?;
    let mode = record.as_ref().map(|r| r.mode);
    let policy = record.as_ref().map(|r| r.restart).unwrap_or_default();
    let stops = exits_on(record.as_ref(), signal)?;
    if policy != Restart::No && stops {
        ctx.store.mark_exit_on_next(&args.name)?;
    }
    // Only nspawn's machines have a place for marks.
    if record.is_some() {
        mark_signal(sd, &ctx.store, &args.name, signal).await?;
    }
    let delivered: Result<()> = async {
        if mode == Some(Mode::App) {
            let pid1 = record.as_ref().is_some_and(ImageRecord::program_is_pid1);
            let (leader, payload) = wait_for_payload(sd, &args.name, pid1).await?;
            let payload = payload.with_context(|| {
                format!(
                    "{} has no program running under its init (leader PID {leader})",
                    args.name
                )
            })?;
            signal_payload(sd, &args.name, leader, payload, pid1, signal)
                .await
                .with_context(|| {
                    format!("sending {} to PID {payload} of {}", args.signal, args.name)
                })
        } else {
            sd.kill_machine(&args.name, "leader", signal).await
        }
    }
    .await;
    if let Err(e) = delivered {
        // The marks were set ahead of the signal, so that a release right after it finds
        // them; a signal that never went must not end the machine's next run instead.
        if record.is_some() {
            ctx.store.take_exit_on_next(&args.name)?;
            ctx.store.forget_signal(&args.name)?;
        }
        return Err(e);
    }
    // A paused machine is thawed for a signal that stops it, which it could not act on
    // frozen.
    if stops {
        let _ = sd
            .thaw_unit(&format!("systemd-nspawn@{}.service", args.name))
            .await;
    }
    let image = record.as_ref().map(|r| r.reference.as_str()).unwrap_or("");
    crate::api::events::emit_machine(
        &ctx.store,
        "kill",
        &args.name,
        &[("image", image), ("signal", &signal.to_string())],
    );
    Ok(())
}

/// Marks the signal sent to a machine's current run, with which run that is.
async fn mark_signal(sd: &Systemd, store: &Store, name: &str, signal: i32) -> Result<()> {
    let run = sd
        .invocation_id(&format!("systemd-nspawn@{name}.service"))
        .await
        .ok();
    store.mark_signal(name, signal, run.as_deref())
}

/// None keeps the remembered value.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UpdateRequest {
    pub name: String,
    pub restart: Option<Restart>,
    /// Bytes, 0 for none.
    pub memory: Option<u64>,
    /// Memory and swap together in bytes, -1 for no bound on swap, 0 for the default.
    pub memory_swap: Option<i64>,
    /// CPUs (0.5), 0 for none.
    pub cpus: Option<f64>,
    /// Processes, 0 for none.
    pub pids_limit: Option<u64>,
    /// The --health-* flags.
    pub health: crate::health::Overrides,
}

/// update, through the record and the drop-in: at the reload systemd applies a
/// running unit's new limits to its cgroup, and reads Restart= again. Returns whether it
/// was running.
pub async fn update(ctx: &Context, args: &UpdateRequest) -> Result<bool> {
    validate_entry_name(&args.name)?;
    if args.restart.is_none()
        && args.memory.is_none()
        && args.memory_swap.is_none()
        && args.cpus.is_none()
        && args.pids_limit.is_none()
        && args.health.is_empty()
    {
        bail!(
            "nothing to update; give --memory, --memory-swap, --cpus, --pids-limit, --restart or a --health flag"
        );
    }
    let sd = ctx.sd().await?;
    refuse_foreign(sd, &args.name).await?;
    let store = &ctx.store;
    let _lock = store.lock().await?;
    let mut record = store.load_image(&args.name)?.with_context(|| {
        format!(
            "{} is not an image managed by nspawn; a restart policy or limits need one",
            args.name
        )
    })?;
    let unit = format!("systemd-nspawn@{}.service", args.name);
    let state = sd.unit_status(&unit).await?;
    // A start or restart is about to read the unit's settings.
    if state.restarting() {
        bail!(
            "machine {} is restarting; update it once it runs, or stop it first",
            args.name
        );
    }
    if state.active == "activating" || store.is_starting(&args.name) {
        bail!("machine {} is starting; update it once it runs", args.name);
    }
    let previous_policy = record.restart;
    if let Some(restart) = args.restart {
        if record.remove_on_exit && restart != Restart::No {
            bail!(
                "{} was started with --rm: a machine removed when it ends cannot be restarted",
                args.name
            );
        }
        record.restart = restart;
    }
    apply_limits(
        &mut record.limits,
        args.memory,
        args.memory_swap,
        args.cpus,
        args.pids_limit,
    )?;
    record.limits.check(record.mode)?;
    if !args.health.is_empty() {
        let hc = args.health.apply(record.effective_healthcheck())?;
        record.healthcheck = Some(hc);
    }
    // The drop-in keeps the cgroup's access to the CDI nodes of a running machine:
    // looked up before anything is written, so that a spec gone since the start leaves
    // the record and the unit as they were.
    let mut tuning = record.tuning.clone();
    if let Some(plan) = crate::cdi::plan(&record.tuning)
        .with_context(|| format!("looking up the CDI devices of {}", args.name))?
    {
        tuning.devices.extend(crate::cdi::device_paths(
            &plan,
            &cdi_nodes_dir(store, &args.name),
        ));
    }
    store.record_image(&record)?;
    let route =
        settings::namespace_route(sd, &args.name, record.mode, bridge::namespaced(&record)).await?;
    let app_argv =
        settings::wrapped_argv(sd, &args.name, record.mode, &route, &record.tuning).await?;
    let reload = settings::write_hooks(
        &args.name,
        &ctx.config,
        &route,
        app_argv.as_deref(),
        &settings::HookSpec {
            restart: record.restart,
            limits: &record.limits,
            remove_on_exit: record.remove_on_exit,
            tuning: &tuning,
            api_socket: record.takes_api_socket(),
        },
    )?;
    let running = state.active == "active";
    let stopped_by_hand = stopped_by_hand(record.restart, previous_policy, running);
    let changed = if record.restart.enabled_at_boot() && !stopped_by_hand {
        sd.enable_unit(&unit).await?
    } else if args.restart.is_some() && previous_policy.enabled_at_boot() {
        sd.disable_unit(&unit).await?
    } else {
        false
    };
    reload_if_needed(sd, &unit, reload || changed).await?;
    // The runner reads the record once, at its start.
    if running && !args.health.is_empty() {
        crate::health::stop_runner(sd, &args.name).await?;
        crate::health::clear_status(&args.name);
        if record.effective_healthcheck().is_some_and(|h| h.active()) {
            crate::health::start_runner(ctx, &args.name).await?;
        }
    }
    crate::api::events::emit_machine(
        &ctx.store,
        "update",
        &args.name,
        &[("image", &record.reference)],
    );
    Ok(running)
}

/// An app's stop signal (SIGTERM unless its image names one), or a booted init's halt and
/// poweroff requests.
/// Whether a signal ends the machine's restarts: SIGKILL and its stop signal (SIGTERM
/// for an app that names none), not one its program may handle.
fn exits_on(record: Option<&ImageRecord>, signal: i32) -> Result<bool> {
    Ok(signal == libc::SIGKILL || stop_signals(record)?.contains(&signal))
}

fn stop_signals(record: Option<&ImageRecord>) -> Result<Vec<i32>> {
    match record {
        // An image's stop signal nspawn cannot read is SIGTERM, as its settings make it.
        Some(r) if r.mode == Mode::App => Ok(vec![r
            .effective_stop_signal()
            .and_then(|s| signal_number(s).ok())
            .unwrap_or(libc::SIGTERM)]),
        _ => Ok(vec![libc::SIGRTMIN() + 3, libc::SIGRTMIN() + 4]),
    }
}

/// Waits, within reason, until nothing of the previous run is left: a unit shutting
/// down, a machine machined still lists, a release hook that would undo what the start
/// prepares. A machine that really runs, or starts elsewhere, is an error.
async fn wait_for_previous(sd: &Systemd, name: &str, unit: &str, stub: bool) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let state = sd.unit_status(unit).await?;
        if state.restarting() {
            bail!("machine {name} is restarting after its program ended; stop it first, or see journalctl -u {unit}");
        }
        let active = state.active;
        let registered = sd.machine_exists(name).await?;
        // Its unit runs the hooks of a start already, which may wait for the machines it
        // depends on for as long as they take.
        if active == "activating" && state.sub == "start-pre" && !registered {
            bail!("machine {name} is already starting; see journalctl -u {unit}");
        }
        match active.as_str() {
            "active" | "activating" if registered && machine_alive(sd, name, stub).await => {
                if active == "activating" {
                    bail!("machine {name} is already starting");
                }
                bail!("machine {name} is already running");
            }
            "active" | "activating" | "reloading" | "deactivating" => {}
            _ if registered => {}
            _ => return Ok(()),
        }
        if Instant::now() > deadline {
            bail!("machine {name} is still going away; see journalctl -u {unit}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The leader, and for an app under the stub init its program: the stub outlives it for
/// a moment.
async fn machine_alive(sd: &Systemd, name: &str, stub: bool) -> bool {
    let Ok(leader) = sd.machine_leader(name).await else {
        return false;
    };
    if !std::path::Path::new(&format!("/proc/{leader}")).exists() {
        return false;
    }
    !stub || payload_pid(leader).is_some()
}

/// Repeats the poweroff request every couple of seconds: right after a start the init may
/// have no handler yet, and the kernel drops what PID 1 of a namespace does not handle.
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
            let _ = sd.poweroff_machine(name).await;
            requested = Instant::now();
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Ok(true)
}

async fn wait_gone(sd: &Systemd, name: &str, timeout: Duration) -> Result<bool> {
    // One past what the clock holds is none (NO_STOP_TIMEOUT): until it is gone.
    let deadline = Instant::now().checked_add(timeout);
    while sd.machine_exists(name).await? {
        if deadline.is_some_and(|deadline| Instant::now() > deadline) {
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Ok(true)
}

/// Signal names as OCI configs write them: SIGTERM, TERM, 15, SIGRTMIN+3, RTMAX-1.
pub fn signal_number(name: &str) -> Result<i32> {
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
            return match value.checked_add(offset) {
                Some(n) if (libc::SIGRTMIN()..=libc::SIGRTMAX()).contains(&n) => Ok(n),
                _ => bail!("signal {name} is outside the realtime range"),
            };
        }
    }
    // The other names of the same signals.
    let full = match full.as_str() {
        "SIGIOT" => "SIGABRT".to_string(),
        "SIGCLD" => "SIGCHLD".to_string(),
        "SIGPOLL" => "SIGIO".to_string(),
        _ => full,
    };
    let signal: nix::sys::signal::Signal = full
        .parse()
        .map_err(|_| anyhow::anyhow!("unknown signal {name}"))?;
    Ok(signal as i32)
}

/// A signal as systemd writes it (SIGTERM, SIGRTMIN+3), from any form `signal_number`
/// takes; systemd's own parser wants its case and its realtime notation.
pub fn signal_name(name: &str) -> Result<String> {
    let number = signal_number(name)?;
    if number == libc::SIGRTMIN() {
        return Ok("SIGRTMIN".to_string());
    }
    if number > libc::SIGRTMIN() {
        return Ok(format!("SIGRTMIN+{}", number - libc::SIGRTMIN()));
    }
    let signal = nix::sys::signal::Signal::try_from(number)
        .map_err(|_| anyhow::anyhow!("unknown signal {name}"))?;
    Ok(signal.as_str().to_string())
}

pub async fn signal_program(sd: &Systemd, name: &str, pid1: bool, signal: i32) -> Result<()> {
    let (leader, payload) = wait_for_payload(sd, name, pid1).await?;
    let payload = payload.with_context(|| {
        format!("{name} has no program running under its init (leader PID {leader})")
    })?;
    signal_payload(sd, name, leader, payload, pid1, signal)
        .await
        .with_context(|| format!("sending signal {signal} to PID {payload} of {name}"))
}

/// Signals an app's program through a pidfd taken before its parentage is checked: the
/// PID read a moment ago may belong to another process by now, a pidfd follows the one
/// process it names.
async fn signal_payload(
    sd: &Systemd,
    name: &str,
    leader: u32,
    payload: i32,
    pid1: bool,
    signal: i32,
) -> Result<()> {
    let fd = crate::nsenter::pidfd_open(nix::unistd::Pid::from_raw(payload))
        .map_err(|e| anyhow::anyhow!("PID {payload}: {}", e.desc()))?;
    let ours = if pid1 {
        u32::try_from(payload).ok() == Some(leader)
    } else {
        parent_of(payload) == Some(leader)
    };
    if !ours || sd.machine_leader(name).await? != leader {
        bail!("the program of {name} changed while it was signalled; try again");
    }
    crate::nsenter::pidfd_signal(&fd, signal).map_err(|e| anyhow::anyhow!("{}", e.desc()))
}

/// The parent of a process, from /proc.
fn parent_of(pid: i32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat_ppid_and_start(&stat).map(|(ppid, _)| ppid)
}

/// The process of an app's program: the leader when it runs as PID 1, the stub init's
/// child otherwise.
fn program_pid(leader: u32, pid1: bool) -> Option<i32> {
    if pid1 {
        i32::try_from(leader).ok()
    } else {
        payload_pid(leader)
    }
}

/// The stub init's child: from the kernel's children list, or else the oldest child by
/// start time, since anything re-parented to the stub came later.
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

/// systemd-nspawn's child keeps the binary's name until it hands over to the stub init,
/// which renames itself, or executes the machine's init: both come after it has dropped
/// its capabilities and loaded its seccomp filter. The stub init's child keeps it too,
/// until it executes the program.
fn in_nspawn_setup(comm: &str) -> bool {
    comm.trim_end() == "systemd-nspawn"
}

/// systemd-nspawn reports a machine started once its child has mounted its file systems,
/// before the child confines itself: a command entering the machine in between would take
/// the child's capabilities instead of the machine's. Waits, within reason, for the rest.
/// https://github.com/systemd/systemd/issues/43976
async fn wait_past_setup(machine: &str, leader: u32) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let comm = std::fs::read_to_string(format!("/proc/{leader}/comm"))
            .with_context(|| format!("reading the name of the leader of {machine}"))?;
        if !in_nspawn_setup(&comm) {
            return Ok(());
        }
        if Instant::now() > deadline {
            bail!("{machine} is still being set up by systemd-nspawn; try again");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Parent PID and start time (field 22) of a /proc/PID/stat line, whose comm may hold
/// spaces and parentheses.
fn stat_ppid_and_start(stat: &str) -> Option<(u32, u64)> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // rest starts at field 3 (state), so ppid is index 1 and starttime index 19.
    Some((fields.get(1)?.parse().ok()?, fields.get(19)?.parse().ok()?))
}

/// kill(2) with a raw number: nix's Signal has no realtime signals, which images use.
#[cfg(test)]
fn send_signal(pid: i32, signal: i32) -> nix::Result<()> {
    let fd = crate::nsenter::pidfd_open(nix::unistd::Pid::from_raw(pid))?;
    crate::nsenter::pidfd_signal(&fd, signal)
}

/// Starts a command inside a machine without waiting for it. `extra_env` comes after the
/// image's and the remembered one.
pub async fn spawn_in_namespaces(
    ctx: &Context,
    machine: &str,
    command: &[String],
    user: &str,
    extra_env: &[String],
    workdir: Option<&str>,
    stdio: nsenter::Stdio,
) -> Result<nsenter::Process> {
    validate_entry_name(machine)?;
    let sd = ctx.sd().await?;
    refuse_foreign(sd, machine).await?;
    if !sd.machine_exists(machine).await? {
        bail!("machine {machine} is not running");
    }
    // A process cannot join a frozen cgroup and run: it would wait for the thaw.
    let unit = format!("systemd-nspawn@{machine}.service");
    if sd.freezer_state(&unit).await.ok().as_deref() == Some("frozen") {
        bail!("machine {machine} is paused; unpause it before running a command in it");
    }
    let record = ctx.store.load_image(machine)?;
    let record = record.as_ref();
    let leader = sd.machine_leader(machine).await?;
    // Held before the leader is checked again, so the PID cannot be reused unnoticed.
    let leader_fd = nsenter::pidfd_open(nix::unistd::Pid::from_raw(leader as i32))
        .with_context(|| format!("opening the leader of {machine}"))?;
    if sd.machine_leader(machine).await? != leader {
        bail!("machine {machine} changed while the command was being started; try again");
    }
    wait_past_setup(machine, leader).await?;
    let user = exec_user(record, user);
    let working_dir = workdir.or_else(|| record.and_then(|r| r.effective_working_dir()));
    let env = record
        .map(|r| volume::merge_env(&r.run.env, &r.env))
        .unwrap_or_default();
    let env = volume::merge_env(&env, extra_env);
    let privileged = record.is_some_and(|r| r.tuning.privileged);
    tokio::task::block_in_place(|| {
        nsenter::spawn(
            leader,
            &leader_fd,
            command,
            user,
            working_dir,
            &env,
            stdio,
            privileged,
        )
    })
    .with_context(|| format!("running a command inside {machine}"))
}

/// Whom a command run inside a machine runs as, None for root: the user asked for, or
/// without one an app's own (its --user, else its image's USER). A booted machine's image
/// user is nobody's to take.
fn exec_user<'a>(record: Option<&'a ImageRecord>, asked: &'a str) -> Option<&'a str> {
    let user = if asked.is_empty() {
        record
            .filter(|r| r.mode == Mode::App)
            .and_then(ImageRecord::effective_user)?
    } else {
        asked
    };
    (user != "root").then_some(user)
}

/// machined's login session on a booted machine (`path` empty for the user's shell). A
/// machine just started has no D-Bus for a few seconds, so it is retried for a while.
pub async fn open_shell(
    ctx: &Context,
    machine: &str,
    user: &str,
    path: &str,
    args: Vec<String>,
    env: Vec<String>,
) -> Result<(OwnedFd, String)> {
    validate_entry_name(machine)?;
    let sd = ctx.sd().await?;
    refuse_foreign(sd, machine).await?;
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogsRequest {
    pub machine: String,
    /// Keep reading; starts from the last lines unless `lines` says otherwise.
    pub follow: bool,
    pub lines: Option<u32>,
    /// journalctl --since syntax.
    pub since: Option<String>,
    /// journalctl --until syntax; nothing is followed then.
    pub until: Option<String>,
    pub timestamps: bool,
    /// Also systemd's messages about the unit.
    pub all: bool,
    /// Booted machines: the machine's own journal instead of its console output.
    pub inside: bool,
    /// The journald namespace the machine writes to (--log-driver local), read together
    /// with the system's journal, which holds what it wrote before and systemd's lines.
    pub namespace: Option<String>,
    /// journalctl's JSON entries instead of text, for whoever needs each line's time.
    pub json: bool,
}

/// What a logs request needs from the machine's record: refused when its output was
/// dropped, its journal namespace, and the start at its creation. A name without a record
/// is read as it is.
pub async fn prepare_logs(ctx: &Context, request: &mut LogsRequest) -> Result<()> {
    let Some(record) = ctx.store.load_image(&request.machine)? else {
        // A machine nspawn keeps no record of has logs while machined has it, running
        // or as an image; a name nothing has, removed machines of it included, has none.
        let sd = ctx.sd().await?;
        if sd.machine_exists(&request.machine).await?
            || sd
                .list_images()
                .await?
                .iter()
                .any(|i| i.name == request.machine)
        {
            return Ok(());
        }
        bail!("no machine named {}", request.machine);
    };
    check_logs_kept(&record, request.inside)?;
    if let Some(namespace) = record.tuning.journal_namespace() {
        if let Ok(sd) = ctx.sd().await {
            // Its files must exist for journalctl to read them.
            let _ = crate::api::run::ensure_journal_namespace(sd, namespace).await;
        }
        request.namespace = Some(namespace.to_string());
    }
    let created = record.created;
    let mut moved = std::mem::take(request);
    moved = tokio::task::spawn_blocking(move || {
        from_creation(&mut moved, created, journal_time_before);
        moved
    })
    .await?;
    *request = moved;
    Ok(())
}

const FOLLOW_TAIL: u32 = 10;

/// journalctl's arguments for logs: the console output is in the unit's journal,
/// earlier runs included (`from_creation` leaves out other machines of the name); a
/// booted machine also has a journal of its own.
/// Refuses logs of a machine whose output was dropped (--log-driver none), rather than
/// showing nothing; a booted machine's own journal is still there.
pub fn check_logs_kept(record: &ImageRecord, inside: bool) -> Result<()> {
    if inside || !record.tuning.drops_output() {
        return Ok(());
    }
    let hint = if record.mode == Mode::Boot {
        "; --inside reads its own journal"
    } else {
        ""
    };
    bail!(
        "{} keeps no output: its log driver is none (start it with --log-driver journal to keep it){hint}",
        record.name
    )
}

/// Starts logs where the machine began: the unit's journal also holds what machines of
/// the same name wrote before it was made, and so may a booted machine's own. A later
/// `since` stands; an `until` before the creation leaves nothing to show. `before(a, b)`
/// says whether `a` is no later than `b`, in journalctl's syntax, which the two are in.
pub fn from_creation(
    request: &mut LogsRequest,
    created: u64,
    mut before: impl FnMut(&str, &str) -> bool,
) {
    let creation = format!("@{created}");
    if !request
        .since
        .as_deref()
        .is_none_or(|since| before(since, &creation))
    {
        return;
    }
    request.since = Some(creation.clone());
    if let Some(until) = request.until.clone() {
        if !before(&creation, &until) {
            // journalctl still reads the until, so that one it cannot parse is reported.
            request.since = Some(until);
            request.lines = Some(0);
        }
    }
}

/// Whether journalctl takes `a` as no later than `b`: it refuses a --since later than
/// its --until, and one it cannot read.
pub fn journal_time_before(a: &str, b: &str) -> bool {
    std::process::Command::new("journalctl")
        .args([
            "--quiet",
            "--lines=0",
            &format!("--since={a}"),
            &format!("--until={b}"),
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

pub fn journalctl_arguments(args: &LogsRequest) -> Vec<String> {
    // --all: a line with colours or a CR would read "[N B blob data]" otherwise.
    let mut argv = vec![
        "--no-pager".to_string(),
        "--quiet".to_string(),
        "--all".to_string(),
    ];
    let output = if args.json {
        "json"
    } else if args.timestamps {
        "short-iso"
    } else {
        "cat"
    };
    if args.inside {
        argv.push(format!("--machine={}", args.machine));
        argv.push(format!("--output={output}"));
    } else {
        if let Some(namespace) = &args.namespace {
            argv.push(format!("--namespace=+{namespace}"));
        }
        argv.push(format!("--unit=systemd-nspawn@{}.service", args.machine));
        argv.push(format!("--output={output}"));
        if !args.all {
            argv.push("_TRANSPORT=stdout".to_string());
        }
    }
    match (args.lines, args.follow) {
        // journalctl reads the count into an int and refuses more.
        (Some(n), _) if n > i32::MAX as u32 => argv.push("--lines=all".to_string()),
        (Some(n), _) => argv.push(format!("--lines={n}")),
        (None, true) => argv.push(format!("--lines={FOLLOW_TAIL}")),
        (None, false) => {}
    }
    if let Some(since) = &args.since {
        argv.push(format!("--since={since}"));
    }
    if let Some(until) = &args.until {
        argv.push(format!("--until={until}"));
    }
    if args.follow && args.until.is_none() {
        argv.push("--follow".to_string());
    }
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_unit_in_its_start_hooks_mounts_no_volume_yet() {
        let state = |active: &str, sub: &str| UnitState {
            load: "loaded".into(),
            active: active.into(),
            sub: sub.into(),
        };
        assert!(!mounts_volumes(&state("activating", "start-pre")));
        assert!(!mounts_volumes(&state("activating", "condition")));
        assert!(mounts_volumes(&state("activating", "start")));
        assert!(mounts_volumes(&state("activating", "start-post")));
        assert!(mounts_volumes(&state("active", "running")));
        assert!(mounts_volumes(&state("activating", "auto-restart")));
        assert!(!mounts_volumes(&state("inactive", "dead")));
    }

    #[test]
    fn a_path_takes_one_mount() {
        let record = |volumes: serde_json::Value, tmpfs: serde_json::Value| -> ImageRecord {
            serde_json::from_value(serde_json::json!({
                "name": "db", "reference": "r", "manifest_digest": "sha256:a", "layers": [],
                "backend": "overlay", "created": 1,
                "volumes": volumes, "tuning": { "tmpfs": tmpfs },
            }))
            .unwrap()
        };
        let both = record(
            serde_json::json!([{ "source": "pgdata", "target": "/var/lib/pg" }]),
            serde_json::json!(["/var/lib/pg/:size=1m"]),
        );
        assert_eq!(
            duplicate_mount_points(&both).unwrap_err().to_string(),
            "Duplicate mount point: /var/lib/pg"
        );
        // A doubled slash is the same place to systemd-nspawn, on either side.
        for (target, tmpfs) in [
            ("/var/lib/pg", "/var/lib//pg"),
            ("/var/lib//pg/", "/var/lib/pg:mode=0700"),
        ] {
            let both = record(
                serde_json::json!([{ "source": "pgdata", "target": target }]),
                serde_json::json!([tmpfs]),
            );
            assert_eq!(
                duplicate_mount_points(&both).unwrap_err().to_string(),
                "Duplicate mount point: /var/lib/pg",
                "{target} {tmpfs}"
            );
        }
        // Two volumes, as a record of an earlier release may have them.
        let twice = record(
            serde_json::json!([
                { "source": "a", "target": "/data" },
                { "source": "b", "target": "/data/" },
            ]),
            serde_json::json!([]),
        );
        assert!(duplicate_mount_points(&twice).is_err());
        // Two tmpfs on one path are one.
        let tmpfs = record(
            serde_json::json!([]),
            serde_json::json!(["/tmp", "/tmp/:size=1m"]),
        );
        assert!(duplicate_mount_points(&tmpfs).is_ok());
        let apart = record(
            serde_json::json!([{ "source": "pgdata", "target": "/var/lib/pg" }]),
            serde_json::json!(["/tmp"]),
        );
        assert!(duplicate_mount_points(&apart).is_ok());
    }

    #[test]
    fn volumes_are_seeded_from_the_image_when_new_or_empty() {
        let named: crate::volume::Volume = "data:/var/lib/app".parse().unwrap();
        let host: crate::volume::Volume = "/srv/data:/var/lib/app".parse().unwrap();
        assert!(seeds(&named, true, false, || false));
        // Made ahead by volume create or compose up: filled while still empty, by a new
        // machine only.
        assert!(seeds(&named, false, true, || true));
        assert!(!seeds(&named, false, true, || false));
        assert!(!seeds(&named, false, false, || true));
        assert!(!seeds(&host, false, true, || true));
    }

    #[test]
    fn kernel_releases_compare_by_major_and_minor() {
        assert!(release_at_least("6.8.0-45-generic\n", 6, 3));
        assert!(release_at_least("6.3.0", 6, 3));
        assert!(!release_at_least("6.2.16-arch1", 6, 3));
        assert!(!release_at_least("5.15.0", 6, 3));
        assert!(release_at_least("7.2.11-arch1-1", 6, 3));
    }

    #[test]
    fn signals_wait_for_the_program_itself() {
        let names = |pid: i32| match pid {
            7 => Some("systemd-nspawn".to_string()),
            8 => Some("sh".to_string()),
            _ => None,
        };
        assert_eq!(
            running_program(Some(7), names),
            None,
            "the child has not executed the program yet"
        );
        assert_eq!(running_program(Some(8), names), Some(8));
        assert_eq!(running_program(Some(9), names), None, "gone meanwhile");
        assert_eq!(running_program(None, names), None);
    }

    #[test]
    fn nspawn_setup_by_the_leader_name() {
        assert!(in_nspawn_setup("systemd-nspawn\n"));
        assert!(!in_nspawn_setup("(sd-stubinit)\n"));
        assert!(!in_nspawn_setup("systemd\n"));
        assert!(!in_nspawn_setup("sh\n"));
    }

    #[test]
    fn interfaces_go_through_the_hooks_for_an_app_on_the_bridge_alone() {
        let record = |mode: &str, network: &str, no_network: bool, container: Option<&str>| {
            let mut r: ImageRecord = serde_json::from_str(&format!(
                r#"{{"name": "kali", "reference": "r", "manifest_digest": "d", "layers": [], "backend": "overlay", "created": 0, "mode": "{mode}", "network": "{network}", "no_network": {no_network}}}"#
            ))
            .unwrap();
            r.network_container = container.map(str::to_string);
            r.tuning.interfaces = vec!["wlan0".to_string()];
            r
        };
        let app = record("app", "bridge", false, None);
        assert_eq!(interface_route(&app), InterfaceRoute::Hooks);
        assert_eq!(hook_interfaces(&app), ["wlan0"]);
        for (mode, network, no_network, container) in [
            ("boot", "bridge", false, None),
            ("boot", "veth", false, None),
            ("app", "bridge", true, None),
            ("boot", "bridge", true, None),
            ("app", "host", false, None),
            ("app", "bridge", false, Some("vpn")),
        ] {
            let r = record(mode, network, no_network, container);
            assert_eq!(
                interface_route(&r),
                InterfaceRoute::Settings,
                "{mode} {network} {no_network} {container:?}"
            );
            assert!(hook_interfaces(&r).is_empty());
        }
    }

    #[test]
    fn processes_are_collected_below_the_unit_cgroup() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("cgroup.procs"), "").unwrap();
        for (dir, procs) in [
            ("supervisor", "100\n"),
            ("payload", "101\n102\n"),
            ("payload/system.slice", "\n"),
            ("payload/system.slice/a.service", "103\n"),
        ] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
            std::fs::write(root.join(dir).join("cgroup.procs"), procs).unwrap();
        }
        let mut pids = Vec::new();
        cgroup_pids(root, &mut pids);
        pids.sort_unstable();
        assert_eq!(pids, [100, 101, 102, 103]);
    }

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
    fn a_machine_that_is_gone_is_stopped_when_there_is_something_to_stop() {
        let state = |active: &str, sub: &str| UnitState {
            load: "loaded".to_string(),
            active: active.to_string(),
            sub: sub.to_string(),
        };
        let restarting = state("activating", "auto-restart");
        let dead = state("inactive", "dead");
        assert!(
            stoppable_when_gone(true, &dead),
            "nspawn's leaves things behind"
        );
        assert!(
            stoppable_when_gone(false, &restarting),
            "a restart loop of anybody's"
        );
        assert!(!stoppable_when_gone(false, &dead));
        // Its stop hooks run: the stop job waits for them.
        let stopping = state("deactivating", "stop-post");
        assert!(stopping.shutting_down() && !stopping.busy());
        assert!(stoppable_when_gone(false, &stopping));
        // An update keeps a hand-stopped unless-stopped machine off the boot list.
        assert!(stopped_by_hand(
            Restart::UnlessStopped,
            Restart::UnlessStopped,
            false
        ));
        assert!(!stopped_by_hand(
            Restart::UnlessStopped,
            Restart::UnlessStopped,
            true
        ));
        assert!(!stopped_by_hand(
            Restart::UnlessStopped,
            Restart::Always,
            false
        ));
        assert!(!stopped_by_hand(
            Restart::Always,
            Restart::UnlessStopped,
            false
        ));
        // SIGKILL ends restarts and a start still waiting for what it depends on.
        assert!(kill_ends(&restarting));
        assert!(kill_ends(&state("activating", "start-pre")));
        assert!(!kill_ends(&dead));
        assert!(!kill_ends(&stopping));
    }

    #[test]
    fn exec_runs_as_the_machine_s_user_unless_told_otherwise() {
        let record = |mode: &str, image: Option<&str>, flag: Option<&str>| {
            let mut r: ImageRecord = serde_json::from_value(serde_json::json!({
                "name": "web", "reference": "r", "manifest_digest": "d", "layers": [],
                "backend": "overlay", "created": 0, "mode": mode,
            }))
            .unwrap();
            r.run.user = image.map(str::to_string);
            r.tuning.user = flag.map(str::to_string);
            r
        };
        let app = record("app", Some("node"), None);
        assert_eq!(exec_user(Some(&app), ""), Some("node"));
        assert_eq!(exec_user(Some(&app), "1000:1000"), Some("1000:1000"));
        assert_eq!(exec_user(Some(&app), "root"), None);
        // --user wins over the image's, as for the machine's program.
        let flagged = record("app", Some("node"), Some("65534"));
        assert_eq!(exec_user(Some(&flagged), ""), Some("65534"));
        // Root when nothing names a user, or the machine boots, or it is not nspawn's.
        assert_eq!(exec_user(Some(&record("app", None, None)), ""), None);
        assert_eq!(
            exec_user(Some(&record("app", Some("root"), None)), ""),
            None
        );
        assert_eq!(
            exec_user(Some(&record("boot", Some("node"), None)), ""),
            None
        );
        assert_eq!(exec_user(None, ""), None);
        assert_eq!(exec_user(None, "nobody"), Some("nobody"));
    }

    #[test]
    fn a_signal_ends_the_restarts_when_it_stops() {
        let app = |stop: Option<&str>| {
            let mut r: ImageRecord = serde_json::from_value(serde_json::json!({
                "name": "web", "reference": "r", "manifest_digest": "d", "layers": [],
                "backend": "overlay", "created": 0, "mode": "app",
            }))
            .unwrap();
            r.tuning.stop_signal = stop.map(str::to_string);
            r
        };
        // No stop signal of its own: SIGTERM, not one its program may handle.
        assert!(exits_on(Some(&app(None)), libc::SIGTERM).unwrap());
        assert!(!exits_on(Some(&app(None)), libc::SIGUSR1).unwrap());
        // With one: that one, and SIGKILL.
        let quit = app(Some("SIGQUIT"));
        assert!(exits_on(Some(&quit), libc::SIGQUIT).unwrap());
        assert!(exits_on(Some(&quit), libc::SIGKILL).unwrap());
        assert!(!exits_on(Some(&quit), libc::SIGHUP).unwrap());
    }

    #[test]
    fn a_failed_kill_still_stops_the_unit() {
        assert!(stop_job_after_kill(Latch::AfterSignal, true));
        assert!(stop_job_after_kill(Latch::AfterSignal, false));
        assert!(!stop_job_after_kill(Latch::NotNeeded, true));
        assert!(stop_job_after_kill(Latch::NotNeeded, false));
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
        assert_eq!(unit_word(&state("active", "running")), "closing");
        assert_eq!(unit_word(&state("deactivating", "stop-post")), "closing");
        assert_eq!(unit_word(&state("inactive", "dead")), "stopped");
        assert_eq!(unit_word(&state("failed", "failed")), "stopped");
    }

    #[test]
    fn ps_without_all_lists_what_runs_or_restarts() {
        assert!(listed(false, "restarting"));
        for state in ["starting", "closing", "stopped"] {
            assert!(!listed(false, state), "{state}");
            assert!(listed(true, state), "{state}");
        }
    }

    #[test]
    fn limits_given_on_a_start_replace_only_themselves() {
        let mut limits = Limits {
            memory: 64 << 20,
            milli_cpus: 500,
            pids: 100,
            memory_swap: 128 << 20,
        };
        apply_limits(&mut limits, None, None, Some(2.0), Some(0)).unwrap();
        assert_eq!(
            limits,
            Limits {
                memory: 64 << 20,
                milli_cpus: 2000,
                pids: 0,
                memory_swap: 128 << 20,
            }
        );
        apply_limits(&mut limits, Some(0), None, None, None).unwrap();
        assert_eq!(limits.memory, 0);
        assert_eq!(limits.memory_swap, 0, "a total goes with the memory limit");
        apply_limits(&mut limits, None, Some(-1), None, None).unwrap();
        apply_limits(&mut limits, Some(0), None, None, None).unwrap();
        assert_eq!(limits.memory_swap, -1, "no bound on swap stays");
        assert!(apply_limits(&mut limits, None, None, Some(-1.0), None).is_err());
    }

    #[test]
    fn signals_go_through_a_pidfd() {
        // A PID that cannot exist has no pidfd; a process that does takes signal 0, and
        // a realtime number is one the kernel takes (EINVAL would say otherwise).
        let rt = libc::SIGRTMIN() + 3;
        assert_eq!(send_signal(i32::MAX, rt), Err(nix::errno::Errno::ESRCH));
        assert_eq!(send_signal(std::process::id() as i32, 0), Ok(()));
        assert_eq!(
            parent_of(std::process::id() as i32),
            Some(std::os::unix::process::parent_id())
        );
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
        assert_eq!(signal_number("SIGIOT").unwrap(), libc::SIGABRT);
        assert_eq!(signal_number("cld").unwrap(), libc::SIGCHLD);
        assert_eq!(signal_number("POLL").unwrap(), libc::SIGIO);
        assert!(signal_number("RTMIN+2147483647").is_err(), "no overflow");
        assert_eq!(signal_number("RTMAX-1").unwrap(), libc::SIGRTMAX() - 1);
        assert!(signal_number("SIGRTMIN+200").is_err());
        assert!(signal_number("0").is_err());
        assert!(signal_number("SIGBOGUS").is_err());
        for (text, name) in [
            ("term", "SIGTERM"),
            ("3", "SIGQUIT"),
            ("SIGRTMIN", "SIGRTMIN"),
            ("RTMAX-1", "SIGRTMIN+29"),
        ] {
            assert_eq!(signal_name(text).unwrap(), name, "{text}");
        }
        assert!(signal_name("SIGBOGUS").is_err());
    }

    #[test]
    fn whose_program_is_pid1() {
        let record = |json: &str| serde_json::from_str::<ImageRecord>(json).unwrap();
        let base = r#""name": "web", "reference": "r", "manifest_digest": "d", "layers": [], "backend": "overlay", "created": 0"#;
        // (mode, the image's s6-overlay, --init) -> PID 1
        for (mode, pid1, init, expected) in [
            ("app", false, None, false),
            ("app", true, None, true),
            ("app", true, Some(true), false),
            ("app", false, Some(false), true),
            ("app", false, Some(true), false),
            ("boot", true, Some(false), false),
        ] {
            let init = init.map_or(String::new(), |i| format!(r#", "tuning": {{"init": {i}}}"#));
            let r = record(&format!(
                r#"{{{base}, "mode": "{mode}", "pid1": {pid1}{init}}}"#
            ));
            assert_eq!(r.program_is_pid1(), expected, "{mode} {pid1} {init}");
            assert_eq!(r.under_stub_init(), mode == "app" && !expected);
        }
        // A record that says nothing of either runs its program under the stub.
        assert!(record(&format!(r#"{{{base}, "mode": "app"}}"#)).under_stub_init());
        // A record of --init as a bool reads as --init.
        let old = record(&format!(
            r#"{{{base}, "mode": "app", "tuning": {{"init": true}}}}"#
        ));
        assert_eq!(old.tuning.init, Some(true));
    }

    #[test]
    fn what_asks_a_machine_to_end() {
        let record = |json: &str| serde_json::from_str::<ImageRecord>(json).unwrap();
        let base = r#""name": "web", "reference": "r", "manifest_digest": "d", "layers": [], "backend": "overlay", "created": 0"#;
        let app = record(&format!(r#"{{{base}, "mode": "app"}}"#));
        assert_eq!(stop_signals(Some(&app)).unwrap(), [libc::SIGTERM]);
        let quits = record(&format!(
            r#"{{{base}, "mode": "app", "run": {{"stop_signal": "SIGQUIT"}}}}"#
        ));
        assert_eq!(stop_signals(Some(&quits)).unwrap(), [libc::SIGQUIT]);
        let booted = record(&format!(r#"{{{base}, "mode": "boot"}}"#));
        let init = [libc::SIGRTMIN() + 3, libc::SIGRTMIN() + 4];
        assert_eq!(stop_signals(Some(&booted)).unwrap(), init);
        assert_eq!(stop_signals(None).unwrap(), init);
    }

    #[test]
    fn logs_start_where_the_machine_began() {
        // Times as numbers, compared the way journalctl would; "x" it cannot read.
        let before = |a: &str, b: &str| {
            let t = |s: &str| s.trim_start_matches('@').parse::<u64>().ok();
            matches!((t(a), t(b)), (Some(a), Some(b)) if a <= b)
        };
        let logs = |since: Option<&str>, until: Option<&str>| {
            let mut request = LogsRequest {
                machine: "web".into(),
                since: since.map(str::to_string),
                until: until.map(str::to_string),
                ..LogsRequest::default()
            };
            from_creation(&mut request, 100, before);
            (request.since, request.until, request.lines)
        };
        let some = |s: &str| Some(s.to_string());
        assert_eq!(logs(None, None), (some("@100"), None, None));
        assert_eq!(
            logs(Some("@50"), None),
            (some("@100"), None, None),
            "what came before the machine is another's"
        );
        assert_eq!(logs(Some("@150"), None), (some("@150"), None, None));
        assert_eq!(
            logs(Some("x"), None),
            (some("x"), None, None),
            "journalctl reports what it cannot read"
        );
        assert_eq!(logs(None, Some("@200")), (some("@100"), some("@200"), None));
        assert_eq!(
            logs(Some("@20"), Some("@50")),
            (some("@50"), some("@50"), Some(0)),
            "nothing of the machine is that old"
        );
        assert_eq!(
            logs(None, Some("x")),
            (some("x"), some("x"), Some(0)),
            "an until journalctl cannot read still reaches it"
        );
        assert_eq!(
            logs(Some("@150"), Some("@120")),
            (some("@150"), some("@120"), None),
            "a since after the until is the caller's, for journalctl to refuse"
        );
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
                "--all",
                "--unit=systemd-nspawn@web.service",
                "--output=cat",
                "_TRANSPORT=stdout"
            ]
        );
        let until = LogsRequest {
            follow: true,
            until: Some("now".into()),
            ..base
        };
        let argv = journalctl_arguments(&until);
        assert!(argv.contains(&"--until=now".to_string()));
        assert!(
            !argv.iter().any(|a| a == "--follow"),
            "nothing is followed up to a time"
        );
        let follow = LogsRequest {
            machine: "web".into(),
            follow: true,
            ..LogsRequest::default()
        };
        let argv = journalctl_arguments(&follow);
        assert!(argv.contains(&"--lines=10".to_string()) && argv.last().unwrap() == "--follow");
        // Every line, which journalctl takes as all and not as a number past an int.
        let every = LogsRequest {
            lines: Some(u32::MAX),
            ..follow.clone()
        };
        assert!(journalctl_arguments(&every).contains(&"--lines=all".to_string()));
        let full = LogsRequest {
            machine: "web".into(),
            follow: true,
            lines: Some(50),
            since: Some("10 min ago".into()),
            until: None,
            timestamps: true,
            all: true,
            inside: false,
            namespace: None,
            json: false,
        };
        assert_eq!(
            journalctl_arguments(&full),
            vec![
                "--no-pager",
                "--quiet",
                "--all",
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
                "--all",
                "--machine=fedora-44",
                "--output=cat"
            ]
        );
        // A local machine: its namespace and the system's journal, interleaved.
        let local = LogsRequest {
            machine: "web".into(),
            namespace: Some("nspawn".into()),
            ..LogsRequest::default()
        };
        assert_eq!(
            journalctl_arguments(&local),
            vec![
                "--no-pager",
                "--quiet",
                "--all",
                "--namespace=+nspawn",
                "--unit=systemd-nspawn@web.service",
                "--output=cat",
                "_TRANSPORT=stdout"
            ]
        );
    }

    #[test]
    fn logs_of_a_machine_that_drops_its_output_are_refused() {
        let record = |mode: &str, driver: Option<&str>| {
            let mut r: ImageRecord = serde_json::from_str(&format!(
                r#"{{"name": "quiet", "reference": "r", "manifest_digest": "d", "layers": [], "backend": "overlay", "created": 0, "mode": "{mode}"}}"#
            ))
            .unwrap();
            r.tuning.log_driver = driver.map(str::to_string);
            r
        };
        assert!(check_logs_kept(&record("app", None), false).is_ok());
        let app = check_logs_kept(&record("app", Some("none")), false).unwrap_err();
        assert!(app.to_string().contains("keeps no output"), "{app}");
        assert!(!app.to_string().contains("--inside"));
        let boot = check_logs_kept(&record("boot", Some("none")), false).unwrap_err();
        assert!(boot.to_string().contains("--inside"), "{boot}");
        assert!(check_logs_kept(&record("boot", Some("none")), true).is_ok());
    }
}
