//! The terminal side of machines: the ps table, start and stop, and the commands that
//! own the terminal (exec, shell, logs), whose streams come from the service.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context as _, Result};
use nix::sys::signal::{pthread_sigmask, SigSet, SigmaskHow, Signal};
use zbus::zvariant::{OwnedObjectPath, Value};

use crate::cli::{ExecArgs, LogsArgs, PsArgs, ShellArgs, StartArgs, StopArgs};
use crate::client::{self, Client, Dict, Options, ProcessProxy};
use crate::output::{human_duration, table};
use crate::pty;
use crate::store::now_unix;

pub async fn ls(args: PsArgs, client: &Client) -> Result<()> {
    let now = now_unix();
    let rows = client
        .manager
        .list_machines(args.all)
        .await
        .map_err(client::error)?
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
    if args.image_command {
        options.insert("image_command", Value::from(true));
    }
    if !args.command.is_empty() {
        options.insert("command", Value::from(args.command));
    }
    let outcome = client
        .manager
        .start_machine(&args.name, options)
        .await
        .map_err(client::error)?;
    match outcome.as_str() {
        "ended" => println!("{} ran and ended already", args.name),
        _ => println!("started {}", args.name),
    }
    Ok(())
}

pub async fn stop(args: StopArgs, client: &Client) -> Result<()> {
    let mut options = Options::new();
    options.insert("force", Value::from(args.force));
    options.insert("wait", Value::from(args.wait));
    options.insert("timeout", Value::from(args.timeout));
    let outcome = client
        .manager
        .stop_machine(&args.name, options)
        .await
        .map_err(client::error)?;
    match outcome.as_str() {
        "was-not-running" => println!("{} was not running", args.name),
        _ => println!("stopped {}", args.name),
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
    let (fd, _pty) = client
        .manager
        .shell(&args.machine, &args.user)
        .await
        .map_err(client::error)?;
    tokio::task::block_in_place(|| pty::run_session(OwnedFd::from(fd)))
}

async fn run_command(client: &Client, machine: &str, argv: &[String], user: &str) -> Result<i32> {
    let tty = nix::unistd::isatty(io::stdin()).unwrap_or(false);
    let (rows, cols) = pty::window_size().unwrap_or((24, 80));
    let mut options = Options::new();
    options.insert("tty", Value::from(tty));
    options.insert("rows", Value::from(rows as u64));
    options.insert("cols", Value::from(cols as u64));
    let (mut fds, process): (HashMap<String, zbus::zvariant::OwnedFd>, OwnedObjectPath) = client
        .manager
        .exec(machine, argv, user, options)
        .await
        .map_err(client::error)?;
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
    exit_status(client, &process).await
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
    let out = thread::spawn(move || copy(File::from(stdout), io::stdout().lock()));
    let err = thread::spawn(move || copy(File::from(stderr), io::stderr().lock()));
    out.join()
        .map_err(|_| anyhow::anyhow!("stdout pump panicked"))??;
    err.join()
        .map_err(|_| anyhow::anyhow!("stderr pump panicked"))??;
    // Input still pending is of no use once the command is gone.
    drop(writer);
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

/// The exit status from the process object once it has exited; the streams end a
/// moment before the service learns of the exit.
async fn exit_status(client: &Client, process: &OwnedObjectPath) -> Result<i32> {
    let proxy = ProcessProxy::builder(&client.connection)
        .path(process.clone())?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .context("reaching the process object")?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if proxy.state().await.map_err(client::error)? == "exited" {
            return proxy.exit_status().await.map_err(client::error);
        }
        if Instant::now() > deadline {
            bail!("the command's streams closed but the service has not seen it exit");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// docker logs: the lines come through a pipe from the service, journalctl behind it.
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
    let fd = client
        .manager
        .logs(&args.machine, options)
        .await
        .map_err(client::error)?;
    tokio::task::block_in_place(|| copy(File::from(OwnedFd::from(fd)), io::stdout().lock()))
}
