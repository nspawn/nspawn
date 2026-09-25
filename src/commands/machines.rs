//! The terminal side of machines: the ps table, start, run and stop, and the commands
//! that own the terminal (exec, shell, logs), whose streams come from the service.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
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
                state_column(m),
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

/// The state, and the healthcheck's verdict next to it as docker ps shows it.
fn state_column(machine: &Dict) -> String {
    let state = client::string(machine, "state");
    match client::string(machine, "health").as_str() {
        "" => state,
        "starting" => format!("{state} (health: starting)"),
        health => format!("{state} ({health})"),
    }
}

/// Whether nspawn installed the machine's image: only then is there a record.
fn recorded(machine: &Dict) -> bool {
    machine.contains_key("reference")
}

/// The kind of network, or the address on each bridge network (with the network's name
/// unless it is the default one) and the published ports.
fn network_column(machine: &Dict) -> String {
    if !recorded(machine) {
        return "-".to_string();
    }
    let networks = client::strings(machine, "networks");
    if networks.is_empty() {
        return client::string(machine, "network");
    }
    let addresses = client::dict_to_json(machine);
    let mut parts: Vec<String> = networks
        .iter()
        .map(|network| {
            let address = addresses["addresses"][network].as_str().unwrap_or("");
            match (network.as_str(), address) {
                ("bridge", "") => "bridge".to_string(),
                ("bridge", address) => address.to_string(),
                (network, "") => network.to_string(),
                (network, address) => format!("{network}:{address}"),
            }
        })
        .collect();
    parts.extend(client::strings(machine, "ports"));
    parts.join(" ")
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
    let mut options = start_options(args.options, args.command)?;
    if args.image_command {
        options.insert("image_command", Value::from(true));
    }
    start_machine(client, &args.name, options).await
}

/// The options of a StartMachine call from what start or run was given.
fn start_options(args: StartOptions, command: Vec<String>) -> Result<Options<'static>> {
    let mut options = Options::new();
    options.insert("wait", Value::from(args.wait));
    if !args.network.is_empty() {
        options.insert("networks", Value::from(args.network));
    }
    if !args.network_alias.is_empty() {
        options.insert("aliases", Value::from(args.network_alias));
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
    if !command.is_empty() {
        options.insert("command", Value::from(command));
    }
    super::put_health(&mut options, args.health);
    super::put_tuning(&mut options, args.tuning);
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

/// The name of a machine of run --rm without --name: the image's local name and a random
/// part, as docker names its containers when not told.
fn run_name(base: &str) -> Result<String> {
    let mut bytes = [0u8; 4];
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .context("reading /dev/urandom")?;
    let base: String = base.chars().take(54).collect();
    Ok(format!("{base}-{}", hex::encode(bytes)))
}

/// docker run: a machine from a local image or from the registry, started with the
/// options given, which it keeps as after start.
pub async fn run(args: RunArgs, client: &Client, config: &Config) -> Result<()> {
    if !args.detach && !args.options.wait {
        bail!("--no-wait goes with -d: an attached run follows the machine anyway");
    }
    if args.detach && (args.tty || args.interactive) {
        bail!("-d runs the machine in the background; -i and -t are for a run that stays attached");
    }
    let registry = super::registry_name(client, config).await;
    let image = ImageRef::parse(&args.reference, &registry)?;
    let name = match &args.name {
        Some(name) => name.clone(),
        // run --rm keeps the image, as docker does (see below): the machine it removes
        // gets a name of its own.
        None if args.rm => run_name(&image.local_name())?,
        None => image.local_name(),
    };
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
    // A pulled image is a machine, so --rm would remove the image too and the next run
    // would download it again: it is pulled under its own name and kept, and the machine
    // is made from it, unless another image has that name.
    let base = image.local_name();
    let keep_image = args.rm
        && matches!(source, Source::Registry)
        && base != name
        && images
            .iter()
            .all(|(n, r)| n != &base || r == &image.to_string());
    match source {
        Source::Local(source) => {
            client
                .run_job_to_stderr(|| manager.create_machine(&source, &name, options))
                .await?;
        }
        Source::Registry if keep_image => {
            let mut create = client::registry_options(config);
            create.insert("force", Value::from(args.force));
            if args.backend != crate::cli::BackendChoice::Auto {
                create.insert(
                    "backend",
                    Value::from(format!("{:?}", args.backend).to_lowercase()),
                );
            }
            options.insert("name", Value::from(base.clone()));
            // An older copy under that name is replaced (--pull always).
            options.insert("force", Value::from(true));
            if args.mode != ModeChoice::Auto {
                options.insert(
                    "mode",
                    Value::from(format!("{:?}", args.mode).to_lowercase()),
                );
            }
            client
                .run_job_to_stderr(|| manager.pull_image(&args.reference, options))
                .await?;
            client
                .run_job_to_stderr(|| manager.create_machine(&base, &name, create))
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
                .run_job_to_stderr(|| manager.pull_image(&args.reference, options))
                .await?;
        }
    }
    let mut options = start_options(args.options, args.command)?;
    if args.rm {
        options.insert("remove", Value::from(true));
    }
    let started = if args.detach {
        start_machine(client, &name, options).await.map(|()| None)
    } else {
        run_attached(client, &name, options, args.tty, args.interactive)
            .await
            .map(Some)
    };
    let code = match started {
        Ok(code) => code,
        Err(e) => {
            // docker run --rm leaves nothing behind when the start fails.
            if args.rm {
                let mut options = Options::new();
                options.insert("force", Value::from(true));
                let names = [name.clone()];
                let _ = client
                    .run_job_to_stderr(|| client.manager.remove_machines(&names, options))
                    .await;
            }
            return Err(e);
        }
    };
    let Some(code) = code else { return Ok(()) };
    if args.rm {
        removed(client, &name).await;
    }
    std::process::exit(code);
}

/// An attached run of a machine made already: its console or its program's output, a
/// shell for -it on a booted one.
async fn run_attached(
    client: &Client,
    name: &str,
    options: Options<'_>,
    tty: bool,
    interactive: bool,
) -> Result<i32> {
    let booted = client
        .manager
        .get_image(name)
        .await
        .map_err(client::error)
        .map(|image| client::string(&image, "mode") == "boot")?;
    match (booted, tty, interactive) {
        (true, true, true) => booted_shell(client, name, options).await,
        (true, false, false) | (false, _, _) => {
            attached(client, name, options, tty, interactive).await
        }
        (true, _, _) => bail!(
            "{name} boots an init system: without -i and -t run shows its console, with -it it opens a shell"
        ),
    }
}

/// docker run without -d: the machine's output (or its terminal) until it ends, and its
/// exit code. Ctrl-C and the like go to its program; a third Ctrl-C within a second
/// leaves it running and returns.
async fn attached(
    client: &Client,
    name: &str,
    mut options: Options<'_>,
    tty: bool,
    interactive: bool,
) -> Result<i32> {
    let (rows, cols) = pty::window_size().unwrap_or((24, 80));
    options.insert("tty", Value::from(tty));
    options.insert("rows", Value::from(rows as u64));
    options.insert("cols", Value::from(cols as u64));
    if let Ok(term) = std::env::var("TERM") {
        options.insert("term", Value::from(term));
    }
    let stdin = io::stdin();
    let mut fds = HashMap::new();
    if interactive && !tty {
        fds.insert("stdin", zbus::zvariant::Fd::from(stdin.as_fd()));
    }
    let (mut handed, process, notes) = client
        .manager
        .run_machine(name, options, fds)
        .await
        .map_err(client::error)?;
    for note in &notes {
        eprintln!("{note}");
    }
    let ended = Ended::watch(&client.connection, process).await?;
    if let Some(master) = handed.remove("tty") {
        tokio::task::block_in_place(|| pty::run_session(OwnedFd::from(master)))?;
        return ended.status().await;
    }
    let Some(output) = handed.remove("stdout") else {
        bail!("the service returned neither a terminal nor the output");
    };
    let output = OwnedFd::from(output);
    let mut copy = tokio::task::spawn_blocking(move || copy(File::from(output), io::stdout()));
    let mut forwarded = Forwarded::new()?;
    loop {
        tokio::select! {
            done = &mut copy => {
                done.map_err(|_| anyhow::anyhow!("the output pump panicked"))??;
                break;
            }
            signal = forwarded.next() => {
                let Some(signal) = signal else { continue };
                if forwarded.detaching() {
                    eprintln!("\n{name} keeps running; nspawn stop {name} stops it");
                    std::process::exit(0);
                }
                if let Err(e) = ended.signal(signal).await {
                    eprintln!("note: {e:#}");
                }
            }
        }
    }
    ended.status().await
}

/// The signals `run` passes on to the program, and the Ctrl-C count that detaches.
struct Forwarded {
    streams: Vec<(i32, tokio::signal::unix::Signal)>,
    interrupts: Vec<std::time::Instant>,
}

impl Forwarded {
    fn new() -> Result<Self> {
        use tokio::signal::unix::{signal, SignalKind};
        let mut streams = Vec::new();
        for (number, kind) in [
            (nix::libc::SIGINT, SignalKind::interrupt()),
            (nix::libc::SIGTERM, SignalKind::terminate()),
            (nix::libc::SIGHUP, SignalKind::hangup()),
            (nix::libc::SIGQUIT, SignalKind::quit()),
        ] {
            streams.push((number, signal(kind).context("catching signals")?));
        }
        Ok(Forwarded {
            streams,
            interrupts: Vec::new(),
        })
    }

    async fn next(&mut self) -> Option<i32> {
        let futures = self.streams.iter_mut().map(|(number, stream)| {
            let number = *number;
            Box::pin(async move { stream.recv().await.map(|_| number) })
        });
        let (signal, _, _) = futures_util::future::select_all(futures).await;
        if signal == Some(nix::libc::SIGINT) {
            let now = std::time::Instant::now();
            self.interrupts
                .retain(|t| now.duration_since(*t) < std::time::Duration::from_secs(1));
            self.interrupts.push(now);
        }
        signal
    }

    /// Three Ctrl-C within a second.
    fn detaching(&self) -> bool {
        self.interrupts.len() >= 3
    }
}

/// docker run -it of a booted image: a shell once the machine is up, and the machine
/// powered off when the shell ends, whose exit code is the run's.
async fn booted_shell(client: &Client, name: &str, options: Options<'_>) -> Result<i32> {
    let (_, notes) = client
        .manager
        .start_machine(name, options)
        .await
        .map_err(client::error)?;
    for note in &notes {
        eprintln!("{note}");
    }
    let shell = [
        "/bin/sh".to_string(),
        "-c".to_string(),
        "if [ -x /bin/bash ]; then exec /bin/bash -l; else exec /bin/sh -l; fi".to_string(),
    ];
    let code = run_command(client, name, &shell, "").await;
    let mut stop = Options::new();
    stop.insert("wait", Value::from(true));
    let stopped = client.manager.stop_machine(name, stop).await;
    let code = code?;
    stopped.map_err(client::error)?;
    Ok(code)
}

/// run --rm: the machine goes once its unit is down, in a unit of its own; returns once
/// it is gone (or after a while).
async fn removed(client: &Client, name: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while client.manager.get_image(name).await.is_ok() {
        if std::time::Instant::now() > deadline {
            eprintln!("note: {name} is still there; nspawn rm {name} removes it");
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

pub async fn stop(args: StopArgs, client: &Client) -> Result<()> {
    let mut options = Options::new();
    options.insert("force", Value::from(args.force));
    options.insert("wait", Value::from(args.wait));
    if let Some(timeout) = args.timeout {
        options.insert("timeout", Value::from(timeout));
    }
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
        super::put_health(&mut options, args.health.clone());
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
    fn machines_of_run_rm_get_names_of_their_own() {
        let a = run_name("alpine-3").unwrap();
        let b = run_name("alpine-3").unwrap();
        assert!(
            a.starts_with("alpine-3-") && a.len() == "alpine-3-".len() + 8,
            "{a}"
        );
        assert_ne!(a, b);
        let long = run_name(&"x".repeat(80)).unwrap();
        assert!(long.len() <= 64);
        crate::reference::validate_machine_name(&long).unwrap();
    }

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
