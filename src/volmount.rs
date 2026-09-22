//! Volumes for machines under managed user namespaces (mstack): systemd-nspawn cannot
//! idmap a bind mount there ("Failed to clone: Operation not permitted") and machined
//! refuses to bind mount into any user-namespaced machine. nspawn does it from the host,
//! where it has the privileges: a detached copy of the source tree, idmapped to the
//! machine's user namespace, moved into the machine's mount namespace once it runs.

use std::ffi::{CStr, CString};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use anyhow::{bail, Context, Result};
use nix::libc;
use nix::sched::{setns, CloneFlags};
use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::{fork, ForkResult};

/// Follow a symlink as the target (merged-usr images have /lib -> usr/lib and the like).
const MOVE_MOUNT_T_SYMLINKS: libc::c_uint = 0x10;

/// Mounts `source` at `target` inside the running machine whose leader is `leader`, with
/// the machine's root owning it.
pub fn mount_into_machine(leader: u32, source: &Path, target: &str, read_only: bool) -> Result<()> {
    let c_source = CString::new(source.as_os_str().as_bytes())?;
    // A detached, recursive copy of the source tree.
    let flags =
        libc::OPEN_TREE_CLONE | libc::OPEN_TREE_CLOEXEC | libc::AT_RECURSIVE as libc::c_uint;
    let fd = unsafe {
        libc::syscall(
            libc::SYS_open_tree,
            libc::AT_FDCWD,
            c_source.as_ptr(),
            flags,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("cloning {} for the machine", source.display()));
    }
    let tree: OwnedFd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
    // Idmapped to the machine's user namespace, read-only when asked.
    let userns = File::open(format!("/proc/{leader}/ns/user"))
        .with_context(|| format!("opening the user namespace of PID {leader}"))?;
    let mut attr = libc::mount_attr {
        attr_set: libc::MOUNT_ATTR_IDMAP
            | if read_only {
                libc::MOUNT_ATTR_RDONLY
            } else {
                0
            },
        attr_clr: 0,
        propagation: 0,
        userns_fd: userns.as_raw_fd() as u64,
    };
    let mut r = unsafe {
        libc::syscall(
            libc::SYS_mount_setattr,
            tree.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH | libc::AT_RECURSIVE,
            &mut attr as *mut libc::mount_attr,
            std::mem::size_of::<libc::mount_attr>(),
        )
    };
    if r < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL) {
        // A filesystem (or a submount) without idmapped mounts: docker mounts it plainly,
        // so does nspawn, with a note, since root inside then appears as nobody there.
        eprintln!(
            "note: {} cannot be idmapped (unsupported filesystem); attached with the host's ownership",
            source.display()
        );
        attr.attr_set &= !libc::MOUNT_ATTR_IDMAP;
        attr.userns_fd = 0;
        r = unsafe {
            libc::syscall(
                libc::SYS_mount_setattr,
                tree.as_raw_fd(),
                c"".as_ptr(),
                libc::AT_EMPTY_PATH | libc::AT_RECURSIVE,
                &mut attr as *mut libc::mount_attr,
                std::mem::size_of::<libc::mount_attr>(),
            )
        };
    }
    if r < 0 {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("preparing {} for the machine", source.display()));
    }
    let mntns = File::open(format!("/proc/{leader}/ns/mnt"))
        .with_context(|| format!("opening the mount namespace of PID {leader}"))?;
    let c_target = CString::new(target)?;
    // Two helpers, since setns() into a mount namespace needs a single-threaded process:
    // one becomes the machine's root to create the mount point (the host's uid 0 is not
    // mapped there, so it could not create anything: EOVERFLOW), the other keeps the
    // host's privileges to attach the idmapped tree.
    run_helper(
        || make_mount_point(&userns, &mntns, &c_target),
        &format!("creating {target} inside the machine"),
    )?;
    run_helper(
        || attach(&mntns, &tree, &c_target),
        &format!(
            "attaching {} at {target} inside the machine",
            source.display()
        ),
    )
}

/// Forks, runs `work` in the child and reports its exit code as an error when non-zero.
/// SAFETY: the child only performs syscalls on data prepared by the caller until it exits.
fn run_helper(work: impl FnOnce() -> i32, what: &str) -> Result<()> {
    match unsafe { fork() }.with_context(|| format!("forking for {what}"))? {
        ForkResult::Parent { child } => {
            match waitpid(child, None).context("waiting for the mount helper")? {
                WaitStatus::Exited(_, 0) => Ok(()),
                other => bail!("{what} failed ({other:?})"),
            }
        }
        ForkResult::Child => {
            let code = work();
            unsafe { libc::_exit(code) }
        }
    }
}

fn make_mount_point(userns: &File, mntns: &File, target: &CStr) -> i32 {
    if let Err(e) = setns(userns, CloneFlags::CLONE_NEWUSER) {
        eprintln!("error: joining the machine's user namespace: {e}");
        return 1;
    }
    if let Err(e) = setns(mntns, CloneFlags::CLONE_NEWNS) {
        eprintln!("error: joining the machine's mount namespace: {e}");
        return 1;
    }
    if nix::unistd::setgid(nix::unistd::Gid::from_raw(0)).is_err()
        || nix::unistd::setuid(nix::unistd::Uid::from_raw(0)).is_err()
    {
        eprintln!("error: cannot become root inside the machine");
        return 1;
    }
    let path = Path::new(std::ffi::OsStr::from_bytes(target.to_bytes()));
    if let Err(e) = std::fs::create_dir_all(path) {
        eprintln!("error: creating {} inside the machine: {e}", path.display());
        return 1;
    }
    0
}

fn attach(mntns: &File, tree: &OwnedFd, target: &CStr) -> i32 {
    if let Err(e) = setns(mntns, CloneFlags::CLONE_NEWNS) {
        eprintln!("error: joining the machine's mount namespace: {e}");
        return 1;
    }
    let r = unsafe {
        libc::syscall(
            libc::SYS_move_mount,
            tree.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_FDCWD,
            target.as_ptr(),
            libc::MOVE_MOUNT_F_EMPTY_PATH | MOVE_MOUNT_T_SYMLINKS,
        )
    };
    if r < 0 {
        eprintln!(
            "error: attaching the volume at {}: {}",
            target.to_string_lossy(),
            io::Error::last_os_error()
        );
        return 1;
    }
    0
}
