//! What a project's machines say and run: logs, up attached to its services, exec,
//! run, cp, and the lifecycle hooks of a service.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{IsTerminal, Write};
use std::os::fd::OwnedFd;
use std::time::Duration;

use anyhow::{bail, Context as _, Result};
use tokio::sync::mpsc;
use zbus::zvariant::Value as Bus;

use super::convert::{self, Spec};
use super::progress::Status;
use super::up::{self, Converge};
use super::{machine_name, Engine, Machine};
use crate::client::{self, Ended, Options};
use crate::compose::cli::{CpArgs, ExecArgs, LogsArgs, RunArgs, UpArgs};
use crate::compose::project::Project;
use crate::compose::value::{Map, Value};

/// The name a machine goes by in the output of logs and up: its service and number when
/// it has the name compose gives it, its whole name otherwise.
pub fn display_name(project: &str, m: &Machine) -> String {
    let default = machine_name(project, &m.service, "", m.number);
    let prefix = format!("{}-", project.replace('_', "-"));
    if m.name == default && !m.oneoff {
        m.name.strip_prefix(&prefix).unwrap_or(&m.name).to_string()
    } else {
        m.name.clone()
    }
}

/// Whether the output gets colors: useAnsi of the mode pre_run resolved (always, or auto
/// on a terminal; any other word is never), and none with --no-color or NO_COLOR.
fn colors(e: &Engine<'_>, no_color: bool) -> bool {
    if no_color || std::env::var("NO_COLOR").is_ok_and(|v| !v.is_empty()) {
        return false;
    }
    match e.ansi.as_str() {
        "always" => true,
        "auto" => std::io::stdout().is_terminal(),
        _ => false,
    }
}

/// The colors of the prefixes, one a service in turn.
const RAINBOW: [&str; 10] = [
    "36", "33", "32", "35", "34", "36;1", "33;1", "32;1", "35;1", "34;1",
];

fn paint(code: Option<&str>, text: &str) -> String {
    match code {
        Some(code) => format!("\x1b[{code}m{text}\x1b[0m"),
        None => text.to_string(),
    }
}

/// A line of a machine's output, or the end of one of its streams.
enum Line {
    Text(usize, bool, Vec<u8>),
    Closed(usize),
}

/// A machine whose output is followed.
struct Followed {
    machine: Machine,
    name: String,
    prefix: String,
    color: Option<&'static str>,
    ended: Option<Ended>,
    open: usize,
    done: bool,
}

/// The time a line of the output starts with, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stamp {
    None,
    /// When the machine wrote it, in UTC, as the engine stamps a container's log: the
    /// lines come as journalctl's JSON entries.
    Written,
    /// When up received it, in local time, as compose's log consumer stamps one.
    Received,
}

/// jsonmessage.RFC3339NanoFixed of a time in microseconds, at `offset` seconds east of
/// UTC: nine digits of seconds, and Z for UTC or the offset as -05:00.
fn rfc3339_nano_fixed(usec: u64, offset: i64) -> String {
    let local = usec.saturating_add_signed(offset * 1_000_000);
    let text = crate::dockerapi::util::rfc3339_nano(local, true);
    if offset == 0 {
        return text;
    }
    let sign = if offset < 0 { '-' } else { '+' };
    let minutes = offset.unsigned_abs() / 60;
    format!(
        "{}{sign}{:02}:{:02}",
        text.trim_end_matches('Z'),
        minutes / 60,
        minutes % 60
    )
}

/// A line of the output as it is printed: its stamp first when it has one; a JSON entry
/// without a message gives none.
fn stamped(stamp: Stamp, line: Vec<u8>) -> Option<Vec<u8>> {
    let (text, usec, offset) = match stamp {
        Stamp::None => return Some(line),
        Stamp::Written => {
            let entry = serde_json::from_slice(&line).ok()?;
            let (text, usec) = crate::dockerapi::streams::entry_line(&entry)?;
            (text, usec, 0)
        }
        Stamp::Received => {
            let usec = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_micros() as u64;
            let offset = crate::compose::gotime::local_offset((usec / 1_000_000) as i64);
            (line, usec, offset)
        }
    };
    let text = text.strip_suffix(b"\n").map(<[u8]>::to_vec).unwrap_or(text);
    let mut out = rfc3339_nano_fixed(usec, offset).into_bytes();
    out.push(b' ');
    out.extend_from_slice(&text);
    Some(out)
}

/// Starts following the output of `machines` from journalctl through the service, each
/// line to the channel, stamped as `stamp` says.
async fn follow(
    e: &Engine<'_>,
    machines: &[Machine],
    options: &Options<'static>,
    prefix: bool,
    color: bool,
    stamp: Stamp,
    tx: &mpsc::UnboundedSender<Line>,
) -> Result<Vec<Followed>> {
    follow_from(e, machines, options, prefix, color, stamp, 0, 0, tx).await
}

/// follow for machines that come after `base` others, whose prefixes are `width` wide at
/// least: their colors go on from there, and their lines carry their place among all.
#[allow(clippy::too_many_arguments)]
async fn follow_from(
    e: &Engine<'_>,
    machines: &[Machine],
    options: &Options<'static>,
    prefix: bool,
    color: bool,
    stamp: Stamp,
    base: usize,
    width: usize,
    tx: &mpsc::UnboundedSender<Line>,
) -> Result<Vec<Followed>> {
    let names: Vec<String> = machines.iter().map(|m| display_name(&e.name, m)).collect();
    let width = (names.iter().map(String::len).max().unwrap_or(0) + 1).max(width);
    let mut followed = Vec::new();
    for (i, (m, name)) in machines.iter().zip(names).enumerate() {
        let i = base + i;
        let color = color.then(|| RAINBOW[i % RAINBOW.len()]);
        let prefix = if prefix {
            paint(color, &format!("{name:<width$} | "))
        } else {
            String::new()
        };
        let mut f = Followed {
            machine: m.clone(),
            name,
            prefix,
            color,
            ended: None,
            open: 0,
            done: false,
        };
        let logs = e.client.manager.logs(&m.name, options.clone()).await;
        match logs {
            Ok((mut fds, process)) => {
                f.ended = Some(Ended::watch(&e.client.connection, process).await?);
                for (key, to_stderr) in [("stdout", false), ("stderr", true)] {
                    let Some(fd) = fds.remove(key) else { continue };
                    let fd = OwnedFd::from(fd);
                    let tx = tx.clone();
                    f.open += 1;
                    std::thread::spawn(move || {
                        let reader = std::io::BufReader::new(std::fs::File::from(fd));
                        for line in std::io::BufRead::split(reader, b'\n') {
                            let Ok(line) = line else { break };
                            // journalctl's own errors on stderr are text, JSON or not.
                            let stamp = if to_stderr && stamp == Stamp::Written {
                                Stamp::None
                            } else {
                                stamp
                            };
                            let Some(line) = stamped(stamp, line) else {
                                continue;
                            };
                            if tx.send(Line::Text(i, to_stderr, line)).is_err() {
                                return;
                            }
                        }
                        let _ = tx.send(Line::Closed(i));
                    });
                }
            }
            Err(err) => {
                e.warn(format!("{}: {:#}", m.name, client::error(err)));
                f.done = true;
            }
        }
        followed.push(f);
    }
    Ok(followed)
}

/// printer.HandleEvent of an exit: `NAME exited with code N`, in the machine's color.
fn exited(followed: &[Followed], project: &str, m: &Machine, restarting: bool) {
    let (name, color) = followed
        .iter()
        .find(|f| f.machine.name == m.name)
        .map(|f| (f.name.clone(), f.color))
        .unwrap_or_else(|| (display_name(project, m), None));
    let code = m.exit_code.unwrap_or(0);
    let text = if restarting {
        format!("\r\x1b[K{name} exited with code {code} (restarting)\n")
    } else {
        format!("\r\x1b[K{name} exited with code {code}\n")
    };
    let _ = std::io::stdout().write_all(paint(color, &text).as_bytes());
}

fn print_line(f: &Followed, to_stderr: bool, line: &[u8]) {
    let mut text = f.prefix.as_bytes().to_vec();
    text.extend_from_slice(line);
    text.push(b'\n');
    if to_stderr {
        let _ = std::io::stderr().write_all(&text);
    } else {
        let _ = std::io::stdout().write_all(&text);
    }
}

/// The services logs reads and, with -f, watches: those named; with the files and none
/// named, the project's but those with attach: false, or every one of them when that
/// leaves none; without the files (None), every machine of the project's label.
fn logged_services(project: Option<&Project>, named: &[String]) -> Option<BTreeSet<String>> {
    if !named.is_empty() {
        return Some(named.iter().cloned().collect());
    }
    let project = project?;
    let shown: BTreeSet<String> = project
        .services
        .iter()
        .filter(|(_, s)| s.get(&["attach"]) != Some(&Value::Bool(false)))
        .map(|(name, _)| name.clone())
        .collect();
    if shown.is_empty() {
        return Some(project.services.keys().cloned().collect());
    }
    Some(shown)
}

pub async fn logs(e: &mut Engine<'_>, a: &LogsArgs) -> Result<()> {
    if let Some(project) = &e.project {
        for name in &a.service {
            if !project.services.contains_key(name) && !project.disabled_services.contains_key(name)
            {
                bail!("no such service: {name}");
            }
        }
    }
    let watching = logged_services(e.project.as_ref(), &a.service);
    let watched =
        |m: &Machine| !m.oneoff && watching.as_ref().is_none_or(|w| w.contains(&m.service));
    let mut machines: Vec<Machine> = e
        .machines()
        .await?
        .into_iter()
        .filter(|m| watched(m))
        .collect();
    // getSpecifiedContainer: the replica of that number, in whatever state.
    if let Some(index) = a.index.filter(|i| *i > 0) {
        machines.retain(|m| i64::from(m.number) == index);
        if machines.is_empty() {
            bail!(
                "service {} is not running container #{index}",
                crate::compose::gostr::quote(&a.service[0])
            );
        }
    }
    let mut options = Options::new();
    options.insert("follow", Bus::from(a.follow));
    if let Some(n) = log_lines(a.tail.as_deref(), a.follow)? {
        options.insert("lines", Bus::from(n));
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    for (key, given) in [("since", &a.since), ("until", &a.until)] {
        if let Some(text) = given {
            let time = journal_time(text, now)
                .map_err(|e| anyhow::anyhow!("invalid value for {key:?}: {e}"))?;
            options.insert(key, Bus::from(time));
        }
    }
    // The engine's stamp on each line, from the time the journal gives the entry.
    options.insert("json", Bus::from(a.timestamps));
    let stamp = if a.timestamps {
        Stamp::Written
    } else {
        Stamp::None
    };
    let (tx, mut rx) = mpsc::unbounded_channel();
    let color = colors(e, a.no_color);
    let mut followed = follow(e, &machines, &options, !a.no_log_prefix, color, stamp, &tx).await?;
    // Kept to follow the machines that come, with -f; without, the output ends with the
    // streams.
    let tx = a.follow.then_some(tx);
    // The monitor of a followed logs: what each watched machine was at the last look, and
    // whether it still counts; one that stops for good no longer does, one that starts
    // (anew or for the first time) does again.
    let mut alive: BTreeMap<String, (String, bool)> = machines
        .iter()
        .map(|m| (m.name.clone(), (m.state.clone(), true)))
        .collect();
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut stopped = false;
    let mut signalled = false;
    let mut streams = true;
    loop {
        tokio::select! {
            line = rx.recv(), if streams => match line {
                Some(Line::Text(i, err, text)) => print_line(&followed[i], err, &text),
                Some(Line::Closed(i)) => {
                    followed[i].open = followed[i].open.saturating_sub(1);
                }
                // Without -f, the output ends when journalctl has said it all.
                None if !a.follow => break,
                None => streams = false,
            },
            _ = tick.tick(), if a.follow => {
                let Ok(now) = e.machines().await else { continue };
                let now: Vec<Machine> = now.into_iter().filter(|m| watched(m)).filter(|m| {
                    a.index.filter(|i| *i > 0).is_none_or(|i| i64::from(m.number) == i)
                }).collect();
                for m in &now {
                    let before = alive.get(&m.name).cloned();
                    match before {
                        None => {
                            // A machine made since, as a scale or a recreate makes one:
                            // its output from its start, as much of it as --tail asks.
                            let mut start = options.clone();
                            start.remove("since");
                            let width = followed.iter().map(|f| f.name.len() + 1).max().unwrap_or(0);
                            if let Some(tx) = &tx {
                                let base = followed.len();
                                let more = follow_from(e, std::slice::from_ref(m), &start, !a.no_log_prefix, color, stamp, base, width, tx).await?;
                                followed.extend(more);
                            }
                            alive.insert(m.name.clone(), (m.state.clone(), m.state != "stopped"));
                        }
                        Some((was, _)) => {
                            let restarting = m.state == "restarting" && was != "restarting";
                            let ended = m.state == "stopped" && was != "stopped";
                            if ended || restarting {
                                exited(&followed, &e.name, m, restarting);
                            }
                            let counts = match (was.as_str(), m.state.as_str()) {
                                (_, "stopped") if was != "stopped" => false,
                                ("stopped", s) if s != "stopped" => true,
                                _ => alive[&m.name].1,
                            };
                            alive.insert(m.name.clone(), (m.state.clone(), counts));
                        }
                    }
                }
                // One removed is gone for good.
                for (name, entry) in alive.iter_mut() {
                    if !now.iter().any(|m| m.name == *name) {
                        entry.1 = false;
                    }
                }
                if alive.values().all(|(_, counts)| !counts) {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    while let Ok(line) = rx.try_recv() {
                        if let Line::Text(i, err, text) = line {
                            print_line(&followed[i], err, &text);
                        }
                    }
                    stopped = true;
                    break;
                }
            }
            _ = interrupt.recv() => {
                stopped = true;
                signalled = true;
                break;
            }
            _ = term.recv() => {
                stopped = true;
                signalled = true;
                break;
            }
        }
    }
    // Ended on their own, journalctl's statuses are the command's; stopped here, theirs
    // say nothing.
    let mut code = 0;
    for f in &mut followed {
        if let Some(ended) = f.ended.take() {
            if stopped {
                let _ = ended.signal(15).await;
            } else {
                code = code.max(ended.status().await.unwrap_or(0));
            }
        }
    }
    if signalled {
        std::process::exit(130);
    }
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

/// run's -e and --env-from-file as compose reads them (getEnvironment): dotenv files
/// (quotes, export, comments), a bare KEY and ${KEY} taken from -e and the files before;
/// -e wins over them, and of the files the first that sets a variable.
fn env_from_files(
    flags: BTreeMap<String, String>,
    files: &[String],
) -> Result<BTreeMap<String, String>> {
    let mut given = flags;
    for file in files {
        let text = std::fs::read_to_string(file)
            .map_err(|e| anyhow::anyhow!(crate::compose::gostr::path_error("open", file, &e)))?;
        let mut vars = crate::compose::dotenv::Env::new();
        {
            let lookup = |k: &str| given.get(k).cloned();
            crate::compose::dotenv::parse(&text, &mut vars, &lookup, &mut |_| {})
                .map_err(|e| anyhow::anyhow!("reading {file}: {e}"))?;
        }
        for (k, v) in vars {
            given.entry(k).or_insert(v);
        }
    }
    Ok(given)
}

/// A time of --since or --until as journalctl reads one, from what the flags take: a
/// duration ago (42m, 1h30m), unix seconds, or an RFC 3339 time or date.
fn journal_time(text: &str, now: f64) -> Result<String, String> {
    let offset = crate::compose::gotime::local_offset(now as i64);
    crate::compose::gotime::get_timestamp(text, now, offset).map(|t| format!("@{t}"))
}

/// The lines logs asks for: --tail's number, or the whole log, which following alone
/// would cut to the last ten.
fn log_lines(tail: Option<&str>, follow: bool) -> Result<Option<u64>> {
    match tail.filter(|t| *t != "all") {
        Some(tail) => tail
            .parse()
            .map(Some)
            .with_context(|| format!("invalid --tail {tail:?}: a number or all")),
        None if follow => Ok(Some(u64::from(u32::MAX))),
        None => Ok(None),
    }
}

/// The services an attached up follows the output of (AttachTo), and those it watches:
/// up ends once the machines of those it watches are down, and their exits are what
/// --abort-on-container-exit and --exit-code-from look at. It watches the services
/// named, else those it attaches to, else every one.
pub fn attach_sets(a: &UpArgs, project: &Project) -> Result<(BTreeSet<String>, BTreeSet<String>)> {
    let mut attach: BTreeSet<String> = if !a.attach.is_empty() {
        let unexpected: Vec<&str> = a
            .attach
            .iter()
            .filter(|s| !project.services.contains_key(*s))
            .map(String::as_str)
            .collect();
        if !unexpected.is_empty() {
            bail!(
                "cannot attach to services not included in up: {}",
                unexpected.join(", ")
            );
        }
        a.attach.iter().cloned().collect()
    } else {
        // The services launched, their dependencies too with --attach-dependencies,
        // but not those that opt out with attach: false.
        let mut launched: BTreeSet<String> = if a.service.is_empty() {
            project.services.keys().cloned().collect()
        } else {
            a.service.iter().cloned().collect()
        };
        if a.attach_dependencies && !a.service.is_empty() {
            let mut stack: Vec<String> = launched.iter().cloned().collect();
            while let Some(name) = stack.pop() {
                let Some(service) = project.services.get(&name).and_then(Value::as_map) else {
                    continue;
                };
                for dep in up::dependencies(service) {
                    if project.services.contains_key(&dep) && launched.insert(dep.clone()) {
                        stack.push(dep);
                    }
                }
            }
        }
        launched
            .into_iter()
            .filter(|s| {
                convert::service(project, s)
                    .map(|s| s.get("attach") != Some(&Value::Bool(false)))
                    .unwrap_or(true)
            })
            .collect()
    };
    for s in &a.no_attach {
        attach.remove(s);
    }
    let watched = if a.service.is_empty() {
        attach.clone()
    } else {
        a.service.iter().cloned().collect()
    };
    Ok((attach, watched))
}

/// up without -d: the output of the services' machines until those it watches are down,
/// or until Ctrl-C stops them; the exit code --exit-code-from and the abort flags ask
/// for, and 130 once a signal ended it.
pub async fn attach(
    e: &mut Engine<'_>,
    a: &UpArgs,
    started: &[Machine],
    since: std::time::SystemTime,
) -> Result<()> {
    let project = e.project().clone();
    // The services this up names, every one without a name: what an abort or a Ctrl+C
    // stops and kills, as stop and kill take Create.Services.
    let named: Vec<String> = if a.service.is_empty() {
        project.services.keys().cloned().collect()
    } else {
        a.service.clone()
    };
    let (attach_to, watching) = attach_sets(a, &project)?;
    let attached: Vec<Machine> = started
        .iter()
        .filter(|m| attach_to.contains(&m.service))
        .cloned()
        .collect();
    let mut names: Vec<String> = attached.iter().map(|m| display_name(&e.name, m)).collect();
    names.sort();
    println!("Attaching to {}", names.join(", "));
    // From before the machines started, to the microsecond, and every line since: a run
    // of a moment ago said its own lines.
    let since = since
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let mut options = Options::new();
    options.insert("follow", Bus::from(true));
    options.insert("lines", Bus::from(u64::from(u32::MAX)));
    options.insert(
        "since",
        Bus::from(format!("@{}.{:06}", since.as_secs(), since.subsec_micros())),
    );
    // compose's consumer stamps a line when it gets it.
    let stamp = if a.timestamps {
        Stamp::Received
    } else {
        Stamp::None
    };
    let (tx, mut rx) = mpsc::unbounded_channel();
    let color = colors(e, a.no_color);
    let mut followed = follow(e, &attached, &options, !a.no_log_prefix, color, stamp, &tx).await?;
    drop(tx);
    // The machines up watches, with the one followed for each that is.
    let mut watched: Vec<(Machine, Option<usize>, bool)> = started
        .iter()
        .filter(|m| watching.is_empty() || watching.contains(&m.service))
        .map(|m| {
            let i = followed.iter().position(|f| f.machine.name == m.name);
            (m.clone(), i, false)
        })
        .collect();
    let abort =
        a.abort_on_container_exit || a.abort_on_container_failure || a.exit_code_from.is_some();
    let mut code: Option<i32> = None;
    let mut stopping = false;
    let mut signalled = false;
    let mut streams = true;
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    // Made once: an interrupt during the graceful stop still counts.
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    while watched.iter().any(|(_, _, done)| !done) {
        tokio::select! {
            line = rx.recv(), if streams => match line {
                Some(Line::Text(i, err, text)) => print_line(&followed[i], err, &text),
                Some(Line::Closed(_)) => {}
                None => streams = false,
            },
            _ = tick.tick() => {
                // The streams of machines that are down end, watched or not.
                for f in followed.iter_mut().filter(|f| !f.done) {
                    let Ok(dict) = e.client.manager.get_machine(&f.machine.name).await else {
                        f.done = true;
                        continue;
                    };
                    let now = Machine::from_dict(dict);
                    if now.state == "stopped" && !watched.iter().any(|(m, _, _)| m.name == f.machine.name) {
                        f.done = true;
                        if let Some(ended) = f.ended.take() {
                            tokio::time::sleep(Duration::from_millis(300)).await;
                            let _ = ended.signal(15).await;
                        }
                    }
                }
                // The watched machines that went down since the last look.
                let mut down = Vec::new();
                for (i, (m, _, done)) in watched.iter_mut().enumerate().filter(|(_, w)| !w.2) {
                    let Ok(dict) = e.client.manager.get_machine(&m.name).await else {
                        *done = true;
                        continue;
                    };
                    let now = Machine::from_dict(dict);
                    let restarting = now.state == "restarting";
                    if now.state == "stopped" || (restarting && !m.state.eq("restarting")) {
                        down.push((i, now.exit_code.unwrap_or(0), restarting));
                    }
                    m.state = now.state;
                }
                if !down.is_empty() {
                    // Their last lines are on their way.
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    while let Ok(line) = rx.try_recv() {
                        if let Line::Text(i, err, text) = line {
                            print_line(&followed[i], err, &text);
                        }
                    }
                }
                let mut first_exit: Option<i32> = None;
                for (i, exit, restarting) in down {
                    let (m, f, done) = &mut watched[i];
                    let (name, color) = match f {
                        Some(f) => (followed[*f].name.clone(), followed[*f].color),
                        None => (display_name(&e.name, m), None),
                    };
                    let text = if restarting {
                        format!("\r\x1b[K{name} exited with code {exit} (restarting)\n")
                    } else {
                        format!("\r\x1b[K{name} exited with code {exit}\n")
                    };
                    let _ = std::io::stdout().write_all(paint(color, &text).as_bytes());
                    if restarting {
                        continue;
                    }
                    *done = true;
                    if let Some(f) = f {
                        followed[*f].done = true;
                        if let Some(ended) = followed[*f].ended.take() {
                            let _ = ended.signal(15).await;
                        }
                    }
                    // The first exit, or under --abort-on-container-failure the first
                    // that failed, wherever it is in the batch.
                    let counts = !a.abort_on_container_failure || exit != 0;
                    if first_exit.is_none() && counts {
                        first_exit = Some(exit);
                    }
                    if a.exit_code_from.as_deref() == Some(m.service.as_str()) {
                        code = Some(exit);
                    }
                }
                if let Some(exit) = first_exit {
                    if abort && !stopping {
                        stopping = true;
                        if code.is_none() && a.exit_code_from.is_none() {
                            code = Some(exit);
                        }
                        e.progress.event("Compose", "Stopping", "Aborting on container exit...", Status::Working);
                        stop_all(e, &project, &named, a.timeout).await?;
                    }
                }
            }
            _ = interrupt.recv() => {
                signalled = true;
                if stopping {
                    kill_all(e, &named).await;
                    break;
                }
                stopping = true;
                e.progress.event("Compose", "Stopping", "Gracefully Stopping... press Ctrl+C again to force", Status::Working);
                // A second Ctrl+C while the machines stop kills them.
                tokio::select! {
                    stopped = stop_all(e, &project, &named, a.timeout) => stopped?,
                    _ = interrupt.recv() => {
                        kill_all(e, &named).await;
                        break;
                    }
                }
            }
            _ = term.recv() => {
                signalled = true;
                stopping = true;
                stop_all(e, &project, &named, a.timeout).await?;
            }
        }
    }
    for f in &mut followed {
        if let Some(ended) = f.ended.take() {
            let _ = ended.signal(15).await;
        }
    }
    // A run a signal ended is one canceled, whatever the machines exited with.
    if signalled {
        std::process::exit(130);
    }
    if let Some(code) = code.filter(|c| *c != 0) {
        std::process::exit(code);
    }
    Ok(())
}

/// Stops every machine of the services named, dependents first.
async fn stop_all(
    e: &Engine<'_>,
    project: &crate::compose::project::Project,
    named: &[String],
    timeout: Option<i64>,
) -> Result<()> {
    let order = up::order(project);
    let mut machines = e.machines().await?;
    machines.sort_by_key(|m| {
        std::cmp::Reverse(order.iter().position(|s| *s == m.service).unwrap_or(0))
    });
    // The machines of this up's services, one waiting to be restarted among them.
    for m in machines
        .iter()
        .filter(|m| !m.oneoff && named.contains(&m.service) && busy(m))
    {
        e.progress.container(&m.name, "Stopping", Status::Working);
        super::lifecycle::pre_stop(e, m).await?;
        let mut options = Options::new();
        if let Some(t) = timeout {
            options.insert("timeout", Bus::from(crate::policy::stop_seconds(t)));
        }
        e.client
            .manager
            .stop_machine(&m.name, options)
            .await
            .map_err(client::error)?;
        e.progress.container(&m.name, "Stopped", Status::Done);
    }
    Ok(())
}

async fn kill_all(e: &Engine<'_>, named: &[String]) {
    let Ok(machines) = e.machines().await else {
        return;
    };
    for m in machines
        .iter()
        .filter(|m| named.contains(&m.service) && busy(m))
    {
        let mut options = Options::new();
        options.insert("signal", Bus::from("SIGKILL".to_string()));
        let _ = e.client.manager.kill_machine(&m.name, options).await;
    }
}

/// Whether a machine runs, or is about to: stopping it cancels a pending restart.
fn busy(m: &Machine) -> bool {
    m.running() || matches!(m.state.as_str(), "restarting" | "starting")
}

/// The machine of a service's replica that is running, or why there is none.
/// getSpecifiedContainer: of the machines of `service` in `machines`, the replica `index`
/// asks for (any when none does), the service's own before one-offs, the lowest number
/// first.
fn specified<'m>(
    machines: &'m [Machine],
    service: &str,
    index: Option<i64>,
) -> Result<&'m Machine> {
    let index = index.filter(|i| *i > 0);
    let mut mine: Vec<&Machine> = machines
        .iter()
        .filter(|m| m.service == service)
        .filter(|m| index.is_none_or(|i| i64::from(m.number) == i))
        .collect();
    mine.sort_by_key(|m| (m.oneoff, m.number));
    match (mine.first(), index) {
        (Some(m), _) => Ok(m),
        (None, Some(i)) => bail!(
            "service {} is not running container #{i}",
            crate::compose::gostr::quote(service)
        ),
        (None, None) => bail!(
            "service {} is not running",
            crate::compose::gostr::quote(service)
        ),
    }
}

/// exec's machine: a running one of the service, one-offs too.
async fn running_replica(e: &Engine<'_>, service: &str, index: Option<i64>) -> Result<Machine> {
    if let Some(project) = &e.project {
        if !project.services.contains_key(service) {
            bail!("no such service: {service}");
        }
    }
    let running: Vec<Machine> = e
        .machines()
        .await?
        .into_iter()
        .filter(Machine::running)
        .collect();
    specified(&running, service, index).cloned()
}

pub async fn exec(e: &mut Engine<'_>, a: &ExecArgs) -> Result<()> {
    let received = usize::from(a.service.is_some()) + a.command.len();
    let Some(service) = &a.service else {
        bail!("requires at least 2 arg(s), only received {received}");
    };
    if a.command.is_empty() {
        bail!("requires at least 2 arg(s), only received {received}");
    }
    let m = running_replica(e, service, a.index).await?;
    // -t is taken and does nothing, as in the reference: -T alone decides, and without
    // it a terminal is given when the output is one.
    let tty = a.no_tty.map(|no| !no);
    let spec = crate::commands::machines::ExecSpec {
        env: crate::volume::expand_env(&a.env)?,
        workdir: a.workdir.clone(),
        tty,
        detach: a.detach,
        no_stdin: a.interactive == Some(false),
    };
    let user = a.user.clone().unwrap_or_default();
    let code =
        crate::commands::machines::run_command(&e.client, &m.name, &a.command, &user, spec).await?;
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

/// A lifecycle hook of a service (post_start, pre_stop) run inside its machine.
pub async fn hook(e: &Engine<'_>, machine: &str, hook: &Map, kind: &str) -> Result<()> {
    let command: Vec<String> = hook
        .get("command")
        .and_then(Value::as_seq)
        .map(|c| c.iter().map(|v| v.to_string()).collect())
        .unwrap_or_default();
    if command.is_empty() {
        return Ok(());
    }
    let env: Vec<String> = hook
        .get("environment")
        .and_then(Value::as_map)
        .map(|m| {
            m.iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| format!("{k}={v}"))
                .collect()
        })
        .unwrap_or_default();
    let spec = crate::commands::machines::ExecSpec {
        env,
        workdir: hook
            .get("working_dir")
            .and_then(Value::as_str)
            .filter(|w| !w.is_empty())
            .map(String::from),
        tty: Some(false),
        detach: false,
        // A hook reads nothing: what comes on stdin is the command's own.
        no_stdin: true,
    };
    let user = hook
        .get("user")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let code =
        crate::commands::machines::run_command(&e.client, machine, &command, &user, spec).await?;
    if code != 0 {
        bail!("{machine} {kind} hook exited with status {code}");
    }
    Ok(())
}

/// run's -e as getEnvironment reads it: KEY=VALUE as given, a bare KEY from the
/// project's environment (the process's, .env and --env-file), and left out when that has
/// none, so that the service keeps its own.
fn run_env(
    given: &[String],
    environment: &crate::compose::dotenv::Env,
) -> BTreeMap<String, String> {
    given
        .iter()
        .filter_map(|item| match item.split_once('=') {
            Some((k, v)) => Some((k.to_string(), v.to_string())),
            None => environment.get(item).map(|v| (item.clone(), v.clone())),
        })
        .collect()
}

/// run's pseudo terminal, as its PreRunE decides it: -t gives one, -T none; with
/// neither, nor -i, a standard input that is no terminal gives none; else one when
/// standard output is a terminal. The service's own tty does not count.
fn run_tty(
    tty: Option<bool>,
    no_tty: Option<bool>,
    interactive: Option<bool>,
    stdin_terminal: bool,
    stdout_terminal: bool,
) -> bool {
    match (tty, no_tty) {
        (Some(tty), _) => tty,
        (None, Some(no_tty)) => !no_tty,
        (None, None) if interactive.is_none() && !stdin_terminal => false,
        (None, None) => stdout_terminal,
    }
}

pub async fn run(e: &mut Engine<'_>, a: &RunArgs) -> Result<()> {
    let Some(service_name) = a.service.clone() else {
        bail!("requires at least 1 arg(s), only received 0");
    };
    up::check_pull(a.pull.as_deref())?;
    if a.label.iter().any(|l| !l.contains('=')) {
        bail!("label must be set as KEY=VALUE");
    }
    let project = e.project().clone();
    let service = convert::service(&project, &service_name)?.clone();
    // What nspawn cannot do for the service it runs, and the ones it brings up first:
    // a resource only other services use refuses nothing here.
    let mut checked = project
        .clone()
        .with_selected_services(std::slice::from_ref(&service_name), a.no_deps)
        .map_err(anyhow::Error::msg)?;
    if !e.args.all_resources {
        checked.without_unnecessary_resources();
    }
    up::check_refusals(&checked)?;
    // The networks, volumes and secrets of the project as run loaded it, made as up makes
    // them: run's come with what the service depends on, which may be nothing.
    up::networks(e, false, None).await?;
    up::volumes(e).await?;
    up::secrets(e).await?;
    let flags = super::build::Flags {
        quiet: a.quiet_build,
        ..Default::default()
    };
    // The services it depends on come up first, as up would make them.
    if !a.no_deps {
        let deps = up::dependencies(&service);
        if !deps.is_empty() {
            let converge = Converge {
                services: deps,
                start: true,
                pull: a.pull.clone(),
                quiet_pull: a.quiet_pull,
                build: Some(flags.clone()),
                force_build: a.build,
                ..Default::default()
            };
            let mut deps_project = project.clone();
            let keep: Vec<String> = converge.services.clone();
            deps_project = deps_project
                .with_selected_services(&keep, false)
                .map_err(anyhow::Error::msg)?;
            let saved = e.project.replace(deps_project);
            let result = up::converge_project(e, &converge).await;
            e.project = saved;
            result?;
            // And the one-off starts once they are as its depends_on asks.
            up::wait_dependencies(e, &project, &service, &service_name, None).await?;
        }
    }
    let pulls = up::Pulls {
        flag: a.pull.as_deref(),
        quiet: a.quiet_pull,
        build: Some(&flags),
        force_build: a.build,
        ..Default::default()
    };
    let images = up::images(e, std::slice::from_ref(&service_name), &pulls).await?;
    let Some(image) = images.get(&service_name) else {
        bail!("service {service_name:?} has no image to run");
    };
    let name = match &a.name {
        Some(name) => name.clone(),
        None => {
            let mut bytes = [0u8; 6];
            std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom")?, &mut bytes)?;
            format!(
                "{}-{}-run-{}",
                project.name.replace('_', "-"),
                service_name.replace('_', "-"),
                hex::encode(bytes)
            )
        }
    };
    let spec = Spec {
        project: &project,
        service: &service_name,
        number: 1,
        name: name.clone(),
        oneoff: true,
        image_digest: image.digest.clone(),
        image_cmd: image.cmd.clone(),
        env_files: e.args.env_file.clone(),
        image_volumes: image.volumes.clone(),
    };
    let mut made = convert::options(&spec)?;
    let o = &mut made.options;
    if a.service_ports {
        let full = Spec {
            oneoff: false,
            ..spec
        };
        if let Some(publish) = convert::options(&full)?.options.remove("publish") {
            o.insert("publish", publish);
        }
    }
    if !a.publish.is_empty() {
        o.insert("publish", Bus::from(a.publish.clone()));
    }
    if !a.command.is_empty() {
        if o.get("entrypoint").is_some() && a.entrypoint.is_none() {
            // The service's entrypoint, then the command given.
            let entry: Vec<String> = service
                .get("entrypoint")
                .and_then(Value::as_seq)
                .map(|e| e.iter().map(|v| v.to_string()).collect())
                .unwrap_or_default();
            let mut argv = entry;
            argv.extend(a.command.iter().cloned());
            o.insert("command", Bus::from(argv));
        } else {
            o.remove("entrypoint");
            o.insert("command", Bus::from(a.command.clone()));
        }
    }
    if let Some(entrypoint) = &a.entrypoint {
        let words = crate::compose::golib::shell_words(entrypoint).map_err(anyhow::Error::msg)?;
        let mut argv = words;
        argv.extend(a.command.iter().cloned());
        // --entrypoint "" alone keeps the image's command, as an empty entrypoint of the
        // file does.
        if argv.is_empty() {
            argv.clone_from(&image.cmd);
        }
        o.insert("entrypoint", Bus::from(String::new()));
        o.insert("command", Bus::from(argv));
    }
    if !a.env.is_empty() || !a.env_from_file.is_empty() {
        let mut env: BTreeMap<String, String> = match o.remove("env") {
            Some(Bus::Array(items)) => items
                .iter()
                .filter_map(|v| String::try_from(v.clone()).ok())
                .filter_map(|l| {
                    l.split_once('=')
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                })
                .collect(),
            _ => BTreeMap::new(),
        };
        let flags = run_env(&a.env, &project.environment);
        env.extend(env_from_files(flags, &a.env_from_file)?);
        o.insert(
            "env",
            Bus::from(
                env.into_iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>(),
            ),
        );
    }
    if !a.label.is_empty() {
        let mut labels: Vec<String> = match o.remove("label") {
            Some(Bus::Array(items)) => items
                .iter()
                .filter_map(|v| String::try_from(v.clone()).ok())
                .collect(),
            _ => Vec::new(),
        };
        labels.extend(a.label.iter().cloned());
        o.insert("label", Bus::from(labels));
    }
    if let Some(user) = &a.user {
        o.insert("user", Bus::from(user.clone()));
    }
    if let Some(workdir) = &a.workdir {
        o.insert("working_dir", Bus::from(workdir.clone()));
    }
    if !a.volume.is_empty() {
        let mut volumes: Vec<String> = match o.remove("volume") {
            Some(Bus::Array(items)) => items
                .iter()
                .filter_map(|v| String::try_from(v.clone()).ok())
                .collect(),
            _ => Vec::new(),
        };
        for volume in &a.volume {
            volumes.push(run_volume(
                e.project(),
                &e.cwd,
                volume,
                &name,
                &mut made.anonymous,
            )?);
        }
        o.insert("volume", Bus::from(volumes));
    }
    if !a.cap_add.is_empty() || !a.cap_drop.is_empty() {
        let mut taken = |key: &str| -> Vec<String> {
            match o.remove(key) {
                Some(Bus::Array(items)) => items
                    .iter()
                    .filter_map(|v| String::try_from(v.clone()).ok())
                    .collect(),
                _ => Vec::new(),
            }
        };
        let (add, drop) = merge_caps(taken("cap_add"), taken("cap_drop"), &a.cap_add, &a.cap_drop);
        for (key, list) in [("cap_add", add), ("cap_drop", drop)] {
            if !list.is_empty() {
                o.insert(key, Bus::from(list));
            }
        }
    }
    e.progress.container(&name, "Creating", Status::Working);
    let manager = &e.client.manager;
    let options = made.options;
    for path in &made.host_paths {
        std::fs::create_dir_all(path).with_context(|| format!("creating {path}"))?;
    }
    up::anonymous_volumes(e, &made.anonymous, false).await?;
    e.client
        .run_job_silent(|| manager.create_machine(&image.name, &name, options), true)
        .await
        .with_context(|| format!("creating {name}"))?;
    e.progress.container(&name, "Created", Status::Done);
    let mut start = Options::new();
    if a.rm {
        start.insert("remove", Bus::from(true));
    }
    if a.detach {
        start.insert("wait", Bus::from(true));
        let started = e
            .client
            .manager
            .start_machine(&name, start)
            .await
            .map_err(client::error);
        if let Err(err) = started {
            if a.rm {
                crate::commands::machines::remove_unless_running(&e.client, &name).await;
            }
            return Err(err);
        }
        println!("{name}");
        return Ok(());
    }
    let tty = run_tty(
        a.tty,
        a.no_tty,
        a.interactive,
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
    );
    let interactive = a.interactive.unwrap_or(true);
    let code = match crate::commands::machines::attached(&e.client, &name, start, tty, interactive)
        .await
    {
        Ok(code) => code,
        Err(err) => {
            if a.rm {
                crate::commands::machines::remove_unless_running(&e.client, &name).await;
            }
            return Err(err);
        }
    };
    // The removal is waited for: a down right after finds the project's networks free.
    // Its anonymous volumes go with it, as with --rm they do.
    if a.rm {
        crate::commands::machines::removed(&e.client, &name).await;
        up::remove_anonymous(e, &name).await?;
    }
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

/// A `run -v` as compose's buildMount takes it: a relative bind source from the command
/// line's working directory (filepath.Abs), a volume of the project by its name.
/// One without a source is an anonymous volume of `machine`, which `anonymous` gets.
fn run_volume(
    project: &crate::compose::project::Project,
    cwd: &str,
    text: &str,
    machine: &str,
    anonymous: &mut Vec<String>,
) -> Result<String> {
    let spec = crate::compose::mount::parse(text).map_err(anyhow::Error::msg)?;
    let ro = if spec.read_only { ":ro" } else { "" };
    if spec.source.is_empty() {
        let name = convert::anonymous_volume(machine, &spec.target);
        anonymous.push(name.clone());
        return Ok(format!("{name}:{}{ro}", spec.target));
    }
    let source = if spec.kind == "bind" {
        crate::compose::paths::abs(&spec.source, cwd)
    } else if project.volumes.contains_key(&spec.source) {
        match convert::volume_device(project, &spec.source)? {
            Some(device) => {
                if device.read_only {
                    return Ok(format!("{}:{}:ro", device.path, spec.target));
                }
                device.path
            }
            None => convert::volume_name(project, &spec.source),
        }
    } else {
        spec.source
    };
    Ok(format!("{source}:{}{ro}", spec.target))
}

/// applyRunOptions: run's --cap-add goes after the service's and takes the cap out of
/// its cap_drop, --cap-drop the other way round.
fn merge_caps(
    mut add: Vec<String>,
    mut drop: Vec<String>,
    run_add: &[String],
    run_drop: &[String],
) -> (Vec<String>, Vec<String>) {
    if !run_add.is_empty() {
        add.extend(run_add.iter().cloned());
        drop.retain(|c| !run_add.contains(c));
    }
    if !run_drop.is_empty() {
        drop.extend(run_drop.iter().cloned());
        add.retain(|c| !run_drop.contains(c));
    }
    (add, drop)
}

/// splitCpArg: SERVICE:PATH, or a local path: an absolute one, one without a colon, or
/// one whose part before it starts with a dot (./file:name).
fn split_cp_arg(arg: &str) -> (&str, &str) {
    if arg.starts_with('/') {
        return ("", arg);
    }
    match arg.split_once(':') {
        Some((service, path)) if !service.starts_with('.') => (service, path),
        _ => ("", arg),
    }
}

/// listContainersTargetedForCopy: the replica --index names, in whatever state; else
/// every machine of the service (one-offs with --all), the first only to copy from it.
fn copy_targets<'m>(
    machines: &'m [Machine],
    service: &str,
    index: Option<i64>,
    all: bool,
    from: bool,
) -> Result<Vec<&'m Machine>> {
    if index.is_some_and(|i| i > 0) {
        let own: Vec<Machine> = machines.iter().filter(|m| !m.oneoff).cloned().collect();
        let name = specified(&own, service, index)?.name.clone();
        return Ok(machines.iter().filter(|m| m.name == name).collect());
    }
    let mut mine: Vec<&Machine> = machines
        .iter()
        .filter(|m| m.service == service && (all || !m.oneoff))
        .collect();
    mine.sort_by_key(|m| (m.oneoff, m.number));
    if mine.is_empty() {
        bail!(
            "no container found for service {}",
            crate::compose::gostr::quote(service)
        );
    }
    if from {
        mine.truncate(1);
    }
    Ok(mine)
}

pub async fn cp(e: &mut Engine<'_>, a: &CpArgs) -> Result<()> {
    let (Some(source), Some(destination)) = (&a.source, &a.destination) else {
        bail!("'docker compose cp' requires 2 arguments");
    };
    let (from_service, from_path) = split_cp_arg(source);
    let (to_service, to_path) = split_cp_arg(destination);
    let (service, from) = match (from_service.is_empty(), to_service.is_empty()) {
        (false, false) => bail!("copying between services is not supported"),
        (true, true) => bail!("unknown copy direction"),
        (false, true) => (from_service, true),
        (true, false) => (to_service, false),
    };
    let machines = e.machines().await?;
    for m in copy_targets(&machines, service, a.index, a.all, from)? {
        let (args, message) = if from {
            (
                crate::cli::CpArgs {
                    source: format!("{}:{from_path}", m.name),
                    destination: to_path.to_string(),
                },
                format!("{}:{from_path} to {to_path}", m.name),
            )
        } else {
            (
                crate::cli::CpArgs {
                    source: from_path.to_string(),
                    destination: format!("{}:{to_path}", m.name),
                },
                format!("{from_path} to {}:{to_path}", m.name),
            )
        };
        e.progress
            .event(&m.name, "Copying", &message, Status::Working);
        crate::commands::copy::cp(args, &e.client).await?;
        e.progress.event(&m.name, "Copied", &message, Status::Done);
    }
    Ok(())
}

#[cfg(test)]
mod tests {

    #[test]
    fn run_takes_its_terminal_and_variables_as_docker_compose_does() {
        // -t, then -T (-T=false gives one), then no flag: none from a pipe, one on a
        // terminal.
        assert!(run_tty(Some(true), None, None, false, false));
        assert!(!run_tty(None, Some(true), None, true, true));
        assert!(run_tty(None, Some(false), None, false, false));
        assert!(!run_tty(None, None, None, false, true));
        assert!(run_tty(None, None, None, true, true));
        assert!(!run_tty(None, None, None, true, false));
        // -i given: the pipe does not count, the output does.
        assert!(run_tty(None, None, Some(true), false, true));
        let environment = BTreeMap::from([("FROM_DOTENV".to_string(), "1".to_string())]);
        let env = run_env(
            &[
                "A=b".to_string(),
                "FROM_DOTENV".to_string(),
                "NOWHERE".to_string(),
                "E=".to_string(),
            ],
            &environment,
        );
        assert_eq!(
            env,
            BTreeMap::from([
                ("A".to_string(), "b".to_string()),
                ("E".to_string(), String::new()),
                ("FROM_DOTENV".to_string(), "1".to_string()),
            ]),
            "a variable set nowhere is left to the service"
        );
    }

    fn replica(service: &str, number: u32, oneoff: bool) -> Machine {
        Machine {
            name: if oneoff {
                format!("p-{service}-run-{number}")
            } else {
                format!("p-{service}-{number}")
            },
            service: service.to_string(),
            number,
            oneoff,
            state: "running".to_string(),
            health: String::new(),
            exit_code: None,
            labels: BTreeMap::new(),
            created: 0,
            started: 0,
            dict: crate::client::Dict::new(),
        }
    }

    #[test]
    fn exec_and_cp_take_the_replicas_docker_compose_takes() {
        let machines = [
            replica("web", 2, false),
            replica("web", 3, false),
            replica("web", 1, true),
            replica("db", 1, false),
        ];
        let name = |m: Result<&Machine>| m.map(|m| m.name.clone()).map_err(|e| e.to_string());
        // The lowest number of the service's own, whatever the first is.
        assert_eq!(
            name(specified(&machines, "web", None)),
            Ok("p-web-2".to_string())
        );
        assert_eq!(
            name(specified(&machines, "web", Some(3))),
            Ok("p-web-3".to_string())
        );
        assert_eq!(
            name(specified(&machines, "web", Some(1))),
            Ok("p-web-run-1".to_string()),
            "exec takes a one-off of that number"
        );
        assert_eq!(
            name(specified(&machines, "web", Some(9))),
            Err("service \"web\" is not running container #9".to_string())
        );
        assert_eq!(
            name(specified(&machines, "cache", None)),
            Err("service \"cache\" is not running".to_string())
        );
        let names = |r: Result<Vec<&Machine>>| {
            r.map(|v| v.iter().map(|m| m.name.clone()).collect::<Vec<_>>())
                .map_err(|e| e.to_string())
        };
        // Into every replica, from the first; one-offs only with --all.
        assert_eq!(
            names(copy_targets(&machines, "web", None, false, false)),
            Ok(vec!["p-web-2".to_string(), "p-web-3".to_string()])
        );
        assert_eq!(
            names(copy_targets(&machines, "web", None, true, false)),
            Ok(vec![
                "p-web-2".to_string(),
                "p-web-3".to_string(),
                "p-web-run-1".to_string()
            ])
        );
        assert_eq!(
            names(copy_targets(&machines, "web", None, false, true)),
            Ok(vec!["p-web-2".to_string()])
        );
        assert_eq!(
            names(copy_targets(&machines, "web", Some(1), false, false)),
            Err("service \"web\" is not running container #1".to_string()),
            "an index names a replica of the service's own"
        );
        assert_eq!(
            names(copy_targets(&machines, "cache", None, false, false)),
            Err("no container found for service \"cache\"".to_string())
        );
        assert_eq!(split_cp_arg("web:/etc/hosts"), ("web", "/etc/hosts"));
        assert_eq!(split_cp_arg("/tmp/a:b"), ("", "/tmp/a:b"));
        assert_eq!(split_cp_arg("./a:b"), ("", "./a:b"));
        assert_eq!(split_cp_arg("file"), ("", "file"));
        assert_eq!(split_cp_arg("dir/sub:x"), ("dir/sub", "x"));
    }

    #[test]
    fn an_attached_up_follows_and_watches_what_docker_compose_does() {
        let p = convert::tests::project(
            "attach",
            "services:\n  db:\n    image: x\n  web:\n    image: x\n    depends_on: [db]\n  quiet:\n    image: x\n    attach: false\n",
        );
        let set = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>();
        let names = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let sets = |a: UpArgs| attach_sets(&a, &p).map_err(|e| e.to_string());
        // Every service but those with attach: false, all of them watched.
        assert_eq!(
            sets(UpArgs::default()),
            Ok((set(&["db", "web"]), set(&["db", "web"])))
        );
        // A service named: its output, and up ends with it.
        let web = UpArgs {
            service: names(&["web"]),
            ..Default::default()
        };
        assert_eq!(sets(web), Ok((set(&["web"]), set(&["web"]))));
        // --attach-dependencies shows the dependencies' output and still ends with web.
        let deps = UpArgs {
            service: names(&["web"]),
            attach_dependencies: true,
            ..Default::default()
        };
        assert_eq!(sets(deps), Ok((set(&["db", "web"]), set(&["web"]))));
        // Nothing to attach to watches everything.
        let none = UpArgs {
            no_attach: names(&["db", "web"]),
            ..Default::default()
        };
        assert_eq!(sets(none), Ok((set(&[]), set(&[]))));
        let unknown = UpArgs {
            attach: names(&["wbe"]),
            ..Default::default()
        };
        assert_eq!(
            sets(unknown),
            Err("cannot attach to services not included in up: wbe".to_string())
        );
        let quiet = UpArgs {
            attach: names(&["quiet"]),
            ..Default::default()
        };
        assert_eq!(sets(quiet), Ok((set(&["quiet"]), set(&["quiet"]))));
    }

    use super::*;

    #[test]
    fn a_run_volume_is_read_as_compose_reads_it() {
        let p = crate::compose::engine::convert::tests::project(
            "runvol",
            "services:\n  web:\n    image: x\n    volumes: [data:/data]\nvolumes:\n  data: {}\n",
        );
        let mut anonymous = Vec::new();
        let mut rv =
            |text: &str| run_volume(&p, "/elsewhere", text, "runvol-web-run-1", &mut anonymous);
        let named = rv("data:/backup").unwrap();
        assert!(named.ends_with("_data:/backup"), "{named}");
        assert_eq!(rv("other:/o:ro").unwrap(), "other:/o:ro");
        // From where the command runs, not the project's directory.
        assert_eq!(rv("./conf/../site:/srv").unwrap(), "/elsewhere/site:/srv");
        assert_eq!(rv("/abs//x/:/srv").unwrap(), "/abs/x:/srv");
        // No source: an anonymous volume of the one-off.
        let x = convert::anonymous_volume("runvol-web-run-1", "/x");
        assert_eq!(rv("/x").unwrap(), format!("{x}:/x"));
        assert_eq!(anonymous, [x]);
    }

    #[test]
    fn run_caps_are_merged_as_compose_merges_them() {
        let v = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            merge_caps(
                v(&["SYS_PTRACE"]),
                v(&["NET_RAW", "CHOWN"]),
                &v(&["NET_RAW"]),
                &[]
            ),
            (v(&["SYS_PTRACE", "NET_RAW"]), v(&["CHOWN"]))
        );
        assert_eq!(
            merge_caps(
                v(&["SYS_PTRACE", "NET_ADMIN"]),
                v(&[]),
                &[],
                &v(&["NET_ADMIN"])
            ),
            (v(&["SYS_PTRACE"]), v(&["NET_ADMIN"]))
        );
    }

    #[test]
    fn the_first_env_file_that_sets_a_variable_wins() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base.env");
        let local = dir.path().join("local.env");
        std::fs::write(&base, "DB_HOST=base\nPORT=1\n").unwrap();
        std::fs::write(
            &local,
            "DB_HOST=local\nURL=${DB_HOST}:${PORT}\nTOKEN=file\n",
        )
        .unwrap();
        let flags = BTreeMap::from([("TOKEN".to_string(), "flag".to_string())]);
        let files = [base.display().to_string(), local.display().to_string()];
        let env = env_from_files(flags, &files).unwrap();
        assert_eq!(env["DB_HOST"], "base");
        // compose-go's expansion asks the lookup (-e, the earlier files) before the file.
        assert_eq!(env["URL"], "base:1");
        assert_eq!(env["TOKEN"], "flag");
        let missing = dir.path().join("none.env").display().to_string();
        assert!(env_from_files(BTreeMap::new(), &[missing])
            .unwrap_err()
            .to_string()
            .starts_with("open "));
    }

    #[test]
    fn times_are_read_as_the_reference_reads_them() {
        assert_eq!(journal_time("42m", 10_000.0).unwrap(), "@7480");
        assert_eq!(journal_time("1h30m", 10_000.0).unwrap(), "@4600");
        assert_eq!(journal_time("1700000000", 0.0).unwrap(), "@1700000000");
        assert_eq!(journal_time("1700000000.5", 0.0).unwrap(), "@1700000000.5");
        assert_eq!(
            journal_time("2026-10-19T07:00:00Z", 0.0).unwrap(),
            "@1792393200.000000000"
        );
        // What journalctl reads but the flags do not take is refused before it gets
        // there.
        assert!(journal_time("2026-10-19 07:00:00", 0.0).is_err());
        assert!(journal_time("yesterday", 0.0).is_err());
    }

    #[test]
    fn logs_take_the_whole_log_unless_told() {
        assert_eq!(log_lines(None, false).unwrap(), None);
        assert_eq!(log_lines(None, true).unwrap(), Some(u64::from(u32::MAX)));
        assert_eq!(
            log_lines(Some("all"), true).unwrap(),
            Some(u64::from(u32::MAX))
        );
        assert_eq!(log_lines(Some("5"), true).unwrap(), Some(5));
        assert!(log_lines(Some("x"), false).is_err());
    }
}
