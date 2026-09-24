//! The bridge networks: the default one and those made with `network create`, what is
//! on them, and the hooks the machine units run around a start and a stop.

use std::net::Ipv4Addr;
use std::time::Duration;

use anyhow::{bail, Result};

use crate::api::images::Removal;
use crate::api::{line, machines, require_root, Context, Event, Report};
use crate::backend::BackendChoice;
use crate::bridge::{self, NetSpec, PortMap, Subnet, DEFAULT_NETWORK, RESERVED_NETWORKS};
use crate::config::Config;
use crate::hostnet;
use crate::oci::Mode;
use crate::settings::Network;
use crate::store::{now_unix, ImageRecord, Store};
use crate::volmount;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeInfo {
    pub bridge: String,
    pub subnet: Subnet,
    pub gateway: Ipv4Addr,
    /// The name every machine resolves to the host.
    pub host_name: String,
}

/// One machine on a network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkEntry {
    pub name: String,
    pub address: Option<Ipv4Addr>,
    pub ports: Vec<PortMap>,
    pub running: bool,
}

/// A network as `network ls` shows it: what it is and the machines it has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkSummary {
    pub spec: NetSpec,
    pub machines: Vec<String>,
}

fn bridge_info(ctx: &Context) -> BridgeInfo {
    BridgeInfo {
        bridge: ctx.config.bridge.clone(),
        subnet: ctx.config.subnet,
        gateway: ctx.config.subnet.gateway(),
        host_name: bridge::HOST_NAME.to_string(),
    }
}

/// Every network: the default one first, then those made with `network create`.
pub fn all(store: &Store, config: &Config) -> Result<Vec<NetSpec>> {
    let mut out = vec![config.default_network()];
    out.extend(store.list_networks()?);
    Ok(out)
}

/// A network by name.
pub fn find(store: &Store, config: &Config, name: &str) -> Result<NetSpec> {
    if name == DEFAULT_NETWORK {
        return Ok(config.default_network());
    }
    store.load_network(name)?.ok_or_else(|| {
        anyhow::anyhow!("network {name} does not exist; nspawn network create {name} makes it")
    })
}

/// The network of a machine on a bridge.
pub fn of(store: &Store, config: &Config, record: &ImageRecord) -> Result<NetSpec> {
    find(store, config, bridge::network_of(record))
}

/// What `--network` names: one of the kinds, or a network made with `network create`,
/// which is of the bridge kind.
pub fn choice(text: &str) -> Result<(Network, Option<String>)> {
    Ok(match text {
        DEFAULT_NETWORK => (Network::Bridge, None),
        "veth" => (Network::Veth, None),
        "host" => (Network::Host, None),
        name => {
            validate_network_name(name)?;
            (Network::Bridge, Some(name.to_string()))
        }
    })
}

/// Names of user-defined networks: letters, digits, '_' and '-', not one `--network`
/// gives a meaning of its own.
pub fn validate_network_name(name: &str) -> Result<()> {
    if RESERVED_NETWORKS.contains(&name) {
        bail!("{name} is not a name a network can take: --network gives it a meaning of its own");
    }
    if name.is_empty()
        || name.len() > 64
        || name.starts_with('-')
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        bail!("network name {name:?}: 1 to 64 letters, digits, '_' and '-', not starting with '-'");
    }
    Ok(())
}

/// Brings up every network with its NAT and firewall exceptions (start does this for
/// its machine's network on its own; here for boot-time setup and troubleshooting).
/// Returns the default network and the names of all.
pub async fn up(ctx: &Context, report: Report<'_>) -> Result<(BridgeInfo, Vec<String>)> {
    let sd = ctx.sd().await?;
    let store = &ctx.store;
    let _lock = store.lock_for(Duration::from_secs(60)).await?;
    let all = all(store, &ctx.config)?;
    for net in &all {
        bridge::up(net, &all, sd, report).await?;
    }
    bridge::sync_ports(store, sd).await?;
    Ok((bridge_info(ctx), all.into_iter().map(|n| n.name).collect()))
}

/// The machines of one network, with their addresses and published ports.
async fn entries(ctx: &Context, network: &str) -> Result<Vec<NetworkEntry>> {
    let sd = ctx.sd().await?;
    let mut entries = Vec::new();
    for r in ctx.store.list_images()? {
        if r.network != Network::Bridge || bridge::network_of(&r) != network {
            continue;
        }
        entries.push(NetworkEntry {
            running: sd.machine_exists(&r.name).await?,
            name: r.name,
            address: r.address,
            ports: r.ports,
        });
    }
    Ok(entries)
}

/// The default network and the machines on it (the listing of 1.1, kept for its
/// clients; `inspect` covers every network).
pub async fn list(ctx: &Context) -> Result<(BridgeInfo, Vec<NetworkEntry>)> {
    Ok((bridge_info(ctx), entries(ctx, DEFAULT_NETWORK).await?))
}

/// Every network with the machines it has, like docker network ls.
pub fn list_networks(ctx: &Context) -> Result<Vec<NetworkSummary>> {
    let records = ctx.store.list_images()?;
    Ok(all(&ctx.store, &ctx.config)?
        .into_iter()
        .map(|spec| NetworkSummary {
            machines: records
                .iter()
                .filter(|r| r.network == Network::Bridge && bridge::network_of(r) == spec.name)
                .map(|r| r.name.clone())
                .collect(),
            spec,
        })
        .collect())
}

/// One network and its machines, like docker network inspect.
pub async fn inspect(ctx: &Context, name: &str) -> Result<(NetSpec, Vec<NetworkEntry>)> {
    let spec = find(&ctx.store, &ctx.config, name)?;
    Ok((spec, entries(ctx, name).await?))
}

/// docker network create: a bridge of its own with a subnet of its own (the next free
/// /24 of network_pool unless one is given), brought up at once.
pub async fn create(
    ctx: &Context,
    name: &str,
    subnet: Option<&str>,
    internal: bool,
    report: Report<'_>,
) -> Result<NetSpec> {
    require_root("network create")?;
    validate_network_name(name)?;
    let sd = ctx.sd().await?;
    let store = &ctx.store;
    let _lock = store.lock().await?;
    let mut all = all(store, &ctx.config)?;
    if all.iter().any(|n| n.name == name) {
        bail!("network {name} exists already");
    }
    let interface = bridge::network_interface(name);
    if let Some(other) = all.iter().find(|n| n.interface == interface) {
        bail!(
            "network {name} would use {interface}, which is {}'s; pick another name",
            other.name
        );
    }
    if std::path::Path::new("/sys/class/net")
        .join(&interface)
        .exists()
    {
        bail!("{interface}, the bridge network {name} would use, exists already; remove it or pick another name");
    }
    let own: Vec<String> = all.iter().map(|n| n.interface.clone()).collect();
    let mut taken: Vec<Subnet> = all.iter().map(|n| n.subnet).collect();
    taken.extend(bridge::host_subnets(&own));
    let subnet = match subnet {
        Some(text) => {
            let wanted: Subnet = text.parse()?;
            if let Some(n) = all.iter().find(|n| n.subnet.overlaps(&wanted)) {
                bail!("{wanted} overlaps {} of network {}", n.subnet, n.name);
            }
            if let Some(t) = taken.iter().find(|t| t.overlaps(&wanted)) {
                bail!("{wanted} overlaps {t}, which this host already uses");
            }
            wanted
        }
        None => bridge::free_subnet(ctx.config.network_pool, &taken)?,
    };
    let spec = NetSpec {
        name: name.to_string(),
        interface,
        subnet,
        internal,
        created: now_unix(),
    };
    store.record_network(&spec)?;
    all.push(spec.clone());
    if let Err(e) = bridge::up(&spec, &all, sd, report).await {
        // Nothing half made stays behind.
        all.pop();
        let _ = bridge::down(&spec, &all, sd).await;
        store.remove_network(name)?;
        return Err(e);
    }
    crate::api::events::emit(
        "network",
        "create",
        name,
        &[("subnet", &subnet.to_string())],
    );
    Ok(spec)
}

/// docker network rm: a network no machine names goes with its bridge and rules. One
/// that is in use, unknown or the default is refused and does not stop the others.
pub async fn remove(ctx: &Context, names: &[String], report: Report<'_>) -> Result<Removal> {
    require_root("network rm")?;
    let sd = ctx.sd().await?;
    let store = &ctx.store;
    let _lock = store.lock().await?;
    let records = store.list_images_strict()?;
    let mut removal = Removal::default();
    for name in names {
        match remove_one(ctx, sd, &records, name).await {
            Ok(()) => {
                line(report, format!("removed {name}"));
                crate::api::events::emit("network", "remove", name, &[]);
                removal.removed.push(name.clone());
            }
            Err(e) => removal.failed.push((name.clone(), format!("{e:#}"))),
        }
    }
    Ok(removal)
}

async fn remove_one(
    ctx: &Context,
    sd: &crate::systemd::Systemd,
    records: &[ImageRecord],
    name: &str,
) -> Result<()> {
    if name == DEFAULT_NETWORK {
        bail!("the default network cannot be removed");
    }
    let store = &ctx.store;
    let spec = find(store, &ctx.config, name)?;
    let users = users(records, name);
    if !users.is_empty() {
        bail!(
            "network {name} is in use by {}; remove them or start them on another network first",
            users.join(", ")
        );
    }
    let remaining: Vec<NetSpec> = all(store, &ctx.config)?
        .into_iter()
        .filter(|n| n.name != name)
        .collect();
    bridge::down(&spec, &remaining, sd).await?;
    store.remove_network(name)
}

/// The machines whose records name a network, sorted.
fn users(records: &[ImageRecord], network: &str) -> Vec<String> {
    let mut users: Vec<String> = records
        .iter()
        .filter(|r| r.network == Network::Bridge && bridge::network_of(r) == network)
        .map(|r| r.name.clone())
        .collect();
    users.sort();
    users
}

/// docker network prune: every user-defined network no machine names.
pub async fn prune(ctx: &Context, report: Report<'_>) -> Result<Vec<String>> {
    require_root("network prune")?;
    let unused: Vec<String> = {
        let records = ctx.store.list_images_strict()?;
        ctx.store
            .list_networks()?
            .into_iter()
            .filter(|n| users(&records, &n.name).is_empty())
            .map(|n| n.name)
            .collect()
    };
    let removal = remove(ctx, &unused, report).await?;
    if let Some(error) = removal.error() {
        bail!("{error}");
    }
    Ok(removal.removed)
}

/// ExecStartPre of systemd-nspawn@NAME.service: the same preparation `start` does, so
/// that machinectl, a boot-time enablement or a restart get their network too.
pub async fn prepare(ctx: &Context, name: &str) -> Result<()> {
    crate::reference::validate_entry_name(name)?;
    let sd = ctx.sd().await?;
    let store = &ctx.store;
    let _lock = store.lock_for(Duration::from_secs(60)).await?;
    let record = store.load_image(name)?;
    machines::prepare(sd, store, &ctx.config, name, record, &to_journal)
        .await
        .map(|_| ())
}

/// The hooks run under systemd: what the library remarks goes to the unit's journal.
fn to_journal(event: Event) {
    if let Event::Line(text) | Event::Note(text) = event {
        eprintln!("{text}");
    }
}

/// ExecStartPost: the machine is registered, its ports can be published. Volumes of an
/// mstack machine are attached first and without the store lock: the machine's boot is
/// waiting for them, and a long pull or create must not hold them up.
pub async fn publish(ctx: &Context, name: &str) -> Result<()> {
    let sd = ctx.sd().await?;
    let store = &ctx.store;
    let Some(record) = store.load_image(name)? else {
        return Ok(());
    };
    if record.backend == BackendChoice::Mstack {
        let leader = sd.machine_leader(name).await?;
        for volume in &record.volumes {
            let source = volume.host_path(&store.volumes_dir());
            volmount::mount_into_machine(leader, &source, &volume.target, volume.read_only)?;
        }
        // nspawn does not put the veth of a managed user namespace on the bridge.
        if record.network == Network::Bridge && record.mode == Mode::Boot {
            let net = of(store, &ctx.config, &record)?;
            bridge::adopt_managed_veth(&net.interface, leader)?;
        }
    }
    let _lock = store.lock_for(Duration::from_secs(60)).await?;
    match record.network {
        Network::Bridge => bridge::sync_ports(store, sd).await?,
        Network::Veth if hostnet::firewalld_running(sd).await => {
            hostnet::admit(sd, name, &to_journal).await?;
        }
        _ => {}
    }
    Ok(())
}

/// ExecStopPost: runs however the machine ended (stop, exit, crash, machinectl). No lock:
/// it runs inside the stop job that `images rm` and friends wait for while holding it.
pub async fn release(ctx: &Context, name: &str) -> Result<()> {
    crate::reference::validate_entry_name(name)?;
    let record = ctx.store.load_image(name)?;
    machines::release_machine(name, record.as_ref())?;
    // `kill` sent the machine its stop signal: a stop job queued while the unit is still
    // winding down is what keeps systemd from restarting it (it is not waited for, it
    // ends after this hook). Best effort: at shutdown nothing restarts anyway.
    if ctx.store.take_exit_on_next(name)? {
        if let Ok(sd) = ctx.sd().await {
            let unit = format!("systemd-nspawn@{name}.service");
            if let Err(e) = sd.queue_stop(&unit).await {
                eprintln!("warning: {e:#}; {name} may be restarted");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_choices_and_names() {
        assert_eq!(choice("bridge").unwrap(), (Network::Bridge, None));
        assert_eq!(choice("veth").unwrap(), (Network::Veth, None));
        assert_eq!(choice("host").unwrap(), (Network::Host, None));
        assert_eq!(
            choice("backend").unwrap(),
            (Network::Bridge, Some("backend".to_string()))
        );
        for bad in [
            "none",
            "default",
            "",
            "-x",
            "a b",
            "a/b",
            "a.b",
            &"x".repeat(65),
        ] {
            assert!(choice(bad).is_err(), "{bad:?}");
        }
        assert!(validate_network_name("bridge").is_err());
        assert!(validate_network_name("front_end-2").is_ok());
    }

    #[test]
    fn hosts_files_list_the_machines_of_their_own_network() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(&tmp.path().join("machines"), &tmp.path().join("state"));
        std::fs::create_dir_all(store.images_dir()).unwrap();
        let record = |name: &str, network: Option<&str>, addr: &str| -> ImageRecord {
            let mut r: ImageRecord = serde_json::from_str(&format!(
                r#"{{"name": "{name}", "reference": "r", "manifest_digest": "d", "layers": [], "backend": "overlay", "created": 0, "address": "{addr}"}}"#
            ))
            .unwrap();
            r.network_name = network.map(str::to_string);
            r
        };
        for r in [
            record("web", Some("front"), "10.99.1.2"),
            record("api", Some("front"), "10.99.1.3"),
            record("db", None, "10.99.0.2"),
        ] {
            store.record_image(&r).unwrap();
            std::fs::create_dir_all(store.machine_files_dir(&r.name)).unwrap();
        }
        let all = vec![
            NetSpec {
                name: DEFAULT_NETWORK.into(),
                interface: "nspawn0".into(),
                subnet: "10.99.0.0/24".parse().unwrap(),
                internal: false,
                created: 0,
            },
            NetSpec {
                name: "front".into(),
                interface: "nsbr-front".into(),
                subnet: "10.99.1.0/24".parse().unwrap(),
                internal: false,
                created: 1,
            },
        ];
        bridge::write_hosts_files(&store, &all).unwrap();
        let hosts = |name: &str| {
            std::fs::read_to_string(store.machine_files_dir(name).join("hosts")).unwrap()
        };
        assert!(hosts("web").contains("10.99.1.3 api\n"));
        assert!(hosts("web").contains("10.99.1.1 host.nspawn.internal\n"));
        assert!(!hosts("web").contains(" db\n"), "db is on another network");
        assert!(!hosts("db").contains(" web\n"));
        assert!(hosts("db").contains("10.99.0.1 host.nspawn.internal\n"));
        // What a 1.1 record reads as, and a round trip.
        let loaded = store.load_image("db").unwrap().unwrap();
        assert_eq!(loaded.network_name, None);
        let text = std::fs::read_to_string(store.images_dir().join("db.json")).unwrap();
        assert!(
            !text.contains("network_name"),
            "the default network writes what 1.1 wrote"
        );
        assert_eq!(
            store
                .load_image("web")
                .unwrap()
                .unwrap()
                .network_name
                .as_deref(),
            Some("front")
        );
        store.record_network(&all[1]).unwrap();
        assert_eq!(store.list_networks().unwrap(), vec![all[1].clone()]);
        assert_eq!(store.load_network("front").unwrap(), Some(all[1].clone()));
        store.remove_network("front").unwrap();
        assert!(store.list_networks().unwrap().is_empty());
    }
}
