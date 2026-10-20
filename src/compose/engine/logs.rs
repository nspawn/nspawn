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
    if let Some(tail) = a.tail.as_deref().filter(|t| *t != "all") {
        let n: u64 = tail
            .parse()
            .with_context(|| format!("invalid --tail {tail:?}: a number or all"))?;
        options.insert("lines", Bus::from(n));
    }
    if let Some(since) = &a.since {
        options.insert("since", Bus::from(since.clone()));
    }
    if let Some(until) = &a.until {
        options.insert("until", Bus::from(until.clone()));
    }
    options.insert("timestamps", Bus::from(a.timestamps));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let color = colors(e, a.no_color);
    let mut followed = follow(e, &machines, &options, !a.no_log_prefix, color, &tx).await?;
    drop(tx);
    let mut tick = tokio::time::interval(Duration::from_millis(500));
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
                    break;
                }
            }
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    for f in &mut followed {
        if let Some(ended) = f.ended.take() {
            let _ = ended.signal(15).await;
        }
    }
    Ok(())
}

/// up without -d: the output of the services' machines until they are all down, or
/// until Ctrl-C stops them; the exit code --exit-code-from and the abort flags ask for.
pub async fn attach(e: &mut Engine<'_>, a: &UpArgs, started: &[Machine]) -> Result<()> {
    let project = e.project().clone();
    let deps: Vec<String> = if a.attach_dependencies || !a.attach.is_empty() {
        Vec::new()
    } else {
        // Without --attach-dependencies, the services named are attached, not those
        // they brought up.
        let named: Vec<String> = if a.service.is_empty() {
            project.services.keys().cloned().collect()
        } else {
            a.service.clone()
        };
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
    // From now on, to the microsecond: a run of a moment ago said its own lines.
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let mut options = Options::new();
    options.insert("follow", Bus::from(true));
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
                    if first_exit.is_none() {
                        first_exit = Some(exit);
                    }
                    if a.exit_code_from.as_deref() == Some(f.machine.service.as_str()) {
                        code = Some(exit);
                    }
                }
                if let Some(exit) = first_exit {
                    let triggers = if a.abort_on_container_failure { exit != 0 } else { abort };
                    if triggers && !stopping {
                        stopping = true;
                        if code.is_none() && a.exit_code_from.is_none() {
                            code = Some(exit);
                        }
                        e.progress.event("Compose", "Stopping", "Aborting on container exit...", Status::Working);
                        stop_all(e, &project, a.timeout).await?;
                    }
                }
                if followed.iter().all(|f| f.done) {
                    break;
                }
            }
            _ = tokio::signal::ctrl_c() => {
                if stopping {
                    kill_all(e).await;
                    break;
                }
                stopping = true;
                e.progress.event("Compose", "Stopping", "Gracefully Stopping... press Ctrl+C again to force", Status::Working);
                stop_all(e, &project, a.timeout).await?;
            }
            _ = term.recv() => {
                stopping = true;
                stop_all(e, &project, a.timeout).await?;
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

/// Stops every machine of the project's services, dependents first.
async fn stop_all(
    e: &Engine<'_>,
    project: &crate::compose::project::Project,
    timeout: Option<i64>,
) -> Result<()> {
    let order = up::order(project);
    let mut machines = e.machines().await?;
    machines.sort_by_key(|m| {
        std::cmp::Reverse(order.iter().position(|s| *s == m.service).unwrap_or(0))
    });
    for m in machines.iter().filter(|m| m.running() && !m.oneoff) {
        e.progress.container(&m.name, "Stopping", Status::Working);
        let mut options = Options::new();
        if let Some(t) = timeout {
            options.insert("timeout", Bus::from(t.max(0) as u64));
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

async fn kill_all(e: &Engine<'_>) {
    let Ok(machines) = e.machines().await else {
        return;
    };
    for m in machines.iter().filter(|m| m.running()) {
        let mut options = Options::new();
        options.insert("signal", Bus::from("SIGKILL".to_string()));
        let _ = e.client.manager.kill_machine(&m.name, options).await;
    }
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
    let tty = if a.no_tty == Some(true) || a.tty == Some(false) {
        Some(false)
    } else if a.tty == Some(true) {
        Some(true)
    } else {
        None
    };
    let spec = crate::commands::machines::ExecSpec {
        env: crate::volume::expand_env(&a.env)?,
        workdir: a.workdir.clone(),
        tty,
        detach: a.detach,
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
    // What nspawn cannot do for the service it runs, and the ones it brings up first.
    up::check_refusals(
        &project
            .clone()
            .with_selected_services(std::slice::from_ref(&service_name), a.no_deps)
            .map_err(anyhow::Error::msg)?,
    )?;
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
        no_deps: true,
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
        for file in &a.env_from_file {
            let text = std::fs::read_to_string(file).with_context(|| format!("reading {file}"))?;
            for line in text
                .lines()
                .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
            {
                if let Some((k, v)) = line.split_once('=') {
                    env.insert(k.trim().to_string(), v.to_string());
                }
            }
        }
        for item in crate::volume::expand_env(&a.env)? {
            if let Some((k, v)) = item.split_once('=') {
                env.insert(k.to_string(), v.to_string());
            }
        }
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
        volumes.extend(a.volume.iter().cloned());
        o.insert("volume", Bus::from(volumes));
    }
    for (flag, key) in [(&a.cap_add, "cap_add"), (&a.cap_drop, "cap_drop")] {
        if !flag.is_empty() {
            o.insert(key, Bus::from(flag.clone()));
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
        e.client
            .manager
            .start_machine(&name, start)
            .await
            .map_err(client::error)?;
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
    let code =
        crate::commands::machines::attached(&e.client, &name, start, tty, interactive).await?;
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
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
