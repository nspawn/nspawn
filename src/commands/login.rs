use std::io::{self, BufRead, Write};

use anyhow::{bail, Context, Result};
use nix::sys::signal::{self, SigHandler, Signal};
use nix::sys::termios::{self, LocalFlags, SetArg};
use nix::unistd::isatty;

use crate::auth::{self, Credentials};
use crate::cli::{LoginArgs, LogoutArgs};
use crate::commands::require_root;
use crate::config::Config;

/// docker login: checks the credentials against the registry and keeps them for pull,
/// push and search.
pub async fn login(args: LoginArgs, config: &Config) -> Result<()> {
    require_root("login")?;
    let registry = args.registry.unwrap_or_else(|| config.registry.clone());
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
    if username.is_empty() || password.is_empty() {
        bail!("a username and a password are needed");
    }
    let credentials = Credentials { username, password };
    // The CA certificate applies to the hub only.
    let ca_cert = (auth::canonical(&registry) == auth::canonical(&config.registry))
        .then_some(config.ca_cert.as_deref())
        .flatten();
    let asked = auth::verify(&registry, &credentials, ca_cert).await?;
    auth::store(&registry, &credentials)?;
    if asked {
        println!("logged in to {registry} as {}", credentials.username);
    } else {
        println!(
            "{registry} did not ask for credentials; kept them for {} anyway in {}",
            credentials.username,
            auth::STORE
        );
    }
    Ok(())
}

pub fn logout(args: LogoutArgs, config: &Config) -> Result<()> {
    require_root("logout")?;
    let registry = args.registry.unwrap_or_else(|| config.registry.clone());
    if auth::forget(&registry)? {
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
