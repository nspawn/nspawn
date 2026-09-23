//! The terminal side of login: prompts for what the command line did not give.

use std::io::{self, BufRead, Write};

use anyhow::{Context as _, Result};
use nix::sys::signal::{self, SigHandler, Signal};
use nix::sys::termios::{self, LocalFlags, SetArg};
use nix::unistd::isatty;

use crate::auth;
use crate::cli::{LoginArgs, LogoutArgs};
use crate::client::{self, Client};
use crate::config::Config;

pub async fn login(args: LoginArgs, client: &Client, config: &Config) -> Result<()> {
    let username = match args.username {
        Some(u) => u,
        None => prompt("Username: ", false)?,
    };
    let password = if args.password_stdin {
        let mut text = String::new();
        io::stdin()
            .lock()
            .read_line(&mut text)
            .context("reading the password from stdin")?;
        text.trim_end_matches(['\n', '\r']).to_string()
    } else {
        prompt("Password: ", true)?
    };
    let done = client
        .manager
        .login(
            args.registry.as_deref().unwrap_or(""),
            &username,
            &password,
            client::registry_options(config),
        )
        .await
        .map_err(client::error)?;
    let registry = client::string(&done, "registry");
    let username = client::string(&done, "username");
    if client::bool(&done, "asked") {
        println!("logged in to {registry} as {username}");
    } else {
        println!(
            "{registry} did not ask for credentials; kept them for {username} anyway in {}",
            auth::STORE
        );
    }
    Ok(())
}

pub async fn logout(args: LogoutArgs, client: &Client, config: &Config) -> Result<()> {
    let registry = args.registry.unwrap_or_else(|| config.registry.clone());
    let removed = client
        .manager
        .logout(&registry)
        .await
        .map_err(client::error)?;
    if removed {
        println!("removed the credentials for {registry}");
    } else {
        println!("no credentials stored for {registry} in {}", auth::STORE);
    }
    Ok(())
}

/// Reads one line from the terminal, without echo for secrets.
fn prompt(label: &str, hidden: bool) -> Result<String> {
    let stdin = io::stdin();
    print!("{label}");
    io::stdout().flush()?;
    let saved = if hidden && isatty(&stdin).unwrap_or(false) {
        let original = termios::tcgetattr(&stdin)?;
        let mut quiet = original.clone();
        quiet.local_flags.remove(LocalFlags::ECHO);
        termios::tcsetattr(&stdin, SetArg::TCSANOW, &quiet)?;
        // Ctrl-C would kill us with the echo still off; ignore it while it is.
        let previous = unsafe { signal::signal(Signal::SIGINT, SigHandler::SigIgn) }.ok();
        Some((original, previous))
    } else {
        None
    };
    let mut text = String::new();
    let read = stdin.lock().read_line(&mut text);
    if let Some((original, previous)) = saved {
        let _ = termios::tcsetattr(&stdin, SetArg::TCSANOW, &original);
        if let Some(previous) = previous {
            let _ = unsafe { signal::signal(Signal::SIGINT, previous) };
        }
        println!();
    }
    read.context("reading from the terminal")?;
    Ok(text.trim_end_matches(['\n', '\r']).to_string())
}
