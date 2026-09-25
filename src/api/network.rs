//! The bridge networks (the default one and those of `network create`) and the hooks the
//! machine units run around a start and a stop.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::time::Duration;

use anyhow::{bail, Result};

use crate::api::images::Removal;
use crate::api::{line, machines, require_root, Context, Event, Report};
use crate::backend::BackendChoice;
use crate::bridge::{
    self, Attachment, NetSpec, PortMap, Subnet, DEFAULT_NETWORK, RESERVED_NETWORKS,
};
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkEntry {
    pub name: String,
    /// On this network.
    pub address: Option<Ipv4Addr>,
    /// Its other names on this network.
    pub aliases: Vec<String>,
    pub ports: Vec<PortMap>,
    pub running: bool,
}

/// A network and the machines that name it.
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

/// Every network, the default one first.
pub fn all(store: &Store, config: &Config) -> Result<Vec<NetSpec>> {
    let mut out = vec![config.default_network()];
    out.extend(store.list_networks()?);
    Ok(out)
}

pub fn find(store: &Store, config: &Config, name: &str) -> Result<NetSpec> {
    if name == DEFAULT_NETWORK {
        return Ok(config.default_network());
    }
    store.load_network(name)?.ok_or_else(|| {
        anyhow::anyhow!("network {name} does not exist; nspawn network create {name} makes it")
    })
}

/// The networks a record joins, the primary one first.
pub fn nets_of(store: &Store, config: &Config, record: &ImageRecord) -> Result<Vec<NetSpec>> {
    bridge::networks_of(record)
        .iter()
        .map(|n| find(store, config, n))
        .collect()
}

/// What `--network` asks, once or several times: a kind (veth, host, none), or bridge
/// networks, the first one primary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkChoice {
    pub kind: Network,
    /// A user-defined primary network; None is the default one.
    pub name: Option<String>,
    /// Bridge networks joined besides the primary one.
    pub extras: Vec<String>,
    /// --network none.
    pub none: bool,
}

pub fn choices(texts: &[String]) -> Result<NetworkChoice> {
    let Some((first, rest)) = texts.split_first() else {
        bail!("--network needs a value");
    };
    let mut choice = NetworkChoice {
        kind: Network::Bridge,
        name: None,
        extras: Vec::new(),
        none: false,
    };
    match first.as_str() {
        DEFAULT_NETWORK => {}
        "veth" => choice.kind = Network::Veth,
        "host" => choice.kind = Network::Host,
        "none" => choice.none = true,
        name => {
            validate_network_name(name)?;
            choice.name = Some(name.to_string());
        }
    }
    for text in rest {
        if choice.kind != Network::Bridge || choice.none {
            bail!("--network {first} stands alone; a machine joins several networks of the bridge kind only");
        }
        match text.as_str() {
            "veth" | "host" | "none" => {
                bail!("--network {text} stands alone; a machine joins several networks of the bridge kind only")
            }
            DEFAULT_NETWORK => {}
            name => validate_network_name(name)?,
        }
        if choice.name.as_deref().unwrap_or(DEFAULT_NETWORK) == text || choice.extras.contains(text)
        {
            bail!("network {text} given twice");
        }
        choice.extras.push(text.clone());
    }
    Ok(choice)
}

/// Puts the choice on a record, keeping the addresses it has on networks it stays on and
/// the aliases on them.
pub fn apply(record: &mut ImageRecord, choice: &NetworkChoice) {
    let had: BTreeMap<String, Ipv4Addr> = bridge::networks_of(record)
        .into_iter()
        .filter_map(|n| bridge::address_on(record, n).map(|a| (n.to_string(), a)))
        .collect();
    record.network = choice.kind;
    record.network_name = choice.name.clone();
    record.no_network = choice.none;
    record.address = had.get(bridge::network_of(record)).copied();
    record.extra_networks = choice
        .extras
        .iter()
        .map(|n| Attachment {
            network: n.clone(),
            address: had.get(n).copied(),
        })
        .collect();
    let joined: Vec<String> = bridge::networks_of(record)
        .into_iter()
        .map(str::to_string)
        .collect();
    record.aliases.retain(|network, _| joined.contains(network));
}

/// --network-alias values: NAME on the primary network, or NETWORK=NAME on one of
/// `networks`; "none" alone clears them.
pub fn parse_aliases(
    values: &[String],
    networks: &[&str],
) -> Result<BTreeMap<String, Vec<String>>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    if values.len() == 1 && values[0] == "none" {
        return Ok(out);
    }
    let Some(primary) = networks.first() else {
        bail!("aliases are names on a bridge network; the machine joins none");
    };
    for value in values {
        let (network, alias) = match value.split_once('=') {
            Some((network, alias)) => (network, alias),
            None => (*primary, value.as_str()),
        };
        if !networks.contains(&network) {
            bail!("alias {value}: the machine does not join network {network}");
        }
        validate_alias(alias)?;
        let list = out.entry(network.to_string()).or_default();
        if !list.iter().any(|a| a == alias) {
            list.push(alias.to_string());
        }
    }
    Ok(out)
}

/// A name as /etc/hosts takes it.
pub fn validate_alias(alias: &str) -> Result<()> {
    if alias.is_empty()
        || alias.len() > 253
        || alias.starts_with('-')
        || alias.starts_with('.')
        || !alias
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    {
        bail!("alias {alias:?}: letters, digits, '.', '-' and '_', not starting with '-' or '.'");
    }
    Ok(())
}

/// Letters, digits, '_' and '-', and none of the names `--network` reserves.
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

/// Brings up every network, for boot-time setup and troubleshooting (start brings up its
/// machine's own). Returns the default network and the names of all.
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

async fn entries(ctx: &Context, network: &str) -> Result<Vec<NetworkEntry>> {
    let sd = ctx.sd().await?;
    let mut entries = Vec::new();
    for r in ctx.store.list_images()? {
        if !bridge::joins(&r, network) {
            continue;
        }
        entries.push(NetworkEntry {
            running: sd.machine_exists(&r.name).await?,
            address: bridge::address_on(&r, network),
            aliases: r.aliases.get(network).cloned().unwrap_or_default(),
            name: r.name,
            ports: r.ports,
        });
    }
    Ok(entries)
}

pub fn list_networks(ctx: &Context) -> Result<Vec<NetworkSummary>> {
    let records = ctx.store.list_images()?;
    Ok(all(&ctx.store, &ctx.config)?
        .into_iter()
        .map(|spec| NetworkSummary {
            machines: records
                .iter()
                .filter(|r| bridge::joins(r, &spec.name))
                .map(|r| r.name.clone())
                .collect(),
            spec,
        })
        .collect())
}

pub async fn inspect(ctx: &Context, name: &str) -> Result<(NetSpec, Vec<NetworkEntry>)> {
    let spec = find(&ctx.store, &ctx.config, name)?;
    Ok((spec, entries(ctx, name).await?))
}

/// docker network create: a bridge and a subnet of its own (the next free /24 of
/// network_pool unless given), brought up at once.
pub async fn create(
    ctx: &Context,
    name: &str,
    subnet: Option<&str>,
    internal: bool,
    labels: BTreeMap<String, String>,
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
        labels,
    };
    store.record_network(&spec)?;
    all.push(spec.clone());
    if let Err(e) = bridge::up(&spec, &all, sd, report).await {
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

/// docker network rm, bridge and rules included. A network in use, unknown or the default
/// is refused without stopping the others.
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

fn users(records: &[ImageRecord], network: &str) -> Vec<String> {
    let mut users: Vec<String> = records
        .iter()
        .filter(|r| bridge::joins(r, network))
        .map(|r| r.name.clone())
        .collect();
    users.sort();
    users
}

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

/// ExecStartPre: what `start` prepares, so that machinectl, boot and restarts get it too.
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

fn to_journal(event: Event) {
    if let Event::Line(text) | Event::Note(text) = event {
        eprintln!("{text}");
    }
}

/// ExecStartPost: the machine is registered, its ports can be published. An mstack
/// machine's volumes go first and without the store lock: its boot waits for them, and a
/// long pull holding the lock must not hold them up.
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
        // nspawn does not put the veths of a managed user namespace on the bridges.
        if bridge::bridge_kind(&record) && record.mode == Mode::Boot {
            for (i, net) in nets_of(store, &ctx.config, &record)?.iter().enumerate() {
                bridge::adopt_managed_veth(&net.interface, leader, &format!("host{i}"))?;
            }
        }
    } else if bridge::bridge_kind(&record) && record.mode == Mode::Boot {
        // The veths of VirtualEthernetExtra= come up unattached.
        for (i, net) in nets_of(store, &ctx.config, &record)?
            .iter()
            .enumerate()
            .skip(1)
        {
            bridge::attach_to_bridge(&bridge::host_end_name_at(name, i), &net.interface)?;
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
    if record
        .effective_healthcheck()
        .is_some_and(|h| !h.disabled())
    {
        crate::health::start_runner(ctx, name).await?;
    }
    Ok(())
}

/// ExecStopPost, however the machine ended. No lock: it runs inside the stop job that
/// `images rm` waits for while holding it.
pub async fn release(ctx: &Context, name: &str) -> Result<()> {
    crate::reference::validate_entry_name(name)?;
    let record = ctx.store.load_image(name)?;
    machines::release_machine(name, record.as_ref())?;
    crate::health::clear_status(name);
    crate::api::secrets::clear(name);
    // `kill` sent the stop signal: a stop job queued while the unit winds down keeps
    // systemd from restarting it. Not waited for: it ends after this hook.
    if ctx.store.take_exit_on_next(name)? {
        if let Ok(sd) = ctx.sd().await {
            let unit = format!("systemd-nspawn@{name}.service");
            if let Err(e) = sd.queue_stop(&unit).await {
                eprintln!("warning: {e:#}; {name} may be restarted");
            }
        }
    }
    // run --rm: not from inside the unit's own stop; a transient unit does it once this
    // one is down. At shutdown that is refused, and the service sweeps it up later.
    if record.as_ref().is_some_and(|r| r.remove_on_exit) {
        if let Err(e) = remove_later(ctx, name).await {
            eprintln!("warning: {e:#}; nspawn removes {name} when its service next starts");
        }
    }
    Ok(())
}

async fn remove_later(ctx: &Context, name: &str) -> Result<()> {
    let sd = ctx.sd().await?;
    let invocation = std::env::var("INVOCATION_ID").unwrap_or_default();
    let mut argv = crate::settings::hook_argv(&ctx.config)?;
    argv.extend([
        "remove-after-exit".to_string(),
        name.to_string(),
        invocation.clone(),
    ]);
    let unit = format!(
        "nspawn-rm-{name}-{}.service",
        invocation.get(..8).unwrap_or("0")
    );
    sd.start_transient(
        &unit,
        crate::api::run::transient_service(
            &format!("Remove machine {name} once it stopped"),
            &argv,
        ),
        "fail",
    )
    .await
}

/// run --rm, from the transient unit: waits for the machine's unit to be down after run
/// `invocation`, then removes it, unless it was started again or kept meanwhile.
pub async fn remove_after_exit(ctx: &Context, name: &str, invocation: &str) -> Result<()> {
    crate::reference::validate_entry_name(name)?;
    let sd = ctx.sd().await?;
    let unit = format!("systemd-nspawn@{name}.service");
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        let state = sd.unit_status(&unit).await?;
        if !state.busy() {
            break;
        }
        if !invocation.is_empty() && sd.invocation_id(&unit).await? != invocation {
            return Ok(());
        }
        if std::time::Instant::now() > deadline {
            bail!("{unit} did not stop within two minutes; {name} is left");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    if !ctx
        .store
        .load_image(name)?
        .is_some_and(|r| r.remove_on_exit)
    {
        return Ok(());
    }
    sd.reset_failed(&unit).await?;
    let removal =
        crate::api::images::remove_machines(ctx, &[name.to_string()], false, &to_journal).await?;
    if let Some(error) = removal.error() {
        bail!("{error}");
    }
    Ok(())
}

/// Removes the run --rm machines that ended while nothing could remove them (at
/// shutdown).
pub async fn remove_ended(ctx: &Context) -> Result<()> {
    let sd = ctx.sd().await?;
    let ended: Vec<String> = {
        let mut ended = Vec::new();
        for r in ctx.store.list_images()? {
            if !r.remove_on_exit || ctx.store.is_starting(&r.name) {
                continue;
            }
            let unit = format!("systemd-nspawn@{}.service", r.name);
            if !sd.unit_status(&unit).await?.busy() && !sd.machine_exists(&r.name).await? {
                ended.push(r.name);
            }
        }
        ended
    };
    if ended.is_empty() {
        return Ok(());
    }
    for name in &ended {
        let _ = sd
            .reset_failed(&format!("systemd-nspawn@{name}.service"))
            .await;
    }
    crate::api::images::remove_machines(ctx, &ended, false, &|_| {}).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_choices_and_names() {
        let one = |text: &str| choices(&[text.to_string()]);
        assert_eq!(
            one("bridge").unwrap(),
            NetworkChoice {
                kind: Network::Bridge,
                name: None,
                extras: Vec::new(),
                none: false
            }
        );
        assert_eq!(one("veth").unwrap().kind, Network::Veth);
        assert_eq!(one("host").unwrap().kind, Network::Host);
        assert!(one("none").unwrap().none);
        assert_eq!(one("backend").unwrap().name.as_deref(), Some("backend"));
        for bad in ["default", "", "-x", "a b", "a/b", "a.b", &"x".repeat(65)] {
            assert!(one(bad).is_err(), "{bad:?}");
        }
        let several = choices(&["front".into(), "bridge".into(), "back".into()]).unwrap();
        assert_eq!(several.name.as_deref(), Some("front"));
        assert_eq!(several.extras, ["bridge", "back"]);
        for bad in [
            ["host", "front"],
            ["front", "veth"],
            ["none", "front"],
            ["front", "front"],
            ["bridge", "bridge"],
        ] {
            assert!(choices(&[bad[0].into(), bad[1].into()]).is_err(), "{bad:?}");
        }
        assert!(validate_network_name("bridge").is_err());
        assert!(validate_network_name("front_end-2").is_ok());
        let aliases = parse_aliases(
            &["www".into(), "back=api".into(), "www".into()],
            &["front", "back"],
        )
        .unwrap();
        assert_eq!(aliases["front"], ["www"]);
        assert_eq!(aliases["back"], ["api"]);
        assert!(parse_aliases(&["other=x".into()], &["front"]).is_err());
        assert!(parse_aliases(&["bad name".into()], &["front"]).is_err());
        assert!(parse_aliases(&["none".into()], &["front"])
            .unwrap()
            .is_empty());
        assert!(parse_aliases(&["x".into()], &[]).is_err());
    }

    #[test]
    fn a_choice_keeps_the_addresses_on_the_networks_kept() {
        let mut r: ImageRecord = serde_json::from_str(
            r#"{"name": "web", "reference": "r", "manifest_digest": "d", "layers": [], "backend": "overlay", "created": 0, "address": "10.99.1.2"}"#,
        )
        .unwrap();
        r.network_name = Some("front".into());
        r.extra_networks.push(Attachment {
            network: "back".into(),
            address: Some("10.99.2.2".parse().unwrap()),
        });
        r.aliases.insert("back".into(), vec!["api".into()]);
        r.aliases.insert("front".into(), vec!["www".into()]);
        apply(&mut r, &choices(&["back".into(), "bridge".into()]).unwrap());
        assert_eq!(r.network_name.as_deref(), Some("back"));
        assert_eq!(r.address, Some("10.99.2.2".parse().unwrap()));
        assert_eq!(
            r.extra_networks,
            vec![Attachment {
                network: "bridge".into(),
                address: None
            }]
        );
        assert_eq!(r.aliases.keys().collect::<Vec<_>>(), ["back"]);
        assert_eq!(bridge::networks_of(&r), ["back", "bridge"]);
        assert!(bridge::joins(&r, "bridge") && !bridge::joins(&r, "front"));
        apply(&mut r, &choices(&["none".into()]).unwrap());
        assert!(r.no_network && r.extra_networks.is_empty() && r.aliases.is_empty());
        assert!(!bridge::joins(&r, "bridge"));
        apply(&mut r, &choices(&["host".into()]).unwrap());
        assert_eq!(r.network, Network::Host);
        assert!(!r.no_network);
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
        let mut web = record("web", Some("front"), "10.99.1.2");
        web.extra_networks.push(Attachment {
            network: DEFAULT_NETWORK.into(),
            address: Some("10.99.0.5".parse().unwrap()),
        });
        web.aliases
            .insert(DEFAULT_NETWORK.into(), vec!["www".into()]);
        web.aliases
            .insert("front".into(), vec!["www".into(), "api".into()]);
        for r in [
            web,
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
                labels: BTreeMap::new(),
            },
            NetSpec {
                name: "front".into(),
                interface: "nsbr-front".into(),
                subnet: "10.99.1.0/24".parse().unwrap(),
                internal: false,
                created: 1,
                labels: BTreeMap::new(),
            },
        ];
        bridge::write_hosts_files(&store, &all).unwrap();
        let hosts = |name: &str| {
            std::fs::read_to_string(store.machine_files_dir(name).join("hosts")).unwrap()
        };
        assert!(
            hosts("web").contains("10.99.1.2 web www\n"),
            "its alias, not the name of another member: {}",
            hosts("web")
        );
        assert!(hosts("web").contains("10.99.0.5 web www\n"));
        assert!(hosts("web").contains("10.99.1.3 api\n"));
        assert!(
            hosts("web").contains("10.99.0.2 db\n"),
            "db shares the default network with web"
        );
        assert!(
            hosts("web").contains("10.99.1.1 host.nspawn.internal\n"),
            "the gateway of the primary network"
        );
        assert!(
            hosts("api").contains("10.99.1.2 web www\n") && !hosts("api").contains("10.99.0."),
            "api sees web on front alone"
        );
        assert!(
            hosts("db").contains("10.99.0.5 web www\n") && !hosts("db").contains("10.99.1."),
            "db sees web on the default network alone"
        );
        assert!(hosts("db").contains("10.99.0.1 host.nspawn.internal\n"));
        // A machine of the default network has no network_name, in memory or on disk.
        let loaded = store.load_image("db").unwrap().unwrap();
        assert_eq!(loaded.network_name, None);
        let text = std::fs::read_to_string(store.images_dir().join("db.json")).unwrap();
        for key in ["network_name", "extra_networks", "aliases", "no_network"] {
            assert!(
                !text.contains(key),
                "a machine of one network writes no {key}"
            );
        }
        let web = store.load_image("web").unwrap().unwrap();
        assert_eq!(web.network_name.as_deref(), Some("front"));
        assert_eq!(web.extra_networks[0].network, DEFAULT_NETWORK);
        assert_eq!(web.aliases["front"], ["www", "api"]);
        store.record_network(&all[1]).unwrap();
        assert_eq!(store.list_networks().unwrap(), vec![all[1].clone()]);
        assert_eq!(store.load_network("front").unwrap(), Some(all[1].clone()));
        store.remove_network("front").unwrap();
        assert!(store.list_networks().unwrap().is_empty());
    }
}
