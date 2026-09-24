//! The terminal side of machines: the ps table, start, run and stop, and the commands
//! that own the terminal (exec, shell, logs), whose streams come from the service.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::thread;

use anyhow::{bail, Context as _, Result};
use nix::sys::signal::{pthread_sigmask, SigSet, SigmaskHow, Signal};
use zbus::zvariant::{OwnedObjectPath, Value};

use crate::cli::{
    EventsArgs, ExecArgs, KillArgs, LogsArgs, ModeChoice, PsArgs, PullPolicy, RunArgs, ShellArgs,
    StartArgs, StartOptions, StopArgs, UpdateArgs,
};
use crate::client::{self, Client, Dict, Ended, Options};
use crate::config::Config;
use crate::output::{human_duration, table};
use crate::pty;
use crate::reference::ImageRef;
use crate::store::now_unix;

pub async fn ls(args: PsArgs, client: &Client) -> Result<()> {
    let now = now_unix();
    let machines = client
        .manager
        .list_machines(args.all)
        .await
        .map_err(client::error)?;
    if args.output.json {
        client::print_json(&serde_json::Value::Array(
            machines.iter().map(client::dict_to_json).collect(),
        ));
        return Ok(());
    }
    let rows = machines
        .iter()
        .map(|m| {
            let (image, mode, command) = describe(m);
            let started = client::u64(m, "started");
            let leader = client::u64(m, "leader");
            vec![
                client::string(m, "name"),
                image,
                mode,
                command,
                client::string(m, "state"),
                if started > 0 {
                    human_duration(now.saturating_sub(started))
                } else {
                    "-".to_string()
                },
                if leader > 0 {
                    leader.to_string()
                } else {
                    "-".to_string()
                },
                network_column(m),
                client::dash(client::string(m, "os")),
            ]
        })
        .collect();
    println!(
        "{}",
        table(
            &["MACHINE", "IMAGE", "MODE", "COMMAND", "STATE", "UP", "PID", "NETWORK", "OS"],
            rows
        )
    );
    Ok(())
}

/// Whether nspawn installed the machine's image: only then is there a record.
fn recorded(machine: &Dict) -> bool {
    machine.contains_key("reference")
}

/// Address and published ports on the bridge, or the kind of network otherwise.
fn network_column(machine: &Dict) -> String {
    if !recorded(machine) {
        return "-".to_string();
    }
    match client::string(machine, "network").as_str() {
        "bridge" => {
            let address = client::string(machine, "address");
            let mut parts = vec![if address.is_empty() {
                "bridge".to_string()
            } else {
                address
            }];
            parts.extend(client::strings(machine, "ports"));
            parts.join(" ")
        }
        "host" => "host".to_string(),
        _ => "veth".to_string(),
    }
}

/// Image reference, mode and command of a machine, when nspawn installed its image.
fn describe(machine: &Dict) -> (String, String, String) {
    if !recorded(machine) {
        return ("-".to_string(), "-".to_string(), "-".to_string());
    }
    let mode = client::string(machine, "mode");
    let command = if mode == "boot" {
        "init".to_string()
    } else {
        let joined = client::strings(machine, "command").join(" ");
        if joined.chars().count() > 40 {
            format!("{}...", joined.chars().take(37).collect::<String>())
        } else {
            joined
        }
    };
    (client::string(machine, "reference"), mode, command)
}

pub async fn start(args: StartArgs, client: &Client) -> Result<()> {
    let mut options = start_options(args.options)?;
    if args.image_command {
        options.insert("image_command", Value::from(true));
    }
    start_machine(client, &args.name, options).await
}

/// The options of a StartMachine call from what start or run was given.
fn start_options(args: StartOptions) -> Result<Options<'static>> {
    let mut options = Options::new();
    options.insert("wait", Value::from(args.wait));
    if let Some(network) = args.network {
        options.insert(
            "network",
            Value::from(format!("{network:?}").to_lowercase()),
        );
    }
    if !args.publish.is_empty() {
        options.insert("publish", Value::from(args.publish));
    }
    if let Some(entrypoint) = args.entrypoint {
        options.insert("entrypoint", Value::from(entrypoint));
    }
    if !args.env.is_empty() {
        // A bare VAR means this environment, not the service's.
        options.insert("env", Value::from(crate::volume::expand_env(&args.env)?));
    }
    if !args.volume.is_empty() {
        options.insert("volume", Value::from(args.volume));
    }
    if !args.label.is_empty() {
        options.insert("label", Value::from(args.label));
    }
    if let Some(restart) = args.restart {
        options.insert("restart", Value::from(restart.name()));
    }
    if let Some(memory) = args.memory {
        options.insert("memory", Value::from(memory));
    }
    if let Some(cpus) = args.cpus {
        options.insert("cpus", Value::from(cpus));
    }
    if let Some(pids) = args.pids_limit {
        options.insert("pids_limit", Value::from(pids));
    }
    if !args.command.is_empty() {
        options.insert("command", Value::from(args.command));
    }
    Ok(options)
}

async fn start_machine(client: &Client, name: &str, options: Options<'_>) -> Result<()> {
    let (outcome, notes) = client
        .manager
        .start_machine(name, options)
        .await
        .map_err(client::error)?;
    for note in &notes {
        eprintln!("{note}");
    }
    match outcome.as_str() {
        "ended" => println!("{name} ran and ended already"),
        "restarting" => println!(
            "{name} ended right after starting and is being restarted; see nspawn logs {name}"
        ),
        _ => println!("started {name}"),
    }
    Ok(())
}

/// Where run takes a machine from.
#[derive(Debug, PartialEq, Eq)]
enum Source {
    /// A local image with the reference: create, no registry.
    Local(String),
    Registry,
}

/// `images` are the local images as (name, reference). An existing machine of that name
/// is started with start, or made anew with --force; a mode other than auto can only
/// come with a pull.
fn run_source(
    images: &[(String, String)],
    reference: &str,
    name: &str,
    force: bool,
    pull: PullPolicy,
    mode_given: bool,
) -> Result<Source> {
    if !force && images.iter().any(|(n, _)| n == name) {
        bail!("machine {name} exists: nspawn start {name}, or nspawn run --force to make it anew");
    }
    let local = images
        .iter()
        .find(|(n, r)| r == reference && n != name)
        .map(|(n, _)| n.clone());
    Ok(match (pull, local) {
        (PullPolicy::Always, _) => Source::Registry,
        (PullPolicy::Never, _) if mode_given => {
            bail!("--mode takes a pull of the image; drop it, or use --pull missing")
        }
        (PullPolicy::Never, None) => {
            bail!("no local image of {reference}; pull it, or use --pull missing")
        }
        (_, Some(source)) if !mode_given => Source::Local(source),
        _ => Source::Registry,
    })
}

/// docker run -d: the machine from a local image with the reference or from the registry,
/// then started with the options given, which it keeps like after start.
pub async fn run(args: RunArgs, client: &Client, config: &Config) -> Result<()> {
    let registry = super::registry_name(client, config).await;
    let image = ImageRef::parse(&args.reference, &registry)?;
    let name = args.name.clone().unwrap_or_else(|| image.local_name());
    let images: Vec<(String, String)> = client
        .manager
        .list_images()
        .await
        .map_err(client::error)?
        .iter()
        .map(|i| (client::string(i, "name"), client::string(i, "reference")))
        .collect();
    let source = run_source(
        &images,
        &image.to_string(),
        &name,
        args.force,
        args.pull,
        args.mode != ModeChoice::Auto,
    )?;
    let mut options = client::registry_options(config);
    options.insert("force", Value::from(args.force));
    if args.backend != crate::cli::BackendChoice::Auto {
        options.insert(
            "backend",
            Value::from(format!("{:?}", args.backend).to_lowercase()),
        );
    }
    let manager = &client.manager;
    match source {
        Source::Local(source) => {
            client
                .run_job(|| manager.create_machine(&source, &name, options))
                .await?;
        }
        Source::Registry => {
            options.insert("name", Value::from(name.clone()));
            if args.mode != ModeChoice::Auto {
                options.insert(
                    "mode",
                    Value::from(format!("{:?}", args.mode).to_lowercase()),
                );
            }
            client
                .run_job(|| manager.pull_image(&args.reference, options))
                .await?;
        }
    }
    start_machine(client, &name, start_options(args.options)?).await
}

pub async fn stop(args: StopArgs, client: &Client) -> Result<()> {
    let mut options = Options::new();
    options.insert("force", Value::from(args.force));
    options.insert("wait", Value::from(args.wait));
    options.insert("timeout", Value::from(args.timeout));
    let (outcome, notes) = client
        .manager
        .stop_machine(&args.name, options)
        .await
        .map_err(client::error)?;
    for note in &notes {
        eprintln!("{note}");
    }
    match outcome.as_str() {
        "was-not-running" => println!("{} was not running", args.name),
        _ => println!("stopped {}", args.name),
    }
    Ok(())
}

/// docker kill: every name gets the signal, each one printed once it did; a name that
/// could not be signalled is reported and makes the command fail at the end.
pub async fn kill(args: KillArgs, client: &Client) -> Result<()> {
    let mut failed = 0;
    for name in &args.names {
        let mut options = Options::new();
        options.insert("signal", Value::from(args.signal.clone()));
        match client.manager.kill_machine(name, options).await {
            Ok(notes) => {
                for note in &notes {
                    eprintln!("{note}");
                }
                println!("{name}");
            }
            Err(e) => {
                eprintln!("error: {:#}", client::error(e));
                failed += 1;
            }
        }
    }
    if failed > 0 {
        bail!(
            "{failed} of {} machines could not be signalled",
            args.names.len()
        );
    }
    Ok(())
}

/// docker update: every name gets the same changes, each one printed once it has them.
pub async fn update(args: UpdateArgs, client: &Client) -> Result<()> {
    let mut failed = 0;
    for name in &args.names {
        let mut options = Options::new();
        if let Some(restart) = args.restart {
            options.insert("restart", Value::from(restart.name()));
        }
        if let Some(memory) = args.memory {
            options.insert("memory", Value::from(memory));
        }
        if let Some(cpus) = args.cpus {
            options.insert("cpus", Value::from(cpus));
        }
        if let Some(pids) = args.pids_limit {
            options.insert("pids_limit", Value::from(pids));
        }
        match client.manager.update_machine(name, options).await {
            Ok(_) => println!("{name}"),
            Err(e) => {
                eprintln!("error: {:#}", client::error(e));
                failed += 1;
            }
        }
    }
    if failed > 0 {
        bail!(
            "{failed} of {} machines could not be updated",
            args.names.len()
        );
    }
    Ok(())
}

/// docker exec: the service runs the command inside the machine and hands its streams
/// over, a pseudo terminal when this is one, pipes otherwise; the exit status comes
/// back through the process object.
pub async fn exec(args: ExecArgs, client: &Client) -> Result<()> {
    let code = run_command(client, &args.machine, &args.command, &args.user).await?;
    std::process::exit(code);
}

/// A shell: the login session machined offers for a booted machine, the namespaces
/// for an app (nothing inside to log in with).
pub async fn shell(args: ShellArgs, client: &Client) -> Result<()> {
    let image = client.manager.get_image(&args.machine).await.ok();
    if image.is_some_and(|i| client::string(&i, "mode") == "app") {
        let shell = vec!["/bin/sh".to_string()];
        let code = run_command(client, &args.machine, &shell, &args.user).await?;
        std::process::exit(code);
    }
    let mut options = Options::new();
    if let Some(term) = terminal_env() {
        options.insert("env", Value::from(vec![term]));
    }
    let (fd, _pty) = client
        .manager
        .shell(&args.machine, &args.user, options)
        .await
        .map_err(client::error)?;
    tokio::task::block_in_place(|| pty::run_session(OwnedFd::from(fd)))
}

/// TERM as this terminal has it, for the machine's side of a pseudo terminal.
fn terminal_env() -> Option<String> {
    std::env::var("TERM")
        .ok()
        .filter(|t| !t.is_empty())
        .map(|t| format!("TERM={t}"))
}

async fn run_command(client: &Client, machine: &str, argv: &[String], user: &str) -> Result<i32> {
    let tty = nix::unistd::isatty(io::stdin()).unwrap_or(false);
    let (rows, cols) = pty::window_size().unwrap_or((24, 80));
    let mut options = Options::new();
    options.insert("tty", Value::from(tty));
    options.insert("rows", Value::from(rows as u64));
    options.insert("cols", Value::from(cols as u64));
    if tty {
        if let Some(term) = terminal_env() {
            options.insert("env", Value::from(vec![term]));
        }
    }
    let (mut fds, process): (HashMap<String, zbus::zvariant::OwnedFd>, OwnedObjectPath) = client
        .manager
        .exec(machine, argv, user, options)
        .await
        .map_err(client::error)?;
    let ended = Ended::watch(&client.connection, process).await?;
    let mut take = |name: &str| fds.remove(name).map(OwnedFd::from);
    if let Some(master) = take("tty") {
        tokio::task::block_in_place(|| pty::run_session(master))?;
    } else {
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (take("stdin"), take("stdout"), take("stderr"))
        else {
            bail!("the service returned neither a terminal nor pipes");
        };
        tokio::task::block_in_place(|| pump_pipes(stdin, stdout, stderr))?;
    }
    // The streams end when the command (and whatever it left behind) closes them; the
    // exit status may take its time after that, as with docker.
    ended.status().await
}

/// Copies this process's streams to and from the command's pipes until its output ends.
fn pump_pipes(stdin: OwnedFd, stdout: OwnedFd, stderr: OwnedFd) -> Result<()> {
    let writer = thread::spawn(move || {
        // The command may exit without reading its input; a broken pipe is then an
        // error to see, not a signal to die of.
        let mut set = SigSet::empty();
        set.add(Signal::SIGPIPE);
        let _ = pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&set), None);
        let mut to_command = File::from(stdin);
        let mut buf = [0u8; 8192];
        let mut input = io::stdin().lock();
        loop {
            match input.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if to_command.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let outcome = pump_output(stdout, stderr);
    // Input still pending is of no use once the command is gone.
    drop(writer);
    outcome
}

/// Copies a process's output pipes to ours until both end.
fn pump_output(stdout: OwnedFd, stderr: OwnedFd) -> Result<()> {
    let out = thread::spawn(move || copy(File::from(stdout), io::stdout().lock()));
    let err = thread::spawn(move || copy(File::from(stderr), io::stderr().lock()));
    out.join()
        .map_err(|_| anyhow::anyhow!("stdout pump panicked"))??;
    err.join()
        .map_err(|_| anyhow::anyhow!("stderr pump panicked"))??;
    Ok(())
}

fn copy(mut from: File, mut to: impl Write) -> Result<()> {
    let mut buf = [0u8; 8192];
    loop {
        match from.read(&mut buf) {
            Ok(0) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e).context("reading from the command"),
            Ok(n) => {
                to.write_all(&buf[..n])?;
                to.flush()?;
            }
        }
    }
}

/// docker events: the service reads the journal and sends one JSON object per event.
pub async fn events(args: EventsArgs, client: &Client) -> Result<()> {
    // A bad filter is said before anything is asked.
    crate::api::events::Filters::parse(&args.filters)?;
    let mut options = Options::new();
    if let Some(since) = args.since {
        options.insert("since", Value::from(since));
    }
    if let Some(until) = args.until {
        options.insert("until", Value::from(until));
    }
    if !args.filters.is_empty() {
        options.insert("filters", Value::from(args.filters));
    }
    let (mut fds, process) = client
        .manager
        .events(options)
        .await
        .map_err(client::error)?;
    let ended = Ended::watch(&client.connection, process).await?;
    let (Some(stdout), Some(stderr)) = (fds.remove("stdout"), fds.remove("stderr")) else {
        bail!("the service returned no streams for the events");
    };
    let json = args.json;
    let err = thread::spawn(move || copy(File::from(OwnedFd::from(stderr)), io::stderr().lock()));
    tokio::task::block_in_place(|| -> Result<()> {
        use std::io::BufRead;
        let mut out = io::stdout().lock();
        for line in io::BufReader::new(File::from(OwnedFd::from(stdout))).lines() {
            let line = line.context("reading the events")?;
            if json {
                writeln!(out, "{line}")?;
            } else if let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) {
                writeln!(out, "{}", event_line(&event))?;
            }
            out.flush()?;
        }
        Ok(())
    })?;
    err.join()
        .map_err(|_| anyhow::anyhow!("stderr pump panicked"))??;
    let code = ended.status().await?;
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

/// An event as docker events prints it: time, type, action, name, then its attributes
/// and labels.
fn event_line(event: &serde_json::Value) -> String {
    let text = |key: &str| event.get(key).and_then(|v| v.as_str()).unwrap_or("");
    let mut details: Vec<String> = Vec::new();
    for key in ["attributes", "labels"] {
        if let Some(map) = event.get(key).and_then(|v| v.as_object()) {
            for (k, v) in map {
                details.push(format!("{k}={}", v.as_str().unwrap_or("")));
            }
        }
    }
    let mut line = format!(
        "{} {} {} {}",
        text("time"),
        text("type"),
        text("action"),
        text("name")
    );
    if !details.is_empty() {
        line.push_str(&format!(" ({})", details.join(", ")));
    }
    line
}

/// docker logs: journalctl's output comes through pipes from the service, and its exit
/// status through the process object.
pub async fn logs(args: LogsArgs, client: &Client) -> Result<()> {
    let mut options = Options::new();
    options.insert("follow", Value::from(args.follow));
    if let Some(lines) = args.lines {
        options.insert("lines", Value::from(lines as u64));
    }
    if let Some(since) = args.since {
        options.insert("since", Value::from(since));
    }
    options.insert("timestamps", Value::from(args.timestamps));
    options.insert("all", Value::from(args.all));
    options.insert("inside", Value::from(args.inside));
    let (mut fds, process) = client
        .manager
        .logs(&args.machine, options)
        .await
        .map_err(client::error)?;
    let ended = Ended::watch(&client.connection, process).await?;
    let (Some(stdout), Some(stderr)) = (fds.remove("stdout"), fds.remove("stderr")) else {
        bail!("the service returned no streams for the logs");
    };
    tokio::task::block_in_place(|| pump_output(OwnedFd::from(stdout), OwnedFd::from(stderr)))?;
    let code = ended.status().await?;
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_read_like_docker_events() {
        let event = serde_json::json!({
            "time": "2026-09-24T10:00:00.000001Z",
            "type": "machine",
            "action": "die",
            "name": "web",
            "attributes": {"exit_code": "1", "code": "exited"},
            "labels": {"caddy": "web.example"},
        });
        assert_eq!(
            event_line(&event),
            "2026-09-24T10:00:00.000001Z machine die web (code=exited, exit_code=1, caddy=web.example)"
        );
        let bare = serde_json::json!({"time": "t", "type": "volume", "action": "create", "name": "data", "attributes": {}, "labels": {}});
        assert_eq!(event_line(&bare), "t volume create data");
    }

    #[test]
    fn run_reuses_a_local_image_and_asks_the_registry_otherwise() {
        let nginx = "docker.io/library/nginx:1.27";
        let images = vec![
            ("web".to_string(), nginx.to_string()),
            (
                "fedora-44".to_string(),
                "hub.nspawn.org/fedora:44".to_string(),
            ),
            ("vm".to_string(), String::new()),
        ];
        let source = |name: &str, force: bool, pull: PullPolicy, mode: bool| {
            run_source(&images, nginx, name, force, pull, mode)
        };
        assert_eq!(
            source("web2", false, PullPolicy::Missing, false).unwrap(),
            Source::Local("web".to_string()),
            "a local image with the reference is made into another machine"
        );
        assert_eq!(
            source("web2", false, PullPolicy::Always, false).unwrap(),
            Source::Registry
        );
        assert_eq!(
            source("web2", false, PullPolicy::Missing, true).unwrap(),
            Source::Registry,
            "a mode of its own takes a pull"
        );
        let taken = source("web", false, PullPolicy::Missing, false).unwrap_err();
        assert!(taken.to_string().contains("nspawn start web"), "{taken}");
        assert!(
            source("vm", false, PullPolicy::Missing, false).is_err(),
            "an image machined lists takes the name too"
        );
        assert_eq!(
            source("web", true, PullPolicy::Missing, false).unwrap(),
            Source::Registry,
            "made anew from the registry: the one local copy is the machine itself"
        );
        assert!(run_source(
            &images,
            "hub.nspawn.org/x:1",
            "x",
            false,
            PullPolicy::Never,
            false
        )
        .is_err());
        assert_eq!(
            source("web2", false, PullPolicy::Never, false).unwrap(),
            Source::Local("web".to_string())
        );
        assert!(source("web2", false, PullPolicy::Never, true).is_err());
    }
}
