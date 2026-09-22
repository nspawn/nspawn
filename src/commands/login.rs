//! The terminal side of login: prompts for what the command line did not give.

use std::io::{self, BufRead, Write};

use anyhow::{Context as _, Result};
use nix::sys::signal::{self, SigHandler, Signal};
use nix::sys::termios::{self, LocalFlags, SetArg};
use nix::unistd::isatty;

use crate::api::{self, Context};
use crate::auth::{self, Credentials};
use crate::cli::{LoginArgs, LogoutArgs};

pub async fn login(args: LoginArgs, ctx: &Context) -> Result<()> {
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
    let done = api::login::login(ctx, args.registry, Credentials { username, password }).await?;
    if done.asked {
        println!("logged in to {} as {}", done.registry, done.username);
    } else {
        println!(
            "{} did not ask for credentials; kept them for {} anyway in {}",
            done.registry,
            done.username,
            auth::STORE
        );
    }
    Ok(())
}

pub fn logout(args: LogoutArgs, ctx: &Context) -> Result<()> {
    let done = api::login::logout(ctx, args.registry)?;
    if done.removed {
        println!("removed the credentials for {}", done.registry);
    } else {
        println!(
            "no credentials stored for {} in {}",
            done.registry,
            auth::STORE
        );
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
