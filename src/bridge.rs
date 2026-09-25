//! docker0-style bridges run by nspawn itself, so that they work whatever manages the
//! host's network. The default one comes from nspawn.toml, others from `network create`.
//!
//! A bridge carries the first address of its subnet; each machine gets a fixed address,
//! handed to its systemd-networkd through a mounted .network file, and a generated
//! /etc/hosts. The nftables table `ip nspawn` masquerades, DNATs published ports and keeps
//! the networks apart; loopback access to published ports goes through route_localnet,
//! as docker does without its userland proxy.

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

use crate::api::{note, Report};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Subnet {
    pub network: Ipv4Addr,
    pub prefix: u8,
}

impl Subnet {
    fn mask(&self) -> u32 {
        u32::MAX << (32 - self.prefix)
    }

    /// The bridge's own address.
    pub fn gateway(&self) -> Ipv4Addr {
        Ipv4Addr::from(u32::from(self.network) + 1)
    }

    pub fn contains(&self, addr: Ipv4Addr) -> bool {
        u32::from(addr) & self.mask() == u32::from(self.network)
    }

    /// An address a machine may keep: in the subnet, and not the network, gateway or
    /// broadcast address (the subnet may have changed since it was given).
    pub fn usable(&self, addr: Ipv4Addr) -> bool {
        self.contains(addr)
            && addr != self.network
            && addr != self.gateway()
            && u32::from(addr) != (u32::from(self.network) | !self.mask())
    }

    pub fn allocate(&self, used: &[Ipv4Addr]) -> Result<Ipv4Addr> {
        let first = u32::from(self.network) + 2;
        let last = (u32::from(self.network) | !self.mask()) - 1;
        (first..=last)
            .map(Ipv4Addr::from)
            .find(|a| !used.contains(a))
            .with_context(|| format!("no free address left in {self}"))
    }
}

impl Subnet {
    pub fn overlaps(&self, other: &Subnet) -> bool {
        self.contains(other.network) || other.contains(self.network)
    }
}

impl Serialize for Subnet {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Subnet {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        text.parse().map_err(serde::de::Error::custom)
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

pub const DEFAULT_NETWORK: &str = "bridge";

/// Names `--network` gives a meaning of its own.
pub const RESERVED_NETWORKS: [&str; 5] = [DEFAULT_NETWORK, "veth", "host", "none", "default"];

/// The default network of nspawn.toml or one of `network create`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetSpec {
    pub name: String,
    pub interface: String,
    pub subnet: Subnet,
    /// Its machines reach each other and the host, nothing beyond.
    #[serde(default)]
    pub internal: bool,
    /// Unix seconds; 0 for the default network.
    #[serde(default)]
    pub created: u64,
}

/// nsbr-NAME, hashed when too long for an interface name (15 characters).
pub fn network_interface(name: &str) -> String {
    let plain = format!("nsbr-{name}");
    if plain.len() <= 15 {
        plain
    } else {
        format!("nsbr-{}", short_hash(name))
    }
}

fn short_hash(name: &str) -> String {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in name.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    format!("{hash:08x}")
}

/// The first /24 of `pool` that overlaps nothing in `taken`.
pub fn free_subnet(pool: Subnet, taken: &[Subnet]) -> Result<Subnet> {
    let prefix = pool.prefix.max(24);
    let size = 1u64 << (32 - prefix);
    let first = u64::from(u32::from(pool.network));
    let end = first + (1u64 << (32 - pool.prefix));
    let mut at = first;
    while at < end {
        let candidate = Subnet {
            network: Ipv4Addr::from(at as u32),
            prefix,
        };
        if !taken.iter().any(|t| t.overlaps(&candidate)) {
            return Ok(candidate);
        }
        at += size;
    }
    bail!("no free /{prefix} left in {pool}; give one with --subnet or widen network_pool in nspawn.toml")
}

/// The host's IPv4 addresses and main routes, but those on nspawn's own bridges.
pub fn host_subnets(own: &[String]) -> Vec<Subnet> {
    let mut out = Vec::new();
    for args in [
        &["-4", "-o", "addr", "show"][..],
        &["-4", "route", "show"][..],
    ] {
        if let Ok(output) = Command::new("ip").args(args).output() {
            out.extend(subnets_in(&String::from_utf8_lossy(&output.stdout), own));
        }
    }
    out
}

fn subnets_in(text: &str, own: &[String]) -> Vec<Subnet> {
    let mut out = Vec::new();
    for line in text.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        let device = words
            .iter()
            .position(|w| *w == "dev")
            .and_then(|i| words.get(i + 1).copied())
            // addr show: "2: eth0    inet 192.168.1.5/24 ..."
            .or_else(|| words.get(1).map(|w| w.trim_end_matches(':')));
        if device.is_some_and(|d| own.iter().any(|o| o == d)) {
            continue;
        }
        let candidate = match words.iter().position(|w| *w == "inet") {
            Some(i) => words.get(i + 1).copied(),
            None => words.first().copied(),
        };
        let Some(candidate) = candidate.filter(|c| *c != "default") else {
            continue;
        };
        let text = if candidate.contains('/') {
            candidate.to_string()
        } else {
            format!("{candidate}/32")
        };
        // Prefixes the bridge refuses (below 8 or above 30) still take their room.
        if let Some((addr, prefix)) = text.split_once('/') {
            if let (Ok(addr), Ok(prefix)) = (addr.parse::<Ipv4Addr>(), prefix.parse::<u8>()) {
                if (1..=32).contains(&prefix) {
                    let mask = if prefix == 32 {
                        u32::MAX
                    } else {
                        u32::MAX << (32 - prefix)
                    };
                    out.push(Subnet {
                        network: Ipv4Addr::from(u32::from(addr) & mask),
                        prefix,
                    });
                }
            }
        }
    }
    out
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

/// docker's -p: HOST:CONTAINER[/udp].
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

/// "none" alone clears the list.
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

/// Creates a network's bridge, forwarding, NAT and firewall exceptions; idempotent. The
/// table is written for `all` networks, so that no other network loses its rules.
pub async fn up(net: &NetSpec, all: &[NetSpec], sd: &Systemd, report: Report<'_>) -> Result<()> {
    let name = net.interface.as_str();
    let subnet = net.subnet;
    let address = format!("{}/{}", subnet.gateway(), subnet.prefix);
    let sys = Path::new("/sys/class/net").join(name);
    if !sys.exists() {
        run("ip", &["link", "add", name, "type", "bridge"])?;
        run("ip", &["link", "set", "dev", name, "alias", MANAGED_ALIAS])?;
    } else {
        // Only a bridge nspawn made, or an unmarked one with nothing but our address, is
        // taken over: never strip docker0 or virbr0 of their addresses.
        let is_bridge = sys.join("bridge").is_dir();
        let alias = fs::read_to_string(sys.join("ifalias")).unwrap_or_default();
        let addresses = ipv4_addresses(name)?;
        if !adoptable(is_bridge, alias.trim(), &addresses, &address) {
            bail!(
                "{name} exists and is not a bridge nspawn created (addresses: {}); {}",
                if addresses.is_empty() {
                    "none".to_string()
                } else {
                    addresses.join(", ")
                },
                if net.name == DEFAULT_NETWORK {
                    "pick another name with `bridge` in nspawn.toml"
                } else {
                    "remove it or name the network otherwise"
                }
            );
        }
        if alias.trim() != MANAGED_ALIAS {
            run("ip", &["link", "set", "dev", name, "alias", MANAGED_ALIAS])?;
        }
    }
    run("ip", &["addr", "replace", &address, "dev", name])?;
    prune_addresses(name, &address)?;
    // IPv4 only: no IPv6 link-local address on the bridge. Hosts booted without IPv6
    // refuse both, which is fine.
    let _ = run("ip", &["link", "set", "dev", name, "addrgenmode", "none"]);
    let _ = run("ip", &["-6", "addr", "flush", "dev", name, "scope", "link"]);
    run("ip", &["link", "set", name, "up"])?;
    sysctl("net/ipv4/ip_forward", "1")?;
    sysctl(&format!("net/ipv4/conf/{name}/route_localnet"), "1")?;
    nft(&base_ruleset(all))?;
    if !net.internal {
        allow_forwarding_past_iptables(name, report)?;
    }
    if hostnet::firewalld_running(sd).await {
        hostnet::trust_interface(sd, name, report).await?;
    }
    Ok(())
}

/// Undoes `up` for a network being removed; the table is written for `remaining`.
pub async fn down(net: &NetSpec, remaining: &[NetSpec], sd: &Systemd) -> Result<()> {
    let name = net.interface.as_str();
    let sys = Path::new("/sys/class/net").join(name);
    if sys.exists() {
        let alias = fs::read_to_string(sys.join("ifalias")).unwrap_or_default();
        if alias.trim() == MANAGED_ALIAS {
            run("ip", &["link", "del", name])?;
        }
    }
    if table_exists() {
        nft(&base_ruleset(remaining))?;
    }
    for chain in ["DOCKER-USER", "FORWARD"] {
        for rule in forwarding_rules(name) {
            let check = [&["-C", chain][..], &rule[..]].concat();
            while iptables(&check) {
                run("iptables", &[&["-w", "-D", chain][..], &rule[..]].concat())
                    .with_context(|| format!("removing the exception for {name} from {chain}"))?;
            }
        }
    }
    if hostnet::firewalld_running(sd).await {
        hostnet::release(sd, &[name.to_string()]).await;
    }
    Ok(())
}

/// docker (iptables mode) and ufw set FORWARD to DROP. The accept rules go into
/// DOCKER-USER, which docker keeps for this and never flushes, or else to the top of
/// FORWARD.
fn allow_forwarding_past_iptables(bridge: &str, report: Report<'_>) -> Result<()> {
    let chain = if iptables(&["-S", "DOCKER-USER"]) {
        "DOCKER-USER"
    } else if iptables(&["-S", "FORWARD"]) && forward_policy_is_drop() {
        "FORWARD"
    } else {
        // Rules without the iptables command to edit them, or an nftables ruleset of its
        // own: the machines would lose the outside world silently, so say it.
        if let Some(who) = filtered_forwarding() {
            note(
                report,
                format!(
                    "warning: {who} drops forwarded traffic and nspawn could not add its exception ({}): machines on the bridge will not reach anything beyond it, and published ports will answer on this host alone",
                    if iptables(&["-V"]) {
                        "the rules are not in a table iptables can reach"
                    } else {
                        "iptables is not installed"
                    }
                ),
            );
        }
        return Ok(());
    };
    for rule in &forwarding_rules(bridge) {
        if !iptables(&[&["-C", chain][..], &rule[..]].concat()) {
            run("iptables", &[&["-w", "-I", chain][..], &rule[..]].concat()).with_context(
                || format!("letting the bridge's traffic through the {chain} chain"),
            )?;
        }
    }
    Ok(())
}

/// Out of the bridge anything; in, only published ports and replies, as docker does.
fn forwarding_rules(bridge: &str) -> [Vec<&str>; 2] {
    [
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
    ]
}

/// What drops forwarded traffic, as nft sees it (iptables-nft rules included).
fn filtered_forwarding() -> Option<&'static str> {
    let ruleset = Command::new("nft")
        .args(["list", "ruleset"])
        .output()
        .ok()?;
    filtered_forwarding_in(&String::from_utf8_lossy(&ruleset.stdout))
}

fn filtered_forwarding_in(ruleset: &str) -> Option<&'static str> {
    let mut lines = ruleset.lines().map(str::trim);
    if lines.clone().any(|l| l.starts_with("chain DOCKER-USER")) {
        return Some("docker");
    }
    lines
        .any(|l| l.contains("hook forward") && l.contains("policy drop"))
        .then_some("a firewall on this host")
}

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

/// The nftables table for every network: DNAT of published ports (loopback included),
/// masquerading out of each non-internal bridge, hairpin masquerading for a machine that
/// reaches a published port through the host's address, a guard so that route_localnet
/// does not expose the host's loopback services, and a forward chain that keeps networks
/// apart but for published ports. A drop here is final whatever other tables accept.
/// Priorities are numeric: older nft (1.0.6) rejects dstnat in the output hook.
pub fn base_ruleset(all: &[NetSpec]) -> String {
    let mut text = format!(
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
	chain forward {{
		type filter hook forward priority 0; policy accept;
	}}
}}
flush chain ip {TABLE} prerouting
flush chain ip {TABLE} output
flush chain ip {TABLE} postrouting
flush chain ip {TABLE} input
flush chain ip {TABLE} forward
add rule ip {TABLE} prerouting fib daddr type local dnat ip to meta l4proto . th dport map @ports
add rule ip {TABLE} output fib daddr type local dnat ip to meta l4proto . th dport map @ports
"
    );
    for net in all {
        let (bridge, subnet) = (&net.interface, net.subnet);
        if !net.internal {
            text.push_str(&format!(
                "add rule ip {TABLE} postrouting ip saddr {subnet} oifname != \"{bridge}\" masquerade\n"
            ));
        }
        text.push_str(&format!(
            "add rule ip {TABLE} postrouting ip saddr {subnet} oifname \"{bridge}\" ct status dnat masquerade
add rule ip {TABLE} postrouting ip saddr 127.0.0.0/8 oifname \"{bridge}\" masquerade
add rule ip {TABLE} input iifname \"{bridge}\" ct status & dnat == 0 ip saddr 127.0.0.0/8 drop
add rule ip {TABLE} input iifname \"{bridge}\" ct status & dnat == 0 ip daddr 127.0.0.0/8 drop
"
        ));
    }
    for net in all.iter().filter(|n| n.internal) {
        let bridge = &net.interface;
        text.push_str(&format!(
            "add rule ip {TABLE} forward iifname \"{bridge}\" oifname != \"{bridge}\" drop
add rule ip {TABLE} forward oifname \"{bridge}\" iifname != \"{bridge}\" drop
"
        ));
    }
    text.push_str(&format!(
        "add rule ip {TABLE} forward ct status dnat accept\n"
    ));
    for net in all {
        let others: Vec<String> = all
            .iter()
            .filter(|o| o.interface != net.interface)
            .map(|o| format!("\"{}\"", o.interface))
            .collect();
        if !others.is_empty() {
            text.push_str(&format!(
                "add rule ip {TABLE} forward iifname \"{}\" oifname {{ {} }} drop\n",
                net.interface,
                others.join(", ")
            ));
        }
    }
    text
}

/// The ifalias nspawn puts on its bridges.
pub const MANAGED_ALIAS: &str = "nspawn";

pub fn adoptable(is_bridge: bool, alias: &str, addresses: &[String], wanted: &str) -> bool {
    is_bridge && (alias == MANAGED_ALIAS || addresses.iter().all(|a| a == wanted))
}

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

fn prune_addresses(bridge: &str, wanted: &str) -> Result<()> {
    for addr in ipv4_addresses(bridge)? {
        if addr != wanted {
            run("ip", &["addr", "del", &addr, "dev", bridge])?;
        }
    }
    Ok(())
}

/// Removes a machine's ports from the map entry by entry, so that it can run without
/// the store lock.
pub fn withdraw_ports(record: &ImageRecord) -> Result<()> {
    if !table_exists() {
        return Ok(());
    }
    for p in &record.ports {
        // One script per element: a missing one fails the whole transaction.
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

pub fn netns_path(name: &str) -> String {
    format!("/run/netns/{}", netns_name(name))
}

fn netns_name(name: &str) -> String {
    format!("nspawn-{name}")
}

/// vb-NAME, hashed when too long for an interface name.
pub fn host_end_name(name: &str) -> String {
    let plain = format!("vb-{name}");
    if plain.len() <= 15 {
        return plain;
    }
    format!("vb-{}", short_hash(name))
}

/// An app machine's network namespace, ready before its program starts: a veth pair on
/// the bridge, the address and the default route.
pub fn create_netns(net: &NetSpec, name: &str, addr: Ipv4Addr) -> Result<()> {
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
            &["link", "set", &host_end, "master", &net.interface, "up"],
        )?;
        run("ip", &["-n", &ns, "link", "set", "lo", "up"])?;
        let address = format!("{addr}/{}", net.subnet.prefix);
        run("ip", &["-n", &ns, "addr", "add", &address, "dev", "host0"])?;
        // No link-local IPv6 address for machined to hand out under the machine's name.
        // A host booted without IPv6 refuses the setting and has none anyway.
        let _ = run(
            "ip",
            &["-n", &ns, "link", "set", "host0", "addrgenmode", "none"],
        );
        run("ip", &["-n", &ns, "link", "set", "host0", "up"])?;
        let gateway = net.subnet.gateway().to_string();
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

/// Under managed user namespaces (mstack) systemd-nsresourced makes the veth and Bridge=
/// is not applied, so the host end (the peer of host0, found through the machine's
/// sysfs) is put on the bridge here. Idempotent.
pub fn adopt_managed_veth(bridge: &str, leader: u32) -> Result<()> {
    let iflink = fs::read_to_string(format!("/proc/{leader}/root/sys/class/net/host0/iflink"))
        .context("reading the peer index of host0 inside the machine")?;
    let index: u32 = iflink
        .trim()
        .parse()
        .context("parsing the peer index of host0")?;
    let name = interface_by_index(Path::new("/sys/class/net"), index).with_context(|| {
        format!("no host interface with index {index} is the peer of the machine's host0")
    })?;
    if let Ok(master) = fs::read_link(format!("/sys/class/net/{name}/master")) {
        let master = master.file_name().and_then(|f| f.to_str()).unwrap_or("");
        if master == bridge {
            return Ok(());
        }
        bail!("{name}, the host end of the machine's veth, is already on {master}");
    }
    run("ip", &["link", "set", &name, "master", bridge, "up"])
}

pub fn interface_by_index(sys_net: &Path, index: u32) -> Option<String> {
    for entry in fs::read_dir(sys_net).ok()?.flatten() {
        let Ok(text) = fs::read_to_string(entry.path().join("ifindex")) else {
            continue;
        };
        if text.trim().parse::<u32>().ok() == Some(index) {
            return entry.file_name().to_str().map(|s| s.to_string());
        }
    }
    None
}

/// Best effort; the veth goes with the namespace.
pub fn delete_netns(name: &str) {
    if Path::new(&netns_path(name)).exists() {
        let _ = run("ip", &["netns", "del", &netns_name(name)]);
    }
}

pub fn resolv_conf(dns: &[IpAddr]) -> String {
    let mut out = String::from("# Generated by nspawn; do not edit.\n");
    for server in dns {
        out.push_str(&format!("nameserver {server}\n"));
    }
    out
}

/// host0's .network file. No IPv6 link-local address: machined would hand it out under
/// the machine's name, ahead of the IPv4 one.
pub fn network_file(addr: Ipv4Addr, subnet: Subnet, dns: &[IpAddr]) -> String {
    let mut out = format!(
        "# Generated by nspawn; do not edit.\n[Match]\nName=host0\n\n[Network]\nAddress={addr}/{}\nGateway={}\nLLMNR=yes\nLinkLocalAddressing=no\nIPv6AcceptRA=no\n",
        subnet.prefix,
        subnet.gateway()
    );
    for server in dns {
        out.push_str(&format!("DNS={server}\n"));
    }
    out
}

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

/// The configured servers, else the host's upstream ones (its loopback resolver is out
/// of the machines' reach), else public resolvers.
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
        .filter(|addr| addr.is_ipv4() && !addr.is_loopback())
        .collect()
}

/// Gives the machine an address on its network if it has none there, writes its files
/// and refreshes every machine's hosts file.
pub fn prepare_machine(
    store: &Store,
    config: &Config,
    net: &NetSpec,
    all: &[NetSpec],
    record: &mut ImageRecord,
) -> Result<Ipv4Addr> {
    let addr = match record.address.filter(|a| net.subnet.usable(*a)) {
        Some(addr) => addr,
        None => {
            // Strict: an address handed out twice is worse than a start refused over an
            // unreadable record.
            let used: Vec<Ipv4Addr> = store
                .list_images_strict()?
                .iter()
                .filter(|r| r.name != record.name)
                .filter_map(|r| r.address)
                .collect();
            let addr = net.subnet.allocate(&used)?;
            record.address = Some(addr);
            store.record_image(record)?;
            addr
        }
    };
    let dir = store.machine_files_dir(&record.name);
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let dns = upstream_dns(&config.dns);
    for (file, text) in [
        ("host0.network", network_file(addr, net.subnet, &dns)),
        ("resolv.conf", resolv_conf(&dns)),
    ] {
        let path = dir.join(file);
        fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    }
    write_hosts_files(store, all)?;
    Ok(addr)
}

pub fn network_of(record: &ImageRecord) -> &str {
    record.network_name.as_deref().unwrap_or(DEFAULT_NETWORK)
}

/// Rewrites every hosts file in place (running machines see it through the bind mount),
/// each with the machines of its own network.
pub fn write_hosts_files(store: &Store, all: &[NetSpec]) -> Result<()> {
    let mut networks: BTreeMap<String, BTreeMap<String, Ipv4Addr>> = BTreeMap::new();
    for r in store.list_images_strict()? {
        if r.network != Network::Bridge {
            continue;
        }
        if let Some(addr) = r.address {
            networks
                .entry(network_of(&r).to_string())
                .or_default()
                .insert(r.name, addr);
        }
    }
    for (network, members) in &networks {
        // A network removed under a stopped machine: its start says so.
        let Some(net) = all.iter().find(|n| &n.name == network) else {
            continue;
        };
        for (name, addr) in members {
            let dir = store.machine_files_dir(name);
            if !dir.is_dir() {
                continue; // never started on the bridge yet; its start creates the files
            }
            let path = dir.join("hosts");
            fs::write(
                &path,
                hosts_file(name, *addr, net.subnet.gateway(), members),
            )
            .with_context(|| format!("writing {}", path.display()))?;
        }
    }
    Ok(())
}

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
        if !holds_ports(store, sd, &r.name).await? {
            continue;
        }
        for p in &r.ports {
            used.insert((p.protocol, p.host), (r.name.clone(), addr, p.container));
        }
    }
    Ok(used)
}

/// Starting (not listed by machined yet), or registered and not closing (machined lists
/// a closing machine while ExecStopPost has already withdrawn its ports).
async fn holds_ports(store: &Store, sd: &Systemd, name: &str) -> Result<bool> {
    if store.is_starting(name) {
        return Ok(true);
    }
    if !sd.machine_exists(name).await? {
        return Ok(false);
    }
    let (_, active) = sd
        .unit_state(&format!("systemd-nspawn@{name}.service"))
        .await?;
    Ok(matches!(
        active.as_str(),
        "active" | "activating" | "reloading"
    ))
}

/// A port taken by another machine or by a host service (the DNAT would hijack it).
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

fn host_port_free(port: PortMap) -> bool {
    match port.protocol {
        Protocol::Tcp => std::net::TcpListener::bind((Ipv4Addr::UNSPECIFIED, port.host)).is_ok(),
        Protocol::Udp => std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port.host)).is_ok(),
    }
}

pub async fn sync_ports(store: &Store, sd: &Systemd) -> Result<()> {
    sync_ports_except(store, sd, "").await
}

/// Leaves out a machine machined may still list while it closes.
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
    fn what_filters_forwarding_is_read_from_the_ruleset() {
        let docker = "table ip filter {\n\tchain FORWARD {\n\t\ttype filter hook forward priority filter; policy drop;\n\t\tcounter jump DOCKER-USER\n\t}\n\tchain DOCKER-USER {\n\t\tcounter return\n\t}\n}\n";
        assert_eq!(filtered_forwarding_in(docker), Some("docker"));
        let ufw = "table ip filter {\n\tchain FORWARD {\n\t\ttype filter hook forward priority filter; policy drop;\n\t}\n}\n";
        assert_eq!(filtered_forwarding_in(ufw), Some("a firewall on this host"));
        // What nspawn and machined leave on a host that filters nothing.
        let ours = format!(
            "table ip {TABLE} {{\n\tchain prerouting {{\n\t\ttype nat hook prerouting priority -100; policy accept;\n\t}}\n}}\ntable ip io.systemd.nat {{\n\tchain fwd {{\n\t\ttype filter hook forward priority 0; policy accept;\n\t}}\n}}\n"
        );
        assert_eq!(filtered_forwarding_in(&ours), None);
        assert_eq!(filtered_forwarding_in(""), None);
    }

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
    fn interface_lookup_by_index() {
        let tmp = tempfile::tempdir().unwrap();
        for (name, index) in [("lo", 1), ("eth0", 2), ("ns-8947f178a7c6", 76)] {
            std::fs::create_dir(tmp.path().join(name)).unwrap();
            std::fs::write(tmp.path().join(name).join("ifindex"), format!("{index}\n")).unwrap();
        }
        std::fs::create_dir(tmp.path().join("bonding_masters")).unwrap();
        assert_eq!(
            interface_by_index(tmp.path(), 76).as_deref(),
            Some("ns-8947f178a7c6")
        );
        assert_eq!(interface_by_index(tmp.path(), 2).as_deref(), Some("eth0"));
        assert_eq!(interface_by_index(tmp.path(), 99), None);
        assert_eq!(interface_by_index(Path::new("/nonexistent"), 1), None);
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
        assert!(text.contains("\nLinkLocalAddressing=no\nIPv6AcceptRA=no\n"));
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

        let rules = base_ruleset(&[net("bridge", "nspawn0", "10.99.0.0/24", false)]);
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

    fn net(name: &str, interface: &str, subnet: &str, internal: bool) -> NetSpec {
        NetSpec {
            name: name.into(),
            interface: interface.into(),
            subnet: subnet.parse().unwrap(),
            internal,
            created: 0,
        }
    }

    #[test]
    fn networks_are_kept_apart_in_one_table() {
        let default = net("bridge", "nspawn0", "10.99.0.0/24", false);
        let web = net("web", "nsbr-web", "10.99.1.0/24", false);
        let db = net("db", "nsbr-db", "10.99.2.0/24", true);
        let one = base_ruleset(std::slice::from_ref(&default));
        assert!(one
            .contains("chain forward {\n\t\ttype filter hook forward priority 0; policy accept;"));
        assert!(one.contains("flush chain ip nspawn forward\n"));
        assert!(
            !one.contains("oifname {"),
            "one network has nothing to be kept from"
        );
        let all = base_ruleset(&[default, web, db]);
        for masquerade in [
            "10.99.0.0/24 oifname != \"nspawn0\"",
            "10.99.1.0/24 oifname != \"nsbr-web\"",
        ] {
            assert!(
                all.contains(&format!("ip saddr {masquerade} masquerade")),
                "{masquerade}"
            );
        }
        assert!(
            !all.contains("10.99.2.0/24 oifname != \"nsbr-db\" masquerade"),
            "an internal network has no way out"
        );
        assert!(all.contains("iifname \"nsbr-db\" ct status & dnat == 0 ip daddr 127.0.0.0/8 drop"));
        let internal = all
            .find("forward iifname \"nsbr-db\" oifname != \"nsbr-db\" drop")
            .unwrap();
        assert!(all.contains("forward oifname \"nsbr-db\" iifname != \"nsbr-db\" drop"));
        let published = all.find("forward ct status dnat accept").unwrap();
        let apart = all
            .find("forward iifname \"nsbr-web\" oifname { \"nspawn0\", \"nsbr-db\" } drop")
            .unwrap();
        assert!(
            internal < published && published < apart,
            "internal networks first, then published ports, then the others apart"
        );
        assert!(
            all.contains("forward iifname \"nspawn0\" oifname { \"nsbr-web\", \"nsbr-db\" } drop")
        );
    }

    #[test]
    fn network_interfaces_fit_an_interface_name() {
        assert_eq!(network_interface("web"), "nsbr-web");
        assert_eq!(network_interface("0123456789"), "nsbr-0123456789");
        let long = network_interface("a-rather-long-network");
        assert_eq!(long.len(), 13);
        assert!(long.starts_with("nsbr-"));
        assert_ne!(long, network_interface("a-rather-long-networl"));
    }

    #[test]
    fn subnets_for_new_networks() {
        let pool: Subnet = "10.99.0.0/16".parse().unwrap();
        let taken: Vec<Subnet> = ["10.99.0.0/24", "10.99.1.0/24", "10.99.3.0/25"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        assert_eq!(
            free_subnet(pool, &taken).unwrap().to_string(),
            "10.99.2.0/24"
        );
        let lan: Subnet = "10.99.0.0/22".parse().unwrap();
        assert_eq!(
            free_subnet(pool, &[lan]).unwrap().to_string(),
            "10.99.4.0/24"
        );
        let small: Subnet = "192.168.50.0/24".parse().unwrap();
        assert_eq!(
            free_subnet(small, &[]).unwrap().to_string(),
            "192.168.50.0/24"
        );
        assert!(free_subnet(small, &[small]).is_err());
        let a: Subnet = "10.0.0.0/8".parse().unwrap();
        let b: Subnet = "10.99.5.0/24".parse().unwrap();
        assert!(a.overlaps(&b) && b.overlaps(&a));
        assert!(!b.overlaps(&"10.99.6.0/24".parse().unwrap()));
        assert_eq!(serde_json::to_string(&b).unwrap(), "\"10.99.5.0/24\"");
        assert_eq!(
            serde_json::from_str::<Subnet>("\"10.99.5.0/24\"").unwrap(),
            b
        );
        assert!(serde_json::from_str::<Subnet>("\"10.99.5.0\"").is_err());
    }

    #[test]
    fn what_the_host_uses_is_read_from_ip() {
        let addr = "1: lo    inet 127.0.0.1/8 scope host lo\\       valid_lft forever preferred_lft forever
2: enp1s0    inet 192.168.122.33/24 brd 192.168.122.255 scope global dynamic noprefixroute enp1s0\\       valid_lft 3000sec
5: nspawn0    inet 10.99.0.1/24 brd 10.99.0.255 scope global nspawn0\\       valid_lft forever
";
        let route = "default via 192.168.122.1 dev enp1s0 proto dhcp src 192.168.122.33 metric 100
10.99.0.0/24 dev nspawn0 proto kernel scope link src 10.99.0.1
172.16.0.0/12 via 192.168.122.5 dev enp1s0
192.168.122.0/24 dev enp1s0 proto kernel scope link src 192.168.122.33 metric 100
";
        let own = vec!["nspawn0".to_string()];
        let found: Vec<String> = subnets_in(addr, &own)
            .into_iter()
            .chain(subnets_in(route, &own))
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            found,
            [
                "127.0.0.0/8",
                "192.168.122.0/24",
                "172.16.0.0/12",
                "192.168.122.0/24"
            ]
        );
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
