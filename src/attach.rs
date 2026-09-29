//! A terminal, an input or an output for an app's program (run -t, -i, and a run of a
//! machine whose log driver is none). systemd-nspawn takes its
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
    /// A pipe for stdout and stderr, and the caller's stdin when `stdin`: the output of a
    /// machine that keeps none in the journal.
    Output { stdin: bool },
}

impl Mode {
    fn encode(&self) -> Vec<u8> {
        match self {
            Mode::Tty { term } => format!("tty\n{term}\n").into_bytes(),
            Mode::Stdin => b"stdin\n".to_vec(),
            Mode::Output { stdin: false } => b"output\n".to_vec(),
            Mode::Output { stdin: true } => b"output\nstdin\n".to_vec(),
        }
    }

    /// How many descriptors come with the mode.
    fn descriptors(&self) -> usize {
        match self {
            Mode::Output { stdin: true } => 2,
            _ => 1,
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
            "output" => match lines.next() {
                None => Some(Mode::Output { stdin: false }),
                Some("stdin") => Some(Mode::Output { stdin: true }),
                Some(_) => None,
            },
            _ => None,
        }
    }

    fn console(&self) -> &'static str {
        match self {
            Mode::Tty { .. } => "--console=interactive",
            Mode::Stdin | Mode::Output { .. } => "--console=pipe",
        }
    }
}

/// The ExecStart of a machine. With `journal`, the output systemd-nspawn relays goes to
/// that journald namespace instead of the unit's own stdout, unless a run takes it.
pub fn exec(name: &str, journal: Option<&str>, argv: &[OsString]) -> Result<Infallible> {
    let Some((program, rest)) = argv.split_first() else {
        bail!("attach-exec: nothing to run");
    };
    let mut command = std::process::Command::new(program);
    let mut taken = false;
    let mut stdin_handed = false;
    match receive(name) {
        Ok(Some((mode, fds, mut stream))) => {
            taken = !matches!(mode, Mode::Stdin);
            stdin_handed = matches!(mode, Mode::Stdin);
            let fd = &fds[0];
            match &mode {
                Mode::Tty { term } => {
                    nix::unistd::dup2_stdin(fd).context("attaching standard input")?;
                    nix::unistd::dup2_stdout(fd).context("attaching standard output")?;
                    // As the controlling terminal of the session systemd made, resizes
                    // reach systemd-nspawn as SIGWINCH.
                    let _ = nix::unistd::setsid();
                    if let Err(e) = nsenter::take_controlling_terminal(fd) {
                        eprintln!("attach-exec: the terminal cannot be the controlling one: {e}");
                    }
                    command.env("TERM", term);
                }
                Mode::Stdin => {
                    nix::unistd::dup2_stdin(fd).context("attaching standard input")?;
                }
                Mode::Output { stdin } => {
                    nix::unistd::dup2_stdout(fd).context("attaching standard output")?;
                    nix::unistd::dup2_stderr(fd).context("attaching standard error")?;
                    if *stdin {
                        nix::unistd::dup2_stdin(&fds[1]).context("attaching standard input")?;
                    }
                    keep_own_messages_in_journal(&mut command);
                }
            }
            command.arg(mode.console());
            stream.write_all(b"ok").context("answering the run")?;
        }
        Ok(None) => {}
        // The run notices that nothing arrived; the machine starts anyway.
        Err(e) => eprintln!("attach-exec: {e:#}"),
    }
    // A terminal or a pipe of a run carries the output itself.
    if let (Some(namespace), false) = (journal, taken) {
        match journal_stream(namespace, name) {
            Ok(stream) => {
                nix::unistd::dup2_stdout(&stream).context("attaching the journal")?;
                // With --console=pipe the program writes to systemd-nspawn's stderr too.
                if stdin_handed {
                    nix::unistd::dup2_stderr(&stream).context("attaching the journal")?;
                    keep_own_messages_in_journal(&mut command);
                }
            }
            // Better the system's journal than no machine.
            Err(e) => eprintln!("attach-exec: {e:#}; the output goes to the system's journal"),
        }
    }
    let error = command.args(rest).exec();
    bail!("running {}: {error}", program.to_string_lossy())
}

/// systemd-nspawn logs to its stderr unless that is the unit's journal stream: once its
/// stderr is the program's output, its own messages (a notice on every start since 262)
/// would land among the program's lines. They go to the system's journal instead. The
/// machine's environment is systemd-nspawn's to build, so this stays outside.
fn keep_own_messages_in_journal(command: &mut std::process::Command) {
    command.env("SYSTEMD_LOG_TARGET", "journal");
}

/// A stream into journald's `namespace`, as sd_journal_stream_fd_with_namespace() opens
/// it: the header names the identifier, priority info, level prefixes parsed as a
/// service's are, and nothing forwarded. journald files the lines under this process's
/// unit, the machine's.
fn journal_stream(namespace: &str, identifier: &str) -> Result<OwnedFd> {
    let path = journal_stream_path(namespace);
    let stream =
        UnixStream::connect(&path).with_context(|| format!("connecting to {}", path.display()))?;
    stream
        .shutdown(std::net::Shutdown::Read)
        .context("closing the journal stream's reading side")?;
    (&stream)
        .write_all(journal_stream_header(identifier).as_bytes())
        .with_context(|| format!("writing to {}", path.display()))?;
    Ok(stream.into())
}

fn journal_stream_path(namespace: &str) -> PathBuf {
    PathBuf::from(format!("/run/systemd/journal.{namespace}/stdout"))
}

fn journal_stream_header(identifier: &str) -> String {
    format!("{identifier}\n\n6\n1\n0\n0\n0\n")
}

fn receive(name: &str) -> Result<Option<(Mode, Vec<OwnedFd>, UnixStream)>> {
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
    let (bytes, fds) = nsenter::receive_fds(&stream)?;
    let mode = Mode::decode(&bytes).context("the run sent something unexpected")?;
    if fds.len() != mode.descriptors() {
        bail!(
            "the run sent {} descriptors instead of {}",
            fds.len(),
            mode.descriptors()
        );
    }
    Ok(Some((mode, fds, stream)))
}

/// The service's end of one run's socket, removed when dropped.
pub struct Listener {
    name: String,
    path: PathBuf,
    /// The inode bound, so that only this socket is removed: another run of the same
    /// name may have replaced it meanwhile.
    ino: u64,
    listener: tokio::net::UnixListener,
}

impl Listener {
    /// Binds the run's socket. One a run still waits on is left alone and refused; one
    /// left behind by a run that went (nothing answers) is replaced.
    pub fn bind(name: &str) -> Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let dir = PathBuf::from(DIR);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restricting {}", dir.display()))?;
        let path = socket_path(name);
        let listener = match tokio::net::UnixListener::bind(&path) {
            Ok(listener) => listener,
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                if UnixStream::connect(&path).is_ok() {
                    bail!("a run of {name} is waiting to hand its terminal or input over already");
                }
                std::fs::remove_file(&path)
                    .with_context(|| format!("removing {}", path.display()))?;
                tokio::net::UnixListener::bind(&path)
                    .with_context(|| format!("listening on {}", path.display()))?
            }
            Err(e) => return Err(e).with_context(|| format!("listening on {}", path.display())),
        };
        let ino = std::fs::symlink_metadata(&path)
            .with_context(|| format!("looking at {}", path.display()))?
            .ino();
        Ok(Listener {
            name: name.to_string(),
            path,
            ino,
            listener,
        })
    }

    /// Hands `fds` over when the machine's ExecStart asks, and waits for it to take them.
    /// Only a root process in the machine's own unit is answered.
    pub async fn hand_over(&self, mode: &Mode, fds: &[OwnedFd], timeout: Duration) -> Result<()> {
        if fds.len() != mode.descriptors() {
            bail!(
                "{} descriptors for a mode that takes {}",
                fds.len(),
                mode.descriptors()
            );
        }
        let fds: Vec<&OwnedFd> = fds.iter().collect();
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
            nsenter::send_fds(&stream, &mode.encode(), &fds)?;
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
        use std::os::unix::fs::MetadataExt;
        if std::fs::symlink_metadata(&self.path)
            .map(|m| m.ino() == self.ino)
            .unwrap_or(false)
        {
            let _ = std::fs::remove_file(&self.path);
        }
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
            Mode::Output { stdin: false },
            Mode::Output { stdin: true },
        ] {
            assert_eq!(Mode::decode(&mode.encode()), Some(mode));
        }
        assert_eq!(Mode::Output { stdin: true }.descriptors(), 2);
        assert_eq!(Mode::Output { stdin: false }.descriptors(), 1);
        assert_eq!(Mode::decode(b"output\nstderr\n"), None);
        assert_eq!(Mode::decode(b"tty\n\n"), None, "a terminal needs its TERM");
        assert_eq!(Mode::decode(b"shell\n"), None);
    }

    #[test]
    fn the_journal_stream_is_opened_as_systemd_does() {
        assert_eq!(
            journal_stream_path("nspawn"),
            PathBuf::from("/run/systemd/journal.nspawn/stdout")
        );
        // Identifier, unit (none), priority 6, level prefix, no syslog, kmsg or console.
        assert_eq!(journal_stream_header("web"), "web\n\n6\n1\n0\n0\n0\n");
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
