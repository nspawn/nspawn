//! The nspawn bridge: a docker0 style network for the machines, run by nspawn itself so
//! that it behaves the same whatever manages the host's network (systemd-networkd,
//! NetworkManager or nothing at all).
//!
//! The bridge carries the first address of the subnet. Every machine gets a fixed address
//! from the same subnet, handed to the systemd-networkd inside it through a .network file
//! mounted at /run/systemd/network/10-host0.network, plus a generated /etc/hosts with the
//! names of all the machines on the bridge. Outgoing traffic is masqueraded and published
//! ports are DNAT'ed in the nftables table `ip nspawn`; loopback access to published ports
//! works through route_localnet, as docker does without its userland proxy.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::process::{Command, Stdio};
use std::str::FromStr;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::hostnet;
use crate::settings::Network;
use crate::store::{ImageRecord, Store};
use crate::systemd::Systemd;

pub const TABLE: &str = "nspawn";
/// Name the machines can use for the host, like host.docker.internal.
pub const HOST_NAME: &str = "host.nspawn.internal";
const FALLBACK_DNS: [IpAddr; 2] = [
    IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
    IpAddr::V4(Ipv4Addr::new(9, 9, 9, 9)),
];

/// An IPv4 subnet in CIDR notation, for example 10.99.0.0/24.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Subnet {
    pub network: Ipv4Addr,
    pub prefix: u8,
}

impl Subnet {
    fn mask(&self) -> u32 {
        u32::MAX << (32 - self.prefix)
    }

    /// The bridge's own address: the first usable one.
    pub fn gateway(&self) -> Ipv4Addr {
        Ipv4Addr::from(u32::from(self.network) + 1)
    }

    pub fn contains(&self, addr: Ipv4Addr) -> bool {
        u32::from(addr) & self.mask() == u32::from(self.network)
    }

    /// An address a machine may keep: inside the subnet and not the network, the gateway
    /// or the broadcast address (the subnet may have changed since it was given).
    pub fn usable(&self, addr: Ipv4Addr) -> bool {
        self.contains(addr)
            && addr != self.network
            && addr != self.gateway()
            && u32::from(addr) != (u32::from(self.network) | !self.mask())
    }

    /// The lowest address not in `used`, leaving out the network, the gateway and the
    /// broadcast address.
    pub fn allocate(&self, used: &[Ipv4Addr]) -> Result<Ipv4Addr> {
        let first = u32::from(self.network) + 2;
        let last = (u32::from(self.network) | !self.mask()) - 1;
        (first..=last)
            .map(Ipv4Addr::from)
            .find(|a| !used.contains(a))
            .with_context(|| format!("no free address left in {self}"))
    }
}

impl FromStr for Subnet {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        let (addr, prefix) = text
            .split_once('/')
            .with_context(|| format!("{text}: expected ADDRESS/PREFIX"))?;
        let addr: Ipv4Addr = addr
            .parse()
            .with_context(|| format!("{text}: bad IPv4 address"))?;
        let prefix: u8 = prefix
            .parse()
            .with_context(|| format!("{text}: bad prefix length"))?;
        if !(8..=30).contains(&prefix) {
            bail!("{text}: the prefix length must be between 8 and 30");
        }
        let mask = u32::MAX << (32 - prefix);
        Ok(Subnet {
            network: Ipv4Addr::from(u32::from(addr) & mask),
            prefix,
        })
    }
}

impl fmt::Display for Subnet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.network, self.prefix)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Tcp,
    Udp,
}

impl Protocol {
    pub fn name(self) -> &'static str {
        match self {
            Protocol::Tcp => "tcp",
            Protocol::Udp => "udp",
        }
    }
}

/// A port published on the host, like docker's -p: HOST:CONTAINER[/udp].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortMap {
    pub host: u16,
    pub container: u16,
    pub protocol: Protocol,
}

impl FromStr for PortMap {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        let (ports, protocol) = match text.rsplit_once('/') {
            Some((ports, proto)) => (ports, proto),
            None => (text, "tcp"),
        };
        let protocol = match protocol {
            "tcp" => Protocol::Tcp,
            "udp" => Protocol::Udp,
            other => bail!("{text}: unknown protocol {other} (tcp or udp)"),
        };
        let (host, container) = match ports.split_once(':') {
            Some((host, container)) => (host, container),
            None => (ports, ports),
        };
        if host.contains(':') || container.contains(':') {
            bail!("{text}: binding to one host address is not supported; ports are published on every address of the host");
        }
        let port = |s: &str| {
            s.parse::<u16>()
                .ok()
                .filter(|p| *p > 0)
                .with_context(|| format!("{text}: bad port {s}"))
        };
        Ok(PortMap {
            host: port(host)?,
            container: port(container)?,
            protocol,
        })
    }
}

impl fmt::Display for PortMap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}->{}/{}",
            self.host,
            self.container,
            self.protocol.name()
        )
    }
}

/// Parses the -p/--publish values; "none" alone clears the list.
pub fn parse_publish(values: &[String]) -> Result<Vec<PortMap>> {
    if values.len() == 1 && values[0] == "none" {
        return Ok(Vec::new());
    }
    let mut out: Vec<PortMap> = Vec::new();
    for value in values {
        let p: PortMap = value.parse()?;
        if out
            .iter()
            .any(|o| o.host == p.host && o.protocol == p.protocol)
        {
            bail!("host port {}/{} given twice", p.host, p.protocol.name());
        }
        out.push(p);
    }
    Ok(out)
}

/// Creates the bridge with its address, forwarding, the NAT table and the firewalld
/// exception. Safe to repeat: everything is idempotent.
pub async fn up(config: &Config, sd: &Systemd) -> Result<()> {
    let name = config.bridge.as_str();
    let subnet = config.subnet;
    let address = format!("{}/{}", subnet.gateway(), subnet.prefix);
    let sys = Path::new("/sys/class/net").join(name);
    if !sys.exists() {
        run("ip", &["link", "add", name, "type", "bridge"])?;
        run("ip", &["link", "set", "dev", name, "alias", MANAGED_ALIAS])?;
    } else {
        // An interface with that name already exists: only a bridge nspawn made (or an
        // unmarked one carrying nothing but our address) may be taken over. Stripping
        // docker0 or virbr0 of their addresses is what this guards against.
        let is_bridge = sys.join("bridge").is_dir();
        let alias = fs::read_to_string(sys.join("ifalias")).unwrap_or_default();
        let addresses = ipv4_addresses(name)?;
        if !adoptable(is_bridge, alias.trim(), &addresses, &address) {
            bail!(
                "{name} exists and is not a bridge nspawn created (addresses: {}); pick another name with `bridge` in nspawn.toml",
                if addresses.is_empty() { "none".to_string() } else { addresses.join(", ") }
            );
        }
        if alias.trim() != MANAGED_ALIAS {
            run("ip", &["link", "set", "dev", name, "alias", MANAGED_ALIAS])?;
        }
    }
    run("ip", &["addr", "replace", &address, "dev", name])?;
    prune_addresses(name, &address)?;
    run("ip", &["link", "set", name, "up"])?;
    sysctl("net/ipv4/ip_forward", "1")?;
    sysctl(&format!("net/ipv4/conf/{name}/route_localnet"), "1")?;
    nft(&base_ruleset(name, subnet))?;
    allow_forwarding_past_iptables(name)?;
    if hostnet::firewalld_running(sd).await {
        hostnet::trust_interface(sd, name).await?;
    }
    Ok(())
}

/// Docker (in its default iptables mode) and ufw set the FORWARD policy to DROP, which
/// would silence every machine on the bridge. Docker reserves the DOCKER-USER chain for
/// rules like ours and never flushes it; without docker, a DROP policy gets the accept
/// rules at the top of FORWARD itself. Nothing happens on hosts without iptables.
fn allow_forwarding_past_iptables(bridge: &str) -> Result<()> {
    let chain = if iptables(&["-S", "DOCKER-USER"]) {
        "DOCKER-USER"
    } else if iptables(&["-S", "FORWARD"]) && forward_policy_is_drop() {
        "FORWARD"
    } else {
        return Ok(());
    };
    // Out of the bridge: anything. Into the bridge: only what was published (DNAT) or
    // belongs to a connection a machine opened, like docker does.
    let rules: [Vec<&str>; 2] = [
        vec!["-i", bridge, "-j", "ACCEPT"],
        vec![
            "-o",
            bridge,
            "-m",
            "conntrack",
            "--ctstate",
            "DNAT,RELATED,ESTABLISHED",
            "-j",
            "ACCEPT",
        ],
    ];
    for rule in &rules {
        if !iptables(&[&["-C", chain][..], &rule[..]].concat()) {
            run("iptables", &[&["-w", "-I", chain][..], &rule[..]].concat()).with_context(
                || format!("letting the bridge's traffic through the {chain} chain"),
            )?;
        }
    }
    Ok(())
}

/// Runs iptables quietly; false when it is missing or the command fails.
fn iptables(args: &[&str]) -> bool {
    Command::new("iptables")
        .arg("-w")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn forward_policy_is_drop() -> bool {
    Command::new("iptables")
        .args(["-w", "-S", "FORWARD"])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .next()
                .map(str::trim)
                == Some("-P FORWARD DROP")
        })
        .unwrap_or(false)
}

/// Priorities are numeric on purpose: the symbolic names (dstnat, srcnat, filter) are not
/// accepted in every hook by older nft (1.0.6 on Debian 12 rejects dstnat in output).
///
/// The nftables table: DNAT of published ports (from outside and from the host itself,
/// loopback included), masquerading of what leaves the bridge, hairpin masquerading when
/// a machine reaches a published port through the host's address (otherwise the reply
/// would bypass the NAT), and a guard so that route_localnet does not let a machine at
/// the host's loopback-only services.
pub fn base_ruleset(bridge: &str, subnet: Subnet) -> String {
    format!(
        "table ip {TABLE} {{
	map ports {{
		type inet_proto . inet_service : ipv4_addr . inet_service
	}}
	chain prerouting {{
		type nat hook prerouting priority -100; policy accept;
	}}
	chain output {{
		type nat hook output priority -100; policy accept;
	}}
	chain postrouting {{
		type nat hook postrouting priority 100; policy accept;
	}}
	chain input {{
		type filter hook input priority 0; policy accept;
	}}
}}
flush chain ip {TABLE} prerouting
flush chain ip {TABLE} output
flush chain ip {TABLE} postrouting
flush chain ip {TABLE} input
add rule ip {TABLE} prerouting fib daddr type local dnat ip to meta l4proto . th dport map @ports
add rule ip {TABLE} output fib daddr type local dnat ip to meta l4proto . th dport map @ports
add rule ip {TABLE} postrouting ip saddr {subnet} oifname != \"{bridge}\" masquerade
add rule ip {TABLE} postrouting ip saddr {subnet} oifname \"{bridge}\" ct status dnat masquerade
add rule ip {TABLE} postrouting ip saddr 127.0.0.0/8 oifname \"{bridge}\" masquerade
add rule ip {TABLE} input iifname \"{bridge}\" ct status & dnat == 0 ip saddr 127.0.0.0/8 drop
add rule ip {TABLE} input iifname \"{bridge}\" ct status & dnat == 0 ip daddr 127.0.0.0/8 drop
"
    )
}

/// The mark nspawn leaves on the bridge it creates (its ifalias).
pub const MANAGED_ALIAS: &str = "nspawn";

/// Whether an existing interface may serve as the bridge: one nspawn marked, or an
/// unmarked bridge that carries no address but ours.
pub fn adoptable(is_bridge: bool, alias: &str, addresses: &[String], wanted: &str) -> bool {
    is_bridge && (alias == MANAGED_ALIAS || addresses.iter().all(|a| a == wanted))
}

/// IPv4 addresses (with prefix) of an interface.
fn ipv4_addresses(interface: &str) -> Result<Vec<String>> {
    let output = Command::new("ip")
        .args(["-4", "-o", "addr", "show", "dev", interface])
        .output()
        .context("running ip")?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            (words.nth(2) == Some("inet"))
                .then(|| words.next().map(str::to_string))
                .flatten()
        })
        .collect())
}

/// Drops IPv4 addresses the bridge carries from an earlier subnet setting.
fn prune_addresses(bridge: &str, wanted: &str) -> Result<()> {
    for addr in ipv4_addresses(bridge)? {
        if addr != wanted {
            run("ip", &["addr", "del", &addr, "dev", bridge])?;
        }
    }
    Ok(())
}

/// Removes a machine's own published ports from the map, entry by entry, so that it can
/// run without the store lock next to another machine's publish.
pub fn withdraw_ports(record: &ImageRecord) -> Result<()> {
    if !table_exists() {
        return Ok(());
    }
    for p in &record.ports {
        // A missing element fails the whole transaction, so one script per element and
        // a failure means it was gone already.
        let _ = nft(&format!(
            "delete element ip {TABLE} ports {{ {} . {} }}\n",
            p.protocol.name(),
            p.host
        ));
    }
    Ok(())
}

fn run(program: &str, args: &[&str]) -> Result<()> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running {program} (is it installed?)"))?;
    if !output.status.success() {
        bail!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn nft(script: &str) -> Result<()> {
    let mut child = Command::new("nft")
        .args(["-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("running nft (is nftables installed?)")?;
    child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(script.as_bytes())
        .context("feeding nft")?;
    let output = child.wait_with_output().context("waiting for nft")?;
    if !output.status.success() {
        bail!(
            "nft rejected the rules: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn table_exists() -> bool {
    Command::new("nft")
        .args(["list", "table", "ip", TABLE])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn sysctl(key: &str, value: &str) -> Result<()> {
    let path = format!("/proc/sys/{key}");
    fs::write(&path, value).with_context(|| format!("writing {path}"))
}

/// Path of the network namespace nspawn prepares for an app machine.
pub fn netns_path(name: &str) -> String {
    format!("/run/netns/{}", netns_name(name))
}

fn netns_name(name: &str) -> String {
    format!("nspawn-{name}")
}

/// Host end of an app machine's veth pair: vb-<name>, hashed when the name is too long
/// for an interface name (15 characters).
pub fn host_end_name(name: &str) -> String {
    let plain = format!("vb-{name}");
    if plain.len() <= 15 {
        return plain;
    }
    let mut hash: u32 = 0x811c_9dc5;
    for byte in name.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    format!("vb-{hash:08x}")
}

/// Builds the network namespace of an app machine before it starts, so that its process
/// finds host0 configured from its first instruction: a veth pair with the host end on
/// the bridge, the machine's address, the bridge as default route. The pair disappears
/// with the namespace.
pub fn create_netns(config: &Config, name: &str, addr: Ipv4Addr) -> Result<()> {
    let ns = netns_name(name);
    delete_netns(name);
    let host_end = host_end_name(name);
    let _ = run("ip", &["link", "del", &host_end]);
    run("ip", &["netns", "add", &ns])?;
    let result = (|| {
        run(
            "ip",
            &[
                "link", "add", &host_end, "type", "veth", "peer", "name", "host0", "netns", &ns,
            ],
        )?;
        run(
            "ip",
            &["link", "set", &host_end, "master", &config.bridge, "up"],
        )?;
        run("ip", &["-n", &ns, "link", "set", "lo", "up"])?;
        let address = format!("{addr}/{}", config.subnet.prefix);
        run("ip", &["-n", &ns, "addr", "add", &address, "dev", "host0"])?;
        run("ip", &["-n", &ns, "link", "set", "host0", "up"])?;
        let gateway = config.subnet.gateway().to_string();
        run(
            "ip",
            &["-n", &ns, "route", "add", "default", "via", &gateway],
        )
    })();
    if result.is_err() {
        delete_netns(name);
    }
    result
}

/// Removes an app machine's network namespace and with it its veth pair. Best effort.
pub fn delete_netns(name: &str) {
    if Path::new(&netns_path(name)).exists() {
        let _ = run("ip", &["netns", "del", &netns_name(name)]);
    }
}

/// The resolv.conf for machines without systemd-resolved.
pub fn resolv_conf(dns: &[IpAddr]) -> String {
    let mut out = String::from("# Generated by nspawn; do not edit.\n");
    for server in dns {
        out.push_str(&format!("nameserver {server}\n"));
    }
    out
}

/// The .network file for host0 inside a machine: fixed address, the bridge as gateway.
pub fn network_file(addr: Ipv4Addr, subnet: Subnet, dns: &[IpAddr]) -> String {
    let mut out = format!(
        "# Generated by nspawn; do not edit.\n[Match]\nName=host0\n\n[Network]\nAddress={addr}/{}\nGateway={}\nLLMNR=yes\n",
        subnet.prefix,
        subnet.gateway()
    );
    for server in dns {
        out.push_str(&format!("DNS={server}\n"));
    }
    out
}

/// The /etc/hosts of one machine: itself, the host and every other machine on the bridge.
pub fn hosts_file(
    name: &str,
    addr: Ipv4Addr,
    gateway: Ipv4Addr,
    members: &BTreeMap<String, Ipv4Addr>,
) -> String {
    let mut out = format!(
        "# Generated by nspawn; do not edit.\n127.0.0.1 localhost\n::1 localhost ip6-localhost ip6-loopback\n{addr} {name}\n{gateway} {HOST_NAME}\n"
    );
    for (other, other_addr) in members {
        if other != name {
            out.push_str(&format!("{other_addr} {other}\n"));
        }
    }
    out
}

/// DNS servers for the machines: the configured ones, else the host's upstream servers,
/// else public resolvers (the host's loopback resolver is out of reach from a machine).
pub fn upstream_dns(configured: &[IpAddr]) -> Vec<IpAddr> {
    if !configured.is_empty() {
        return configured.to_vec();
    }
    for path in ["/run/systemd/resolve/resolv.conf", "/etc/resolv.conf"] {
        if let Ok(text) = fs::read_to_string(path) {
            let servers = nameservers(&text);
            if !servers.is_empty() {
                return servers;
            }
        }
    }
    eprintln!(
        "warning: no DNS server reachable from the machines found on the host; using {} and {} (set dns in nspawn.toml)",
        FALLBACK_DNS[0], FALLBACK_DNS[1]
    );
    FALLBACK_DNS.to_vec()
}

fn nameservers(resolv_conf: &str) -> Vec<IpAddr> {
    resolv_conf
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            (words.next() == Some("nameserver"))
                .then(|| words.next())
                .flatten()
        })
        .filter_map(|word| word.parse::<IpAddr>().ok())
        // The bridge carries no IPv6, so only IPv4 servers are reachable from a machine.
        .filter(|addr| addr.is_ipv4() && !addr.is_loopback())
        .collect()
}

/// Gives the machine its address if it has none, writes its .network file and refreshes
/// the hosts files of every machine on the bridge. The record is saved when it changes.
pub fn prepare_machine(
    store: &Store,
    config: &Config,
    record: &mut ImageRecord,
) -> Result<Ipv4Addr> {
    let addr = match record.address.filter(|a| config.subnet.usable(*a)) {
        Some(addr) => addr,
        None => {
            let used: Vec<Ipv4Addr> = store
                .list_images()?
                .iter()
                .filter(|r| r.name != record.name)
                .filter_map(|r| r.address)
                .collect();
            let addr = config.subnet.allocate(&used)?;
            record.address = Some(addr);
            store.record_image(record)?;
            addr
        }
    };
    let dir = store.machine_files_dir(&record.name);
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let dns = upstream_dns(&config.dns);
    for (file, text) in [
        ("host0.network", network_file(addr, config.subnet, &dns)),
        ("resolv.conf", resolv_conf(&dns)),
    ] {
        let path = dir.join(file);
        fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    }
    write_hosts_files(store, config)?;
    Ok(addr)
}

/// Rewrites the hosts file of every machine on the bridge in place, so that running
/// machines see the change through their bind mount.
pub fn write_hosts_files(store: &Store, config: &Config) -> Result<()> {
    let members: BTreeMap<String, Ipv4Addr> = store
        .list_images_strict()?
        .into_iter()
        .filter(|r| r.network == Network::Bridge)
        .filter_map(|r| r.address.map(|a| (r.name, a)))
        .collect();
    for (name, addr) in &members {
        let dir = store.machine_files_dir(name);
        if !dir.is_dir() {
            continue; // never started on the bridge yet; its start creates the files
        }
        let path = dir.join("hosts");
        fs::write(
            &path,
            hosts_file(name, *addr, config.subnet.gateway(), &members),
        )
        .with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(())
}

/// Host ports published by the running machines, other than `except`.
async fn ports_in_use(
    store: &Store,
    sd: &Systemd,
    except: &str,
) -> Result<BTreeMap<(Protocol, u16), (String, Ipv4Addr, u16)>> {
    let mut used = BTreeMap::new();
    for r in store.list_images_strict()? {
        if r.name == except || r.network != Network::Bridge || r.ports.is_empty() {
            continue;
        }
        let Some(addr) = r.address else { continue };
        if !sd.machine_exists(&r.name).await? {
            continue;
        }
        for p in &r.ports {
            used.insert((p.protocol, p.host), (r.name.clone(), addr, p.container));
        }
    }
    Ok(used)
}

/// Fails when a port the machine wants to publish is taken by another running machine
/// or by a service of the host itself (the DNAT would silently hijack it).
pub async fn check_port_conflicts(store: &Store, sd: &Systemd, record: &ImageRecord) -> Result<()> {
    let used = ports_in_use(store, sd, &record.name).await?;
    for p in &record.ports {
        if let Some((other, _, _)) = used.get(&(p.protocol, p.host)) {
            bail!(
                "host port {}/{} is already published by {other}",
                p.host,
                p.protocol.name()
            );
        }
        if !host_port_free(*p) {
            bail!(
                "host port {}/{} is in use by a service on the host",
                p.host,
                p.protocol.name()
            );
        }
    }
    Ok(())
}

/// Whether nothing on the host listens on the port: a bind test on every address.
fn host_port_free(port: PortMap) -> bool {
    match port.protocol {
        Protocol::Tcp => std::net::TcpListener::bind((Ipv4Addr::UNSPECIFIED, port.host)).is_ok(),
        Protocol::Udp => std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port.host)).is_ok(),
    }
}

/// Rebuilds the DNAT map from the machines that are running on the bridge.
pub async fn sync_ports(store: &Store, sd: &Systemd) -> Result<()> {
    sync_ports_except(store, sd, "").await
}

/// Like `sync_ports`, leaving out a machine that machined may still list while it is
/// closing.
pub async fn sync_ports_except(store: &Store, sd: &Systemd, except: &str) -> Result<()> {
    if !table_exists() {
        return Ok(());
    }
    let mut script = format!("flush map ip {TABLE} ports\n");
    for ((protocol, host), (_, addr, container)) in ports_in_use(store, sd, except).await? {
        script.push_str(&format!(
            "add element ip {TABLE} ports {{ {} . {host} : {addr} . {container} }}\n",
            protocol.name()
        ));
    }
    nft(&script)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subnet_parsing_and_allocation() {
        let s: Subnet = "10.99.0.7/24".parse().unwrap();
        assert_eq!(s.to_string(), "10.99.0.0/24");
        assert_eq!(s.gateway(), Ipv4Addr::new(10, 99, 0, 1));
        assert!(s.contains(Ipv4Addr::new(10, 99, 0, 200)));
        assert!(!s.contains(Ipv4Addr::new(10, 99, 1, 1)));
        assert!(s.usable(Ipv4Addr::new(10, 99, 0, 2)));
        assert!(!s.usable(Ipv4Addr::new(10, 99, 0, 1)), "the gateway");
        assert!(!s.usable(Ipv4Addr::new(10, 99, 0, 0)), "the network");
        assert!(!s.usable(Ipv4Addr::new(10, 99, 0, 255)), "the broadcast");
        assert!(!s.usable(Ipv4Addr::new(10, 98, 0, 2)), "outside");
        assert_eq!(s.allocate(&[]).unwrap(), Ipv4Addr::new(10, 99, 0, 2));
        let used = [Ipv4Addr::new(10, 99, 0, 2), Ipv4Addr::new(10, 99, 0, 4)];
        assert_eq!(s.allocate(&used).unwrap(), Ipv4Addr::new(10, 99, 0, 3));
        let tiny: Subnet = "192.168.7.0/30".parse().unwrap();
        assert_eq!(tiny.allocate(&[]).unwrap(), Ipv4Addr::new(192, 168, 7, 2));
        assert!(
            tiny.allocate(&[Ipv4Addr::new(192, 168, 7, 2)]).is_err(),
            "a /30 has room for exactly one machine"
        );
        assert!("10.0.0.0".parse::<Subnet>().is_err());
        assert!("10.0.0.0/31".parse::<Subnet>().is_err());
        assert!("10.0.0.0/7".parse::<Subnet>().is_err());
        assert!("x/24".parse::<Subnet>().is_err());
    }

    #[test]
    fn foreign_interfaces_are_never_adopted() {
        let ours = "10.99.0.1/24";
        assert!(
            adoptable(true, "nspawn", &["172.17.0.1/16".to_string()], ours),
            "marked: ours whatever it carries now"
        );
        assert!(adoptable(true, "", &[], ours), "an empty unmarked bridge");
        assert!(
            adoptable(true, "", &[ours.to_string()], ours),
            "a bridge from an older nspawn"
        );
        assert!(
            !adoptable(true, "", &["172.17.0.1/16".to_string()], ours),
            "docker0"
        );
        assert!(
            !adoptable(false, "nspawn", &[], ours),
            "not a bridge at all"
        );
    }

    #[test]
    fn port_maps() {
        let p: PortMap = "8080:80".parse().unwrap();
        assert_eq!((p.host, p.container, p.protocol), (8080, 80, Protocol::Tcp));
        assert_eq!(p.to_string(), "8080->80/tcp");
        let p: PortMap = "53:5353/udp".parse().unwrap();
        assert_eq!((p.host, p.container, p.protocol), (53, 5353, Protocol::Udp));
        let p: PortMap = "443".parse().unwrap();
        assert_eq!((p.host, p.container), (443, 443));
        assert!("0:80".parse::<PortMap>().is_err());
        assert!("80:x".parse::<PortMap>().is_err());
        assert!("80:80/sctp".parse::<PortMap>().is_err());
        assert!("127.0.0.1:80:80".parse::<PortMap>().is_err());

        assert!(parse_publish(&["none".to_string()]).unwrap().is_empty());
        assert_eq!(
            parse_publish(&["80:80".into(), "80:81/udp".into()])
                .unwrap()
                .len(),
            2
        );
        assert!(parse_publish(&["80:80".into(), "80:81".into()]).is_err());
    }

    #[test]
    fn generated_files() {
        let subnet: Subnet = "10.99.0.0/24".parse().unwrap();
        let dns = vec![
            "192.168.1.1".parse().unwrap(),
            "2001:db8::53".parse().unwrap(),
        ];
        let text = network_file(Ipv4Addr::new(10, 99, 0, 5), subnet, &dns);
        assert!(text.contains("[Match]\nName=host0\n"));
        assert!(text.contains("Address=10.99.0.5/24\nGateway=10.99.0.1\n"));
        assert!(text.ends_with("DNS=192.168.1.1\nDNS=2001:db8::53\n"));

        let members: BTreeMap<String, Ipv4Addr> = [
            ("web".to_string(), Ipv4Addr::new(10, 99, 0, 2)),
            ("db".to_string(), Ipv4Addr::new(10, 99, 0, 3)),
        ]
        .into_iter()
        .collect();
        let hosts = hosts_file(
            "web",
            Ipv4Addr::new(10, 99, 0, 2),
            subnet.gateway(),
            &members,
        );
        assert!(hosts.contains("127.0.0.1 localhost\n"));
        assert!(hosts.contains("10.99.0.2 web\n"));
        assert!(hosts.contains("10.99.0.1 host.nspawn.internal\n"));
        assert!(hosts.ends_with("10.99.0.3 db\n"));
        assert_eq!(hosts.matches("web").count(), 1);

        let rules = base_ruleset("nspawn0", subnet);
        assert!(rules.contains("ip saddr 10.99.0.0/24 oifname != \"nspawn0\" masquerade"));
        assert!(rules.contains("map @ports"));
        for symbolic in ["priority dstnat", "priority srcnat", "priority filter"] {
            assert!(
                !rules.contains(symbolic),
                "{symbolic}: older nft rejects symbolic priorities in some hooks"
            );
        }
        assert!(rules.contains("hook output priority -100;"));
        assert!(rules.contains("hook postrouting priority 100;"));
    }

    #[test]
    fn app_namespace_names() {
        assert_eq!(netns_path("web"), "/run/netns/nspawn-web");
        assert_eq!(host_end_name("web"), "vb-web");
        assert_eq!(host_end_name("twelve-chars"), "vb-twelve-chars");
        let long = host_end_name("a-rather-long-machine-name");
        assert_eq!(long.len(), 11);
        assert!(long.starts_with("vb-"));
        assert_ne!(long, host_end_name("a-rather-long-machine-nam3"));
        assert_eq!(
            resolv_conf(&["10.0.0.53".parse().unwrap(), "2001:db8::1".parse().unwrap()]),
            "# Generated by nspawn; do not edit.\nnameserver 10.0.0.53\nnameserver 2001:db8::1\n"
        );
    }

    #[test]
    fn upstream_servers_skip_loopback() {
        let text = "# comment\nnameserver 127.0.0.53\nnameserver 192.168.122.1\nsearch lan\nnameserver ::1\nnameserver fe80::1\n";
        assert_eq!(
            nameservers(text),
            vec!["192.168.122.1".parse::<IpAddr>().unwrap()],
            "loopback and IPv6 servers are left out"
        );
        assert!(nameservers("nameserver 127.0.0.1\n").is_empty());
        let configured = vec!["10.0.0.53".parse().unwrap()];
        assert_eq!(upstream_dns(&configured), configured);
    }
}
