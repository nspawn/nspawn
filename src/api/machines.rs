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
    /// How its last run ended, for a stopped machine: docker's exit code.
    pub exit_code: Option<i32>,
}

/// The exit code of the machine's last run, as an attached run would have given it.
async fn last_exit_code(sd: &Systemd, store: &Store, record: &ImageRecord) -> Option<i32> {
    let unit = format!("systemd-nspawn@{}.service", record.name);
    // The unit's own ExecMainCode= goes with it when it is unloaded; the journal keeps
    // what systemd logged when it reaped the process.
    let (code, status) = match sd.exec_main_exit(&unit).await {
        Ok(Some(exit)) => exit,
        _ => crate::journal::last_main_exit(&unit).await?,
    };
    let ending = crate::api::run::Ending {
        code,
        status,
        result: String::new(),
    };
    Some(crate::api::run::exit_code(
        &ending,
        store.last_signal(&record.name),
        record.mode == Mode::App,
    ))
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
        });
    }
    let unit = sd
        .unit_status(&format!("systemd-nspawn@{name}.service"))
        .await?;
    match record {
        Some(record) => {
            let state = unit_word(&unit).to_string();
            let exit_code = if state == "stopped" {
                last_exit_code(sd, &ctx.store, &record).await
            } else {
                None
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
        });
    }
    // Records machined does not list. Those between two runs of a restart policy show
    // without -a, as in docker ps; starting, closing and stopped ones only with -a.
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
            let exit_code = if state == "stopped" {
                last_exit_code(sd, &ctx.store, r).await
            } else {
                None
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
            });
        }
    }
    Ok(summaries)
}

fn listed(all: bool, state: &str) -> bool {
    all || state == "restarting"
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
    let state = sd
        .unit_status(&format!("systemd-nspawn@{name}.service"))
        .await?;
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

/// Everything a machine needs before its unit starts. Shared by `start` and the
/// ExecStartPre hook, so idempotent; the caller holds the store lock.
pub async fn prepare(
    sd: &Systemd,
    store: &Store,
    config: &Config,
    name: &str,
    record: Option<ImageRecord>,
    report: Report<'_>,
) -> Result<Network> {
    let Some(mut record) = record else {
        // Not ours: the stock template uses --network-veth.
        hostnet::ensure_networkd(sd, report).await?;
        return Ok(Network::Veth);
    };
    // A new run: the marks `kill` and `stop` left for the last one go.
    store.take_exit_on_next(name)?;
    store.forget_signal(name)?;
    let bridged = bridge::bridge_kind(&record);
    if bridged && record.mode == Mode::App && record.backend == BackendChoice::Mstack {
        bail!(
            "{name} is an mstack app: managed user namespaces cannot join the network namespace prepared for the bridge; pull it again with --backend overlay, or start it with --network host"
        );
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
    } else {
        store.record_image(&record)?;
        None
    };
    if !record.tuning.sysctls.is_empty() && (record.mode != Mode::App || !bridged) {
        bail!("--sysctl values are set in the network namespace nspawn makes for an app machine on a bridge network; {name} has none");
    }
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
            // What the image has there, owner included, as docker seeds a volume: from
            // the layers, which earlier runs have not touched, or the tree itself
            // where there are none.
            let target = volume.target.trim_start_matches('/');
            let image_path =
                crate::backend::image_path(store, &record.layers, record.backend, target).or_else(
                    || {
                        (record.backend == BackendChoice::Flat)
                            .then(|| crate::backend::root_path(store, name, record.backend, target))
                            .flatten()
                    },
                );
            if let Some(image_path) = image_path {
                crate::volume::seed(&image_path, &source)?;
            }
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
    // systemd-nspawn keeps there; --tmpfs /var/run, docker's habit for a read-only
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
    // A booted machine's systemd sets the hostname from /etc/hostname, over the one
    // systemd-nspawn set: --hostname goes in as that file, as docker writes it.
    let hostname_file = match (&record.tuning.hostname, record.mode) {
        (Some(hostname), Mode::Boot) => {
            let dir = store.machine_files_dir(name);
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
            let path = dir.join("hostname");
            std::fs::write(&path, format!("{hostname}\n"))
                .with_context(|| format!("writing {}", path.display()))?;
            Some(path)
        }
        _ => None,
    };
    let managed_userns = record.backend == BackendChoice::Mstack;
    if managed_userns && !record.tuning.secrets.is_empty() {
        bail!("{name} runs under managed user namespaces (mstack), where secrets cannot be attached yet; pull it again with --backend overlay");
    }
    binds.extend(crate::api::secrets::materialize(store, &record)?);
    if record.mode == Mode::App {
        binds.extend(crate::getent::shim(store, name, &record)?);
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
        points.extend(
            record
                .tuning
                .devices
                .iter()
                .map(|d| (d.container.clone(), false)),
        );
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
    let route = settings::namespace_route(sd, name, record.mode, bridged).await?;
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
    let app_argv = settings::app_argv(sd, name, record.mode, &route).await?;
    if settings::write_hooks(
        name,
        config,
        &route,
        app_argv.as_deref(),
        &settings::HookSpec {
            restart: record.restart,
            limits: &record.limits,
            remove_on_exit: record.remove_on_exit,
            tuning: &record.tuning,
        },
    )? {
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
    let mode = store.load_image(&args.name)?.map(|r| r.mode);
    wait_for_previous(sd, &args.name, &unit, mode).await?;
    let lock = store.lock().await?;
    // Again under the lock: another start may be preparing it, or it may be up by now.
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
                r.aliases =
                    crate::api::network::parse_aliases(&args.aliases, &bridge::networks_of(r))?;
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
            if !args.health.is_empty() {
                let hc = args.health.apply(r.effective_healthcheck())?;
                r.healthcheck = Some(hc);
            }
            args.tuning.apply(&mut r.tuning)?;
            if r.mode == Mode::Boot
                && (r.tuning.user.is_some() || r.tuning.working_dir.is_some() || r.tuning.init)
            {
                bail!(
                    "{} boots an init system; --user, --workdir and --init only apply to the program of an app image",
                    args.name
                );
            }
            // Set on every start, so that a start without --rm keeps the machine.
            if args.remove && r.restart != Restart::No {
                bail!("--rm and a restart policy exclude each other: a machine removed when it ends cannot be restarted (--restart no)");
            }
            r.remove_on_exit = args.remove;
        }
        None if !args.command.is_empty()
            || !args.network.is_empty()
            || !args.aliases.is_empty()
            || !args.publish.is_empty()
            || args.entrypoint.is_some()
            || !args.env.is_empty()
            || !args.volume.is_empty()
            || !args.label.is_empty()
            || args.restart.is_some()
            || args.memory.is_some()
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
    let network = prepare(sd, store, config, &args.name, record, report).await?;
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
    if sd.start_machine(&args.name).await? {
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

/// 0 removes a limit; None keeps it.
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

/// docker pause: the machine's cgroup is frozen, every process in it with it.
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
    crate::api::events::emit(
        "machine",
        if on { "pause" } else { "unpause" },
        name,
        &[("image", &image)],
    );
    Ok(())
}

/// A process of a running machine, as docker top shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Process {
    pub pid: u32,
    /// The uid as the machine sees it, "root" for 0.
    pub user: String,
    /// CPU time, HH:MM:SS.
    pub time: String,
    pub command: String,
}

/// docker top: the processes in the machine's PID namespace, from its cgroup.
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
    // still be going while the new one is not there yet. Without that split (another
    // supervisor), what is in the machine's PID namespace is its, and never what is in
    // the host's.
    let payload = unit_cgroup.join("payload");
    for _ in 0..20 {
        if payload.is_dir() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if payload.is_dir() {
        cgroup_pids(&payload, &mut pids);
    } else {
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

/// The network namespace and published ports of a machine that ended. Safe to repeat.
pub fn release_machine(name: &str, record: Option<&ImageRecord>) -> Result<()> {
    if record.map(|r| r.network) == Some(Network::Bridge) {
        bridge::delete_netns(name);
        record.map(bridge::withdraw_ports).transpose()?;
    }
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
    recorded || unit.busy()
}

/// After SIGKILL the stop job goes in for the latch, and whenever the kill failed, since
/// the job is then what makes the machine go.
fn stop_job_after_kill(latch: Latch, killed: bool) -> bool {
    latch == Latch::AfterSignal || !killed
}

pub async fn stop(ctx: &Context, args: &StopRequest, report: Report<'_>) -> Result<StopOutcome> {
    validate_entry_name(&args.name)?;
    let sd = ctx.sd().await?;
    refuse_foreign(sd, &args.name).await?;
    let store = &ctx.store;
    let record = store.load_image(&args.name)?;
    let unit = format!("systemd-nspawn@{}.service", args.name);
    let policy = record.as_ref().map(|r| r.restart).unwrap_or_default();
    // A paused machine cannot act on a signal.
    let _ = sd.thaw_unit(&unit).await;
    // unless-stopped: stopped by hand means not at boot either, until the next start.
    if policy == Restart::UnlessStopped && sd.disable_unit(&unit).await? {
        sd.reload().await?;
    }
    if !sd.machine_exists(&args.name).await? {
        // A start is preparing it: its network must not be released under it.
        if store.is_starting(&args.name) {
            bail!("machine {} is starting; stop it once it runs", args.name);
        }
        // Ended, or between two runs of a restart policy (nspawn's or an administrator's):
        // the stop job ends a pending restart and completes once the last release hook
        // is done, so nothing is released beside it.
        let state = sd.unit_status(&unit).await?;
        if !stoppable_when_gone(record.is_some(), &state) {
            bail!("machine {} is not running", args.name);
        }
        sd.stop_unit_job(&unit).await?.wait().await?;
        release_machine(&args.name, record.as_ref())?;
        sd.reset_failed(&unit).await?;
        return Ok(if state.busy() {
            StopOutcome::Stopped
        } else {
            StopOutcome::WasNotRunning
        });
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
        // systemd 255 answers "Invalid argument" when some process of the unit cannot be
        // signalled although the machine got its SIGKILL, so the answer decides nothing:
        // the stop job goes in either way, and what counts is that the machine goes.
        if record.is_some() {
            store.mark_signal(&args.name, libc::SIGKILL)?;
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
            // docker stop: the image's stop signal, then SIGKILL after the timeout.
            Some(Mode::App) => {
                let signal = record
                    .as_ref()
                    .and_then(|r| r.effective_stop_signal().map(str::to_string))
                    .unwrap_or_else(|| "SIGTERM".to_string());
                let timeout = args
                    .timeout
                    .or_else(|| record.as_ref().and_then(|r| r.tuning.stop_timeout))
                    .unwrap_or(10);
                let (leader, payload) = wait_for_payload(sd, &args.name).await?;
                store.mark_signal(&args.name, signal_number(&signal)?)?;
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
                    let gone = wait_gone(sd, &args.name, Duration::from_secs(timeout)).await?;
                    if latch == Latch::AfterSignal {
                        job = Some(sd.stop_unit_job(&unit).await?);
                    }
                    if !gone {
                        note(
                            report,
                            format!(
                                "{} ignored {signal} for {timeout} seconds; killing it",
                                args.name
                            ),
                        );
                        store.mark_signal(&args.name, libc::SIGKILL)?;
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
        hostnet::release(sd, &admitted).await;
        let _lock = store.lock().await?;
        release_machine(&args.name, record.as_ref())?;
    }
    Ok(StopOutcome::Stopped)
}

/// An app's program (the stub init's child), which signals go to, never the whole cgroup: the stub init
/// reboots on SIGINT and systemd-nspawn dies of SIGQUIT. Waits a moment for the stub to
/// fork it. Returns the leader too.
async fn wait_for_payload(sd: &Systemd, name: &str) -> Result<(u32, Option<i32>)> {
    let leader = sd.machine_leader(name).await?;
    let mut payload = payload_pid(leader);
    let deadline = Instant::now() + Duration::from_secs(2);
    while payload.is_none() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
        payload = payload_pid(leader);
    }
    Ok((leader, payload))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KillRequest {
    pub name: String,
    pub signal: String,
}

/// docker kill. SIGKILL is `stop --force`. Other signals go to an app's program or a
/// booted machine's init, and a machine they end is restarted by its policy, unless it
/// was its stop signal (docker's rule, through the mark the release hook reads).
pub async fn kill(ctx: &Context, args: &KillRequest, report: Report<'_>) -> Result<()> {
    validate_entry_name(&args.name)?;
    let signal = signal_number(&args.signal)?;
    let sd = ctx.sd().await?;
    refuse_foreign(sd, &args.name).await?;
    let unit = format!("systemd-nspawn@{}.service", args.name);
    let registered = sd.machine_exists(&args.name).await?;
    // Between two runs of a restart policy only SIGKILL means something: it ends it.
    if !registered && !(signal == libc::SIGKILL && sd.unit_status(&unit).await?.restarting()) {
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
        let image = ctx
            .store
            .load_image(&args.name)?
            .map(|r| r.reference)
            .unwrap_or_default();
        crate::api::events::emit(
            "machine",
            "kill",
            &args.name,
            &[("image", &image), ("signal", "9")],
        );
        return Ok(());
    }
    let record = ctx.store.load_image(&args.name)?;
    let mode = record.as_ref().map(|r| r.mode);
    let policy = record.as_ref().map(|r| r.restart).unwrap_or_default();
    if policy != Restart::No && stop_signals(record.as_ref())?.contains(&signal) {
        ctx.store.mark_exit_on_next(&args.name)?;
    }
    // Only nspawn's machines have a place for marks.
    if record.is_some() {
        ctx.store.mark_signal(&args.name, signal)?;
    }
    if mode == Some(Mode::App) {
        let (leader, payload) = wait_for_payload(sd, &args.name).await?;
        let payload = payload.with_context(|| {
            format!(
                "{} has no program running under its init (leader PID {leader})",
                args.name
            )
        })?;
        send_signal(payload, signal).with_context(|| {
            format!("sending {} to PID {payload} of {}", args.signal, args.name)
        })?;
    } else {
        sd.kill_machine(&args.name, "leader", signal).await?;
    }
    let image = record.as_ref().map(|r| r.reference.as_str()).unwrap_or("");
    crate::api::events::emit(
        "machine",
        "kill",
        &args.name,
        &[("image", image), ("signal", &signal.to_string())],
    );
    Ok(())
}

/// None keeps the remembered value.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UpdateRequest {
    pub name: String,
    pub restart: Option<Restart>,
    /// Bytes, 0 for none.
    pub memory: Option<u64>,
    /// CPUs (0.5), 0 for none.
    pub cpus: Option<f64>,
    /// Processes, 0 for none.
    pub pids_limit: Option<u64>,
    /// The --health-* flags.
    pub health: crate::health::Overrides,
}

/// docker update, through the record and the drop-in: at the reload systemd applies a
/// running unit's new limits to its cgroup, and reads Restart= again. Returns whether it
/// was running.
pub async fn update(ctx: &Context, args: &UpdateRequest) -> Result<bool> {
    validate_entry_name(&args.name)?;
    if args.restart.is_none()
        && args.memory.is_none()
        && args.cpus.is_none()
        && args.pids_limit.is_none()
        && args.health.is_empty()
    {
        bail!(
            "nothing to update; give --memory, --cpus, --pids-limit, --restart or a --health flag"
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
    apply_limits(&mut record.limits, args.memory, args.cpus, args.pids_limit)?;
    record.limits.check(record.mode)?;
    if !args.health.is_empty() {
        let hc = args.health.apply(record.effective_healthcheck())?;
        record.healthcheck = Some(hc);
    }
    store.record_image(&record)?;
    let route =
        settings::namespace_route(sd, &args.name, record.mode, bridge::bridge_kind(&record))
            .await?;
    let app_argv = settings::app_argv(sd, &args.name, record.mode, &route).await?;
    let reload = settings::write_hooks(
        &args.name,
        &ctx.config,
        &route,
        app_argv.as_deref(),
        &settings::HookSpec {
            restart: record.restart,
            limits: &record.limits,
            remove_on_exit: record.remove_on_exit,
            tuning: &record.tuning,
        },
    )?;
    let running = state.active == "active";
    let changed = if record.restart.enabled_at_boot() {
        sd.enable_unit(&unit).await?
    } else if args.restart.is_some() && previous_policy.enabled_at_boot() {
        sd.disable_unit(&unit).await?
    } else {
        false
    };
    if reload || changed {
        sd.reload().await?;
    }
    // The runner reads the record once, at its start.
    if running && !args.health.is_empty() {
        crate::health::stop_runner(sd, &args.name).await?;
        crate::health::clear_status(&args.name);
        if record
            .effective_healthcheck()
            .is_some_and(|h| !h.disabled())
        {
            crate::health::start_runner(ctx, &args.name).await?;
        }
    }
    crate::api::events::emit(
        "machine",
        "update",
        &args.name,
        &[("image", &record.reference)],
    );
    Ok(running)
}

/// An app's stop signal (SIGTERM unless its image names one), or a booted init's halt and
/// poweroff requests.
fn stop_signals(record: Option<&ImageRecord>) -> Result<Vec<i32>> {
    match record {
        Some(r) if r.mode == Mode::App => Ok(vec![signal_number(
            r.effective_stop_signal().unwrap_or("SIGTERM"),
        )?]),
        _ => Ok(vec![libc::SIGRTMIN() + 3, libc::SIGRTMIN() + 4]),
    }
}

/// Waits, within reason, until nothing of the previous run is left: a unit shutting
/// down, a machine machined still lists, a release hook that would undo what the start
/// prepares. A machine that really runs, or starts elsewhere, is an error.
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

/// The leader, and for an app its program: the leader outlives it for a moment.
async fn machine_alive(sd: &Systemd, name: &str, mode: Option<Mode>) -> bool {
    let Ok(leader) = sd.machine_leader(name).await else {
        return false;
    };
    if !std::path::Path::new(&format!("/proc/{leader}")).exists() {
        return false;
    }
    mode != Some(Mode::App) || payload_pid(leader).is_some()
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

pub async fn signal_program(sd: &Systemd, name: &str, signal: i32) -> Result<()> {
    let (leader, payload) = wait_for_payload(sd, name).await?;
    let payload = payload.with_context(|| {
        format!("{name} has no program running under its init (leader PID {leader})")
    })?;
    send_signal(payload, signal)
        .with_context(|| format!("sending signal {signal} to PID {payload} of {name}"))
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

/// Parent PID and start time (field 22) of a /proc/PID/stat line, whose comm may hold
/// spaces and parentheses.
fn stat_ppid_and_start(stat: &str) -> Option<(u32, u64)> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // rest starts at field 3 (state), so ppid is index 1 and starttime index 19.
    Some((fields.get(1)?.parse().ok()?, fields.get(19)?.parse().ok()?))
}

/// kill(2) with a raw number: nix's Signal has no realtime signals, which images use.
fn send_signal(pid: i32, signal: i32) -> nix::Result<()> {
    nix::errno::Errno::result(unsafe { libc::kill(pid, signal) }).map(drop)
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
    let record = ctx.store.load_image(machine)?;
    let record = record.as_ref();
    let leader = sd.machine_leader(machine).await?;
    // Held before the leader is checked again, so the PID cannot be reused unnoticed.
    let leader_fd = nsenter::pidfd_open(nix::unistd::Pid::from_raw(leader as i32))
        .with_context(|| format!("opening the leader of {machine}"))?;
    if sd.machine_leader(machine).await? != leader {
        bail!("machine {machine} changed while the command was being started; try again");
    }
    let user = if user == "root" || user.is_empty() {
        None
    } else {
        Some(user)
    };
    let working_dir = workdir.or_else(|| record.and_then(|r| r.effective_working_dir()));
    let env = record
        .map(|r| volume::merge_env(&r.run.env, &r.env))
        .unwrap_or_default();
    let env = volume::merge_env(&env, extra_env);
    tokio::task::block_in_place(|| {
        nsenter::spawn(leader, &leader_fd, command, user, working_dir, &env, stdio)
    })
    .with_context(|| format!("running a command inside {machine}"))
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
}

const FOLLOW_TAIL: u32 = 10;

/// journalctl's arguments for docker logs: the console output is in the unit's journal,
/// earlier runs included; a booted machine also has a journal of its own.
pub fn journalctl_arguments(args: &LogsRequest) -> Vec<String> {
    // --all: a line with colours or a CR would read "[N B blob data]" otherwise.
    let mut argv = vec![
        "--no-pager".to_string(),
        "--quiet".to_string(),
        "--all".to_string(),
    ];
    let output = if args.timestamps { "short-iso" } else { "cat" };
    if args.inside {
        argv.push(format!("--machine={}", args.machine));
        argv.push(format!("--output={output}"));
    } else {
        argv.push(format!("--unit=systemd-nspawn@{}.service", args.machine));
        argv.push(format!("--output={output}"));
        if !args.all {
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
            ..base.clone()
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
        let full = LogsRequest {
            machine: "web".into(),
            follow: true,
            lines: Some(50),
            since: Some("10 min ago".into()),
            until: None,
            timestamps: true,
            all: true,
            inside: false,
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
    }
}
