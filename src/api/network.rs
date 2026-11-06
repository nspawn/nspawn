//! The bridge networks (the default one and those of `network create`) and the hooks the
//! machine units run around a start and a stop.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::time::Duration;

use anyhow::{bail, Context as _, Result};

use crate::api::create::validate_api_name;
use crate::api::images::Removal;
use crate::api::{line, machines, require_root, Context, Event, FailureKind, Report};
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

/// The network `name` names: the default one, the one the Docker API knows by that
/// name, or the one of that name. A network's own name is no other's API name (`create`
/// sees to it), so the network of the name is the one a lookup by API name would find
/// first as well.
pub fn find(store: &Store, config: &Config, name: &str) -> Result<NetSpec> {
    if name == DEFAULT_NETWORK {
        return Ok(config.default_network());
    }
    let own = validate_network_name(name).is_ok();
    if own {
        if let Some(spec) = store.load_network(name)? {
            return Ok(spec);
        }
    }
    if validate_api_name(name).is_ok() {
        if let Some(spec) = store
            .list_networks()?
            .into_iter()
            .find(|n| n.api_name.as_deref() == Some(name))
        {
            return Ok(spec);
        }
    }
    Err(FailureKind::NoSuchNetwork.error(if own {
        format!("network {name} does not exist; nspawn network create {name} makes it")
    } else {
        format!("network {name} does not exist")
    }))
}

/// The own name of the network `text` names among `networks` (the user-defined ones):
/// by the name the Docker API knows it by first, then by its own.
fn own_name<'a>(networks: &'a [NetSpec], text: &str) -> Option<&'a str> {
    networks
        .iter()
        .find(|n| n.api_name.as_deref() == Some(text))
        .or_else(|| networks.iter().find(|n| n.name == text))
        .map(|n| n.name.as_str())
}

/// `--network` values with each network named by its own name: one the Docker API
/// knows by another name becomes that. Kinds, `container:NAME` and names no network
/// has stay as they are, for the checks that follow; a name only the API takes and no
/// network has is refused here, as no network of that name can exist.
pub fn resolve_choices(store: &Store, texts: &[String]) -> Result<Vec<String>> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    let networks = store.list_networks()?;
    texts
        .iter()
        .map(|text| resolve_choice(&networks, text))
        .collect()
}

fn resolve_choice(networks: &[NetSpec], text: &str) -> Result<String> {
    if RESERVED_NETWORKS.contains(&text) || text.starts_with("container:") {
        return Ok(text.to_string());
    }
    if let Some(own) = own_name(networks, text) {
        return Ok(own.to_string());
    }
    if validate_network_name(text).is_err() && validate_api_name(text).is_ok() {
        return Err(FailureKind::NoSuchNetwork.error(format!("network {text} does not exist")));
    }
    Ok(text.to_string())
}

/// `NETWORK=VALUE` values (aliases, fixed addresses) with NETWORK by its own name, as
/// `resolve_choices` has it; values without a network stay as they are.
pub fn resolve_keyed(store: &Store, values: &[String]) -> Result<Vec<String>> {
    if !values.iter().any(|v| v.contains('=')) {
        return Ok(values.to_vec());
    }
    let networks = store.list_networks()?;
    Ok(values.iter().map(|value| keyed(&networks, value)).collect())
}

fn keyed(networks: &[NetSpec], value: &str) -> String {
    match value.split_once('=') {
        Some((network, rest)) => match own_name(networks, network) {
            Some(own) => format!("{own}={rest}"),
            None => value.to_string(),
        },
        None => value.to_string(),
    }
}

/// A start's options with the networks they name by their own names.
pub fn resolve_start(
    store: &Store,
    request: &machines::StartRequest,
) -> Result<machines::StartRequest> {
    let mut out = request.clone();
    out.network = resolve_choices(store, &request.network)?;
    out.aliases = resolve_keyed(store, &request.aliases)?;
    out.addresses = resolve_keyed(store, &request.addresses)?;
    Ok(out)
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
    /// --network container:NAME: that machine's network namespace.
    pub container: Option<String>,
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
        container: None,
    };
    if let Some(machine) = first.strip_prefix("container:") {
        crate::reference::validate_machine_name(machine)?;
        if !rest.is_empty() {
            bail!("--network {first} stands alone: the machine takes the network of {machine}, and joins no other");
        }
        choice.container = Some(machine.to_string());
        return Ok(choice);
    }
    match first.as_str() {
        DEFAULT_NETWORK => {}
        "veth" => choice.kind = Network::Veth,
        "host" => choice.kind = Network::Host,
        "none" => choice.none = true,
        name => {
            validate_network_reference(name)?;
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
            text if text.starts_with("container:") => {
                bail!("--network {text} stands alone: the machine takes that network, and joins no other")
            }
            DEFAULT_NETWORK => {}
            name => validate_network_reference(name)?,
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
    record.network_container.clone_from(&choice.container);
    if choice.container.is_some() {
        // Another machine's namespace: no network, address or names of its own.
        record.network = Network::Bridge;
        record.network_name = None;
        record.no_network = false;
        record.address = None;
        record.extra_networks.clear();
        record.aliases.clear();
        record.fixed_addresses.clear();
        return;
    }
    let had: BTreeMap<String, Ipv4Addr> = bridge::networks_of(record)
        .into_iter()
        .filter_map(|n| bridge::address_on(record, n).map(|a| (n.to_string(), a)))
        .collect();
    record.network = choice.kind;
    record.network_name.clone_from(&choice.name);
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
    // An address asked for on a network left is no longer one: back there, the machine
    // takes whatever it gets.
    let bridged = bridge::bridge_kind(record);
    record
        .fixed_addresses
        .retain(|network| bridged && joined.contains(network));
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
        return Err(FailureKind::Invalid.error(format!(
            "alias {alias:?}: letters, digits, '.', '-' and '_', not starting with '-' or '.'"
        )));
    }
    Ok(())
}

/// A network as `--network` names it: by its own name, or by the one the Docker API
/// knows it by, which `resolve_choices` turns into its own.
fn validate_network_reference(name: &str) -> Result<()> {
    if !RESERVED_NETWORKS.contains(&name) && validate_api_name(name).is_ok() {
        return Ok(());
    }
    validate_network_name(name)
}

/// Letters, digits, '_' and '-', and none of the names `--network` reserves.
pub fn validate_network_name(name: &str) -> Result<()> {
    if RESERVED_NETWORKS.contains(&name) {
        return Err(FailureKind::Invalid.error(format!(
            "{name} is not a name a network can take: --network gives it a meaning of its own"
        )));
    }
    if name.is_empty()
        || name.len() > 64
        || name.starts_with('-')
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(FailureKind::Invalid.error(format!(
            "network name {name:?}: 1 to 64 letters, digits, '_' and '-', not starting with '-'"
        )));
    }
    Ok(())
}

/// A network's event about a machine joining or leaving it: the network's metadata, and
/// which machine of the name.
fn emit_membership(action: &str, net: &NetSpec, record: &ImageRecord) {
    let mut attributes = network_attributes(net);
    attributes.push(("container", record.name.clone()));
    attributes.push(("container_created", record.created.to_string()));
    crate::api::events::emit("network", action, &net.name, &borrowed(&attributes));
}

/// What a machine on host networking, sharing another's, without a network or on veth
/// cannot do, in the words tools that read the errors expect where there are such.
fn joins_bridges(record: &ImageRecord, what: &str) -> Result<()> {
    if record.network == Network::Host {
        bail!("container cannot be disconnected from host network or connected to host network");
    }
    if record.network_container.is_some() {
        bail!("container sharing network namespace with another container or host cannot be {what} any other network");
    }
    if record.no_network {
        bail!("container cannot be {what} multiple networks with one of the networks in private (none) mode");
    }
    if record.network == Network::Veth {
        bail!(
            "{} has a veth of its own (--network veth), outside every bridge network",
            record.name
        );
    }
    if record.backend == BackendChoice::Mstack {
        bail!("{} runs under managed user namespaces (mstack), whose interfaces systemd makes; it cannot join a network while it runs", record.name);
    }
    Ok(())
}

/// network connect: the machine joins `network` with `aliases` and, when given, a fixed
/// address on it; a running one at once, with an interface of its own (its default
/// route stays where it is), and from its next start as any other network it joins.
pub async fn connect(
    ctx: &Context,
    network: &str,
    machine: &str,
    aliases: &[String],
    address: Option<Ipv4Addr>,
    report: Report<'_>,
) -> Result<()> {
    require_root("network connect")?;
    let (store, config, sd) = (&ctx.store, &ctx.config, ctx.sd().await?);
    let _lock = store.lock().await?;
    // Looked up under the lock: a network rm may have gone first.
    let net = find(store, config, network)
        .map_err(|_| FailureKind::NoSuchNetwork.error(format!("network {network} not found")))?;
    // The record names networks by their own names; messages by the one given.
    let own = net.name.as_str();
    let mut record = store
        .load_image(machine)?
        .ok_or_else(|| FailureKind::NoSuchMachine.error(format!("No such container: {machine}")))?;
    joins_bridges(&record, "connected to")?;
    if bridge::networks_of(&record).contains(&own) {
        return Err(FailureKind::Exists.error(format!(
            "endpoint with name {machine} already exists in network {network}"
        )));
    }
    for alias in aliases {
        validate_alias(alias)?;
    }
    if let Some(addr) = address {
        bridge::check_requested(store, &net, machine, addr)?;
    }
    let running = sd.machine_exists(machine).await?;
    let addr = match address {
        Some(addr) => addr,
        None if running => {
            let mut used: Vec<Ipv4Addr> = store
                .list_images_strict()?
                .iter()
                .filter_map(|r| bridge::address_on(r, own))
                .collect();
            // What builds' steps hold on it too.
            used.extend(bridge::build_addresses(store, own));
            net.subnet.allocate(&used)?
        }
        // A stopped machine gets one at its next start.
        None => Ipv4Addr::UNSPECIFIED,
    };
    record.extra_networks.push(crate::bridge::Attachment {
        network: own.to_string(),
        address: (!addr.is_unspecified()).then_some(addr),
    });
    if address.is_some() {
        record.fixed_addresses.insert(own.to_string());
    }
    if !aliases.is_empty() {
        let list = record.aliases.entry(own.to_string()).or_default();
        for alias in aliases {
            if !list.contains(alias) {
                list.push(alias.clone());
            }
        }
    }
    if running {
        let all = all(store, config)?;
        bridge::up(&net, &all, sd, report).await?;
        let leader = sd.machine_leader(machine).await?;
        let app = record.mode == Mode::App;
        // A booted machine's networkd has a .network file for each interface it started
        // with: a new one takes a name none of them matches.
        let first = if app { 1 } else { bound_network_files(machine) };
        let inside = bridge::connect_live(machine, leader, app, &net, addr, first)?;
        if !net.internal {
            // The record does not list the network yet: an interface it would not know
            // of does not stay.
            if let Err(e) = bridge::ensure_default_route(machine, leader, app, net.subnet.gateway())
            {
                let _ = bridge::disconnect_live(machine, &net, (!app).then_some(leader));
                return Err(e);
            }
        }
        line(report, format!("{machine}: {inside} on {network}, {addr}"));
    }
    store.record_image(&record)?;
    bridge::write_hosts_files(store, &all(store, config)?)?;
    emit_membership("connect", &net, &record);
    Ok(())
}

/// How many .network files the booted machine `name` bound at its start, one per
/// interface it started with (`VirtualEthernetExtra=` and host0).
fn bound_network_files(name: &str) -> usize {
    std::fs::read_to_string(
        std::path::Path::new(crate::settings::SETTINGS_DIR).join(format!("{name}.nspawn")),
    )
    .map_or(1, |text| {
        text.lines()
            .filter(|l| l.starts_with("BindReadOnly=") && l.contains(".network:"))
            .count()
            .max(1)
    })
}

/// network disconnect: the machine leaves `network`, at once when it runs. Its primary
/// network handed over to the next one it joins; the only one it cannot leave.
pub async fn disconnect(
    ctx: &Context,
    network: &str,
    machine: &str,
    force: bool,
    report: Report<'_>,
) -> Result<()> {
    require_root("network disconnect")?;
    let (store, config, sd) = (&ctx.store, &ctx.config, ctx.sd().await?);
    let _lock = store.lock().await?;
    let net = find(store, config, network)
        .map_err(|_| FailureKind::NoSuchNetwork.error(format!("network {network} not found")))?;
    let own = net.name.as_str();
    let Some(mut record) = store.load_image(machine)? else {
        if force {
            return Ok(());
        }
        return Err(FailureKind::NoSuchMachine.error(format!("No such container: {machine}")));
    };
    joins_bridges(&record, "disconnected from")?;
    let networks: Vec<String> = bridge::networks_of(&record)
        .into_iter()
        .map(str::to_string)
        .collect();
    if !networks.iter().any(|n| n == own) {
        if force {
            return Ok(());
        }
        bail!("container {machine} is not connected to network {network}");
    }
    if networks.len() == 1 {
        bail!("{network} is the only network of {machine}: nspawn network connect another one first, or start it with --network none");
    }
    let running = sd.machine_exists(machine).await?;
    let primary_left = networks[0] == own;
    if primary_left {
        // The next network becomes the primary one, with its address.
        let next = record.extra_networks.remove(0);
        record.network_name =
            (next.network != bridge::DEFAULT_NETWORK).then(|| next.network.clone());
        record.address = next.address;
        if !record.ports.is_empty() {
            let when = if running {
                "now"
            } else {
                "from its next start"
            };
            line(
                report,
                format!(
                    "note: the ports of {machine} go to its address on {} {when}",
                    next.network
                ),
            );
        }
    } else {
        record.extra_networks.retain(|a| a.network != own);
    }
    record.aliases.remove(own);
    record.fixed_addresses.remove(own);
    // A running machine leaves the network first: the record frees its address only once
    // no interface of it holds that address any more, or the next machine given it
    // would share it with this one.
    let left_live = if running {
        let booted = if record.mode == Mode::Boot {
            Some(sd.machine_leader(machine).await?)
        } else {
            None
        };
        bridge::disconnect_live(machine, &net, booted)
            .with_context(|| format!("taking {machine} off {network}; it stays on it"))?
    } else {
        false
    };
    store.record_image(&record)?;
    let all = all(store, config)?;
    bridge::write_hosts_files(store, &all)?;
    emit_membership("disconnect", &net, &record);
    if left_live {
        let live = async {
            line(report, format!("{machine}: left {network}"));
            // Its default route may have gone with the network: the next that leads out
            // takes it.
            let left: Vec<&NetSpec> = bridge::networks_of(&record)
                .iter()
                .filter_map(|n| all.iter().find(|s| s.name == *n))
                .collect();
            if let Some(next) = left.get(bridge::gateway_index(left.iter().copied())) {
                if !next.internal {
                    let leader = sd.machine_leader(machine).await?;
                    bridge::ensure_default_route(
                        machine,
                        leader,
                        record.mode == Mode::App,
                        next.subnet.gateway(),
                    )?;
                }
            }
            // The published ports follow the primary address, which may have changed;
            // the port proxy reads it anew.
            bridge::sync_ports(store, sd).await?;
            if primary_left && !record.ports.is_empty() {
                crate::portproxy::start(ctx, &record).await?;
            }
            Ok::<(), anyhow::Error>(())
        };
        if let Err(e) = live.await {
            line(
                report,
                format!("warning: {machine} left {network}, but: {e:#}"),
            );
        }
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
    // One network that cannot come up keeps neither the others down nor the published
    // ports out; it is said at the end.
    let mut failed = Vec::new();
    for net in &all {
        if let Err(e) = bridge::up(net, &all, sd, report).await {
            failed.push(format!("{}: {e:#}", net.name));
        }
    }
    bridge::sync_ports(store, sd).await?;
    if !failed.is_empty() {
        bail!("{}", failed.join("; "));
    }
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
    let entries = entries(ctx, &spec.name).await?;
    Ok((spec, entries))
}

/// What a create is refused for before anything is looked at: a name that is none. An
/// empty `name` is made of `api_name`.
pub fn check_create(name: &str, api_name: Option<&str>) -> Result<()> {
    if let Some(api_name) = api_name {
        if RESERVED_NETWORKS.contains(&api_name) {
            return validate_network_name(api_name);
        }
        validate_api_name(api_name)?;
    }
    if !name.is_empty() || api_name.is_none() {
        validate_network_name(name)?;
    }
    Ok(())
}

/// The network's own name: the one asked for, or without one the API name itself when
/// it is a network name, else one made of it as a machine's is.
fn network_name(name: &str, api_name: Option<&str>) -> String {
    match api_name.filter(|_| name.is_empty()) {
        Some(api_name) if validate_network_name(api_name).is_err() => {
            crate::api::create::derived_machine_name(api_name)
        }
        Some(api_name) => api_name.to_string(),
        None => name.to_string(),
    }
}

/// Refuses names another network has, its own or its API name: every name a lookup
/// takes leads to one network.
fn check_taken(all: &[NetSpec], name: &str, api_name: Option<&str>) -> Result<()> {
    for wanted in api_name.into_iter().chain([name]) {
        if all
            .iter()
            .any(|n| n.name == wanted || n.api_name.as_deref() == Some(wanted))
        {
            return Err(FailureKind::Exists.error(format!("network {wanted} exists already")));
        }
    }
    Ok(())
}

/// network create: a bridge and a subnet of its own (the next free /24 of
/// network_pool unless given), brought up at once. `api_name` is kept when it is not
/// the network's own name.
pub async fn create(
    ctx: &Context,
    name: &str,
    api_name: Option<&str>,
    subnet: Option<&str>,
    internal: bool,
    labels: BTreeMap<String, String>,
    report: Report<'_>,
) -> Result<NetSpec> {
    require_root("network create")?;
    check_create(name, api_name)?;
    let sd = ctx.sd().await?;
    let store = &ctx.store;
    // Names are chosen and checked under the lock: two creates of one API name make one
    // network.
    let _lock = store.lock().await?;
    let mut all = all(store, &ctx.config)?;
    let name = network_name(name, api_name);
    let name = name.as_str();
    let api_name = api_name.filter(|a| *a != name);
    check_taken(&all, name, api_name)?;
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
            if let Some(n) = all.iter().find(|n| n.subnet.overlaps(wanted)) {
                bail!("{wanted} overlaps {} of network {}", n.subnet, n.name);
            }
            if let Some(t) = taken.iter().find(|t| t.overlaps(wanted)) {
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
        api_name: api_name.map(str::to_string),
    };
    store.record_network(&spec)?;
    all.push(spec.clone());
    if let Err(e) = bridge::up(&spec, &all, sd, report).await {
        all.pop();
        let _ = bridge::down(&spec, &all, sd).await;
        store.remove_network(name)?;
        return Err(e);
    }
    let attributes = network_attributes(&spec);
    crate::api::events::emit("network", "create", name, &borrowed(&attributes));
    Ok(spec)
}

/// network rm, bridge and rules included. A network in use, unknown or the default
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
            Ok(spec) => {
                line(report, format!("removed {name}"));
                let attributes = network_attributes(&spec);
                crate::api::events::emit("network", "remove", &spec.name, &borrowed(&attributes));
                removal.removed.push(name.clone());
            }
            Err(e) => removal.refuse(name, e),
        }
    }
    Ok(removal)
}

async fn remove_one(
    ctx: &Context,
    sd: &crate::systemd::Systemd,
    records: &[ImageRecord],
    name: &str,
) -> Result<NetSpec> {
    if name == DEFAULT_NETWORK {
        bail!("the default network cannot be removed");
    }
    let store = &ctx.store;
    let spec = find(store, &ctx.config, name)?;
    let users = users(records, &spec.name);
    if !users.is_empty() {
        return Err(FailureKind::InUse.error(format!(
            "network {name} is in use by {}; remove them or start them on another network first",
            users.join(", ")
        )));
    }
    let remaining: Vec<NetSpec> = all(store, &ctx.config)?
        .into_iter()
        .filter(|n| n.name != spec.name)
        .collect();
    bridge::down(&spec, &remaining, sd).await?;
    store.remove_network(&spec.name)?;
    Ok(spec)
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

pub async fn prune(ctx: &Context, report: Report<'_>) -> Result<Removal> {
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
    remove(ctx, &unused, report).await
}

/// ExecStartPre: what `start` prepares, so that machinectl, boot and restarts get it too.
pub async fn prepare(ctx: &Context, name: &str) -> Result<()> {
    crate::reference::validate_entry_name(name)?;
    let sd = ctx.sd().await?;
    let store = &ctx.store;
    let _lock = store.lock_for(Duration::from_secs(60)).await?;
    let record = store.load_image(name)?;
    let before = record.clone();
    let publishes = record
        .as_ref()
        .is_some_and(|r| r.network == Network::Bridge && !r.ports.is_empty());
    let prepared = machines::prepare(
        sd,
        store,
        &ctx.config,
        name,
        record,
        machines::Pass::Hook,
        &to_journal,
    )
    .await;
    if let Err(e) = prepared {
        // As start: prepare writes the record before all of its checks, its first start
        // among them, which the next start would then not take as one (a volume
        // emptied after a seed that failed would not be filled again).
        let made = store.load_image(name).ok().flatten();
        let _ = machines::release_machine(name, made.as_ref().or(before.as_ref()));
        if let Some(before) = &before {
            let _ = store.record_image(before);
        }
        return Err(e);
    }
    // Checked under the lock: the claim is what the next unit's check sees of these
    // ports until this machine registers.
    if publishes {
        store.claim_ports(name)?;
    }
    Ok(())
}

/// A hook's notes go to the journal by its native protocol: under the unit, as journald
/// tells by the writer, but not as the machine's output, which logs reads from what came
/// on its standard output.
fn to_journal(event: Event) {
    if let Event::Line(text) | Event::Note(text) = event {
        let priority = if text.starts_with("warning:") {
            "4"
        } else {
            "5"
        };
        let sent = crate::journal::send(&[
            ("MESSAGE", text.as_str()),
            ("PRIORITY", priority),
            ("SYSLOG_IDENTIFIER", "nspawn"),
        ]);
        if sent.is_err() {
            eprintln!("{text}");
        }
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
        // A pidfd of the leader, then machined asked again: the process it holds is the
        // one machined names, not one that took a PID left free.
        let alive = crate::nsenter::pidfd_open(nix::unistd::Pid::from_raw(leader as i32))
            .map_err(|e| anyhow::anyhow!("the leader of {name}: {}", e.desc()))?;
        if sd.machine_leader(name).await? != leader {
            bail!("{name} restarted while its volumes were attached");
        }
        for volume in &record.volumes {
            let source = volume.host_path(&store.volumes_dir());
            volmount::mount_into_machine(
                leader,
                &alive,
                &source,
                &volume.target,
                volume.read_only,
            )?;
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
        Network::Bridge => {
            bridge::sync_ports(store, sd).await?;
            // Registered by now: machined says it holds them.
            store.release_ports_claim(name);
            crate::portproxy::start(ctx, &record).await?;
        }
        Network::Veth if hostnet::firewalld_running(sd).await => {
            hostnet::admit(sd, name, &to_journal).await?;
        }
        _ => {}
    }
    if record.effective_healthcheck().is_some_and(|h| h.active()) {
        crate::health::start_runner(ctx, name).await?;
    }
    Ok(())
}

/// ExecStopPost, however the machine ended. No lock: it runs inside the stop job that
/// `images rm` waits for while holding it.
pub async fn release(ctx: &Context, name: &str) -> Result<()> {
    crate::reference::validate_entry_name(name)?;
    // First: whatever fails below, the machine publishes nothing any more.
    ctx.store.release_ports_claim(name);
    let record = ctx.store.load_image(name)?;
    machines::release_machine(name, record.as_ref())?;
    crate::health::clear_status(name);
    crate::api::secrets::clear(name);
    // The interfaces admit bound to firewalld's trusted zone, whoever stopped the machine.
    if let Ok(sd) = ctx.sd().await {
        hostnet::release_machine(sd, name).await;
        if let Err(e) = crate::health::stop_runner_soon(sd, name).await {
            eprintln!("warning: {e:#}");
        }
        if let Err(e) = crate::portproxy::stop_soon(sd, name).await {
            eprintln!("warning: {e:#}");
        }
    }
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
        // Down: its stop hooks done too, which still read the record and its files. And
        // its start call returned: a program that ends sooner leaves the machine marked
        // as starting, which a removal refuses. A restart's stop is no end: its start
        // follows, which gives the unit another invocation.
        let state = sd.unit_status(&unit).await?;
        if !state.busy()
            && !state.shutting_down()
            && !ctx.store.is_starting(name)
            && !ctx.store.is_restarting(name)
        {
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
    let kept_since = ctx.store.starts_kept_since();
    let ended: Vec<String> = {
        let mut ended = Vec::new();
        for r in ctx.store.list_images()? {
            if !ran_to_be_removed(&r, kept_since)
                || ctx.store.is_starting(&r.name)
                || ctx.store.is_restarting(&r.name)
            {
                continue;
            }
            let unit = format!("systemd-nspawn@{}.service", r.name);
            let state = sd.unit_status(&unit).await?;
            if !state.busy() && !state.shutting_down() && !sd.machine_exists(&r.name).await? {
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

/// Whether a machine removed at its end has had a run that ended: one made with remove
/// waits for its first. A record of a release that kept no first start says nothing, and
/// is taken for one that ran.
fn ran_to_be_removed(record: &ImageRecord, kept_since: Option<u64>) -> bool {
    record.remove_on_exit
        && (record.started.is_some() || kept_since.is_none_or(|kept| record.created < kept))
}

/// What every event of a network says of it.
fn network_attributes(spec: &NetSpec) -> Vec<(&'static str, String)> {
    let mut attributes = vec![
        ("subnet", spec.subnet.to_string()),
        ("interface", spec.interface.clone()),
        ("internal", spec.internal.to_string()),
        ("created", spec.created.to_string()),
    ];
    if let Some(api_name) = &spec.api_name {
        attributes.push(("api_name", api_name.clone()));
    }
    attributes
}

fn borrowed<'a>(attributes: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
    attributes.iter().map(|(k, v)| (*k, v.as_str())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_machine_made_to_be_removed_waits_for_its_first_run() {
        let record = |json: serde_json::Value| -> ImageRecord {
            let mut base = serde_json::json!({
                "name": "web", "reference": "r", "manifest_digest": "d", "layers": [],
                "backend": "overlay", "created": 100, "origin": "create",
                "remove_on_exit": true,
            });
            for (k, v) in json.as_object().unwrap() {
                base[k] = v.clone();
            }
            serde_json::from_value(base).unwrap()
        };
        let made = record(serde_json::json!({}));
        assert!(!ran_to_be_removed(&made, Some(50)), "never started");
        assert!(ran_to_be_removed(
            &record(serde_json::json!({ "started": 120 })),
            Some(50)
        ));
        assert!(
            ran_to_be_removed(&made, Some(150)),
            "made before starts were kept: it may have run"
        );
        assert!(ran_to_be_removed(&made, None));
        let kept = record(serde_json::json!({ "remove_on_exit": false, "started": 120 }));
        assert!(!ran_to_be_removed(&kept, Some(50)));
    }

    #[test]
    fn network_choices_and_names() {
        let one = |text: &str| choices(&[text.to_string()]);
        assert_eq!(
            one("bridge").unwrap(),
            NetworkChoice {
                kind: Network::Bridge,
                name: None,
                extras: Vec::new(),
                none: false,
                container: None,
            }
        );
        assert_eq!(one("veth").unwrap().kind, Network::Veth);
        assert_eq!(one("host").unwrap().kind, Network::Host);
        assert!(one("none").unwrap().none);
        assert_eq!(one("backend").unwrap().name.as_deref(), Some("backend"));
        for bad in ["default", "", "-x", "a b", "a/b", ".x", &"x".repeat(1025)] {
            assert!(one(bad).is_err(), "{bad:?}");
        }
        // Names the Docker API gives a network, resolved before the choice is made.
        for api in ["a.b", &"x".repeat(65)] {
            assert_eq!(one(api).unwrap().name.as_deref(), Some(api));
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
            ["container:vpn", "front"],
            ["front", "container:vpn"],
        ] {
            assert!(choices(&[bad[0].into(), bad[1].into()]).is_err(), "{bad:?}");
        }
        let shared = choices(&["container:vpn".into()]).unwrap();
        assert_eq!(shared.container.as_deref(), Some("vpn"));
        assert!(shared.kind == Network::Bridge && !shared.none && shared.extras.is_empty());
        for bad in ["container:", "container:bad name", "container:a/b"] {
            assert!(choices(&[bad.into()]).is_err(), "{bad:?}");
        }
        let bad = validate_network_name("bridge").unwrap_err();
        assert_eq!(crate::api::kind_of(&bad), Some(FailureKind::Invalid));
        assert!(validate_network_name("front_end-2").is_ok());
        let bad = validate_alias("-www").unwrap_err();
        assert_eq!(crate::api::kind_of(&bad), Some(FailureKind::Invalid));
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
        r.fixed_addresses = ["front".to_string(), "back".to_string()].into();
        apply(&mut r, &choices(&["back".into(), "bridge".into()]).unwrap());
        // Asked for on front, which it left: back there it takes what it gets.
        assert_eq!(r.fixed_addresses.iter().collect::<Vec<_>>(), ["back"]);
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
        assert!(r.fixed_addresses.is_empty());
        assert!(!bridge::joins(&r, "bridge"));
        apply(&mut r, &choices(&["host".into()]).unwrap());
        assert_eq!(r.network, Network::Host);
        assert!(!r.no_network);
        // Another machine's namespace: nothing of the machine's own is left.
        r.aliases.insert("bridge".into(), vec!["www".into()]);
        apply(&mut r, &choices(&["container:vpn".into()]).unwrap());
        assert_eq!(r.network_container.as_deref(), Some("vpn"));
        assert!(r.network == Network::Bridge && !r.no_network && r.network_name.is_none());
        assert!(r.address.is_none() && r.extra_networks.is_empty() && r.aliases.is_empty());
        assert!(!bridge::bridge_kind(&r) && !bridge::joins(&r, "bridge"));
        assert_eq!(bridge::shares_network(&r), Some("vpn"));
        assert!(
            bridge::namespaced(&r),
            "the shared namespace is joined the way a bridge one is"
        );
        apply(&mut r, &choices(&["bridge".into()]).unwrap());
        assert!(r.network_container.is_none() && bridge::bridge_kind(&r));
        assert_eq!(bridge::shares_network(&r), None);
        assert!(bridge::namespaced(&r));
        apply(&mut r, &choices(&["none".into()]).unwrap());
        assert!(!bridge::namespaced(&r));
        apply(&mut r, &choices(&["host".into()]).unwrap());
        assert!(!bridge::namespaced(&r));
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
                api_name: None,
            },
            NetSpec {
                name: "front".into(),
                interface: "nsbr-front".into(),
                subnet: "10.99.1.0/24".parse().unwrap(),
                internal: false,
                created: 1,
                labels: BTreeMap::new(),
                api_name: None,
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
        for key in [
            "network_name",
            "extra_networks",
            "aliases",
            "no_network",
            "network_container",
        ] {
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

    fn spec(name: &str, api_name: Option<&str>) -> NetSpec {
        NetSpec {
            name: name.into(),
            interface: bridge::network_interface(name),
            subnet: "10.99.9.0/24".parse().unwrap(),
            internal: false,
            created: 1,
            labels: BTreeMap::new(),
            api_name: api_name.map(str::to_string),
        }
    }

    #[test]
    fn a_network_made_of_an_api_name_gets_a_name_of_its_own() {
        for (name, api_name) in [("front", None), ("", Some("ci.net")), ("mine", Some("x_y"))] {
            assert!(check_create(name, api_name).is_ok(), "{name} {api_name:?}");
        }
        for (name, api_name) in [
            ("", None),
            ("a.b", None),
            ("", Some("bridge")),
            ("", Some("default")),
            ("", Some("host")),
            ("", Some("_x")),
            ("", Some(".x")),
            ("", Some("a b")),
            ("bad name", Some("ci.net")),
        ] {
            let e = check_create(name, api_name).unwrap_err();
            assert_eq!(
                crate::api::kind_of(&e),
                Some(FailureKind::Invalid),
                "{name:?} {api_name:?}"
            );
        }
        assert_eq!(network_name("", Some("front")), "front");
        assert_eq!(network_name("mine", Some("ci.net")), "mine");
        let derived = network_name("", Some("ci.net"));
        assert!(
            derived.starts_with("ci-net-") && derived.len() == "ci-net-".len() + 8,
            "{derived}"
        );
        assert_eq!(derived, crate::api::create::derived_machine_name("ci.net"));
        let long = network_name("", Some(&"a".repeat(100)));
        assert_eq!(long.len(), 64);
        for made in [derived, long] {
            assert!(validate_network_name(&made).is_ok(), "{made}");
        }
    }

    #[test]
    fn every_name_of_a_network_leads_to_it_alone() {
        let all = vec![
            spec(DEFAULT_NETWORK, None),
            spec("front", None),
            spec("ci-net-1a2b3c4d", Some("ci.net")),
        ];
        check_taken(&all, "back", None).unwrap();
        check_taken(&all, "back", Some("back.net")).unwrap();
        for (name, api_name, said) in [
            ("front", None, "front"),
            ("bridge", None, "bridge"),
            ("ci-net-1a2b3c4d", None, "ci-net-1a2b3c4d"),
            ("other", Some("front"), "front"),
            ("other", Some("ci.net"), "ci.net"),
            ("ci-net-1a2b3c4d", Some("ci.net"), "ci.net"),
        ] {
            let e = check_taken(&all, name, api_name).unwrap_err();
            assert_eq!(crate::api::kind_of(&e), Some(FailureKind::Exists));
            assert_eq!(format!("{e:#}"), format!("network {said} exists already"));
        }
    }

    #[test]
    fn machines_name_networks_by_either_name() {
        let nets = vec![
            spec("front", None),
            spec("ci-net-1a2b3c4d", Some("ci.net")),
            spec("x", Some("front2")),
        ];
        let resolve = |text: &str| resolve_choice(&nets, text);
        assert_eq!(resolve("ci.net").unwrap(), "ci-net-1a2b3c4d");
        assert_eq!(resolve("front").unwrap(), "front");
        assert_eq!(resolve("front2").unwrap(), "x");
        for kept in [
            "bridge",
            "host",
            "none",
            "veth",
            "container:ci.net",
            "nope",
            "bad name",
        ] {
            assert_eq!(
                resolve(kept).unwrap(),
                kept,
                "left to the checks that follow"
            );
        }
        let e = resolve("no.such").unwrap_err();
        assert_eq!(crate::api::kind_of(&e), Some(FailureKind::NoSuchNetwork));
        assert_eq!(format!("{e:#}"), "network no.such does not exist");
        assert_eq!(keyed(&nets, "ci.net=www"), "ci-net-1a2b3c4d=www");
        assert_eq!(keyed(&nets, "front2=10.99.9.5"), "x=10.99.9.5");
        assert_eq!(keyed(&nets, "www"), "www");
        assert_eq!(keyed(&nets, "nope=www"), "nope=www");
        // What --network takes: its own names and the API's.
        let several = choices(&["ci.net".into(), "front".into()]).unwrap();
        assert_eq!(several.name.as_deref(), Some("ci.net"));
        assert!(choices(&[format!("a{}", "b".repeat(100))]).is_ok());
    }

    #[test]
    fn a_network_is_found_by_its_api_name_first() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(&tmp.path().join("machines"), &tmp.path().join("state"));
        store.init().unwrap();
        let config = Config::merge(crate::config::FileConfig::default(), None, None).unwrap();
        store
            .record_network(&spec("ci-net-1a2b3c4d", Some("ci.net")))
            .unwrap();
        store.record_network(&spec("front", None)).unwrap();
        for name in ["ci.net", "ci-net-1a2b3c4d"] {
            assert_eq!(find(&store, &config, name).unwrap().name, "ci-net-1a2b3c4d");
        }
        assert_eq!(find(&store, &config, "front").unwrap().name, "front");
        assert_eq!(
            find(&store, &config, "bridge").unwrap().name,
            DEFAULT_NETWORK
        );
        let e = find(&store, &config, "gone.net").unwrap_err();
        assert_eq!(crate::api::kind_of(&e), Some(FailureKind::NoSuchNetwork));
        assert_eq!(format!("{e:#}"), "network gone.net does not exist");
        let e = find(&store, &config, &"x".repeat(300)).unwrap_err();
        assert_eq!(crate::api::kind_of(&e), Some(FailureKind::NoSuchNetwork));
        let e = find(&store, &config, "gone").unwrap_err();
        assert_eq!(
            format!("{e:#}"),
            "network gone does not exist; nspawn network create gone makes it"
        );
        let resolved = resolve_choices(&store, &["ci.net".into(), "bridge".into()]).unwrap();
        assert_eq!(resolved, ["ci-net-1a2b3c4d", "bridge"]);
        assert_eq!(
            resolve_keyed(&store, &["ci.net=www".into(), "db".into()]).unwrap(),
            ["ci-net-1a2b3c4d=www", "db"]
        );
        let text = std::fs::read_to_string(store.networks_dir().join("front.json")).unwrap();
        assert!(!text.contains("api_name"), "none kept where there is none");
        let text =
            std::fs::read_to_string(store.networks_dir().join("ci-net-1a2b3c4d.json")).unwrap();
        assert!(text.contains(r#""api_name": "ci.net""#), "{text}");
        let attributes = network_attributes(&spec("ci-net-1a2b3c4d", Some("ci.net")));
        assert!(attributes.contains(&("api_name", "ci.net".to_string())));
        assert!(!network_attributes(&spec("front", None))
            .iter()
            .any(|(k, _)| *k == "api_name"));
    }
}
