//! A terminal or an input for an app's program (run -i, -t). systemd-nspawn takes its
//! console mode only on its command line, and the unit's stdio is fixed in its files, so
//! an app machine's ExecStart is `nspawn attach-exec NAME -- ARGV`: when a run waits on
//! the service's socket for that machine, it receives the descriptors there and execs
//! ARGV on them with the matching --console; otherwise it execs ARGV as it is. Nothing is
//! written to the unit, so no later start can pick up a stale terminal.

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

/// One socket per waiting run.
pub const DIR: &str = "/run/nspawn/attach";

pub fn socket_path(name: &str) -> PathBuf {
    PathBuf::from(DIR).join(format!("{name}.sock"))
}

/// What a run hands over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// A pseudo terminal for stdin and stdout, and its TERM.
    Tty { term: String },
    /// The caller's stdin; the output stays in the journal.
    Stdin,
}

impl Mode {
    fn encode(&self) -> Vec<u8> {
        match self {
            Mode::Tty { term } => format!("tty\n{term}\n").into_bytes(),
            Mode::Stdin => b"stdin\n".to_vec(),
        }
    }

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

    fn console(&self) -> &'static str {
        match self {
            Mode::Tty { .. } => "--console=interactive",
            Mode::Stdin => "--console=pipe",
        }
    }
}

/// The ExecStart of an app machine.
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
                // As the controlling terminal of the session systemd made, resizes reach
                // systemd-nspawn as SIGWINCH.
                let _ = nix::unistd::setsid();
                if let Err(e) = nsenter::take_controlling_terminal(&fd) {
                    eprintln!("attach-exec: the terminal cannot be the controlling one: {e}");
                }
                command.env("TERM", term);
            }
            command.arg(mode.console());
            stream.write_all(b"ok").context("answering the run")?;
        }
        Ok(None) => {}
        // The run notices that nothing arrived; the machine starts anyway.
        Err(e) => eprintln!("attach-exec: {e:#}"),
    }
    let error = command.args(rest).exec();
    bail!("running {}: {error}", program.to_string_lossy())
}

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

/// The service's end of one run's socket, removed when dropped.
pub struct Listener {
    name: String,
    path: PathBuf,
    listener: tokio::net::UnixListener,
}

impl Listener {
    pub fn bind(name: &str) -> Result<Self> {
        let dir = PathBuf::from(DIR);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restricting {}", dir.display()))?;
        let path = socket_path(name);
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path)
            .with_context(|| format!("listening on {}", path.display()))?;
        Ok(Listener {
            name: name.to_string(),
            path,
            listener,
        })
    }

    /// Hands `fd` over when the machine's ExecStart asks, and waits for it to take it.
    /// Only a root process in the machine's own unit is answered.
    pub async fn hand_over(&self, mode: &Mode, fd: &OwnedFd, timeout: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let (mut stream, _) = tokio::time::timeout_at(deadline, self.listener.accept())
                .await
                .with_context(|| format!("{} did not ask for its terminal or input", self.name))?
                .context("accepting on the run's socket")?;
            let Ok(credentials) = stream.peer_cred() else {
                continue;
            };
            let in_unit = credentials
                .pid()
                .and_then(|pid| std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok())
                .is_some_and(|cgroup| in_machine_unit(&cgroup, &self.name));
            if credentials.uid() != 0 || !in_unit {
                continue;
            }
            nsenter::send_fds(&stream, &mode.encode(), &[fd])?;
            let mut answer = [0u8; 2];
            use tokio::io::AsyncReadExt;
            tokio::time::timeout_at(deadline, stream.read_exact(&mut answer))
                .await
                .with_context(|| format!("{} did not take its terminal or input", self.name))?
                .context("reading the machine's answer")?;
            if &answer != b"ok" {
                bail!("{} refused its terminal or input", self.name);
            }
            return Ok(());
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Whether a /proc/PID/cgroup is the unit of machine `name` or below it.
fn in_machine_unit(cgroup: &str, name: &str) -> bool {
    let unit = format!("/systemd-nspawn@{name}.service");
    cgroup.lines().any(|line| {
        line.strip_prefix("0::").is_some_and(|path| {
            path.split_once(&unit)
                .is_some_and(|(_, rest)| rest.is_empty() || rest.starts_with('/'))
        })
    })
}

/// Removes the sockets a previous service left.
pub fn sweep() {
    if let Ok(entries) = std::fs::read_dir(DIR) {
        for entry in entries.flatten() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_survive_the_socket() {
        for mode in [
            Mode::Tty {
                term: "xterm-256color".into(),
            },
            Mode::Stdin,
        ] {
            assert_eq!(Mode::decode(&mode.encode()), Some(mode));
        }
        assert_eq!(Mode::decode(b"tty\n\n"), None, "a terminal needs its TERM");
        assert_eq!(Mode::decode(b"shell\n"), None);
    }

    #[test]
    fn only_the_machine_s_own_unit_is_answered() {
        let own = "0::/machine.slice/systemd-nspawn@web.service/supervisor\n";
        assert!(in_machine_unit(own, "web"));
        assert!(in_machine_unit(
            "0::/machine.slice/systemd-nspawn@web.service\n",
            "web"
        ));
        assert!(!in_machine_unit(own, "we"));
        assert!(!in_machine_unit(
            "0::/machine.slice/systemd-nspawn@web.service2/x\n",
            "web"
        ));
        assert!(!in_machine_unit("0::/user.slice/user-1000.slice\n", "web"));
    }
}
