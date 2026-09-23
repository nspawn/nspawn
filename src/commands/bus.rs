//! Going through the org.nspawn service instead of doing the work here: what any other
//! client of the bus does, with the terminal handled on this side.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context as _, Result};
use nix::sys::signal::{pthread_sigmask, SigSet, SigmaskHow, Signal};
use zbus::zvariant::{OwnedObjectPath, Value};

use crate::daemon::{BUS_NAME, MANAGER_PATH};
use crate::pty;

/// Exec over the bus: a pseudo terminal when standard input is one, pipes otherwise.
/// Returns the command's exit status.
pub async fn exec(machine: &str, argv: &[String], user: &str) -> Result<i32> {
    let connection = zbus::Connection::system()
        .await
        .context("connecting to the system bus")?;
    let manager = zbus::Proxy::new(&connection, BUS_NAME, MANAGER_PATH, "org.nspawn.Manager")
        .await
        .context("reaching org.nspawn")?;
    let tty = nix::unistd::isatty(io::stdin()).unwrap_or(false);
    let (rows, cols) = pty::window_size().unwrap_or((24, 80));
    let options: HashMap<&str, Value<'_>> = HashMap::from([
        ("tty", Value::from(tty)),
        ("rows", Value::from(rows as u64)),
        ("cols", Value::from(cols as u64)),
    ]);
    // The reply keeps its own copies of the descriptors; they must go before the
    // command's streams are used, or its input never sees the end of ours.
    let (mut fds, process): (HashMap<String, zbus::zvariant::OwnedFd>, OwnedObjectPath) = {
        let reply = manager
            .call_method("Exec", &(machine, argv, user, options))
            .await
            .with_context(|| format!("Exec on {BUS_NAME}"))?;
        let body = reply.body();
        body.deserialize().context("reading the reply of Exec")?
    };
    let mut take = |name: &str| fds.remove(name).map(OwnedFd::from);
    if let Some(master) = take("tty") {
        tokio::task::block_in_place(|| pty::run_session(master))?;
    } else {
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (take("stdin"), take("stdout"), take("stderr"))
        else {
            bail!("Exec returned neither a terminal nor pipes");
        };
        tokio::task::block_in_place(|| pump_pipes(stdin, stdout, stderr))?;
    }
    exit_status(&connection, &process).await
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
async fn exit_status(connection: &zbus::Connection, process: &OwnedObjectPath) -> Result<i32> {
    // No property cache: the value has to come from the service each time.
    let proxy = zbus::proxy::Builder::<zbus::Proxy<'_>>::new(connection)
        .destination(BUS_NAME)?
        .path(process.clone())?
        .interface("org.nspawn.Process")?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .context("reaching the process object")?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let state: String = proxy.get_property("State").await?;
        if state == "exited" {
            return Ok(proxy.get_property::<i32>("ExitStatus").await?);
        }
        if Instant::now() > deadline {
            bail!("the command's streams closed but the service has not seen it exit");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
