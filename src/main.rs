//! nspawn: docker-like management of systemd-nspawn machines.
//!
//! Images come from an OCI registry (the hub) and are stored as shared layers under
//! /var/lib/nspawn. Machines are driven through the D-Bus APIs of
//! systemd-machined and systemd itself, never through machinectl.

mod api;
mod auth;
mod backend;
mod bridge;
mod cli;
mod client;
mod commands;
mod config;
mod daemon;
mod hostnet;
mod hub;
mod install;
mod journal;
mod layout;
mod nsenter;
mod oci;
mod output;
mod policy;
mod pty;
mod reference;
mod search;
mod settings;
mod store;
mod systemd;
mod unitname;
mod volmount;
mod volume;

use clap::Parser;

#[tokio::main]
async fn main() {
    // Behave like a normal Unix tool in pipelines: die quietly on a closed pipe instead of
    // panicking in println!.
    unsafe {
        let _ = nix::sys::signal::signal(
            nix::sys::signal::Signal::SIGPIPE,
            nix::sys::signal::SigHandler::SigDfl,
        );
    }
    let args = cli::Cli::parse();
    // The service writes into its clients' pipes (events, attached runs): a client that
    // leaves must cost it a write error, not its life.
    if matches!(args.command, cli::Command::Daemon(_)) {
        unsafe {
            let _ = nix::sys::signal::signal(
                nix::sys::signal::Signal::SIGPIPE,
                nix::sys::signal::SigHandler::SigIgn,
            );
        }
    }
    if let Err(err) = commands::run(args).await {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}
