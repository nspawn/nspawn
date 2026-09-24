//! A terminal or an input for the program of an app machine, the way docker run -it
//! gives one. systemd-nspawn only takes its console mode on the command line, and uses a
//! terminal when its own standard input and output are one; the unit's stdio is fixed
//! in its files. So the ExecStart of an app machine runs `nspawn attach-exec NAME --
//! ARGV`: when an attached run of that machine is waiting, it receives the descriptors
//! over a socket of the service and execs systemd-nspawn on them with the matching
//! --console; when none is, it execs ARGV as it is. No drop-in, no reload and no path
//! of a terminal is involved, so nothing is left that a later start could pick up.

use std::convert::Infallible;
use std::ffi::OsString;
use std::io::Write;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::nsenter;

/// Where the service waits for the machine's systemd-nspawn, one socket per run.
pub const DIR: &str = "/run/nspawn/attach";

pub fn socket_path(name: &str) -> PathBuf {
    PathBuf::from(DIR).join(format!("{name}.sock"))
}

/// What a run hands over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// A pseudo terminal, for standard input and output; its TERM goes along.
    Tty { term: String },
    /// The caller's standard input; the output stays the unit's (the journal).
    Stdin,
}

impl Mode {
    fn decode(bytes: &[u8]) -> Option<Mode> {
        let text = std::str::from_utf8(bytes).ok()?;
        let mut lines = text.lines();
        match lines.next()? {
            "tty" => Some(Mode::Tty {
                term: lines.next().filter(|t| !t.is_empty())?.to_string(),
            }),
            "stdin" => Some(Mode::Stdin),
            _ => None,
        }
    }

    /// systemd-nspawn's --console for the mode.
    fn console(&self) -> &'static str {
        match self {
            Mode::Tty { .. } => "--console=interactive",
            Mode::Stdin => "--console=pipe",
        }
    }
}

/// The ExecStart of an app machine: `argv` (systemd-nspawn and its options), with what a
/// waiting run hands over when there is one.
pub fn exec(name: &str, argv: &[OsString]) -> Result<Infallible> {
    let Some((program, rest)) = argv.split_first() else {
        bail!("attach-exec: nothing to run");
    };
    let mut command = std::process::Command::new(program);
    match receive(name) {
        Ok(Some((mode, fd, mut stream))) => {
            nix::unistd::dup2_stdin(&fd).context("attaching standard input")?;
            if let Mode::Tty { term } = &mode {
                nix::unistd::dup2_stdout(&fd).context("attaching standard output")?;
                // systemd made the service a session leader: the terminal becomes its
                // controlling one, and resizes reach systemd-nspawn as SIGWINCH.
                let _ = nix::unistd::setsid();
                if let Err(e) = nsenter::take_controlling_terminal(&fd) {
                    eprintln!("attach-exec: the terminal cannot be the controlling one: {e}");
                }
                command.env("TERM", term);
            }
            command.arg(mode.console());
            // The run goes on once systemd-nspawn has its descriptors.
            stream.write_all(b"ok").context("answering the run")?;
        }
        Ok(None) => {}
        // The run notices that nobody answered and says so; the machine starts anyway.
        Err(e) => eprintln!("attach-exec: {e:#}"),
    }
    let error = command.args(rest).exec();
    bail!("running {}: {error}", program.to_string_lossy())
}

/// The descriptors of a run waiting for `name`, if one is.
fn receive(name: &str) -> Result<Option<(Mode, OwnedFd, UnixStream)>> {
    let path = socket_path(name);
    if !path.exists() {
        return Ok(None);
    }
    // A socket left by a service that went away refuses the connection.
    let Ok(stream) = UnixStream::connect(&path) else {
        return Ok(None);
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .context("setting a timeout on the run's socket")?;
    let (bytes, mut fds) = nsenter::receive_fds(&stream)?;
    let mode = Mode::decode(&bytes).context("the run sent something unexpected")?;
    let fd = fds.pop().context("the run sent no descriptor")?;
    Ok(Some((mode, fd, stream)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_as_the_run_sends_them() {
        assert_eq!(
            Mode::decode(b"tty\nxterm-256color\n"),
            Some(Mode::Tty {
                term: "xterm-256color".into()
            })
        );
        assert_eq!(Mode::decode(b"stdin\n"), Some(Mode::Stdin));
        assert_eq!(Mode::decode(b"tty\n\n"), None, "a terminal needs its TERM");
        assert_eq!(Mode::decode(b"shell\n"), None);
    }
}
