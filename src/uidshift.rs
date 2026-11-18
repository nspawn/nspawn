//! The shift of an overlay machine's tree into the user namespace its first start gives
//! it. No released kernel lets an overlayfs mount be idmapped, so systemd-nspawn shifts
//! such a tree with a recursive chown, which the kernel follows by clearing every file
//! capability (`arping`'s cap_net_raw, `newuidmap`'s cap_setuid). nspawn shifts the tree
//! first, as that chown would and with each capability put back; systemd-nspawn reads
//! the range off the owner of the tree's root and, finding it free, changes nothing.

use std::collections::HashSet;
use std::ffi::OsString;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{bail, Context as _, Result};
use nix::errno::Errno;
use nix::fcntl::{AtFlags, OFlag};
use nix::sys::stat::{fstat, fstatat, FileStat, Mode};
use nix::unistd::{Gid, Uid};
use sha2::{Digest, Sha256};

/// The bases systemd-nspawn picks a machine's range from, as its build sets them by
/// default (container-uid-base-min and -max).
pub const BASE_MIN: u32 = 0x0008_0000;
const BASE_MAX: u32 = 0x6FFF_0000;
/// The low bits of an ID, which the shift keeps.
const LOW: u32 = 0xFFFF;

/// Where a running systemd-nspawn holds the lock of the range it took.
const LOCKS: &str = "/run/systemd/nspawn-uid";

const CAPABILITY: &str = "security.capability";
const ACL_ACCESS: &str = "system.posix_acl_access";
const ACL_DEFAULT: &str = "system.posix_acl_default";
const ACL_XATTR_VERSION: u32 = 2;
const ACL_USER: u16 = 0x02;
const ACL_GROUP: u16 = 0x08;

/// Whether a machine's ExecStart= runs systemd-nspawn in a user namespace whose range it
/// picks (`-U`, `--private-users=pick`): the last of its --private-users options says.
pub fn picks_range(argv: &[String]) -> bool {
    let Some((program, options)) = crate::settings::unwrapped(argv).split_first() else {
        return false;
    };
    if !Path::new(program)
        .file_name()
        .is_some_and(|n| n == "systemd-nspawn")
    {
        return false;
    }
    let mut pick = false;
    for arg in options {
        if arg == "--" {
            break;
        }
        if arg == "-U" {
            pick = true;
        } else if arg == "--private-users" {
            pick = false;
        } else if let Some(value) = arg.strip_prefix("--private-users=") {
            pick = value == "pick";
        }
    }
    pick
}

/// The base a tree is shifted to: its root's owner, when that starts a range.
pub fn base_of(st: &FileStat) -> Option<u32> {
    let base = st.st_uid & !LOW;
    (base != 0 && st.st_uid & LOW == 0 && st.st_gid == base).then_some(base)
}

/// The bases of the ranges the trees of other machines are owned by, read off the roots
/// of their upper directories: two machines with one range could not run together, and
/// the second to start would be shifted again by systemd-nspawn's chown.
pub fn bases_of(uppers: &[PathBuf]) -> HashSet<u32> {
    uppers
        .iter()
        .filter_map(|upper| nix::sys::stat::lstat(upper).ok())
        .filter_map(|st| base_of(&st))
        .collect()
}

/// The `n`th candidate for a machine: from a hash of its name, so that the machine keeps
/// one while it is free.
fn candidate(name: &str, n: u32) -> u32 {
    let digest = Sha256::new()
        .chain_update(name.as_bytes())
        .chain_update(n.to_le_bytes())
        .finalize();
    let hash = u32::from_le_bytes([digest[0], digest[1], digest[2], digest[3]]);
    ((hash % (BASE_MAX - BASE_MIN)) + BASE_MIN) & !LOW
}

/// A free base for the machine: no running systemd-nspawn holds it, and no other machine's
/// tree is owned by it. The user database is not asked, which the hook's SELinux domain
/// cannot read: a base systemd-nspawn finds there (a host user with its ID) makes it pick
/// another range and shift the tree with its own chown.
pub fn pick(name: &str, taken: &HashSet<u32>) -> Option<u32> {
    (0..100)
        .map(|n| candidate(name, n))
        .find(|&base| !taken.contains(&base) && !Path::new(LOCKS).join(base.to_string()).exists())
}

/// The ID that `id` becomes in the range at `base`, as systemd-nspawn's chown gives it.
fn shifted(id: u32, base: u32) -> u32 {
    base | (id & LOW)
}

/// The IDs of an ACL's named users and groups shifted to `base`, or None when nothing
/// changes or the value is not one the kernel writes.
fn shifted_acl(value: &[u8], base: u32) -> Option<Vec<u8>> {
    if value.len() < 4 || !(value.len() - 4).is_multiple_of(8) {
        return None;
    }
    if u32::from_le_bytes([value[0], value[1], value[2], value[3]]) != ACL_XATTR_VERSION {
        return None;
    }
    let mut out = value.to_vec();
    let mut changed = false;
    for entry in out[4..].as_chunks_mut::<8>().0 {
        let tag = u16::from_le_bytes([entry[0], entry[1]]);
        if tag != ACL_USER && tag != ACL_GROUP {
            continue;
        }
        let id = u32::from_le_bytes([entry[4], entry[5], entry[6], entry[7]]);
        let new = shifted(id, base);
        if new != id {
            entry[4..8].copy_from_slice(&new.to_le_bytes());
            changed = true;
        }
    }
    changed.then_some(out)
}

/// The /proc link of a descriptor, which leads to what it holds without opening it.
fn fd_path(fd: &OwnedFd) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", fd.as_raw_fd()))
}

/// An extended attribute of what `fd` holds; None where it has none or the file system
/// keeps none.
fn get(fd: &OwnedFd, name: &str) -> std::io::Result<Option<Vec<u8>>> {
    match xattr::get_deref(fd_path(fd), name) {
        Err(e) if e.raw_os_error() == Some(nix::libc::EOPNOTSUPP) => Ok(None),
        other => other,
    }
}

/// Shifts one entry, held by `fd` (O_PATH for anything but a directory), whose status
/// `st` is. Owners first: chown clears the capability and, root's too, the setuid and
/// setgid bits, which go back after it.
fn patch(fd: &OwnedFd, st: &FileStat, base: u32, path: &Path) -> Result<()> {
    let kind = st.st_mode & nix::libc::S_IFMT;
    let at = || format!("shifting {}", path.display());
    let (uid, gid) = (shifted(st.st_uid, base), shifted(st.st_gid, base));
    if (st.st_uid, st.st_gid) != (uid, gid) {
        let capability = if kind == nix::libc::S_IFREG {
            get(fd, CAPABILITY).with_context(at)?
        } else {
            None
        };
        nix::unistd::fchownat(
            fd.as_fd(),
            "",
            Some(Uid::from_raw(uid)),
            Some(Gid::from_raw(gid)),
            AtFlags::AT_EMPTY_PATH | AtFlags::AT_SYMLINK_NOFOLLOW,
        )
        .with_context(at)?;
        if kind != nix::libc::S_IFLNK && st.st_mode & (nix::libc::S_ISUID | nix::libc::S_ISGID) != 0
        {
            nix::sys::stat::fchmodat(
                nix::fcntl::AT_FDCWD,
                fd_path(fd).as_path(),
                Mode::from_bits_truncate(st.st_mode & 0o7777),
                nix::sys::stat::FchmodatFlags::FollowSymlink,
            )
            .with_context(at)?;
        }
        if let Some(value) = capability {
            xattr::set_deref(fd_path(fd), CAPABILITY, &value).with_context(at)?;
        }
    }
    if kind == nix::libc::S_IFLNK {
        return Ok(());
    }
    let acls: &[&str] = if kind == nix::libc::S_IFDIR {
        &[ACL_ACCESS, ACL_DEFAULT]
    } else {
        &[ACL_ACCESS]
    };
    for name in acls {
        if let Some(value) = get(fd, name).with_context(at)? {
            if let Some(value) = shifted_acl(&value, base) {
                xattr::set_deref(fd_path(fd), name, &value).with_context(at)?;
            }
        }
    }
    Ok(())
}

/// The names in a directory.
fn names(dir: &OwnedFd) -> nix::Result<Vec<OsString>> {
    let mut names = Vec::new();
    for entry in nix::dir::Dir::from_fd(nix::unistd::dup(dir)?)?.iter() {
        let name = entry?.file_name().to_bytes().to_vec();
        if name != b"." && name != b".." {
            names.push(OsString::from(std::ffi::OsStr::from_bytes(&name)));
        }
    }
    Ok(names)
}

/// Shifts what is below the directory `dir`, deepest first; a directory another file
/// system is mounted on stays as it is, and an entry gone meanwhile is passed over.
/// Only directories tell a mount by their device: overlayfs gives every directory its
/// own, and a file of a layer on another file system that one's.
fn walk(dir: &OwnedFd, dev: u64, base: u32, path: &Path, stop: &AtomicBool) -> Result<()> {
    let listed = names(dir).with_context(|| format!("reading {}", path.display()))?;
    for name in listed {
        if stop.load(Ordering::Relaxed) {
            bail!("stopped while shifting {}", path.display());
        }
        let at = path.join(&name);
        let st = match fstatat(dir.as_fd(), name.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(st) => st,
            Err(Errno::ENOENT) => continue,
            Err(e) => return Err(e).with_context(|| format!("inspecting {}", at.display())),
        };
        let directory = st.st_mode & nix::libc::S_IFMT == nix::libc::S_IFDIR;
        if directory && st.st_dev != dev {
            continue;
        }
        let flags = if directory {
            OFlag::O_RDONLY | OFlag::O_DIRECTORY
        } else {
            OFlag::O_PATH
        };
        let fd = match nix::fcntl::openat(
            dir.as_fd(),
            name.as_os_str(),
            flags | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(Errno::ENOENT) => continue,
            Err(e) => return Err(e).with_context(|| format!("opening {}", at.display())),
        };
        // What is opened is what is shifted: an entry replaced since its status was
        // read is looked at anew.
        let st = fstat(&fd).with_context(|| format!("inspecting {}", at.display()))?;
        if directory {
            if st.st_mode & nix::libc::S_IFMT != nix::libc::S_IFDIR || st.st_dev != dev {
                continue;
            }
            walk(&fd, dev, base, &at, stop)?;
        }
        patch(&fd, &st, base, &at)?;
    }
    Ok(())
}

/// Shifts the tree at `root` to the range at `base`: owners as systemd-nspawn's chown
/// sets them, the base plus their low 16 bits, and the IDs of ACL entries too; modes and
/// file capabilities stay. `stop` ends it between two entries, each shifted whole. The
/// root goes last, so that a shift cut short is done again in full, by the next start or
/// by systemd-nspawn.
pub fn shift_tree(root: &Path, base: u32, stop: &AtomicBool) -> Result<()> {
    let fd = nix::fcntl::open(
        root,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .with_context(|| format!("opening {}", root.display()))?;
    let st = fstat(&fd).with_context(|| format!("inspecting {}", root.display()))?;
    walk(&fd, st.st_dev, base, root, stop)?;
    patch(&fd, &st, base, root)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn only_a_range_systemd_nspawn_picks_is_one_to_shift_to() {
        assert!(picks_range(&argv(
            "systemd-nspawn --quiet --keep-unit --boot --link-journal=try-guest --network-veth -U --settings=override --machine=web"
        )));
        assert!(picks_range(&argv(
            "/usr/bin/nspawn attach-exec web -- /usr/bin/systemd-nspawn --quiet -U --machine=web"
        )));
        assert!(picks_range(&argv("systemd-nspawn --private-users=pick")));
        // A machine may be named as the program is.
        assert!(picks_range(&argv(
            "/usr/bin/nspawn attach-exec systemd-nspawn -- systemd-nspawn --quiet -U --machine=systemd-nspawn"
        )));
        assert!(!picks_range(&argv(
            "/usr/bin/nspawn attach-exec systemd-nspawn -- /usr/bin/true -U"
        )));
        assert!(!picks_range(&argv(
            "systemd-nspawn --quiet --boot --machine=web"
        )));
        assert!(!picks_range(&argv("systemd-nspawn -U --private-users=no")));
        assert!(!picks_range(&argv(
            "systemd-nspawn -U --private-users=identity"
        )));
        assert!(!picks_range(&argv(
            "systemd-nspawn --private-users=0x80000"
        )));
        assert!(!picks_range(&argv("/usr/bin/nspawn attach-exec web -U")));
        // The machine's own arguments are not systemd-nspawn's.
        assert!(!picks_range(&argv(
            "systemd-nspawn --machine=web -- /bin/sh -U"
        )));
    }

    #[test]
    fn a_root_owned_by_the_start_of_a_range_says_the_tree_is_shifted() {
        let mut st = nix::sys::stat::stat("/").unwrap();
        (st.st_uid, st.st_gid) = (0, 0);
        assert_eq!(base_of(&st), None);
        (st.st_uid, st.st_gid) = (0x1161_0000, 0x1161_0000);
        assert_eq!(base_of(&st), Some(0x1161_0000));
        (st.st_uid, st.st_gid) = (0x1161_0001, 0x1161_0001);
        assert_eq!(base_of(&st), None);
        (st.st_uid, st.st_gid) = (0x1161_0000, 0x1162_0000);
        assert_eq!(base_of(&st), None);
    }

    #[test]
    fn candidates_are_bases_in_systemd_nspawn_s_span_and_stay_with_the_name() {
        for n in 0..1000 {
            let base = candidate("web", n);
            assert_eq!(base & LOW, 0);
            assert!((BASE_MIN..=BASE_MAX).contains(&base), "{base:#x}");
        }
        assert_eq!(candidate("web", 0), candidate("web", 0));
        assert_ne!(candidate("web", 0), candidate("db", 0));
        assert_ne!(candidate("web", 0), candidate("web", 1));
    }

    #[test]
    fn ids_keep_their_low_bits() {
        assert_eq!(shifted(0, 0x0008_0000), 0x0008_0000);
        assert_eq!(shifted(65534, 0x0008_0000), 0x0008_fffe);
        // One shifted to another range moves to that one.
        assert_eq!(shifted(0x0009_0021, 0x0008_0000), 0x0008_0021);
    }

    fn acl(entries: &[(u16, u16, u32)]) -> Vec<u8> {
        let mut out = ACL_XATTR_VERSION.to_le_bytes().to_vec();
        for (tag, perm, id) in entries {
            out.extend_from_slice(&tag.to_le_bytes());
            out.extend_from_slice(&perm.to_le_bytes());
            out.extend_from_slice(&id.to_le_bytes());
        }
        out
    }

    #[test]
    fn named_users_and_groups_of_an_acl_are_shifted_and_nothing_else() {
        const UNDEFINED: u32 = u32::MAX;
        let before = acl(&[
            (0x01, 7, UNDEFINED),
            (ACL_USER, 5, 1000),
            (0x04, 5, UNDEFINED),
            (ACL_GROUP, 5, 33),
            (0x10, 7, UNDEFINED),
            (0x20, 5, UNDEFINED),
        ]);
        let after = shifted_acl(&before, 0x0008_0000).unwrap();
        assert_eq!(
            after,
            acl(&[
                (0x01, 7, UNDEFINED),
                (ACL_USER, 5, 0x0008_0000 + 1000),
                (0x04, 5, UNDEFINED),
                (ACL_GROUP, 5, 0x0008_0000 + 33),
                (0x10, 7, UNDEFINED),
                (0x20, 5, UNDEFINED),
            ])
        );
        // Shifted already, or only the owner's entries: nothing to write.
        assert_eq!(shifted_acl(&after, 0x0008_0000), None);
        assert_eq!(
            shifted_acl(&acl(&[(0x01, 7, UNDEFINED)]), 0x0008_0000),
            None
        );
        // What the kernel does not write is left alone.
        assert_eq!(shifted_acl(&before[..7], 0x0008_0000), None);
        let mut other = before.clone();
        other[0] = 1;
        assert_eq!(shifted_acl(&other, 0x0008_0000), None);
    }

    #[test]
    fn a_tree_in_its_range_already_is_walked_whole_and_left_as_it_is() {
        // Unprivileged, a chown to another range fails: the walk is checked on a tree
        // whose owners are in the range already, which it leaves as they are.
        let me = (
            nix::unistd::getuid().as_raw(),
            nix::unistd::getgid().as_raw(),
        );
        if me.0 & !LOW != me.1 & !LOW {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::write(root.join("a/b/f"), b"x").unwrap();
        std::os::unix::fs::symlink("/nowhere", root.join("a/link")).unwrap();
        let go_on = AtomicBool::new(false);
        shift_tree(&root, me.0 & !LOW, &go_on).unwrap();
        for path in ["", "a", "a/b", "a/b/f", "a/link"] {
            let st = nix::sys::stat::lstat(&root.join(path)).unwrap();
            assert_eq!((st.st_uid, st.st_gid), me, "{path}");
        }
        assert!(shift_tree(&dir.path().join("none"), 0, &go_on).is_err());
        // A stop ends it before the next entry.
        let e = shift_tree(&root, me.0 & !LOW, &AtomicBool::new(true)).unwrap_err();
        assert!(format!("{e:#}").contains("stopped"), "{e:#}");
    }
}
