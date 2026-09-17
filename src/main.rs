//! nspawn: docker-like management of systemd-nspawn machines.
//!
//! Images come from an OCI registry (the hub) and are stored as shared layers under
//! /var/lib/machines/.nspawn. Machines are driven through the D-Bus APIs of
//! systemd-machined and systemd itself, never through machinectl.

mod backend;
mod cli;
mod commands;
mod config;
mod hub;
mod output;
mod pty;
mod reference;
mod store;
mod systemd;
mod unitname;

use clap::Parser;

#[tokio::main]
async fn main() {
    let args = cli::Cli::parse();
    if let Err(err) = commands::run(args).await {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}
