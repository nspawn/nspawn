//! docker exec for machines that have no D-Bus inside: enter the namespaces of the
//! machine's leader process and run a command there, on a pseudo terminal, on pipes or
//! on the caller's own streams.
//!
//! setns() into a mount namespace is refused for multithreaded processes, and children
//! only land in a PID namespace after a fork, so the work happens in a forked helper: the
//! helper joins the namespaces, forks once more, and the grandchild execs the command.
//! The helper reports the grandchild's PID and exits with its exit code.

use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use anyhow::{bail, Context, Result};
use nix::fcntl::{fcntl, FcntlArg, FdFlag, OFlag};
use nix::libc;
use nix::pty::Winsize;
use nix::sched::{setns, CloneFlags};
use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::{
    chdir, dup2_stderr, dup2_stdin, dup2_stdout, execve, fork, pipe2, setgid, setgroups, setsid,
    setuid, ForkResult, Gid, Pid, Uid,
};

use crate::pty;

/// How the command's standard streams are set up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stdio {
    /// The caller's own: piped input and output pass through byte for byte.
    Inherit,
    /// A pseudo terminal of this size; its master comes back in `Process::master`.
    Pty { rows: u16, cols: u16 },
    /// Three pipes; the caller's ends come back in `Process`.
    Pipes,
}

/// A command started inside a machine and not waited for yet.
pub struct Process {
    /// The helper that forked the command; `wait` reaps it for the command's exit code.
    pub helper: Pid,
    /// The command's PID as the host sees it, when the helper got that far.
    pub pid: Option<u32>,
    pub master: Option<OwnedFd>,
    pub stdin: Option<OwnedFd>,
    pub stdout: Option<OwnedFd>,
    pub stderr: Option<OwnedFd>,
}

/// The command's side of the streams.
enum ChildIo {
    Inherit,
    Pty(OwnedFd),
    Pipes {
        stdin: OwnedFd,
        stdout: OwnedFd,
        stderr: OwnedFd,
    },
}

fn cloexec(fd: &OwnedFd) -> Result<()> {
    fcntl(fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).context("setting close-on-exec")?;
    Ok(())
}

nix::ioctl_write_int_bad!(tiocsctty, libc::TIOCSCTTY);

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

/// Runs `argv` inside the machine whose leader is `leader`, attached to the local terminal
/// (a pseudo terminal when standard input is one, the streams as they are otherwise),
/// with `image_env` plus PATH and TERM when missing. Returns the command's exit code.
pub fn exec(
    leader: u32,
    argv: &[String],
    user: Option<&str>,
    working_dir: Option<&str>,
    image_env: &[String],
) -> Result<i32> {
    // A pseudo terminal only when the caller has one, as docker does with -t: piped
    // input and output pass through byte for byte otherwise, and EOF is a real EOF.
    let stdio = if nix::unistd::isatty(std::io::stdin()).unwrap_or(false) {
        let (rows, cols) = pty::window_size().unwrap_or((24, 80));
        Stdio::Pty { rows, cols }
    } else {
        Stdio::Inherit
    };
    let process = spawn(leader, argv, user, working_dir, image_env, stdio)?;
    let session = match process.master {
        Some(master) => pty::run_session(master),
        None => Ok(()),
    };
    let code = wait(process.helper)?;
    session?;
    Ok(code)
}

/// Reaps the helper of a `Process`: the command's exit code, 128 plus the signal when
/// it died of one.
pub fn wait(helper: Pid) -> Result<i32> {
    let status = waitpid(helper, None).context("waiting for the namespace helper")?;
    Ok(exit_code(status))
}

/// Starts `argv` inside the machine whose leader is `leader`, with `image_env` plus PATH
/// and TERM when missing, its streams set up as `stdio` says.
pub fn spawn(
    leader: u32,
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
    if let Ok(term) = std::env::var("TERM") {
        env.push(CString::new(format!("TERM={term}"))?);
    }
    let c_cwd = CString::new(working_dir.unwrap_or("/"))?;
    let user = user.map(|u| u.to_string());
    // Everything the two sides hold, made before the fork. The command's ends are kept
    // out of what it execs (dup2 onto 0, 1 and 2 clears close-on-exec there).
    let mut parent = Process {
        helper: Pid::from_raw(0),
        pid: None,
        master: None,
        stdin: None,
        stdout: None,
        stderr: None,
    };
    let child_io = match stdio {
        Stdio::Inherit => ChildIo::Inherit,
        Stdio::Pty { rows, cols } => {
            let size = Winsize {
                ws_row: rows,
                ws_col: cols,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            let pty =
                nix::pty::openpty(Some(&size), None).context("allocating a pseudo terminal")?;
            cloexec(&pty.master)?;
            cloexec(&pty.slave)?;
            parent.master = Some(pty.master);
            ChildIo::Pty(pty.slave)
        }
        Stdio::Pipes => {
            let (stdin_r, stdin_w) = pipe2(OFlag::O_CLOEXEC).context("creating a pipe")?;
            let (stdout_r, stdout_w) = pipe2(OFlag::O_CLOEXEC).context("creating a pipe")?;
            let (stderr_r, stderr_w) = pipe2(OFlag::O_CLOEXEC).context("creating a pipe")?;
            parent.stdin = Some(stdin_w);
            parent.stdout = Some(stdout_r);
            parent.stderr = Some(stderr_r);
            ChildIo::Pipes {
                stdin: stdin_r,
                stdout: stdout_w,
                stderr: stderr_w,
            }
        }
    };
    // The helper tells the command's PID through here, right after forking it.
    let (pid_r, pid_w) = pipe2(OFlag::O_CLOEXEC).context("creating a pipe")?;

    // SAFETY: the parent is multithreaded (tokio), so the child only performs syscalls and
    // work on data prepared above until it execs or exits.
    match unsafe { fork() }.context("forking the namespace helper")? {
        ForkResult::Parent { child } => {
            drop(ns_fds);
            drop(child_io);
            drop(pid_w);
            parent.helper = child;
            let mut bytes = [0u8; 4];
            if nix::unistd::read(&pid_r, &mut bytes) == Ok(4) {
                parent.pid = Some(u32::from_ne_bytes(bytes));
            }
            Ok(parent)
        }
        ForkResult::Child => {
            drop(parent);
            drop(pid_r);
            let code = helper(
                &ns_fds,
                cgroup.as_deref(),
                child_io,
                pid_w,
                &c_argv,
                &env,
                &c_cwd,
                user.as_deref(),
            );
            unsafe { libc::_exit(code) }
        }
    }
}

/// The cgroup of the machine's leader, so that what we run is accounted to the machine
/// and dies with it (KillMachine signals the cgroup).
fn leader_cgroup(leader: u32) -> Option<String> {
    let text = std::fs::read_to_string(format!("/proc/{leader}/cgroup")).ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(|path| format!("/sys/fs/cgroup{path}/cgroup.procs"))
}

#[allow(clippy::too_many_arguments)]
fn helper(
    ns_fds: &[(&str, CloneFlags, OwnedFd)],
    cgroup: Option<&str>,
    io: ChildIo,
    pid_w: OwnedFd,
    argv: &[CString],
    env: &[CString],
    cwd: &CString,
    user: Option<&str>,
) -> i32 {
    if let Some(procs) = cgroup {
        // Best effort: cgroup v1 hosts or delegation quirks must not stop exec.
        let _ = std::fs::write(procs, std::process::id().to_string());
    }
    for (name, flag, fd) in ns_fds {
        if let Err(e) = setns(fd, *flag) {
            // Joining the user namespace we are already in is refused with EINVAL; that is
            // what happens for machines that run without private users.
            if *flag == CloneFlags::CLONE_NEWUSER && e == nix::errno::Errno::EINVAL {
                continue;
            }
            eprintln!("error: joining the {name} namespace: {e}");
            return 126;
        }
    }
    match unsafe { fork() } {
        Err(e) => {
            eprintln!("error: forking inside the machine: {e}");
            126
        }
        Ok(ForkResult::Parent { child }) => {
            let _ = nix::unistd::write(&pid_w, &(child.as_raw() as u32).to_ne_bytes());
            drop(pid_w);
            drop(io);
            match waitpid(child, None) {
                Ok(status) => exit_code(status),
                Err(e) => {
                    eprintln!("error: waiting for the command: {e}");
                    126
                }
            }
        }
        Ok(ForkResult::Child) => {
            drop(pid_w);
            let code = grandchild(io, argv, env, cwd, user);
            unsafe { libc::_exit(code) }
        }
    }
}

fn grandchild(
    io: ChildIo,
    argv: &[CString],
    env: &[CString],
    cwd: &CString,
    user: Option<&str>,
) -> i32 {
    match &io {
        ChildIo::Inherit => {}
        ChildIo::Pty(slave) => {
            if setsid().is_err() {
                eprintln!("error: setsid failed");
                return 126;
            }
            if unsafe { tiocsctty(slave.as_raw_fd(), 0) }.is_err() {
                eprintln!("error: cannot take the terminal");
                return 126;
            }
            if dup2_stdin(slave.as_fd()).is_err()
                || dup2_stdout(slave.as_fd()).is_err()
                || dup2_stderr(slave.as_fd()).is_err()
            {
                return 126;
            }
        }
        ChildIo::Pipes {
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
        eprintln!("error: cannot change to {}: {e}", cwd.to_string_lossy());
        return 126;
    }
    // After joining the user namespace our host UID is unmapped and we would run as
    // nobody. Become the machine's root first: only a root-to-user change makes the
    // kernel drop capabilities, so a requested user ends up without any.
    if setgid(Gid::from_raw(0)).is_err() || setuid(Uid::from_raw(0)).is_err() {
        eprintln!("error: cannot become root inside the machine");
        return 126;
    }
    let mut env: Vec<CString> = env.to_vec();
    match user {
        None => env.push(CString::new("HOME=/root").expect("no NUL")),
        Some(user) => {
            let Some((uid, gid, home)) = resolve_user(user) else {
                eprintln!("error: unknown user {user} inside the machine");
                return 126;
            };
            if setgroups(&[]).is_err() || setgid(gid).is_err() || setuid(uid).is_err() {
                eprintln!("error: cannot switch to uid {} inside the machine", uid);
                return 126;
            }
            env.push(CString::new(format!("HOME={home}")).expect("no NUL"));
        }
    }
    // execvpe() would search the PATH of the caller's environment, i.e. the host's, which
    // sudo's secure_path may have trimmed; the command must be found on the machine's.
    let program = match find_program(&argv[0], &env) {
        Some(program) => program,
        None => {
            eprintln!(
                "error: cannot execute {}: not found on the machine's PATH",
                argv[0].to_string_lossy()
            );
            return 127;
        }
    };
    match execve(&program, argv, &env) {
        Ok(_) => 0,
        Err(e) => {
            eprintln!("error: cannot execute {}: {e}", program.to_string_lossy());
            126
        }
    }
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
}
