//! docker exec for machines that have no D-Bus inside: enter the namespaces of the
//! machine's leader process and run a command on a pseudo terminal.
//!
//! setns() into a mount namespace is refused for multithreaded processes, and children
//! only land in a PID namespace after a fork, so the work happens in a forked helper: the
//! helper joins the namespaces, forks once more, and the grandchild execs the command.

use std::ffi::CString;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};

use anyhow::{bail, Context, Result};
use nix::libc;
use nix::sched::{setns, CloneFlags};
use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::{
    chdir, dup2_stderr, dup2_stdin, dup2_stdout, execvpe, fork, setgid, setgroups, setsid, setuid,
    ForkResult, Gid, Uid,
};

use crate::pty;

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

/// Runs `argv` inside the machine whose leader is `leader`, attached to the local terminal.
/// Returns the command's exit code.
pub fn exec(
    leader: u32,
    argv: &[String],
    user: Option<&str>,
    working_dir: Option<&str>,
) -> Result<i32> {
    if argv.is_empty() {
        bail!("no command given");
    }
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
    let mut env = vec![
        CString::new("PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")?,
        CString::new("HOME=/root")?,
    ];
    if let Ok(term) = std::env::var("TERM") {
        env.push(CString::new(format!("TERM={term}"))?);
    }
    let c_cwd = CString::new(working_dir.unwrap_or("/"))?;
    let user = user.map(|u| u.to_string());
    let pty = nix::pty::openpty(None, None).context("allocating a pseudo terminal")?;

    // SAFETY: the parent is multithreaded (tokio), so the child only performs syscalls and
    // work on data prepared above until it execs or exits.
    match unsafe { fork() }.context("forking the namespace helper")? {
        ForkResult::Parent { child } => {
            drop(pty.slave);
            drop(ns_fds);
            let session = pty::run_session(pty.master);
            let status = waitpid(child, None).context("waiting for the namespace helper")?;
            session?;
            Ok(exit_code(status))
        }
        ForkResult::Child => {
            drop(pty.master);
            let code = helper(&ns_fds, &pty.slave, &c_argv, &env, &c_cwd, user.as_deref());
            unsafe { libc::_exit(code) }
        }
    }
}

fn helper(
    ns_fds: &[(&str, CloneFlags, OwnedFd)],
    slave: &OwnedFd,
    argv: &[CString],
    env: &[CString],
    cwd: &CString,
    user: Option<&str>,
) -> i32 {
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
        Ok(ForkResult::Parent { child }) => match waitpid(child, None) {
            Ok(status) => exit_code(status),
            Err(e) => {
                eprintln!("error: waiting for the command: {e}");
                126
            }
        },
        Ok(ForkResult::Child) => {
            let code = grandchild(slave, argv, env, cwd, user);
            unsafe { libc::_exit(code) }
        }
    }
}

fn grandchild(
    slave: &OwnedFd,
    argv: &[CString],
    env: &[CString],
    cwd: &CString,
    user: Option<&str>,
) -> i32 {
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
    if let Err(e) = chdir(cwd.as_c_str()) {
        eprintln!("error: cannot change to {}: {e}", cwd.to_string_lossy());
        return 126;
    }
    // After joining the user namespace our host UID is unmapped and we would run as
    // nobody; become the machine's root (or the requested user) explicitly.
    let (uid, gid) = match user {
        None => (Uid::from_raw(0), Gid::from_raw(0)),
        Some(user) => match resolve_user(user) {
            Some(ids) => ids,
            None => {
                eprintln!("error: unknown user {user} inside the machine");
                return 126;
            }
        },
    };
    if setgroups(&[]).is_err() || setgid(gid).is_err() || setuid(uid).is_err() {
        eprintln!("error: cannot switch to uid {} inside the machine", uid);
        return 126;
    }
    match execvpe(&argv[0], argv, env) {
        Ok(_) => 0,
        Err(e) => {
            eprintln!("error: cannot execute {}: {e}", argv[0].to_string_lossy());
            127
        }
    }
}

/// Looks a user up in the machine's /etc/passwd (we are inside its mount namespace).
/// Accepts numeric "uid" or "uid:gid" as well.
fn resolve_user(user: &str) -> Option<(Uid, Gid)> {
    if let Some((u, g)) = user.split_once(':') {
        return Some((
            Uid::from_raw(u.parse().ok()?),
            Gid::from_raw(g.parse().ok()?),
        ));
    }
    if let Ok(uid) = user.parse::<u32>() {
        return Some((Uid::from_raw(uid), Gid::from_raw(uid)));
    }
    let file = File::open("/etc/passwd").ok()?;
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let fields: Vec<&str> = line.split(':').collect();
        if fields.len() >= 4 && fields[0] == user {
            return Some((
                Uid::from_raw(fields[2].parse().ok()?),
                Gid::from_raw(fields[3].parse().ok()?),
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
            Some((Uid::from_raw(1000), Gid::from_raw(100)))
        );
        assert_eq!(
            resolve_user("65534"),
            Some((Uid::from_raw(65534), Gid::from_raw(65534)))
        );
        assert_eq!(resolve_user("1000:x"), None);
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
