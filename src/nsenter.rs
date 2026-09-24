//! docker exec for machines that have no D-Bus inside: enter the namespaces of the
//! machine's leader process and run a command there, on a pseudo terminal or on pipes.
//!
//! setns() into a mount namespace is refused for multithreaded processes, and children
//! only land in a PID namespace after a fork, so the work happens in a forked helper: the
//! helper joins the namespaces, forks once more, and the grandchild execs the command.
//! The helper reports the command's PID over a socket, with a pidfd and the terminal's
//! master attached, and exits with the command's exit code.
//!
//! The pseudo terminal is allocated inside the machine, as machined and the container
//! runtimes do: a terminal from the host's devpts has no node under the machine's
//! /dev/pts, so tty(1) and everything that opens its terminal by name fail there, and
//! with private users its owner would be nobody.

use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{BufRead, BufReader, IoSlice, IoSliceMut};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use anyhow::{bail, Context, Result};
use nix::errno::Errno;
use nix::fcntl::OFlag;
use nix::libc;
use nix::pty::Winsize;
use nix::sched::{setns, CloneFlags};
use nix::sys::socket::{
    recvmsg, sendmsg, socketpair, AddressFamily, ControlMessage, ControlMessageOwned, MsgFlags,
    SockFlag, SockType,
};
use nix::sys::stat::Mode;
use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::{
    chdir, dup2_stderr, dup2_stdin, dup2_stdout, execve, fork, pipe2, setgid, setgroups, setsid,
    setuid, ForkResult, Gid, Pid, Uid,
};

/// How the command's standard streams are set up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stdio {
    /// A pseudo terminal of this size; its master comes back in `Process::master`.
    Pty { rows: u16, cols: u16 },
    /// Three pipes; the caller's ends come back in `Process`.
    Pipes,
}

/// A command started inside a machine and not waited for yet.
pub struct Process {
    /// The helper that forked the command; `wait` reaps it for the command's exit code.
    pub helper: Pid,
    /// The command's PID as the host sees it.
    pub pid: u32,
    /// A pidfd of the command: signals go there, never to a PID that may have been
    /// given to someone else meanwhile.
    pub pidfd: OwnedFd,
    pub master: Option<OwnedFd>,
    pub stdin: Option<OwnedFd>,
    pub stdout: Option<OwnedFd>,
    pub stderr: Option<OwnedFd>,
}

/// What the helper is asked to set up for the command.
enum ChildIo {
    Pty {
        rows: u16,
        cols: u16,
    },
    Pipes {
        stdin: OwnedFd,
        stdout: OwnedFd,
        stderr: OwnedFd,
    },
}

/// The command's side of its streams, once made.
enum CommandIo {
    /// The master of a terminal allocated inside the machine; the command opens its
    /// slave through it.
    Terminal(OwnedFd),
    Pipes {
        stdin: OwnedFd,
        stdout: OwnedFd,
        stderr: OwnedFd,
    },
}

/// What the helper tells its parent once it knows: the command's PID (its pidfd and,
/// with a terminal, the master ride along as descriptors), or why there is no command.
#[derive(Debug, PartialEq, Eq)]
enum Started {
    Command(u32),
    Failed(String),
}

fn encode(report: &Started) -> Vec<u8> {
    match report {
        Started::Command(pid) => {
            let mut bytes = vec![b'p'];
            bytes.extend_from_slice(&pid.to_ne_bytes());
            bytes
        }
        Started::Failed(text) => {
            let mut bytes = vec![b'e'];
            bytes.extend_from_slice(text.as_bytes());
            bytes
        }
    }
}

fn decode(bytes: &[u8]) -> Option<Started> {
    match bytes.split_first()? {
        (b'p', rest) if rest.len() == 4 => {
            Some(Started::Command(u32::from_ne_bytes(rest.try_into().ok()?)))
        }
        (b'e', rest) => Some(Started::Failed(String::from_utf8_lossy(rest).into_owned())),
        _ => None,
    }
}

nix::ioctl_write_int_bad!(tiocsctty, libc::TIOCSCTTY);
nix::ioctl_write_ptr_bad!(tiocsptlck, libc::TIOCSPTLCK, libc::c_int);
nix::ioctl_write_ptr_bad!(tiocswinsz, libc::TIOCSWINSZ, Winsize);
nix::ioctl_write_int_bad!(tiocgptpeer, libc::TIOCGPTPEER);

/// Namespaces to join, in the order the kernel likes: the user namespace first so that we
/// gain the right capabilities, the mount namespace last.
const NAMESPACES: [(&str, CloneFlags); 7] = [
    ("user", CloneFlags::CLONE_NEWUSER),
    ("cgroup", CloneFlags::CLONE_NEWCGROUP),
    ("ipc", CloneFlags::CLONE_NEWIPC),
    ("uts", CloneFlags::CLONE_NEWUTS),
    ("net", CloneFlags::CLONE_NEWNET),
    ("pid", CloneFlags::CLONE_NEWPID),
    ("mnt", CloneFlags::CLONE_NEWNS),
];

/// Reaps the helper of a `Process`: the command's exit code, 128 plus the signal when
/// it died of one.
pub fn wait(helper: Pid) -> Result<i32> {
    let status = waitpid(helper, None).context("waiting for the namespace helper")?;
    Ok(exit_code(status))
}

/// pidfd_open(2): a handle on `pid` that follows that process alone.
pub fn pidfd_open(pid: Pid) -> nix::Result<OwnedFd> {
    let fd = unsafe {
        libc::syscall(
            libc::SYS_pidfd_open,
            pid.as_raw() as libc::c_long,
            0 as libc::c_long,
        )
    };
    // SAFETY: a successful pidfd_open returns a descriptor nobody else owns.
    Errno::result(fd).map(|fd| unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

/// pidfd_send_signal(2): `signal` to the process behind `pidfd`, ESRCH once it is gone.
/// A raw number, so that the realtime signals images ask for work too.
pub fn pidfd_signal(pidfd: &OwnedFd, signal: i32) -> nix::Result<()> {
    Errno::result(unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd() as libc::c_long,
            signal as libc::c_long,
            std::ptr::null::<libc::siginfo_t>(),
            0 as libc::c_long,
        )
    })
    .map(drop)
}

/// Raises the file capabilities (chown, DAC override and search, fowner, fsetid) of this
/// thread again after a setfsuid(2) away from 0 dropped them: a copy into an idmapped
/// tree writes as the machine's root, and still needs to write where root may.
pub fn raise_file_capabilities() -> nix::Result<()> {
    #[repr(C)]
    struct Header {
        version: u32,
        pid: libc::c_int,
    }
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Data {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }
    const VERSION_3: u32 = 0x2008_0522;
    // capability.h: CAP_CHOWN 0, CAP_DAC_OVERRIDE 1, CAP_DAC_READ_SEARCH 2,
    // CAP_FOWNER 3, CAP_FSETID 4.
    const FILE_CAPABILITIES: u32 = 0b1_1111;
    let mut header = Header {
        version: VERSION_3,
        pid: 0,
    };
    let mut data = [Data::default(); 2];
    // SAFETY: header and data are the layouts capget(2) and capset(2) take for version
    // 3, two data entries; pid 0 is this thread.
    Errno::result(unsafe { libc::syscall(libc::SYS_capget, &mut header, data.as_mut_ptr()) })?;
    data[0].effective |= FILE_CAPABILITIES & data[0].permitted;
    header.version = VERSION_3;
    header.pid = 0;
    Errno::result(unsafe { libc::syscall(libc::SYS_capset, &mut header, data.as_ptr()) }).map(drop)
}

/// Starts `argv` inside the machine whose leader is `leader`, with `image_env` plus PATH
/// when missing (and TERM, on a terminal), its streams set up as `stdio` says.
/// `leader_fd` is a pidfd of the leader, taken while the machine named it: the
/// namespaces are only used once it shows that the PID was not given to another
/// process meanwhile.
pub fn spawn(
    leader: u32,
    leader_fd: &OwnedFd,
    argv: &[String],
    user: Option<&str>,
    working_dir: Option<&str>,
    image_env: &[String],
    stdio: Stdio,
) -> Result<Process> {
    if argv.is_empty() {
        bail!("no command given");
    }
    let cgroup = leader_cgroup(leader);
    let mut ns_fds = Vec::new();
    for (name, flag) in NAMESPACES {
        // Namespaces the machine shares with us (the host's network namespace for app
        // images, for instance) are owned by a user namespace we leave behind at the first
        // setns(), so joining them again would fail with EPERM. Skip them.
        if same_namespace(leader, name)? {
            continue;
        }
        let path = format!("/proc/{leader}/ns/{name}");
        let fd: OwnedFd = File::open(&path)
            .with_context(|| format!("opening {path}"))?
            .into();
        ns_fds.push((name, flag, fd));
    }
    let exec_context = selinux_context_of(leader);
    let bounding = std::fs::read_to_string(format!("/proc/{leader}/status"))
        .ok()
        .and_then(|status| capability_bounding_set(&status));
    // Read through /proc/<leader> above: a process that is still alive now is the one
    // they came from.
    if pidfd_signal(leader_fd, 0).is_err() {
        bail!("the machine ended while the command was being started");
    }
    let c_argv: Vec<CString> = argv
        .iter()
        .map(|a| CString::new(a.as_str()))
        .collect::<Result<_, _>>()?;
    let mut env: Vec<CString> = image_env
        .iter()
        .filter(|v| !v.starts_with("HOME="))
        .map(|v| CString::new(v.as_str()))
        .collect::<Result<_, _>>()?;
    if !image_env.iter().any(|v| v.starts_with("PATH=")) {
        env.push(CString::new(
            "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        )?);
    }
    // The caller says what terminal it has (the service's own environment has none);
    // docker's default stands in otherwise.
    if matches!(stdio, Stdio::Pty { .. }) && !image_env.iter().any(|v| v.starts_with("TERM=")) {
        env.push(CString::new("TERM=xterm")?);
    }
    let c_cwd = CString::new(working_dir.unwrap_or("/"))?;
    let user = user.map(|u| u.to_string());
    // Everything the two sides hold, made before the fork. The command's ends are kept
    // out of what it execs (dup2 onto 0, 1 and 2 clears close-on-exec there).
    let mut ours = (None, None, None);
    let child_io = match stdio {
        Stdio::Pty { rows, cols } => ChildIo::Pty { rows, cols },
        Stdio::Pipes => {
            let (stdin_r, stdin_w) = pipe2(OFlag::O_CLOEXEC).context("creating a pipe")?;
            let (stdout_r, stdout_w) = pipe2(OFlag::O_CLOEXEC).context("creating a pipe")?;
            let (stderr_r, stderr_w) = pipe2(OFlag::O_CLOEXEC).context("creating a pipe")?;
            ours = (Some(stdin_w), Some(stdout_r), Some(stderr_r));
            ChildIo::Pipes {
                stdin: stdin_r,
                stdout: stdout_w,
                stderr: stderr_w,
            }
        }
    };
    // The helper reports through here, once.
    let (report_r, report_w) = socketpair(
        AddressFamily::Unix,
        SockType::SeqPacket,
        None,
        SockFlag::SOCK_CLOEXEC,
    )
    .context("creating a socket pair")?;

    // SAFETY: the parent is multithreaded (tokio), so the child only performs syscalls and
    // work on data prepared above until it execs or exits. It allocates (glibc's fork
    // handlers keep malloc usable) but never takes a lock of the parent's, which is why
    // it writes its complaints with write(2) rather than eprintln.
    match unsafe { fork() }.context("forking the namespace helper")? {
        ForkResult::Parent { child } => {
            drop(ns_fds);
            drop(child_io);
            drop(report_w);
            let (report, mut fds) = receive(&report_r);
            match report {
                Some(Started::Command(pid)) => {
                    let pidfd = if fds.is_empty() {
                        let _ = waitpid(child, None);
                        bail!("the namespace helper reported no pidfd for the command");
                    } else {
                        fds.remove(0)
                    };
                    let master = fds.pop();
                    Ok(Process {
                        helper: child,
                        pid,
                        pidfd,
                        master,
                        stdin: ours.0,
                        stdout: ours.1,
                        stderr: ours.2,
                    })
                }
                Some(Started::Failed(text)) => {
                    let _ = waitpid(child, None);
                    bail!("{text}");
                }
                None => {
                    let _ = waitpid(child, None);
                    bail!("the namespace helper died before running the command");
                }
            }
        }
        ForkResult::Child => {
            drop(ours);
            drop(report_r);
            let code = helper(
                &ns_fds,
                cgroup.as_deref(),
                child_io,
                report_w,
                &c_argv,
                &env,
                &c_cwd,
                user.as_deref(),
                exec_context.as_deref(),
                bounding,
            );
            unsafe { libc::_exit(code) }
        }
    }
}

/// The helper's one message, with the descriptors it attaches.
fn receive(socket: &OwnedFd) -> (Option<Started>, Vec<OwnedFd>) {
    let mut buf = [0u8; 4096];
    let mut fds = Vec::new();
    let bytes = {
        let mut iov = [IoSliceMut::new(&mut buf)];
        let mut space = nix::cmsg_space!([RawFd; 2]);
        let message = loop {
            match recvmsg::<()>(
                socket.as_raw_fd(),
                &mut iov,
                Some(&mut space),
                MsgFlags::MSG_CMSG_CLOEXEC,
            ) {
                Ok(message) => break message,
                Err(Errno::EINTR) => continue,
                Err(_) => return (None, fds),
            }
        };
        if let Ok(controls) = message.cmsgs() {
            for control in controls {
                if let ControlMessageOwned::ScmRights(received) = control {
                    // SAFETY: SCM_RIGHTS hands us fresh descriptors of our own.
                    fds.extend(
                        received
                            .into_iter()
                            .map(|fd| unsafe { OwnedFd::from_raw_fd(fd) }),
                    );
                }
            }
        }
        message.bytes
    };
    (decode(&buf[..bytes]), fds)
}

fn tell(socket: &OwnedFd, report: &Started, fds: &[RawFd]) {
    let bytes = encode(report);
    let iov = [IoSlice::new(&bytes)];
    let rights = [ControlMessage::ScmRights(fds)];
    let controls: &[ControlMessage<'_>] = if fds.is_empty() { &[] } else { &rights };
    let _ = sendmsg::<()>(socket.as_raw_fd(), &iov, controls, MsgFlags::empty(), None);
}

/// Sends `bytes` with `fds` attached (SCM_RIGHTS) over a connected Unix socket, in one
/// message.
pub fn send_fds(socket: &impl AsFd, bytes: &[u8], fds: &[&OwnedFd]) -> Result<()> {
    let raw: Vec<RawFd> = fds.iter().map(|fd| fd.as_raw_fd()).collect();
    let iov = [IoSlice::new(bytes)];
    let rights = [ControlMessage::ScmRights(&raw)];
    let controls: &[ControlMessage<'_>] = if raw.is_empty() { &[] } else { &rights };
    sendmsg::<()>(
        socket.as_fd().as_raw_fd(),
        &iov,
        controls,
        MsgFlags::MSG_NOSIGNAL,
        None,
    )
    .context("sending descriptors")?;
    Ok(())
}

/// Receives one message of at most 256 bytes and the descriptors attached to it.
pub fn receive_fds(socket: &impl AsFd) -> Result<(Vec<u8>, Vec<OwnedFd>)> {
    let mut buf = [0u8; 256];
    let mut space = nix::cmsg_space!([RawFd; 4]);
    let mut iov = [IoSliceMut::new(&mut buf)];
    let message = recvmsg::<()>(
        socket.as_fd().as_raw_fd(),
        &mut iov,
        Some(&mut space),
        MsgFlags::MSG_CMSG_CLOEXEC,
    )
    .context("receiving descriptors")?;
    let mut fds = Vec::new();
    for control in message.cmsgs().context("reading the control messages")? {
        if let ControlMessageOwned::ScmRights(received) = control {
            // SAFETY: SCM_RIGHTS hands us fresh descriptors of our own.
            fds.extend(
                received
                    .into_iter()
                    .map(|fd| unsafe { OwnedFd::from_raw_fd(fd) }),
            );
        }
    }
    let n = message.bytes;
    Ok((buf[..n].to_vec(), fds))
}

/// Makes `tty` the controlling terminal of this process, which leads a session that has
/// none: the kernel then sends it SIGWINCH when the terminal is resized.
pub fn take_controlling_terminal(tty: &impl AsFd) -> nix::Result<()> {
    unsafe { tiocsctty(tty.as_fd().as_raw_fd(), 0) }.map(drop)
}

/// A pseudo terminal of the host, master and slave, sized.
pub fn host_pty(rows: u16, cols: u16) -> Result<(OwnedFd, OwnedFd)> {
    let master = open_terminal(rows, cols).map_err(|e| anyhow::anyhow!(e))?;
    let slave = open_slave(&master).context("opening the pseudo terminal's slave")?;
    Ok((master, slave))
}

/// Writes a line to standard error with write(2): Rust's stderr lock may be held by a
/// thread of the parent that did not come along with the fork.
fn complain(text: &str) {
    let mut bytes = text.as_bytes().to_vec();
    bytes.push(b'\n');
    let _ = unsafe { libc::write(2, bytes.as_ptr() as *const libc::c_void, bytes.len()) };
}

/// The cgroup of the machine's leader, so that what we run is accounted to the machine
/// and dies with it (KillMachine signals the cgroup).
fn leader_cgroup(leader: u32) -> Option<String> {
    let text = std::fs::read_to_string(format!("/proc/{leader}/cgroup")).ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(|path| format!("/sys/fs/cgroup{path}/cgroup.procs"))
}

/// A pseudo terminal from the devpts of the mount namespace we are in (the machine's,
/// by now): its master, unlocked and sized.
fn open_terminal(rows: u16, cols: u16) -> std::result::Result<OwnedFd, String> {
    let flags = OFlag::O_RDWR | OFlag::O_NOCTTY | OFlag::O_CLOEXEC;
    let master = nix::fcntl::open("/dev/pts/ptmx", flags, Mode::empty())
        .or_else(|_| nix::fcntl::open("/dev/ptmx", flags, Mode::empty()))
        .map_err(|e| format!("opening /dev/pts/ptmx inside the machine: {e}"))?;
    let unlock: libc::c_int = 0;
    unsafe { tiocsptlck(master.as_raw_fd(), &unlock) }
        .map_err(|e| format!("unlocking the pseudo terminal: {e}"))?;
    let size = Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let _ = unsafe { tiocswinsz(master.as_raw_fd(), &size) };
    Ok(master)
}

/// The slave of `master`, by the master alone (TIOCGPTPEER): no path lookup, so no
/// mistaking it for a terminal of the same number elsewhere.
fn open_slave(master: &OwnedFd) -> nix::Result<OwnedFd> {
    let flags = OFlag::O_RDWR | OFlag::O_NOCTTY | OFlag::O_CLOEXEC;
    let fd = unsafe { tiocgptpeer(master.as_raw_fd(), flags.bits()) }?;
    // SAFETY: the ioctl returned a new descriptor of ours.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

#[allow(clippy::too_many_arguments)]
fn helper(
    ns_fds: &[(&str, CloneFlags, OwnedFd)],
    cgroup: Option<&str>,
    io: ChildIo,
    socket: OwnedFd,
    argv: &[CString],
    env: &[CString],
    cwd: &CString,
    user: Option<&str>,
    exec_context: Option<&str>,
    bounding: Option<u64>,
) -> i32 {
    // Not dumpable from here on: the command's process is in the machine's PID namespace
    // before it becomes the machine's root and execs, and until then it holds what the
    // service holds; the machine's root must neither trace it nor open its descriptors
    // through /proc. The exec makes the command an ordinary, dumpable program again.
    if let Err(e) = nix::sys::prctl::set_dumpable(false) {
        tell(
            &socket,
            &Started::Failed(format!("keeping the command's process private: {e}")),
            &[],
        );
        return 126;
    }
    if let Some(procs) = cgroup {
        // Best effort: cgroup v1 hosts or delegation quirks must not stop exec.
        let _ = std::fs::write(procs, std::process::id().to_string());
    }
    for (name, flag, fd) in ns_fds {
        if let Err(e) = setns(fd, *flag) {
            // Joining the user namespace we are already in is refused with EINVAL; that is
            // what happens for machines that run without private users.
            if *flag == CloneFlags::CLONE_NEWUSER && e == Errno::EINVAL {
                continue;
            }
            tell(
                &socket,
                &Started::Failed(format!("joining the {name} namespace: {e}")),
                &[],
            );
            return 126;
        }
    }
    let io = match io {
        ChildIo::Pty { rows, cols } => match open_terminal(rows, cols) {
            Ok(master) => CommandIo::Terminal(master),
            Err(text) => {
                tell(&socket, &Started::Failed(text), &[]);
                return 126;
            }
        },
        ChildIo::Pipes {
            stdin,
            stdout,
            stderr,
        } => CommandIo::Pipes {
            stdin,
            stdout,
            stderr,
        },
    };
    match unsafe { fork() } {
        Err(e) => {
            tell(
                &socket,
                &Started::Failed(format!("forking inside the machine: {e}")),
                &[],
            );
            126
        }
        Ok(ForkResult::Parent { child }) => {
            // Taken before the child is reaped, so it can never name another process.
            let pidfd = match pidfd_open(child) {
                Ok(fd) => fd,
                Err(e) => {
                    let _ = nix::sys::signal::kill(child, nix::sys::signal::SIGKILL);
                    let _ = waitpid(child, None);
                    tell(
                        &socket,
                        &Started::Failed(format!("opening a pidfd for the command: {e}")),
                        &[],
                    );
                    return 126;
                }
            };
            let mut fds = vec![pidfd.as_raw_fd()];
            if let CommandIo::Terminal(master) = &io {
                fds.push(master.as_raw_fd());
            }
            tell(&socket, &Started::Command(child.as_raw() as u32), &fds);
            drop(pidfd);
            drop(io);
            drop(socket);
            match waitpid(child, None) {
                Ok(status) => exit_code(status),
                Err(e) => {
                    complain(&format!("error: waiting for the command: {e}"));
                    126
                }
            }
        }
        Ok(ForkResult::Child) => {
            drop(socket);
            let code = grandchild(io, argv, env, cwd, user, exec_context, bounding);
            unsafe { libc::_exit(code) }
        }
    }
}

fn grandchild(
    io: CommandIo,
    argv: &[CString],
    env: &[CString],
    cwd: &CString,
    user: Option<&str>,
    exec_context: Option<&str>,
    bounding: Option<u64>,
) -> i32 {
    // The streams first: from here on, complaints reach whoever runs the command.
    match io {
        CommandIo::Terminal(master) => {
            let slave = match open_slave(&master) {
                Ok(slave) => slave,
                Err(e) => {
                    complain(&format!(
                        "error: opening the terminal inside the machine: {e}"
                    ));
                    return 126;
                }
            };
            drop(master);
            if dup2_stdin(slave.as_fd()).is_err()
                || dup2_stdout(slave.as_fd()).is_err()
                || dup2_stderr(slave.as_fd()).is_err()
            {
                return 126;
            }
            drop(slave);
            if setsid().is_err() {
                complain("error: setsid failed");
                return 126;
            }
            if unsafe { tiocsctty(0, 0) }.is_err() {
                complain("error: cannot take the terminal");
                return 126;
            }
        }
        CommandIo::Pipes {
            stdin,
            stdout,
            stderr,
        } => {
            if dup2_stdin(stdin.as_fd()).is_err()
                || dup2_stdout(stdout.as_fd()).is_err()
                || dup2_stderr(stderr.as_fd()).is_err()
            {
                return 126;
            }
        }
    }
    if let Err(e) = chdir(cwd.as_c_str()) {
        complain(&format!(
            "error: cannot change to {}: {e}",
            cwd.to_string_lossy()
        ));
        return 126;
    }
    // After joining the user namespace our host UID is unmapped and we would run as
    // nobody. Become the machine's root first: only a root-to-user change makes the
    // kernel drop capabilities, so a requested user ends up without any.
    if setgid(Gid::from_raw(0)).is_err() || setuid(Uid::from_raw(0)).is_err() {
        complain("error: cannot become root inside the machine");
        return 126;
    }
    // The capabilities of the machine's own processes and no more, as docker exec gives:
    // the service's bounding set is the host's whole one, and the command is theirs to
    // trace once it runs. Best effort: a security module that refuses (an SELinux
    // policy older than this) must not stop the command, which ran so before.
    if let Some(bounding) = bounding {
        let _ = limit_bounding_set(bounding);
    }
    let mut env: Vec<CString> = env.to_vec();
    match user {
        None => env.push(CString::new("HOME=/root").expect("no NUL")),
        Some(user) => {
            let Some((uid, gid, home)) = resolve_user(user) else {
                complain(&format!("error: unknown user {user} inside the machine"));
                return 126;
            };
            if setgroups(&[]).is_err() || setgid(gid).is_err() || setuid(uid).is_err() {
                complain(&format!(
                    "error: cannot switch to uid {} inside the machine",
                    uid
                ));
                return 126;
            }
            env.push(CString::new(format!("HOME={home}")).expect("no NUL"));
        }
    }
    // With SELinux the command belongs to the machine's domain, not to the domain of
    // whoever runs it here (the confined service, say), as with docker exec.
    if let Some(context) = exec_context {
        // Opened for writing only: creating or truncating is not a thing there.
        let written = std::fs::OpenOptions::new()
            .write(true)
            .open("/proc/self/attr/exec")
            .and_then(|mut f| std::io::Write::write_all(&mut f, context.as_bytes()));
        if written.is_err() {
            complain(&format!(
                "error: cannot run the command in the machine's SELinux context {context}"
            ));
            return 126;
        }
    }
    // execvpe() would search the PATH of the caller's environment, i.e. the host's, which
    // sudo's secure_path may have trimmed; the command must be found on the machine's.
    let program = match find_program(&argv[0], &env) {
        Some(program) => program,
        None => {
            complain(&format!(
                "error: cannot execute {}: not found on the machine's PATH",
                argv[0].to_string_lossy()
            ));
            return 127;
        }
    };
    // Nothing of the service's but the three streams goes along into the machine, and
    // not its SIGPIPE either: the service ignores it, which execve would pass on, and a
    // command in a pipeline must die of a closed pipe as it would anywhere else.
    close_from(3);
    // SAFETY: only the default disposition is set, in a single-threaded child.
    let _ = unsafe {
        nix::sys::signal::signal(
            nix::sys::signal::Signal::SIGPIPE,
            nix::sys::signal::SigHandler::SigDfl,
        )
    };
    match execve(&program, argv, &env) {
        Ok(_) => 0,
        Err(e) => {
            complain(&format!(
                "error: cannot execute {}: {e}",
                program.to_string_lossy()
            ));
            // As the shells and docker have it: 127 for a program that is not there,
            // 126 for one that cannot be run.
            if e == Errno::ENOENT {
                127
            } else {
                126
            }
        }
    }
}

/// The CapBnd of a /proc/PID/status.
fn capability_bounding_set(status: &str) -> Option<u64> {
    status
        .lines()
        .find_map(|line| line.strip_prefix("CapBnd:"))
        .and_then(|hex| u64::from_str_radix(hex.trim(), 16).ok())
}

/// Drops from this thread's bounding set every capability `bounding` lacks; the exec
/// that follows gives root no more than what is left.
fn limit_bounding_set(bounding: u64) -> nix::Result<()> {
    for capability in 0..64 {
        if bounding & (1 << capability) != 0 {
            continue;
        }
        // SAFETY: prctl(2) on this thread's own bounding set.
        match Errno::result(unsafe {
            libc::prctl(libc::PR_CAPBSET_DROP, capability as libc::c_ulong, 0, 0, 0)
        }) {
            // Past the last capability this kernel knows.
            Ok(_) | Err(Errno::EINVAL) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Closes every descriptor from `first` on.
fn close_from(first: libc::c_uint) {
    // SAFETY: close_range(2) only closes descriptors of this process.
    let closed = unsafe { libc::syscall(libc::SYS_close_range, first, libc::c_uint::MAX, 0) };
    if closed != 0 {
        let max = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) }.clamp(1024, 1 << 20);
        for fd in first as libc::c_long..max {
            unsafe { libc::close(fd as libc::c_int) };
        }
    }
}

/// The SELinux context of the machine's init, when SELinux is enabled: what a command
/// run inside the machine gets.
fn selinux_context_of(leader: u32) -> Option<String> {
    if !Path::new("/sys/fs/selinux/enforce").exists() {
        return None;
    }
    let context = std::fs::read_to_string(format!("/proc/{leader}/attr/current")).ok()?;
    let context = context.trim_end_matches('\0').trim().to_string();
    (!context.is_empty()).then_some(context)
}

/// Resolves a program the way a shell would, on the PATH carried by `env` (the machine's).
fn find_program(name: &CStr, env: &[CString]) -> Option<CString> {
    let name_str = name.to_str().ok()?;
    if name_str.contains('/') {
        return Some(name.to_owned());
    }
    let path = env
        .iter()
        .find_map(|v| v.to_str().ok()?.strip_prefix("PATH=").map(str::to_owned))
        .unwrap_or_else(|| "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into());
    for dir in path.split(':').filter(|d| !d.is_empty()) {
        let candidate = Path::new(dir).join(name_str);
        if nix::unistd::access(&candidate, nix::unistd::AccessFlags::X_OK).is_ok()
            && candidate.is_file()
        {
            return CString::new(candidate.as_os_str().as_bytes()).ok();
        }
    }
    None
}

/// Looks a user up in the machine's /etc/passwd (we are inside its mount namespace):
/// uid, gid and home. Accepts numeric "uid" or "uid:gid" as well.
fn resolve_user(user: &str) -> Option<(Uid, Gid, String)> {
    if let Some((u, g)) = user.split_once(':') {
        return Some((
            Uid::from_raw(u.parse().ok()?),
            Gid::from_raw(g.parse().ok()?),
            "/".to_string(),
        ));
    }
    if let Ok(uid) = user.parse::<u32>() {
        return Some((Uid::from_raw(uid), Gid::from_raw(uid), "/".to_string()));
    }
    let file = File::open("/etc/passwd").ok()?;
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let fields: Vec<&str> = line.split(':').collect();
        if fields.len() >= 6 && fields[0] == user {
            let home = if fields[5].is_empty() { "/" } else { fields[5] };
            return Some((
                Uid::from_raw(fields[2].parse().ok()?),
                Gid::from_raw(fields[3].parse().ok()?),
                home.to_string(),
            ));
        }
    }
    None
}

/// Whether `/proc/<leader>/ns/<name>` is the namespace we are in already.
fn same_namespace(leader: u32, name: &str) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let theirs = std::fs::metadata(format!("/proc/{leader}/ns/{name}"))
        .with_context(|| format!("inspecting the {name} namespace of PID {leader}"))?;
    let ours = std::fs::metadata(format!("/proc/self/ns/{name}"))?;
    Ok(theirs.ino() == ours.ino() && theirs.dev() == ours.dev())
}

fn exit_code(status: WaitStatus) -> i32 {
    match status {
        WaitStatus::Exited(_, code) => code,
        WaitStatus::Signaled(_, signal, _) => 128 + signal as i32,
        _ => 126,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_users_do_not_need_passwd() {
        assert_eq!(
            resolve_user("1000:100"),
            Some((Uid::from_raw(1000), Gid::from_raw(100), "/".to_string()))
        );
        assert_eq!(
            resolve_user("65534"),
            Some((Uid::from_raw(65534), Gid::from_raw(65534), "/".to_string()))
        );
        assert_eq!(resolve_user("1000:x"), None);
    }

    #[test]
    fn programs_are_found_on_the_given_path() {
        let env = vec![CString::new("PATH=/nonexistent:/usr/bin:/bin").unwrap()];
        let found = find_program(&CString::new("sh").unwrap(), &env).unwrap();
        assert!(found.to_str().unwrap().ends_with("/sh"));
        assert!(find_program(&CString::new("no-such-program-xyz").unwrap(), &env).is_none());
        assert_eq!(
            find_program(&CString::new("/bin/sh").unwrap(), &env).unwrap(),
            CString::new("/bin/sh").unwrap()
        );
        assert!(
            find_program(&CString::new("sh").unwrap(), &[]).is_some(),
            "default PATH"
        );
    }

    #[test]
    fn our_own_namespaces_are_detected_as_shared() {
        let me = std::process::id();
        for (name, _) in NAMESPACES {
            assert!(same_namespace(me, name).unwrap(), "{name}");
        }
    }

    #[test]
    fn exit_codes() {
        assert_eq!(
            exit_code(WaitStatus::Exited(nix::unistd::Pid::from_raw(1), 3)),
            3
        );
        assert_eq!(
            exit_code(WaitStatus::Signaled(
                nix::unistd::Pid::from_raw(1),
                nix::sys::signal::Signal::SIGKILL,
                false
            )),
            137
        );
    }

    #[test]
    fn the_helper_report_survives_the_socket() {
        for report in [
            Started::Command(4242),
            Started::Failed("joining the mnt namespace: EPERM".to_string()),
        ] {
            assert_eq!(decode(&encode(&report)).unwrap(), report);
        }
        assert!(decode(b"").is_none());
        assert!(decode(b"p12").is_none(), "a truncated PID is no report");
        assert!(decode(b"x").is_none());
    }

    #[test]
    fn the_report_and_its_descriptors_cross_the_socket() {
        let (ours, theirs) = socketpair(
            AddressFamily::Unix,
            SockType::SeqPacket,
            None,
            SockFlag::SOCK_CLOEXEC,
        )
        .unwrap();
        let (read, write) = pipe2(OFlag::O_CLOEXEC).unwrap();
        tell(
            &theirs,
            &Started::Command(7),
            &[read.as_raw_fd(), write.as_raw_fd()],
        );
        let (report, fds) = receive(&ours);
        assert_eq!(report, Some(Started::Command(7)));
        assert_eq!(fds.len(), 2);
        nix::unistd::write(&fds[1], b"x").unwrap();
        let mut buf = [0u8; 1];
        assert_eq!(nix::unistd::read(&fds[0], &mut buf).unwrap(), 1);
        tell(&theirs, &Started::Failed("no".to_string()), &[]);
        let (report, fds) = receive(&ours);
        assert_eq!(report, Some(Started::Failed("no".to_string())));
        assert!(fds.is_empty());
        drop(theirs);
        assert_eq!(receive(&ours).0, None, "a dead helper reads as no report");
    }

    #[test]
    fn a_terminal_is_allocated_unlocked_and_sized() {
        // /dev/pts/ptmx is open to everyone; this runs where the tests run.
        let master = open_terminal(31, 111).unwrap();
        let slave = open_slave(&master).unwrap();
        assert!(nix::unistd::isatty(&slave).unwrap());
        let mut size = Winsize {
            ws_row: 0,
            ws_col: 0,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        nix::ioctl_read_bad!(tiocgwinsz, libc::TIOCGWINSZ, Winsize);
        unsafe { tiocgwinsz(slave.as_raw_fd(), &mut size) }.unwrap();
        assert_eq!((size.ws_row, size.ws_col), (31, 111));
    }

    #[test]
    fn the_bounding_set_is_read_from_the_status() {
        let status = "Name:\tsh\nCapInh:\t0000000000000000\nCapBnd:\t00000000a80425fb\nCapAmb:\t0000000000000000\n";
        assert_eq!(capability_bounding_set(status), Some(0xa80425fb));
        assert_eq!(capability_bounding_set("Name:\tsh\n"), None);
        let ours = std::fs::read_to_string("/proc/self/status").unwrap();
        assert!(capability_bounding_set(&ours).is_some());
    }

    #[test]
    fn a_bounding_set_is_only_ever_narrowed() {
        match unsafe { fork() }.unwrap() {
            ForkResult::Child => {
                // Keeping every capability is always allowed, whoever runs the test.
                let code = if limit_bounding_set(u64::MAX).is_ok() {
                    0
                } else {
                    1
                };
                unsafe { libc::_exit(code) }
            }
            ForkResult::Parent { child } => {
                assert_eq!(exit_code(waitpid(child, None).unwrap()), 0);
            }
        }
    }

    #[test]
    fn descriptors_past_the_streams_are_closed() {
        // In a child, so that the test process keeps its own.
        match unsafe { fork() }.unwrap() {
            ForkResult::Child => {
                let (read_end, write_end) = pipe2(OFlag::empty()).unwrap();
                let (read, write) = (read_end.as_raw_fd(), write_end.as_raw_fd());
                // Closed below by number, not by their owners.
                std::mem::forget(read_end);
                std::mem::forget(write_end);
                close_from(3);
                let open = |fd| unsafe { libc::fcntl(fd, libc::F_GETFD) } != -1;
                let code = if !open(read) && !open(write) && open(0) {
                    0
                } else {
                    1
                };
                unsafe { libc::_exit(code) }
            }
            ForkResult::Parent { child } => {
                assert_eq!(exit_code(waitpid(child, None).unwrap()), 0);
            }
        }
    }

    #[test]
    fn a_pidfd_follows_its_process() {
        let me = pidfd_open(Pid::this()).unwrap();
        pidfd_signal(&me, 0).unwrap();
        assert_eq!(pidfd_signal(&me, 1000), Err(Errno::EINVAL));
    }
}
