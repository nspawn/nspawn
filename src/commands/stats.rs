//! docker stats: the service hands out counters, the rates come from two samples a
//! second apart, and the table is drawn again at every sample.

use std::collections::HashMap;
use std::io::IsTerminal;
use std::time::Duration;

use anyhow::{bail, Result};

use crate::cli::StatsArgs;
use crate::client::{self, Client, Dict};
use crate::output::{human_bytes, table};

/// The counters of one sample.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Counters {
    name: String,
    time_usec: u64,
    cpu_usec: Option<u64>,
    memory: Option<u64>,
    memory_limit: Option<u64>,
    pids: Option<u64>,
    io: Option<(u64, u64)>,
    net: Option<(u64, u64)>,
}

impl Counters {
    fn from_dict(dict: &Dict) -> Self {
        let pair = |a: &str, b: &str| client::maybe_u64(dict, a).zip(client::maybe_u64(dict, b));
        Counters {
            name: client::string(dict, "name"),
            time_usec: client::u64(dict, "time_usec"),
            cpu_usec: client::maybe_u64(dict, "cpu_usec"),
            memory: client::maybe_u64(dict, "memory"),
            memory_limit: client::maybe_u64(dict, "memory_limit"),
            pids: client::maybe_u64(dict, "pids"),
            io: pair("io_read", "io_write"),
            net: pair("net_rx", "net_tx"),
        }
    }
}

/// One line of the table.
#[derive(Debug, Clone, PartialEq)]
struct Row {
    name: String,
    /// Of one CPU, as docker counts it: 200 is two CPUs busy.
    cpu_percent: Option<f64>,
    memory: Option<u64>,
    memory_limit: Option<u64>,
    memory_percent: Option<f64>,
    net: Option<(u64, u64)>,
    io: Option<(u64, u64)>,
    pids: Option<u64>,
}

/// The rates between two samples of a machine; without an earlier one the CPU is
/// unknown.
fn row(prev: Option<&Counters>, cur: &Counters) -> Row {
    let cpu_percent = prev.and_then(|prev| {
        let used = cur.cpu_usec?.checked_sub(prev.cpu_usec?)?;
        let elapsed = cur.time_usec.checked_sub(prev.time_usec)?;
        (elapsed > 0).then(|| used as f64 * 100.0 / elapsed as f64)
    });
    let memory_percent = cur
        .memory
        .zip(cur.memory_limit)
        .filter(|(_, limit)| *limit > 0)
        .map(|(used, limit)| used as f64 * 100.0 / limit as f64);
    Row {
        name: cur.name.clone(),
        cpu_percent,
        memory: cur.memory,
        memory_limit: cur.memory_limit,
        memory_percent,
        net: cur.net,
        io: cur.io,
        pids: cur.pids,
    }
}

fn cells(row: &Row) -> Vec<String> {
    let percent = |p: Option<f64>| p.map(|p| format!("{p:.2}%")).unwrap_or_else(|| "-".into());
    let bytes = |n: Option<u64>| n.map(human_bytes).unwrap_or_else(|| "-".into());
    let pair = |p: Option<(u64, u64)>| match p {
        Some((a, b)) => format!("{} / {}", human_bytes(a), human_bytes(b)),
        None => "-".into(),
    };
    vec![
        row.name.clone(),
        percent(row.cpu_percent),
        format!("{} / {}", bytes(row.memory), bytes(row.memory_limit)),
        percent(row.memory_percent),
        pair(row.net),
        pair(row.io),
        row.pids
            .map(|p| p.to_string())
            .unwrap_or_else(|| "-".into()),
    ]
}

fn json(row: &Row) -> serde_json::Value {
    serde_json::json!({
        "name": row.name,
        "cpu_percent": row.cpu_percent,
        "memory": row.memory,
        "memory_limit": row.memory_limit,
        "memory_percent": row.memory_percent,
        "net_rx": row.net.map(|n| n.0),
        "net_tx": row.net.map(|n| n.1),
        "io_read": row.io.map(|n| n.0),
        "io_write": row.io.map(|n| n.1),
        "pids": row.pids,
    })
}

pub async fn stats(args: StatsArgs, client: &Client) -> Result<()> {
    let sample = || async {
        client
            .manager
            .machine_stats(&args.names)
            .await
            .map_err(client::error)
            .map(|dicts| dicts.iter().map(Counters::from_dict).collect::<Vec<_>>())
    };
    let mut prev: HashMap<String, Counters> = HashMap::new();
    let first = sample().await?;
    for name in &args.names {
        if !first.iter().any(|c| &c.name == name) {
            bail!("machine {name} is not running");
        }
    }
    prev.extend(first.into_iter().map(|c| (c.name.clone(), c)));
    let terminal = std::io::stdout().is_terminal();
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let current = sample().await?;
        let mut rows: Vec<Row> = current.iter().map(|c| row(prev.get(&c.name), c)).collect();
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        if args.json {
            for r in &rows {
                println!("{}", json(r));
            }
        } else {
            if terminal && !args.no_stream {
                // Drawn over the last one, like docker stats.
                print!("\x1b[2J\x1b[H");
            }
            println!(
                "{}",
                table(
                    &[
                        "NAME",
                        "CPU %",
                        "MEM USAGE / LIMIT",
                        "MEM %",
                        "NET I/O",
                        "BLOCK I/O",
                        "PIDS"
                    ],
                    rows.iter().map(cells).collect()
                )
            );
        }
        if args.no_stream {
            return Ok(());
        }
        prev = current.into_iter().map(|c| (c.name.clone(), c)).collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counters(time_usec: u64, cpu_usec: u64) -> Counters {
        Counters {
            name: "web".into(),
            time_usec,
            cpu_usec: Some(cpu_usec),
            memory: Some(64 * 1024 * 1024),
            memory_limit: Some(256 * 1024 * 1024),
            pids: Some(3),
            io: Some((1024, 0)),
            net: None,
        }
    }

    #[test]
    fn rates_come_from_two_samples() {
        let first = counters(1_000_000, 500_000);
        let second = counters(2_000_000, 2_000_000);
        let r = row(Some(&first), &second);
        assert_eq!(r.cpu_percent, Some(150.0), "one and a half CPUs busy");
        assert_eq!(r.memory_percent, Some(25.0));
        assert_eq!(row(None, &second).cpu_percent, None);
        let restarted = counters(3_000_000, 10);
        assert_eq!(
            row(Some(&second), &restarted).cpu_percent,
            None,
            "a counter that went back belongs to a new run"
        );
        assert_eq!(
            cells(&r),
            [
                "web",
                "150.00%",
                "64.0 MiB / 256.0 MiB",
                "25.00%",
                "-",
                "1.0 KiB / 0 B",
                "3"
            ]
        );
    }
}
