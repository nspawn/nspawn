//! The answers that keep coming: logs, stats and events, written into a channel the
//! response body reads from. A client that goes away closes the channel, which ends the
//! writer and what it runs.

use std::collections::BTreeMap;
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
    // A follow ends with the machine, as docker's does with the container: machined's
    // file of it goes when it stops.
    let registered = std::path::Path::new("/run/systemd/machines").join(&query.request.machine);
    let mut ticks = tokio::time::interval(std::time::Duration::from_secs(2));
    let mut gone = 0;
    loop {
        tokio::select! {
            _ = ticks.tick(), if query.request.follow => {
                gone = if registered.exists() { 0 } else { gone + 1 };
                // A tick of grace for its last lines.
                if gone > 1 {
                    break;
                }
            }
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

/// Total CPU time of the host in nanoseconds, from /proc/stat, as docker reports it:
/// user, nice, system, idle, iowait, irq and softirq, steal left out.
fn system_cpu_ns() -> u64 {
    let text = std::fs::read_to_string("/proc/stat").unwrap_or_default();
    let ticks = first_seven(&text);
    let hz = nix::unistd::sysconf(nix::unistd::SysconfVar::CLK_TCK)
        .ok()
        .flatten()
        .filter(|hz| *hz > 0)
        .unwrap_or(100) as u64;
    ticks * (1_000_000_000 / hz)
}

fn first_seven(stat: &str) -> u64 {
    stat.lines()
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .skip(1)
        .take(7)
        .filter_map(|t| t.parse::<u64>().ok())
        .sum()
}

/// The host's CPUs as docker counts them: the cpuN lines of /proc/stat, whatever this
/// process may run on.
fn online_cpus() -> u64 {
    let text = std::fs::read_to_string("/proc/stat").unwrap_or_default();
    match cpu_lines(&text) {
        0 => std::thread::available_parallelism().map_or(1, |n| n.get() as u64),
        n => n,
    }
}

fn cpu_lines(stat: &str) -> u64 {
    stat.lines()
        .filter(|l| {
            l.strip_prefix("cpu")
                .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
        })
        .count() as u64
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
/// A machine's attributes (created, image) and labels.
type Known = (BTreeMap<String, String>, BTreeMap<String, String>);

/// How long what is known of a removed machine stays: a die that comes after the
/// removal (run --rm) comes within it.
const KEPT_AFTER_REMOVAL_USEC: u64 = 5 * 60 * 1_000_000;

/// Forgets, once a while has passed, what is known of the machines removed, so that a
/// stream kept for weeks does not keep every name it saw: the removed machines, by
/// name, created and when.
fn forget_removed(
    known: &mut BTreeMap<String, Known>,
    removed: &mut std::collections::VecDeque<(String, String, u64)>,
    event: &crate::api::events::Event,
) {
    if event.action == "remove" {
        if let Some(created) = event.attributes.get("created") {
            removed.push_back((event.name.clone(), created.clone(), event.time_usec));
        }
    }
    while let Some((name, created, at)) = removed.front() {
        if event.time_usec.saturating_sub(*at) < KEPT_AFTER_REMOVAL_USEC {
            break;
        }
        // Not a machine made anew under the name since.
        if known
            .get(name)
            .is_some_and(|(attributes, _)| attributes.get("created") == Some(created))
        {
            known.remove(name);
        }
        removed.pop_front();
    }
}

/// A machine's event with what the stream knows of the machine: an event without its
/// record (a die after run --rm took it) gets the attributes and labels the machine had,
/// one that says which machine it was replaces what is known of the name.
fn complete(known: &mut BTreeMap<String, Known>, event: &mut crate::api::events::Event) {
    match event.attributes.get("created") {
        Some(created) => {
            // A remove carries which machine it was, but its record is gone with its
            // labels: the ones known of that machine stay.
            if event.labels.is_empty() {
                if let Some((attributes, labels)) = known.get(&event.name) {
                    if attributes.get("created") == Some(created) {
                        event.labels.clone_from(labels);
                    }
                }
            }
            let attributes = BTreeMap::from([
                ("created".to_string(), created.clone()),
                (
                    "image".to_string(),
                    event.attributes.get("image").cloned().unwrap_or_default(),
                ),
            ]);
            known.insert(event.name.clone(), (attributes, event.labels.clone()));
        }
        None => {
            // Only for an event of that machine's time: one before it was made is of an
            // earlier machine of the name, and stays without what this one has.
            let of_it = |attributes: &BTreeMap<String, String>| {
                attributes
                    .get("created")
                    .and_then(|c| c.parse::<u64>().ok())
                    .is_some_and(|created| event.time_usec / 1_000_000 >= created)
            };
            if let Some((attributes, labels)) = known.get(&event.name).filter(|(a, _)| of_it(a)) {
                for (k, v) in attributes {
                    event
                        .attributes
                        .entry(k.clone())
                        .or_insert_with(|| v.clone());
                }
                if event.labels.is_empty() {
                    event.labels.clone_from(labels);
                }
            }
        }
    }
}

pub async fn events(
    ctx: Arc<Context>,
    since: Option<String>,
    until: Option<String>,
    deadline: Option<std::time::Duration>,
    filters: Filters,
    tx: Sender<Bytes>,
) {
    if deadline == Some(std::time::Duration::ZERO) {
        return;
    }
    let mut follow = match crate::api::events::Follow::spawn(
        since.as_deref(),
        until.as_deref(),
        std::process::Stdio::null(),
    ) {
        Ok(follow) => follow,
        Err(_) => return,
    };
    let ends = async {
        match deadline {
            Some(after) => tokio::time::sleep(after).await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(ends);
    // What each machine's record said, for its events once the record is gone: the die
    // of a machine of run --rm comes after its removal more often than not.
    let mut removed = std::collections::VecDeque::new();
    let mut known: BTreeMap<String, Known> = ctx
        .store
        .list_images()
        .unwrap_or_default()
        .into_iter()
        .map(|r| {
            let attributes = BTreeMap::from([
                ("created".to_string(), r.created.to_string()),
                ("image".to_string(), r.reference.clone()),
            ]);
            (r.name.clone(), (attributes, r.effective_labels()))
        })
        .collect();
    loop {
        tokio::select! {
            () = &mut ends => break,
            event = follow.next(&ctx.store) => {
                let Some(mut event) = event else { break };
                if event.kind == "machine" {
                    complete(&mut known, &mut event);
                    forget_removed(&mut known, &mut removed, &event);
                }
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
    fn cpu_counts_as_docker_counts_it() {
        let stat = "cpu  10 1 5 100 2 0 1 7 0 0\ncpu0 5 0 2 50 1 0 0 3 0 0\ncpu1 5 1 3 50 1 0 1 4 0 0\nintr 1\n";
        assert_eq!(first_seven(stat), 119, "steal left out");
        assert_eq!(cpu_lines(stat), 2);
    }

    #[test]
    fn a_removed_machine_keeps_its_labels() {
        let event = |action: &str, created: Option<&str>| crate::api::events::Event {
            time_usec: 10_000_000,
            kind: "machine".into(),
            action: action.into(),
            name: "web".into(),
            attributes: created
                .map(|c| BTreeMap::from([("created".to_string(), c.to_string())]))
                .unwrap_or_default(),
            labels: BTreeMap::new(),
            run: None,
        };
        let mut known = BTreeMap::from([(
            "web".to_string(),
            (
                BTreeMap::from([("created".to_string(), "5".to_string())]),
                BTreeMap::from([("caddy".to_string(), "web.example".to_string())]),
            ),
        )]);
        let mut remove = event("remove", Some("5"));
        complete(&mut known, &mut remove);
        assert_eq!(remove.labels["caddy"], "web.example");
        // A later event of the name still has them.
        let mut die = event("die", None);
        complete(&mut known, &mut die);
        assert_eq!(die.labels["caddy"], "web.example");
        // Another machine of the name does not.
        let mut other = event("remove", Some("9"));
        complete(&mut known, &mut other);
        assert!(other.labels.is_empty());
        // Nor an event from before the machine known of the name was made: it is an
        // earlier machine's.
        let mut earlier = event("start", None);
        earlier.time_usec = 8_000_000;
        complete(&mut known, &mut earlier);
        assert!(!earlier.attributes.contains_key("created"));
    }

    #[test]
    fn a_removed_machine_is_forgotten_a_while_later() {
        let event = |action: &str, created: &str, at: u64| crate::api::events::Event {
            time_usec: at,
            kind: "machine".into(),
            action: action.into(),
            name: "job".into(),
            attributes: BTreeMap::from([("created".to_string(), created.to_string())]),
            labels: BTreeMap::new(),
            run: None,
        };
        let mut known = BTreeMap::new();
        let mut removed = std::collections::VecDeque::new();
        for e in [event("create", "1", 0), event("remove", "1", 10)] {
            let mut e = e;
            complete(&mut known, &mut e);
            forget_removed(&mut known, &mut removed, &e);
        }
        assert!(
            known.contains_key("job"),
            "kept for a die after the removal"
        );
        let mut later = event("start", "2", 10 + KEPT_AFTER_REMOVAL_USEC);
        later.name = "other".into();
        complete(&mut known, &mut later);
        forget_removed(&mut known, &mut removed, &later);
        assert!(!known.contains_key("job"));
        assert!(removed.is_empty());
        // A machine made anew under the name meanwhile stays.
        for e in [
            event("remove", "3", 0),
            event("create", "4", 1),
            event("start", "4", 1 + KEPT_AFTER_REMOVAL_USEC),
        ] {
            let mut e = e;
            complete(&mut known, &mut e);
            forget_removed(&mut known, &mut removed, &e);
        }
        assert!(known.contains_key("job"));
    }

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
