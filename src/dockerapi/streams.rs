//! The answers that keep coming: logs, stats and events, written into a channel the
//! response body reads from. A client that goes away closes the channel, which ends the
//! writer and what it runs.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use serde_json::{json, Value};
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc::Sender;

use super::filters::Filters;
use super::model::Snapshot;
use super::util::{rfc3339_nano, ZERO_TIME};
use crate::api::machines::LogsRequest;
use crate::api::stats::Sample;
use crate::api::Context;

/// A log line as docker multiplexes them without a terminal: the stream (1 for
/// standard output), its length, then the line.
pub fn frame(stream: u8, payload: &[u8]) -> Bytes {
    let mut out = Vec::with_capacity(payload.len() + 8);
    out.extend_from_slice(&[stream, 0, 0, 0]);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    Bytes::from(out)
}

/// A journal entry's MESSAGE, which journalctl writes as an array of bytes when it is
/// not valid UTF-8, and its time in microseconds.
pub fn entry_line(entry: &Value) -> Option<(Vec<u8>, u64)> {
    let usec = entry.get("__REALTIME_TIMESTAMP")?.as_str()?.parse().ok()?;
    let message = match entry.get("MESSAGE")? {
        Value::String(s) => s.as_bytes().to_vec(),
        Value::Array(bytes) => bytes
            .iter()
            .map(|b| b.as_u64().map(|b| b as u8))
            .collect::<Option<Vec<u8>>>()?,
        _ => return None,
    };
    Some((message, usec))
}

pub struct LogsQuery {
    /// 1 for standard output, 2 for standard error: the console carries both as one,
    /// so everything goes as the first stream asked for.
    pub stream: u8,
    pub request: LogsRequest,
}

/// journalctl's entries as frames, until it ends or the client goes.
pub async fn logs(query: LogsQuery, tx: Sender<Bytes>) {
    let argv = crate::api::machines::journalctl_arguments(&query.request);
    let Ok(mut child) = tokio::process::Command::new("journalctl")
        .args(&argv)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
    else {
        return;
    };
    let Some(stdout) = child.stdout.take() else {
        return;
    };
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Ok(Some(line)) = line else { break };
                let Ok(entry) = serde_json::from_str::<Value>(&line) else { continue };
                let Some((mut message, usec)) = entry_line(&entry) else { continue };
                message.push(b'\n');
                let payload = if query.request.timestamps {
                    let mut stamped = format!("{} ", rfc3339_nano(usec, true)).into_bytes();
                    stamped.extend_from_slice(&message);
                    stamped
                } else {
                    message
                };
                if tx.send(frame(query.stream, &payload)).await.is_err() {
                    break;
                }
            }
            _ = tx.closed() => break,
        }
    }
    let _ = child.kill().await;
}

/// Total CPU time of the host in nanoseconds, from /proc/stat, as docker reports it.
fn system_cpu_ns() -> u64 {
    let text = std::fs::read_to_string("/proc/stat").unwrap_or_default();
    let ticks: u64 = text
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .skip(1)
        .take(8)
        .filter_map(|t| t.parse::<u64>().ok())
        .sum();
    let hz = nix::unistd::sysconf(nix::unistd::SysconfVar::CLK_TCK)
        .ok()
        .flatten()
        .filter(|hz| *hz > 0)
        .unwrap_or(100) as u64;
    ticks * (1_000_000_000 / hz)
}

fn online_cpus() -> u64 {
    std::thread::available_parallelism().map_or(1, |n| n.get() as u64)
}

fn now_usec() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as u64)
}

/// One reading with its wall-clock time and the host's CPU time then.
pub struct Reading {
    sample: Sample,
    usec: u64,
    system_ns: u64,
}

impl Reading {
    pub async fn take(ctx: &Context, name: &str) -> Option<Reading> {
        let sample = crate::api::stats::sample(ctx, &[name.to_string()])
            .await
            .ok()?
            .into_iter()
            .next()?;
        Some(Reading {
            sample,
            usec: now_usec(),
            system_ns: system_cpu_ns(),
        })
    }
}

fn cpu(reading: Option<&Reading>) -> Value {
    let (total, system) = reading.map_or((0, 0), |r| {
        (r.sample.cpu_usec.unwrap_or(0) * 1000, r.system_ns)
    });
    let mut cpu = json!({
        "cpu_usage": { "total_usage": total, "usage_in_kernelmode": 0, "usage_in_usermode": 0 },
        "throttling_data": { "periods": 0, "throttled_periods": 0, "throttled_time": 0 },
    });
    if reading.is_some() {
        cpu["system_cpu_usage"] = json!(system);
        cpu["online_cpus"] = json!(online_cpus());
    }
    cpu
}

/// docker's stats object of a reading, with the one before for the CPU's rate.
pub fn stats_json(
    name: &str,
    id: &str,
    pids_limit: u64,
    now: Option<&Reading>,
    before: Option<&Reading>,
) -> Value {
    let s = now.map(|r| &r.sample);
    let get = |f: fn(&Sample) -> Option<u64>| s.and_then(f).unwrap_or(0);
    let mut stats = json!({
        "name": format!("/{name}"),
        "id": id,
        "read": now.map_or_else(|| ZERO_TIME.to_string(), |r| rfc3339_nano(r.usec, false)),
        "preread": before.map_or_else(|| ZERO_TIME.to_string(), |r| rfc3339_nano(r.usec, false)),
        "pids_stats": { "current": get(|s| s.pids) },
        "blkio_stats": {
            "io_service_bytes_recursive": [
                { "major": 0, "minor": 0, "op": "read", "value": get(|s| s.io_read) },
                { "major": 0, "minor": 0, "op": "write", "value": get(|s| s.io_write) },
            ],
            "io_serviced_recursive": null,
            "io_queue_recursive": null,
            "io_service_time_recursive": null,
            "io_wait_time_recursive": null,
            "io_merged_recursive": null,
            "io_time_recursive": null,
            "sectors_recursive": null,
        },
        "num_procs": 0,
        "storage_stats": {},
        "cpu_stats": cpu(now),
        "precpu_stats": cpu(before),
        "memory_stats": {
            "usage": get(|s| s.memory),
            // The usage is without the reclaimable cache already, which clients take
            // away again.
            "stats": { "inactive_file": 0 },
            "limit": get(|s| s.memory_limit),
        },
    });
    if pids_limit > 0 {
        stats["pids_stats"]["limit"] = json!(pids_limit);
    }
    if let Some(s) = s.filter(|s| s.net_rx.is_some()) {
        stats["networks"] = json!({
            "eth0": {
                "rx_bytes": s.net_rx.unwrap_or(0), "rx_packets": 0, "rx_errors": 0, "rx_dropped": 0,
                "tx_bytes": s.net_tx.unwrap_or(0), "tx_packets": 0, "tx_errors": 0, "tx_dropped": 0,
            }
        });
    }
    stats
}

/// A reading every second, each object on its line, until the machine is gone or the
/// client goes.
pub async fn stats(
    ctx: Arc<Context>,
    name: String,
    id: String,
    pids_limit: u64,
    tx: Sender<Bytes>,
) {
    let mut before: Option<Reading> = None;
    loop {
        let now = Reading::take(&ctx, &name).await;
        let text = format!(
            "{}\n",
            stats_json(&name, &id, pids_limit, now.as_ref(), before.as_ref())
        );
        if tx.send(Bytes::from(text)).await.is_err() {
            return;
        }
        if now.is_none() {
            return;
        }
        before = now;
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            _ = tx.closed() => return,
        }
    }
}

/// docker's events, from `since` (unix seconds) or only new ones, until `until` or
/// the client goes.
pub async fn events(
    ctx: Arc<Context>,
    since: Option<String>,
    until: Option<String>,
    filters: Filters,
    tx: Sender<Bytes>,
) {
    let mut follow = match crate::api::events::Follow::spawn(
        since.as_deref(),
        until.as_deref(),
        std::process::Stdio::null(),
    ) {
        Ok(follow) => follow,
        Err(_) => return,
    };
    loop {
        tokio::select! {
            event = follow.next(&ctx.store) => {
                let Some(event) = event else { break };
                let networks = match Snapshot::networks_of(&ctx, &event.name) {
                    Some(nets) if event.kind == "machine" => nets,
                    _ => Vec::new(),
                };
                for message in super::events::translate(&event, &networks) {
                    if !super::events::passes(&filters, &message) {
                        continue;
                    }
                    if tx.send(Bytes::from(format!("{message}\n"))).await.is_err() {
                        follow.finish().await;
                        return;
                    }
                }
            }
            _ = tx.closed() => break,
        }
    }
    follow.finish().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_as_docker_writes_them() {
        assert_eq!(
            frame(1, b"bye\n").to_vec(),
            [1, 0, 0, 0, 0, 0, 0, 4, b'b', b'y', b'e', b'\n']
        );
        let golden = include_bytes!("../../tests/dockerapi/golden/logs-done-tail.body");
        // The last line of the golden run, "bye" on standard output.
        assert_eq!(golden.to_vec(), frame(1, b"bye\n").to_vec());
    }

    #[test]
    fn journal_entries() {
        let text =
            serde_json::json!({"__REALTIME_TIMESTAMP": "1792344104170941", "MESSAGE": "bye"});
        assert_eq!(
            entry_line(&text),
            Some((b"bye".to_vec(), 1_792_344_104_170_941))
        );
        let bytes = serde_json::json!({"__REALTIME_TIMESTAMP": "1", "MESSAGE": [255, 10]});
        assert_eq!(entry_line(&bytes), Some((vec![255, 10], 1)));
        assert_eq!(entry_line(&serde_json::json!({"MESSAGE": "x"})), None);
    }

    #[test]
    fn stats_of_a_machine_that_does_not_run() {
        let stats = stats_json("web", "abc", 0, None, None);
        assert_eq!(stats["name"], "/web");
        assert_eq!(stats["read"], ZERO_TIME);
        assert_eq!(stats["memory_stats"]["usage"], 0);
        assert!(stats["pids_stats"].get("limit").is_none());
        assert!(stats.get("networks").is_none());
    }
}
