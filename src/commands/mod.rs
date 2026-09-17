//! Command implementations.

mod build;
mod hub;
mod images;
mod machines;
mod pull;
mod push;

use anyhow::{bail, Result};

use crate::cli::{Cli, Command, HubCommand, ImagesCommand, MachinesCommand};
use crate::config::Config;

pub async fn run(cli: Cli) -> Result<()> {
    let config = Config::load(
        cli.config.as_deref(),
        cli.registry.clone(),
        cli.ca_cert.clone(),
    )?;
    match cli.command {
        Command::Hub(args) => match args.command {
            HubCommand::Ls(a) => hub::ls(a, &config).await,
            HubCommand::Tags(a) => hub::tags(a, &config).await,
        },
        Command::Pull(args) => pull::run(args, &config).await,
        Command::Build(args) => build::run(args, &config).await,
        Command::Push(args) => push::run(args, &config).await,
        Command::Images(args) => match args.command {
            ImagesCommand::Ls => images::ls(&config).await,
            ImagesCommand::Rm(a) => images::rm(a, &config).await,
        },
        Command::Machines(args) => match args.command {
            MachinesCommand::Ls => machines::ls().await,
        },
        Command::Start(args) => machines::start(args, &config).await,
        Command::Stop(args) => machines::stop(args, &config).await,
        Command::Exec(args) => machines::exec(args, &config).await,
        Command::Shell(args) => machines::shell(args, &config).await,
    }
}

pub fn require_root(action: &str) -> Result<()> {
    if !nix::unistd::geteuid().is_root() {
        bail!("{action} needs root privileges (it writes below /var/lib/machines and /etc/systemd/system)");
    }
    Ok(())
}
