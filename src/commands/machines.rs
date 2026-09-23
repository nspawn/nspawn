//! The terminal side of machines: tables, and the commands that own the terminal
//! (exec, shell, logs).

use anyhow::Result;

use crate::api::machines::{
    self, LogsRequest, StartOutcome, StartRequest, StopOutcome, StopRequest,
};
use crate::api::Context;
use crate::cli::{ExecArgs, LogsArgs, PsArgs, ShellArgs, StartArgs, StopArgs};
use crate::commands::print;
use crate::oci::Mode;
use crate::output::{human_duration, table};
use crate::pty;
use crate::settings::Network;
use crate::store::{now_unix, ImageRecord};

pub async fn ls(args: PsArgs, ctx: &Context) -> Result<()> {
    let now = now_unix();
    let rows = machines::list(ctx, args.all)
        .await?
        .into_iter()
        .map(|m| {
            let (image, mode, command) = describe(m.record.as_ref());
            vec![
                m.name,
                image,
                mode,
                command,
                m.state,
                m.started
                    .map(|t| human_duration(now.saturating_sub(t)))
                    .unwrap_or_else(|| "-".to_string()),
                m.leader
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                network_column(m.record.as_ref()),
                m.os.unwrap_or_else(|| "-".to_string()),
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

/// Address and published ports on the bridge, or the kind of network otherwise.
fn network_column(record: Option<&ImageRecord>) -> String {
    match record {
        Some(r) if r.network == Network::Bridge => {
            let mut parts = vec![r
                .address
                .map(|a| a.to_string())
                .unwrap_or_else(|| "bridge".to_string())];
            parts.extend(r.ports.iter().map(|p| p.to_string()));
            parts.join(" ")
        }
        Some(r) if r.network == Network::Host => "host".to_string(),
        Some(_) => "veth".to_string(),
        None => "-".to_string(),
    }
}

/// Image reference, mode and command of a machine, when nspawn installed its image.
fn describe(record: Option<&ImageRecord>) -> (String, String, String) {
    match record {
        Some(r) => {
            let command = match r.mode {
                Mode::Boot => "init".to_string(),
                Mode::App => {
                    let joined = r.effective_command().join(" ");
                    if joined.chars().count() > 40 {
                        format!("{}...", joined.chars().take(37).collect::<String>())
                    } else {
                        joined
                    }
                }
            };
            (r.reference.clone(), r.mode.name().to_string(), command)
        }
        None => ("-".to_string(), "-".to_string(), "-".to_string()),
    }
}

pub async fn start(args: StartArgs, ctx: &Context) -> Result<()> {
    let request = StartRequest {
        name: args.name,
        wait: args.wait,
        network: args.network,
        publish: args.publish,
        entrypoint: args.entrypoint,
        env: args.env,
        volume: args.volume,
        image_command: args.image_command,
        command: args.command,
    };
    match machines::start(ctx, &request, &print).await? {
        StartOutcome::Started => println!("started {}", request.name),
        StartOutcome::Ended => println!("{} ran and ended already", request.name),
    }
    Ok(())
}

pub async fn stop(args: StopArgs, ctx: &Context) -> Result<()> {
    let request = StopRequest {
        name: args.name,
        force: args.force,
        wait: args.wait,
        timeout: args.timeout,
    };
    match machines::stop(ctx, &request, &print).await? {
        StopOutcome::Stopped => println!("stopped {}", request.name),
        StopOutcome::WasNotRunning => println!("{} was not running", request.name),
    }
    Ok(())
}

pub async fn exec(args: ExecArgs, ctx: &Context) -> Result<()> {
    let code = if args.bus {
        crate::commands::bus::exec(&args.machine, &args.command, &args.user).await?
    } else {
        machines::exec_in_namespaces(ctx, &args.machine, &args.command, &args.user).await?
    };
    std::process::exit(code);
}

/// A shell: through the namespaces for an app (nothing inside to log in with), the login
/// session machined offers for a booted machine.
pub async fn shell(args: ShellArgs, ctx: &Context) -> Result<()> {
    let record = ctx.store.load_image(&args.machine)?;
    if record.as_ref().is_some_and(|r| r.mode == Mode::App) {
        let shell = vec!["/bin/sh".to_string()];
        let code = machines::exec_in_namespaces(ctx, &args.machine, &shell, &args.user).await?;
        std::process::exit(code);
    }
    let (fd, _pty) = machines::open_shell(ctx, &args.machine, &args.user, "", Vec::new()).await?;
    pty::run_session(fd)
}

/// journalctl is the journal's reader, so it does the work and gets the terminal.
pub fn logs(args: LogsArgs) -> Result<()> {
    let request = LogsRequest {
        machine: args.machine,
        follow: args.follow,
        lines: args.lines,
        since: args.since,
        timestamps: args.timestamps,
        all: args.all,
        inside: args.inside,
    };
    let argv = machines::journalctl_arguments(&request);
    let status = std::process::Command::new("journalctl")
        .args(&argv)
        .status()
        .map_err(|e| anyhow::anyhow!("running journalctl: {e}"))?;
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}
