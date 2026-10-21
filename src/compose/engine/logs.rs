//! What a project's machines say and run: logs, up attached to its services, exec,
//! run, cp, and the lifecycle hooks of a service.

use std::collections::BTreeMap;
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

/// Whether the output gets colors: --ansi or COMPOSE_ANSI, NO_COLOR, and a terminal on
/// standard output otherwise.
fn colors(e: &Engine<'_>, no_color: bool) -> bool {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    if no_color || env("NO_COLOR").is_some() || e.args.no_ansi {
        return false;
    }
    let ansi = env("COMPOSE_ANSI")
        .or_else(|| e.args.ansi.clone())
        .unwrap_or_else(|| "auto".to_string());
    match ansi.as_str() {
        "always" => true,
        "never" => false,
        _ => std::io::stdout().is_terminal(),
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

/// Starts following the output of `machines` from journalctl through the service, each
/// line to the channel.
async fn follow(
    e: &Engine<'_>,
    machines: &[Machine],
    options: &Options<'static>,
    prefix: bool,
    color: bool,
    tx: &mpsc::UnboundedSender<Line>,
) -> Result<Vec<Followed>> {
    let names: Vec<String> = machines.iter().map(|m| display_name(&e.name, m)).collect();
    let width = names.iter().map(String::len).max().unwrap_or(0) + 1;
    let mut followed = Vec::new();
    for (i, (m, name)) in machines.iter().zip(names).enumerate() {
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

pub async fn logs(e: &mut Engine<'_>, a: &LogsArgs) -> Result<()> {
    let mut machines: Vec<Machine> = e
        .machines()
        .await?
        .into_iter()
        .filter(|m| a.service.is_empty() || a.service.contains(&m.service))
        .collect();
    if let Some(index) = a.index.filter(|i| *i > 0) {
        machines.retain(|m| m.number == index as u32);
    }
    if let Some(project) = &e.project {
        for name in &a.service {
            if !project.services.contains_key(name) && !project.disabled_services.contains_key(name)
            {
                bail!("no such service: {name}");
            }
        }
        if a.service.is_empty() {
            // Services with attach: false are left out unless named.
            machines.retain(|m| {
                convert::service(project, &m.service)
                    .map(|s| s.get("attach") != Some(&Value::Bool(false)))
                    .unwrap_or(true)
            });
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
    options.insert("timestamps", Bus::from(a.timestamps));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let color = colors(e, a.no_color);
    let mut followed = follow(e, &machines, &options, !a.no_log_prefix, color, &tx).await?;
    drop(tx);
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    let mut stopped = false;
    loop {
        tokio::select! {
            line = rx.recv() => match line {
                Some(Line::Text(i, err, text)) => print_line(&followed[i], err, &text),
                Some(Line::Closed(i)) => {
                    followed[i].open = followed[i].open.saturating_sub(1);
                }
                None => break,
            },
            _ = tick.tick(), if a.follow => {
                // Following ends once every machine followed is down for good.
                let mut all_down = true;
                for f in &followed {
                    let dict = e.client.manager.get_machine(&f.machine.name).await;
                    let down = dict
                        .map(|d| Machine::from_dict(d).state == "stopped")
                        .unwrap_or(true);
                    all_down &= down;
                }
                if all_down {
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
            _ = tokio::signal::ctrl_c() => {
                stopped = true;
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

/// up without -d: the output of the services' machines until they are all down, or
/// until Ctrl-C stops them; the exit code --exit-code-from and the abort flags ask for.
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
    let deps: Vec<String> = if a.attach_dependencies || !a.attach.is_empty() {
        Vec::new()
    } else {
        // Without --attach-dependencies, the services named are attached, not those
        // they brought up.
        project
            .services
            .keys()
            .filter(|s| !named.contains(s))
            .cloned()
            .collect()
    };
    let attached: Vec<Machine> = started
        .iter()
        .filter(|m| a.attach.is_empty() || a.attach.contains(&m.service))
        .filter(|m| !a.no_attach.contains(&m.service))
        .filter(|m| !deps.contains(&m.service))
        .filter(|m| {
            !a.attach.is_empty()
                || convert::service(&project, &m.service)
                    .map(|s| s.get("attach") != Some(&Value::Bool(false)))
                    .unwrap_or(true)
        })
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
    options.insert("timestamps", Bus::from(a.timestamps));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let color = colors(e, a.no_color);
    let mut followed = follow(e, &attached, &options, !a.no_log_prefix, color, &tx).await?;
    drop(tx);
    let abort =
        a.abort_on_container_exit || a.abort_on_container_failure || a.exit_code_from.is_some();
    let mut code: Option<i32> = None;
    let mut stopping = false;
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    // Made once: an interrupt during the graceful stop still counts.
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    loop {
        tokio::select! {
            line = rx.recv() => match line {
                Some(Line::Text(i, err, text)) => print_line(&followed[i], err, &text),
                Some(Line::Closed(_)) => {}
                None => break,
            },
            _ = tick.tick() => {
                // The machines that went down since the last look.
                let mut down = Vec::new();
                for (i, f) in followed.iter_mut().enumerate().filter(|(_, f)| !f.done) {
                    let Ok(dict) = e.client.manager.get_machine(&f.machine.name).await else {
                        f.done = true;
                        continue;
                    };
                    let now = Machine::from_dict(dict);
                    let restarting = now.state == "restarting";
                    if now.state == "stopped" || (restarting && !f.machine.state.eq("restarting")) {
                        down.push((i, now.exit_code.unwrap_or(0), restarting));
                    }
                    f.machine.state = now.state;
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
                    let f = &mut followed[i];
                    let text = if restarting {
                        format!("\r\x1b[K{} exited with code {exit} (restarting)\n", f.name)
                    } else {
                        format!("\r\x1b[K{} exited with code {exit}\n", f.name)
                    };
                    let _ = std::io::stdout().write_all(paint(f.color, &text).as_bytes());
                    if restarting {
                        continue;
                    }
                    f.done = true;
                    if let Some(ended) = f.ended.take() {
                        let _ = ended.signal(15).await;
                    }
                    // The first exit, or under --abort-on-container-failure the first
                    // that failed, wherever it is in the batch.
                    let counts = !a.abort_on_container_failure || exit != 0;
                    if first_exit.is_none() && counts {
                        first_exit = Some(exit);
                    }
                    if a.exit_code_from.as_deref() == Some(f.machine.service.as_str()) {
                        code = Some(exit);
                    }
                }
                if let Some(exit) = first_exit {
                    let triggers = abort;
                    if triggers && !stopping {
                        stopping = true;
                        if code.is_none() && a.exit_code_from.is_none() {
                            code = Some(exit);
                        }
                        e.progress.event("Compose", "Stopping", "Aborting on container exit...", Status::Working);
                        stop_all(e, &project, &named, a.timeout).await?;
                    }
                }
                if followed.iter().all(|f| f.done) {
                    break;
                }
            }
            _ = interrupt.recv() => {
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
async fn running_replica(e: &Engine<'_>, service: &str, index: Option<i64>) -> Result<Machine> {
    if let Some(project) = &e.project {
        if !project.services.contains_key(service) {
            bail!("no such service: {service}");
        }
    }
    let index = index.filter(|i| *i > 0).unwrap_or(1) as u32;
    let machines = e.machines().await?;
    match machines
        .into_iter()
        .find(|m| m.service == service && !m.oneoff && m.number == index)
    {
        Some(m) if m.running() => Ok(m),
        _ => bail!("service {service:?} is not running"),
    }
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

pub async fn run(e: &mut Engine<'_>, a: &RunArgs) -> Result<()> {
    let Some(service_name) = a.service.clone() else {
        bail!("requires at least 1 arg(s), only received 0");
    };
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
    up::networks(e).await?;
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
            up::wait_dependencies(e, &project, &service, &service_name).await?;
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
        let flags: BTreeMap<String, String> = crate::volume::expand_env(&a.env)?
            .iter()
            .filter_map(|item| {
                item.split_once('=')
                    .map(|(k, v)| (k.to_string(), v.to_string()))
            })
            .collect();
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
            volumes.push(run_volume(e.project(), &e.cwd, volume)?);
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
    let terminal = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let tty = match (a.no_tty, a.tty) {
        (Some(true), _) | (_, Some(false)) => false,
        (_, Some(true)) => true,
        _ => terminal && service.get("tty") != Some(&Value::Bool(false)),
    };
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
    if a.rm {
        crate::commands::machines::removed(&e.client, &name).await;
    }
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

/// A `run -v` as compose's buildMount takes it: a relative bind source from the command
/// line's working directory (filepath.Abs), a volume of the project by its name.
fn run_volume(project: &crate::compose::project::Project, cwd: &str, text: &str) -> Result<String> {
    let spec = crate::compose::mount::parse(text).map_err(anyhow::Error::msg)?;
    if spec.source.is_empty() {
        bail!("{text}: a volume without a source is an anonymous one, which nspawn does not have: name it");
    }
    let ro = if spec.read_only { ":ro" } else { "" };
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

pub async fn cp(e: &mut Engine<'_>, a: &CpArgs) -> Result<()> {
    let (Some(source), Some(destination)) = (&a.source, &a.destination) else {
        bail!("'docker compose cp' requires 2 arguments");
    };
    let machines = e.machines().await?;
    let index = a.index.filter(|i| *i > 0).unwrap_or(1) as u32;
    let resolve = |side: &str| -> Result<Vec<String>> {
        let Some((service, path)) = side.split_once(':') else {
            return Ok(vec![side.to_string()]);
        };
        if service.contains('/') || service.is_empty() {
            return Ok(vec![side.to_string()]);
        }
        let mine: Vec<&Machine> = machines
            .iter()
            .filter(|m| m.service == service && (a.all || !m.oneoff))
            .filter(|m| a.all || m.number == index)
            .collect();
        if mine.is_empty() {
            bail!("no container found for service {service:?}");
        }
        Ok(mine.iter().map(|m| format!("{}:{path}", m.name)).collect())
    };
    let sources = resolve(source)?;
    let destinations = resolve(destination)?;
    for s in &sources {
        for d in &destinations {
            let args = crate::cli::CpArgs {
                source: s.clone(),
                destination: d.clone(),
            };
            crate::commands::copy::cp(args, &e.client).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_volume_is_read_as_compose_reads_it() {
        let p = crate::compose::engine::convert::tests::project(
            "runvol",
            "services:\n  web:\n    image: x\n    volumes: [data:/data]\nvolumes:\n  data: {}\n",
        );
        let named = run_volume(&p, "/elsewhere", "data:/backup").unwrap();
        assert!(named.ends_with("_data:/backup"), "{named}");
        assert_eq!(
            run_volume(&p, "/elsewhere", "other:/o:ro").unwrap(),
            "other:/o:ro"
        );
        // From where the command runs, not the project's directory.
        let bind = run_volume(&p, "/elsewhere", "./conf/../site:/srv").unwrap();
        assert_eq!(bind, "/elsewhere/site:/srv");
        assert_eq!(
            run_volume(&p, "/elsewhere", "/abs//x/:/srv").unwrap(),
            "/abs/x:/srv"
        );
        assert!(run_volume(&p, "/elsewhere", "/x").is_err());
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
