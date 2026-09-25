//! docker stats: counters from the cgroup of each machine's unit (systemd-nspawn and the
//! whole machine) and from its interfaces as the machine sees them. Rates are the
//! caller's, from two samples.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::{Context as _, Result};
use nix::time::{clock_gettime, ClockId};

use crate::api::machines::refuse_foreign;
use crate::api::Context;
use crate::reference::validate_entry_name;

/// One reading; None where a file could not be read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sample {
    pub name: String,
    /// CLOCK_MONOTONIC when it was taken, in microseconds.
    pub time_usec: u64,
    pub cpu_usec: Option<u64>,
    /// Without the reclaimable page cache, as docker counts it.
    pub memory: Option<u64>,
    /// The limit, or the host's memory without one.
    pub memory_limit: Option<u64>,
    pub pids: Option<u64>,
    pub io_read: Option<u64>,
    pub io_write: Option<u64>,
    /// All interfaces but loopback; None on the host's network.
    pub net_rx: Option<u64>,
    pub net_tx: Option<u64>,
}

/// One sample of each named machine (every running container without names). Machines
/// that do not run are left out; the caller decides whether that is an error.
pub async fn sample(ctx: &Context, names: &[String]) -> Result<Vec<Sample>> {
    let sd = ctx.sd().await?;
    let names: Vec<String> = if names.is_empty() {
        sd.list_machines()
            .await?
            .into_iter()
            .map(|m| m.name)
            .collect()
    } else {
        for name in names {
            validate_entry_name(name)?;
            refuse_foreign(sd, name).await?;
        }
        names.to_vec()
    };
    let mut samples = Vec::new();
    for name in names {
        // It may have ended since the listing.
        let Ok(unit) = sd.machine_unit(&name).await else {
            continue;
        };
        let Ok(leader) = sd.machine_leader(&name).await else {
            continue;
        };
        let Ok(cgroup) = sd.control_group(&unit).await else {
            continue;
        };
        samples.push(read(&name, &cgroup, leader)?);
    }
    Ok(samples)
}

fn read(name: &str, cgroup: &str, leader: u32) -> Result<Sample> {
    let dir = Path::new("/sys/fs/cgroup").join(cgroup.trim_start_matches('/'));
    let file = |f: &str| fs::read_to_string(dir.join(f)).ok();
    let (io_read, io_write) = file("io.stat").map(|t| io_bytes(&t)).unzip();
    let (net_rx, net_tx) = if own_network(leader) {
        fs::read_to_string(format!("/proc/{leader}/net/dev"))
            .ok()
            .map(|t| net_bytes(&t))
            .unzip()
    } else {
        (None, None)
    };
    let now = clock_gettime(ClockId::CLOCK_MONOTONIC).context("reading the clock")?;
    Ok(Sample {
        name: name.to_string(),
        time_usec: now.tv_sec() as u64 * 1_000_000 + now.tv_nsec() as u64 / 1000,
        cpu_usec: file("cpu.stat").and_then(|t| cpu_usec(&t)),
        memory: file("memory.current")
            .and_then(|current| memory_used(&current, &file("memory.stat").unwrap_or_default())),
        memory_limit: file("memory.max").and_then(|max| {
            memory_limit(
                &max,
                &fs::read_to_string("/proc/meminfo").unwrap_or_default(),
            )
        }),
        pids: file("pids.current").and_then(|t| t.trim().parse().ok()),
        io_read,
        io_write,
        net_rx,
        net_tx,
    })
}

/// Whether the machine has a network namespace of its own (not --network host).
fn own_network(leader: u32) -> bool {
    let ns = |path: &str| fs::metadata(path).map(|m| (m.dev(), m.ino())).ok();
    match (
        ns(&format!("/proc/{leader}/ns/net")),
        ns("/proc/self/ns/net"),
    ) {
        (Some(machine), Some(host)) => machine != host,
        _ => false,
    }
}

fn cpu_usec(cpu_stat: &str) -> Option<u64> {
    field(cpu_stat, "usage_usec")
}

/// memory.current less inactive_file, as docker shows it on cgroup v2.
fn memory_used(current: &str, stat: &str) -> Option<u64> {
    let current: u64 = current.trim().parse().ok()?;
    Some(current.saturating_sub(field(stat, "inactive_file").unwrap_or(0)))
}

fn memory_limit(max: &str, meminfo: &str) -> Option<u64> {
    match max.trim() {
        "max" => meminfo
            .lines()
            .find_map(|l| l.strip_prefix("MemTotal:"))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|kb| kb.parse::<u64>().ok())
            .map(|kb| kb * 1024),
        bytes => bytes.parse().ok(),
    }
}

fn io_bytes(io_stat: &str) -> (u64, u64) {
    let mut read = 0;
    let mut written = 0;
    for line in io_stat.lines() {
        for pair in line.split_whitespace().skip(1) {
            match pair.split_once('=') {
                Some(("rbytes", n)) => read += n.parse::<u64>().unwrap_or(0),
                Some(("wbytes", n)) => written += n.parse::<u64>().unwrap_or(0),
                _ => {}
            }
        }
    }
    (read, written)
}

fn net_bytes(net_dev: &str) -> (u64, u64) {
    let mut rx = 0;
    let mut tx = 0;
    for line in net_dev.lines().skip(2) {
        let Some((interface, counters)) = line.split_once(':') else {
            continue;
        };
        if interface.trim() == "lo" {
            continue;
        }
        let counters: Vec<u64> = counters
            .split_whitespace()
            .map(|n| n.parse().unwrap_or(0))
            .collect();
        rx += counters.first().copied().unwrap_or(0);
        tx += counters.get(8).copied().unwrap_or(0);
    }
    (rx, tx)
}

/// The value of `key` in a flat keyed cgroup file.
fn field(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let (k, v) = line.split_once(' ')?;
        (k == key).then(|| v.trim().parse().ok()).flatten()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cgroup_files_are_read_as_docker_counts_them() {
        assert_eq!(
            cpu_usec("usage_usec 123456\nuser_usec 100000\nsystem_usec 23456\n"),
            Some(123456)
        );
        assert_eq!(cpu_usec("user_usec 1\n"), None);
        let stat = "anon 1000\nfile 5000\ninactive_file 3000\nactive_file 2000\n";
        assert_eq!(memory_used("10000\n", stat), Some(7000));
        assert_eq!(memory_used("1000\n", stat), Some(0));
        assert_eq!(memory_used("garbage", stat), None);
        let meminfo = "MemTotal:        8000000 kB\nMemFree:  100 kB\n";
        assert_eq!(memory_limit("67108864\n", meminfo), Some(67108864));
        assert_eq!(memory_limit("max\n", meminfo), Some(8_192_000_000));
        assert_eq!(memory_limit("max\n", ""), None);
        assert_eq!(
            io_bytes("8:0 rbytes=1000 wbytes=2000 rios=1 wios=2 dbytes=0 dios=0\n253:1 rbytes=24 wbytes=0 rios=1 wios=0\n"),
            (1024, 2000)
        );
        assert_eq!(io_bytes(""), (0, 0));
    }

    #[test]
    fn network_counters_leave_loopback_out() {
        let dev = "Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo:    5000      50    0    0    0     0          0         0     5000      50    0    0    0     0       0          0
 host0: 1234567    1000    0    0    0     0          0         0   654321     900    0    0    0     0       0          0
";
        assert_eq!(net_bytes(dev), (1234567, 654321));
    }
}
