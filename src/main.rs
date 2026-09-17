//! nspawn: docker-like management of systemd-nspawn machines.
//!
//! Images come from an OCI registry (the hub) and are stored as shared layers under
//! /var/lib/nspawn. Machines are driven through the D-Bus APIs of
//! systemd-machined and systemd itself, never through machinectl.

mod backend;
mod cli;
mod commands;
mod config;
mod hostnet;
mod hub;
mod install;
mod layout;
mod nsenter;
mod oci;
mod output;
mod pty;
mod reference;
mod settings;
mod store;
mod systemd;
mod unitname;

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
    if let Err(err) = commands::run(args).await {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}
