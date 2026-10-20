//! cp: files between the host and a machine, as a tar stream. The
//! service packs or unpacks on the machine's side, the command line on the host's, so
//! what lands on the host belongs to whoever ran `cp`, and what lands in a machine to
//! its root.
//!
//! Every path is resolved relative to a directory descriptor, never as a host path:
//! inside a machine with RESOLVE_IN_ROOT, so that a symlink in the machine, absolute or
//! not, stays in the machine, and RESOLVE_NO_MAGICLINKS, so that nothing reaches the
//! host through /proc. The receiving side of the command line resolves the same way
//! beneath the destination directory, so a stream cannot write beside it either.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, bail, Context as _, Result};
use nix::errno::Errno;
use nix::fcntl::{openat, openat2, readlinkat, AtFlags, OFlag, OpenHow, ResolveFlag};
use nix::sys::stat::{fchmod, fstat, fstatat, futimens, mkdirat, Mode, SFlag};
use nix::sys::time::TimeSpec;
use nix::unistd::{fchown, fchownat, symlinkat, unlinkat, Gid, Uid, UnlinkatFlags};

use crate::api::{note, Context, Report};
use crate::backend::{is_mountpoint, BackendChoice};
use crate::reference::validate_machine_name;

/// One side of `cp`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    Local(PathBuf),
    Machine { name: String, path: String },
}

impl Endpoint {
    /// `NAME:PATH` is a machine's path (relative ones start at its root), anything else
    /// a local path; a local path with a colon is written `./a:b`.
    pub fn parse(arg: &str) -> Result<Endpoint> {
        if arg == "-" {
            bail!("cp does not read or write tar streams on - yet; give a path");
        }
        if arg.is_empty() {
            bail!("an empty path");
        }
        if arg.starts_with('/') || arg.starts_with('.') {
            return Ok(Endpoint::Local(PathBuf::from(arg)));
        }
        match arg.split_once(':') {
            None => Ok(Endpoint::Local(PathBuf::from(arg))),
            Some((name, path)) => {
                if name.is_empty() {
                    bail!("{arg}: no machine before the colon");
                }
                validate_machine_name(name)?;
                if path.is_empty() {
                    bail!("{arg}: no path after the colon; {name}:/ is the machine's root");
                }
                let path = if path.starts_with('/') {
                    path.to_string()
                } else {
                    format!("/{path}")
                };
                Ok(Endpoint::Machine {
                    name: name.to_string(),
                    path,
                })
            }
        }
    }
}

/// What a destination is, as far as copying goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Directory,
    /// A character or block device, which no copy replaces.
    Device,
    Other,
}

/// Where the copied tree goes: `top` (the source's top entry, renamed) inside `dir`, or
/// with `top` None the source's contents straight into `dir`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub dir: PathBuf,
    pub top: Option<OsString>,
}

/// docker cp's rules for a destination. `contents` is a source written `DIR/.`,
/// `trailing_slash` a destination written `DIR/`.
pub fn plan(
    source_is_dir: bool,
    source_name: &OsStr,
    contents: bool,
    dst: &Path,
    dst_kind: Option<Kind>,
    trailing_slash: bool,
) -> Result<Plan> {
    let parent = || -> PathBuf {
        match dst.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            Some(_) => PathBuf::from("."),
            None => PathBuf::from("/"),
        }
    };
    let own_name = || -> Result<OsString> {
        dst.file_name()
            .map(OsStr::to_os_string)
            .ok_or_else(|| anyhow!("{} cannot be the name of a copy", dst.display()))
    };
    match (source_is_dir, dst_kind) {
        (_, Some(Kind::Device)) => bail!(
            "invalid output path: {:?} must be a directory or a regular file: got a device",
            dst.display().to_string()
        ),
        (false, None) if trailing_slash => {
            bail!("{} does not exist and names a directory", dst.display())
        }
        (_, None) => Ok(Plan {
            dir: parent(),
            top: Some(own_name()?),
        }),
        (false, Some(Kind::Directory)) => Ok(Plan {
            dir: dst.to_path_buf(),
            top: Some(source_name.to_os_string()),
        }),
        (false, Some(Kind::Other)) if trailing_slash => {
            bail!("{} is not a directory", dst.display())
        }
        (false, Some(Kind::Other)) => Ok(Plan {
            dir: parent(),
            top: Some(own_name()?),
        }),
        (true, Some(Kind::Other)) => bail!(
            "cannot copy a directory onto {}, which is not one",
            dst.display()
        ),
        (true, Some(Kind::Directory)) if contents => Ok(Plan {
            dir: dst.to_path_buf(),
            top: None,
        }),
        (true, Some(Kind::Directory)) => Ok(Plan {
            dir: dst.to_path_buf(),
            top: Some(source_name.to_os_string()),
        }),
    }
}

/// How many links a destination is followed through after the first, as docker cp's
/// CopyInfoDestinationPath counts them.
const DESTINATION_LINKS: usize = 10;

/// How many links resolving a path inside a machine follows, as FollowSymlinkInScope.
const SCOPED_LINKS: usize = 255;

/// docker cp's destination on the command line's side: a final link stands for what it
/// names, each target joined to the link's directory as written, so the copy is written
/// there, whether that exists or not, and the link stays. `read_link` gives a path's
/// link target, None for anything else; `rooted` paths are relative to a root that `..`
/// never climbs above.
pub fn follow_destination(
    dst: &Path,
    rooted: bool,
    mut read_link: impl FnMut(&Path) -> Result<Option<PathBuf>>,
) -> Result<PathBuf> {
    let mut path = dst.to_path_buf();
    let mut followed = 0;
    while let Some(target) = read_link(&path)? {
        if followed > DESTINATION_LINKS {
            bail!("too many symlinks in {}", dst.display());
        }
        followed += 1;
        let joined = if target.is_absolute() {
            target
        } else {
            path.parent().unwrap_or_else(|| Path::new("")).join(target)
        };
        path = clean(&joined, rooted);
    }
    Ok(path)
}

/// docker cp's destination inside a machine: a final link stands for what it names,
/// resolved all the way inside the root as docker's daemon resolves it (LinkTarget),
/// links on the way included, so the copy is written there, whether that exists or not,
/// and the link stays.
pub fn destination_in(root: BorrowedFd<'_>, dst: &Path) -> Result<PathBuf> {
    if dst.as_os_str().is_empty() || link_in(root, dst)?.is_none() {
        return Ok(dst.to_path_buf());
    }
    resolve_in(root, dst)
}

/// `path` beneath `root` with every link on the way followed, the last one too, as
/// FollowSymlinkInScope resolves it: an absolute target starts at the root, `..` stops
/// there, and what does not exist yet is taken as written.
fn resolve_in(root: BorrowedFd<'_>, path: &Path) -> Result<PathBuf> {
    // The names still to walk, the next one last.
    fn push(pending: &mut Vec<OsString>, path: &Path) {
        let names: Vec<OsString> = path
            .components()
            .filter_map(|c| match c {
                Component::Normal(name) => Some(name.to_os_string()),
                Component::ParentDir => Some(OsString::from("..")),
                _ => None,
            })
            .collect();
        pending.extend(names.into_iter().rev());
    }
    let mut pending = Vec::new();
    push(&mut pending, path);
    let mut resolved = PathBuf::new();
    let mut links = 0;
    while let Some(name) = pending.pop() {
        if name == ".." {
            resolved.pop();
            continue;
        }
        let candidate = resolved.join(&name);
        match link_in(root, &candidate)? {
            Some(target) => {
                links += 1;
                if links > SCOPED_LINKS {
                    bail!("too many symlinks in {}", path.display());
                }
                if target.is_absolute() {
                    resolved = PathBuf::new();
                }
                push(&mut pending, &target);
            }
            None => resolved = candidate,
        }
    }
    Ok(resolved)
}

/// A path without `.` and with each `..` taking the name before it, as Go's
/// filepath.Clean; a rooted one loses its leading slash and any `..` above it.
pub(crate) fn clean(path: &Path, rooted: bool) -> PathBuf {
    let mut out: Vec<Component> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) if rooted => out.clear(),
            Component::ParentDir => match out.last() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir) => {}
                _ if rooted => {}
                _ => out.push(component),
            },
            other => out.push(other),
        }
    }
    let cleaned: PathBuf = out.iter().collect();
    if cleaned.as_os_str().is_empty() && !rooted {
        PathBuf::from(".")
    } else {
        cleaned
    }
}

/// The target of a link at `path` beneath `root`, the root being where an absolute
/// one starts; None for anything else, or for a path whose directory is not there.
pub fn link_in(root: BorrowedFd<'_>, path: &Path) -> Result<Option<PathBuf>> {
    let (parent, leaf) = split_leaf(path);
    if leaf == "." || leaf == ".." {
        return Ok(None);
    }
    let dir = match scoped_raw(root, &parent, OFlag::O_PATH | OFlag::O_DIRECTORY) {
        Ok(dir) => dir,
        Err(Errno::ENOENT | Errno::ENOTDIR) => return Ok(None),
        Err(e) => return Err(nix_error(e, &parent)),
    };
    match readlinkat(&dir, leaf.as_os_str()) {
        Ok(target) => Ok(Some(PathBuf::from(target))),
        Err(Errno::EINVAL | Errno::ENOENT) => Ok(None),
        Err(e) => Err(nix_error(e, path)),
    }
}

/// A path as `cp` was given it: `DIR/.` means the contents of DIR, a trailing slash
/// that it must be a directory.
pub fn split_source(path: &str) -> (String, bool) {
    // As filepath.Base reads the last element: trailing slashes do not count.
    let trimmed = path.trim_end_matches('/');
    if trimmed == "." {
        return (".".to_string(), true);
    }
    if let Some(stripped) = trimmed.strip_suffix("/.") {
        let stripped = if stripped.is_empty() { "/" } else { stripped };
        return (stripped.to_string(), true);
    }
    (path.to_string(), false)
}

/// How owners travel: as they are on the command line's side, shifted by the user
/// namespace's first UID on a machine's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ids {
    Keep,
    /// The host UID that is root inside the machine.
    Shift(u32),
}

impl Ids {
    fn outward(self, id: u32) -> u32 {
        match self {
            Ids::Keep => id,
            Ids::Shift(base) if id >= base && id - base < 65536 => id - base,
            Ids::Shift(_) => 65534,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Stats {
    pub entries: u64,
    pub bytes: u64,
    /// Sockets, devices and fifos, which are not copied.
    pub skipped: Vec<String>,
}

fn nix_error(e: Errno, what: &Path) -> anyhow::Error {
    match e {
        Errno::ENOSYS => anyhow!("cp needs Linux 5.6 or newer (openat2)"),
        Errno::EXDEV | Errno::ELOOP => anyhow!(
            "{}: the path leaves the machine or loops through its links",
            what.display()
        ),
        Errno::ENOENT => anyhow!("{}: no such file or directory", what.display()),
        Errno::ENOTDIR => anyhow!("{}: not a directory", what.display()),
        Errno::EAGAIN => anyhow!(
            "{}: its lookup kept crossing renames or mounts on the host; try again",
            what.display()
        ),
        other => anyhow!("{}: {}", what.display(), other.desc()),
    }
}

fn scoped_raw(root: BorrowedFd<'_>, path: &Path, flags: OFlag) -> nix::Result<OwnedFd> {
    let lookup: &Path = if path.as_os_str().is_empty() {
        Path::new(".")
    } else {
        path
    };
    retry_on_eagain(|| {
        openat2(
            root,
            lookup,
            OpenHow::new()
                .flags(flags | OFlag::O_CLOEXEC)
                .resolve(ResolveFlag::RESOLVE_IN_ROOT | ResolveFlag::RESOLVE_NO_MAGICLINKS),
        )
    })
}

/// How often a lookup is tried before its EAGAIN is the answer.
const LOOKUP_ATTEMPTS: usize = 32;

/// openat2 answers EAGAIN when a `..` on the way crossed a rename or a mount anywhere on
/// the system, since it cannot then vouch that the lookup stayed below the root, and its
/// manual leaves the retry to the caller.
fn retry_on_eagain<T>(mut attempt: impl FnMut() -> nix::Result<T>) -> nix::Result<T> {
    for _ in 1..LOOKUP_ATTEMPTS {
        match attempt() {
            Err(Errno::EAGAIN) => continue,
            other => return other,
        }
    }
    attempt()
}

/// Opens `path` beneath `root`, which it can never leave.
pub fn scoped(root: BorrowedFd<'_>, path: &Path, flags: OFlag) -> Result<OwnedFd> {
    scoped_raw(root, path, flags).map_err(|e| nix_error(e, path))
}

/// What `path` beneath `root` is, following links inside it; None when it is not there.
pub fn kind_in(root: BorrowedFd<'_>, path: &Path) -> Result<Option<Kind>> {
    match scoped_raw(root, path, OFlag::O_PATH) {
        Ok(fd) => {
            let st = fstat(&fd).map_err(|e| nix_error(e, path))?;
            Ok(Some(if is(st.st_mode, SFlag::S_IFDIR) {
                Kind::Directory
            } else if is(st.st_mode, SFlag::S_IFCHR) || is(st.st_mode, SFlag::S_IFBLK) {
                Kind::Device
            } else {
                Kind::Other
            }))
        }
        Err(Errno::ENOENT) => Ok(None),
        Err(e) => Err(nix_error(e, path)),
    }
}

/// Whether the directory is on a file system the kernel makes up as it goes rather
/// than one that holds files: procfs, sysfs, cgroups, devpts and their kind (the
/// magics of linux/magic.h).
fn kernel_filesystem(dir: &OwnedFd) -> bool {
    const KERNEL_MAGICS: [u64; 17] = [
        0x9fa0,     // proc
        0x62656572, // sysfs
        0x27e0eb,   // cgroup
        0x63677270, // cgroup2
        0x1cd1,     // devpts
        0x19800202, // mqueue
        0x42494e4d, // binfmt_misc
        0x73636673, // securityfs
        0x64626720, // debugfs
        0x74726163, // tracefs
        0xf97cff8c, // selinuxfs
        0xcafe4a11, // bpf
        0x6165676c, // pstore
        0xde5e81e4, // efivarfs
        0x65735543, // fusectl
        0x62656570, // configfs
        0x958458f6, // hugetlbfs
    ];
    let Ok(stat) = nix::sys::statfs::fstatfs(dir) else {
        return false;
    };
    KERNEL_MAGICS.contains(&(stat.filesystem_type().0 as u64 & 0xffff_ffff))
}

fn is(mode: u32, kind: SFlag) -> bool {
    mode & SFlag::S_IFMT.bits() == kind.bits()
}

/// A path inside a machine, relative to its root.
pub fn inside(path: &str) -> PathBuf {
    PathBuf::from(path.trim_start_matches('/'))
}

/// The parent directory and the last component of a relative path; the root itself is
/// "." in "".
pub fn split_leaf(path: &Path) -> (PathBuf, OsString) {
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => (parent.to_path_buf(), name.to_os_string()),
        // A path that ends in .. names the directory it leads to, not the root: it is
        // looked up whole, the leaf being the directory itself.
        _ if path.ends_with("..") => (path.to_path_buf(), OsString::from(".")),
        _ => (PathBuf::new(), OsString::from(".")),
    }
}

/// Reads exactly `left` bytes, zeros when the file shrank meanwhile or a read failed: the
/// header already promised the size, and a failed packing must break the stream where a
/// header is read, not inside the file. The error is kept for after the entry.
struct Exact<R> {
    file: R,
    left: u64,
    failed: Option<io::Error>,
}

impl<R: Read> Read for Exact<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 {
            return Ok(0);
        }
        let want = buf.len().min(self.left.min(usize::MAX as u64) as usize);
        if self.failed.is_none() {
            match self.file.read(&mut buf[..want]) {
                Ok(0) => {}
                Ok(n) => {
                    self.left -= n as u64;
                    return Ok(n);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => return Err(e),
                Err(e) => self.failed = Some(e),
            }
        }
        buf[..want].fill(0);
        self.left -= want as u64;
        Ok(want)
    }
}

/// How deep `pack` goes into directories: whoever writes a tree makes it as deep as they
/// like, and each level takes a descriptor and a frame of the copy's stack.
pub const MAX_DEPTH: usize = 2048;

/// The stack of a thread that packs a tree, room for `MAX_DEPTH` levels.
pub const COPY_STACK: usize = 32 << 20;

/// Lifts the soft limit of open files to the hard one, as Go programs do: systemd starts
/// services and sessions at 1024, fewer than a tree `MAX_DEPTH` deep takes, and nspawn
/// waits on its descriptors with epoll, never with select.
pub fn raise_open_files() {
    use nix::sys::resource::{getrlimit, setrlimit, Resource};
    if let Ok((soft, hard)) = getrlimit(Resource::RLIMIT_NOFILE) {
        if soft < hard {
            let _ = setrlimit(Resource::RLIMIT_NOFILE, hard, hard);
        }
    }
}

/// Writes `leaf` of `parent` and, for a directory, everything below it as a tar stream
/// whose top entry is named `top`. Links are archived as links, never followed.
pub fn pack<W: Write>(
    parent: BorrowedFd<'_>,
    leaf: &OsStr,
    top: &Path,
    ids: Ids,
    out: W,
) -> Result<Stats> {
    let mut builder = tar::Builder::new(out);
    let mut stats = Stats::default();
    if let Err(e) = pack_entry(&mut builder, parent, leaf, top, ids, &mut stats, 0) {
        // The builder ends the archive when dropped, and the other end would take a
        // clean end for the whole tree: a block that is no header breaks it instead.
        let _ = builder.get_mut().write_all(&[0xff; 512]);
        return Err(e);
    }
    builder
        .into_inner()
        .and_then(|mut out| out.flush())
        .context("finishing the stream")?;
    Ok(stats)
}

fn pack_entry<W: Write>(
    builder: &mut tar::Builder<W>,
    dirfd: BorrowedFd<'_>,
    leaf: &OsStr,
    path: &Path,
    ids: Ids,
    stats: &mut Stats,
    depth: usize,
) -> Result<()> {
    let st = fstatat(dirfd, leaf, AtFlags::AT_SYMLINK_NOFOLLOW).map_err(|e| nix_error(e, path))?;
    let mut header = tar::Header::new_gnu();
    header.set_mode(st.st_mode & 0o7777);
    header.set_mtime(st.st_mtime.max(0) as u64);
    header.set_uid(u64::from(ids.outward(st.st_uid)));
    header.set_gid(u64::from(ids.outward(st.st_gid)));
    if is(st.st_mode, SFlag::S_IFDIR) {
        if depth >= MAX_DEPTH {
            bail!(
                "{}: more than {MAX_DEPTH} directories deep; nothing that deep is copied",
                path.display()
            );
        }
        let dir = openat(
            dirfd,
            leaf,
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| nix_error(e, path))?;
        // The kernel's own file systems mounted in a running machine (/proc, /sys,
        // cgroups) are not the machine's files: /proc/kcore alone reads as the whole
        // of memory.
        if kernel_filesystem(&dir) {
            return Ok(());
        }
        header.set_entry_type(tar::EntryType::Directory);
        header.set_size(0);
        builder
            .append_data(&mut header, path, io::empty())
            .with_context(|| format!("writing {}", path.display()))?;
        stats.entries += 1;
        let listing = dir
            .try_clone()
            .context("duplicating a directory descriptor")?;
        let mut names: Vec<OsString> = Vec::new();
        for entry in nix::dir::Dir::from_fd(listing)
            .map_err(|e| nix_error(e, path))?
            .iter()
        {
            let entry = entry.map_err(|e| nix_error(e, path))?;
            let name = entry.file_name().to_bytes();
            if name != b"." && name != b".." {
                names.push(OsStr::from_bytes(name).to_os_string());
            }
        }
        names.sort();
        for name in names {
            let child = path.join(&name);
            // What goes or changes kind in a running machine between the listing and
            // its turn is left out, as the reference leaves it; anything else fails.
            if let Err(e) = pack_entry(builder, dir.as_fd(), &name, &child, ids, stats, depth + 1) {
                if !went_meanwhile(&e) {
                    return Err(e);
                }
                stats.skipped.push(child.display().to_string());
            }
        }
    } else if is(st.st_mode, SFlag::S_IFREG) {
        // Without blocking, and checked again once open: what the machine swaps in
        // meanwhile, a fifo say, must not hold the copy forever.
        let file = openat(
            dirfd,
            leaf,
            OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| nix_error(e, path))?;
        if !is(
            fstat(&file).map_err(|e| nix_error(e, path))?.st_mode,
            SFlag::S_IFREG,
        ) {
            bail!("{}: changed while it was being copied", path.display());
        }
        let size = st.st_size.max(0) as u64;
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(size);
        let mut data = Exact {
            file: File::from(file),
            left: size,
            failed: None,
        };
        builder
            .append_data(&mut header, path, &mut data)
            .with_context(|| format!("writing {}", path.display()))?;
        if let Some(e) = data.failed {
            return Err(anyhow::Error::new(e).context(format!("reading {}", path.display())));
        }
        stats.entries += 1;
        stats.bytes += size;
    } else if is(st.st_mode, SFlag::S_IFLNK) {
        let target = readlinkat(dirfd, leaf).map_err(|e| nix_error(e, path))?;
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        builder
            .append_link(&mut header, path, Path::new(&target))
            .with_context(|| format!("writing {}", path.display()))?;
        stats.entries += 1;
    } else {
        stats.skipped.push(path.display().to_string());
    }
    Ok(())
}

/// Whether an entry's error says it went or changed kind while the copy walked, as
/// `nix_error` words it.
fn went_meanwhile(e: &anyhow::Error) -> bool {
    let text = e.to_string();
    [
        ": no such file or directory",
        ": not a directory",
        ": the path leaves the machine or loops through its links",
        ": changed while it was being copied",
    ]
    .iter()
    .any(|end| text.ends_with(end))
}

/// Where the stream goes, decided from its first entry: whether it is a directory and
/// the name it carries.
pub struct Target {
    pub root: OwnedFd,
    /// Relative to `root`.
    pub base: PathBuf,
    pub top: Option<OsString>,
}

/// The components of an entry's path, all of them plain names.
fn plain_components(path: &Path) -> Result<Vec<OsString>> {
    let mut out = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => out.push(name.to_os_string()),
            Component::CurDir => {}
            _ => bail!("{}: not a plain relative path", path.display()),
        }
    }
    Ok(out)
}

fn timespec(mtime: u64) -> TimeSpec {
    TimeSpec::new(mtime.min(i64::MAX as u64) as i64, 0)
}

/// Writes the entries of a tar stream beneath the target `decide` picks from its first
/// entry. With `Ids::Shift` everything becomes root's inside the machine; with
/// `Ids::Keep` nothing is chowned and it belongs to whoever runs this.
pub fn unpack<R: Read>(
    reader: R,
    decide: impl FnOnce(bool, &OsStr) -> Result<Target>,
    ids: Ids,
) -> Result<Stats> {
    let mut archive = tar::Archive::new(reader);
    let mut decide = Some(decide);
    let mut target: Option<Target> = None;
    let mut stats = Stats::default();
    // The directories made, by path: a descriptor held for each would run out of them on
    // a tree of a few thousand.
    let mut directories: Vec<(PathBuf, u64)> = Vec::new();
    // A file written whole takes its name only once the next header reads well: the
    // sender breaks the stream after an entry that failed to read, filled to its size.
    let mut pending: Option<Staged> = None;
    for entry in archive.entries().context("reading the stream")? {
        let mut entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                if let Some(staged) = pending.take() {
                    staged.discard();
                }
                return Err(e).context("reading the stream");
            }
        };
        if let Some(staged) = pending.take() {
            staged.finish()?;
        }
        let raw = entry
            .path()
            .context("reading an entry's path")?
            .into_owned();
        let parts = plain_components(&raw)?;
        let Some(first) = parts.first() else {
            continue;
        };
        let kind = entry.header().entry_type();
        if target.is_none() {
            let decide = decide.take().expect("decided once");
            target = Some(decide(kind.is_dir() && parts.len() == 1, first)?);
        }
        let target = target.as_ref().expect("decided above");
        let mut relative: Vec<OsString> = match &target.top {
            Some(top) => std::iter::once(top.clone())
                .chain(parts[1..].iter().cloned())
                .collect(),
            None => parts[1..].to_vec(),
        };
        let Some(leaf) = relative.pop() else {
            // The top directory itself, going into one that is there already.
            continue;
        };
        let parent_path: PathBuf = target.base.join(relative.iter().collect::<PathBuf>());
        let shown = parent_path.join(&leaf);
        let parent = scoped(
            target.root.as_fd(),
            &parent_path,
            OFlag::O_PATH | OFlag::O_DIRECTORY,
        )?;
        let mode = Mode::from_bits_truncate(entry.header().mode().unwrap_or(0o644) & 0o7777);
        let mtime = entry.header().mtime().unwrap_or(0);
        let owner = match ids {
            Ids::Keep => None,
            Ids::Shift(base) => Some((Uid::from_raw(base), Gid::from_raw(base))),
        };
        match kind {
            tar::EntryType::Directory => {
                match mkdirat(&parent, leaf.as_os_str(), Mode::from_bits_truncate(0o700)) {
                    Ok(()) => {
                        let dir = openat(
                            &parent,
                            leaf.as_os_str(),
                            OFlag::O_RDONLY
                                | OFlag::O_DIRECTORY
                                | OFlag::O_NOFOLLOW
                                | OFlag::O_CLOEXEC,
                            Mode::empty(),
                        )
                        .map_err(|e| nix_error(e, &shown))?;
                        if let Some((uid, gid)) = owner {
                            fchown(&dir, Some(uid), Some(gid)).map_err(|e| nix_error(e, &shown))?;
                        }
                        fchmod(&dir, mode).map_err(|e| nix_error(e, &shown))?;
                        directories.push((shown.clone(), mtime));
                    }
                    // One that is there already keeps its owner and mode.
                    Err(Errno::EEXIST) => {
                        scoped(
                            target.root.as_fd(),
                            &shown,
                            OFlag::O_PATH | OFlag::O_DIRECTORY,
                        )
                        .map_err(|_| {
                            anyhow!("{} exists and is not a directory", shown.display())
                        })?;
                    }
                    Err(e) => return Err(nix_error(e, &shown)),
                }
            }
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                if is_dir_at(&parent, &leaf) {
                    bail!("{} is a directory; not replacing it", shown.display());
                }
                // Written beside its name and renamed over it once complete, so that a
                // copy cut short leaves the file that was there, not half of the new one.
                let staged = staged_leaf();
                let file = openat(
                    &parent,
                    staged.as_os_str(),
                    OFlag::O_WRONLY
                        | OFlag::O_CREAT
                        | OFlag::O_EXCL
                        | OFlag::O_NOFOLLOW
                        | OFlag::O_CLOEXEC,
                    Mode::from_bits_truncate(0o600),
                )
                .map_err(|e| nix_error(e, &shown))?;
                let mut file = File::from(file);
                let size = entry.header().entry_size().unwrap_or(0);
                let written: Result<u64> = (|| {
                    let bytes = io::copy(&mut entry, &mut file)
                        .with_context(|| format!("writing {}", shown.display()))?;
                    // A stream cut short ends an entry early without an error: the file
                    // that was there stays.
                    if bytes != size {
                        bail!(
                            "{}: the stream ended {} bytes short",
                            shown.display(),
                            size - bytes.min(size)
                        );
                    }
                    if let Some((uid, gid)) = owner {
                        fchown(&file, Some(uid), Some(gid)).map_err(|e| nix_error(e, &shown))?;
                    }
                    // After the chown, which clears setuid and setgid.
                    fchmod(&file, mode).map_err(|e| nix_error(e, &shown))?;
                    futimens(&file, &timespec(mtime), &timespec(mtime))
                        .map_err(|e| nix_error(e, &shown))?;
                    Ok(bytes)
                })();
                match written {
                    Ok(bytes) => {
                        stats.bytes += bytes;
                        pending = Some(Staged {
                            parent,
                            staged,
                            leaf,
                            shown,
                        });
                    }
                    Err(e) => {
                        let _ = unlinkat(&parent, staged.as_os_str(), UnlinkatFlags::NoRemoveDir);
                        return Err(e);
                    }
                }
            }
            tar::EntryType::Symlink => {
                let link = entry
                    .link_name()
                    .context("reading a link's target")?
                    .ok_or_else(|| anyhow!("{}: a link without a target", shown.display()))?
                    .into_owned();
                replace_leaf(&parent, &leaf, &shown)?;
                symlinkat(&link, &parent, leaf.as_os_str()).map_err(|e| nix_error(e, &shown))?;
                if let Some((uid, gid)) = owner {
                    fchownat(
                        &parent,
                        leaf.as_os_str(),
                        Some(uid),
                        Some(gid),
                        AtFlags::AT_SYMLINK_NOFOLLOW,
                    )
                    .map_err(|e| nix_error(e, &shown))?;
                }
            }
            tar::EntryType::Link => {
                bail!(
                    "{}: hard links are not copied; the stream should carry files",
                    shown.display()
                )
            }
            _ => {
                stats.skipped.push(shown.display().to_string());
                continue;
            }
        }
        stats.entries += 1;
    }
    // The stream ended cleanly after it.
    if let Some(staged) = pending.take() {
        staged.finish()?;
    }
    let Some(target) = target else {
        bail!("nothing arrived to copy");
    };
    // Last: writing into a directory changes its time. Set through its parent, which
    // needs no read permission on it, as a directory of mode 0311 has none.
    for (path, mtime) in directories.iter().rev() {
        let (parent, leaf) = match (path.parent(), path.file_name()) {
            (Some(parent), Some(leaf)) => (parent, leaf),
            _ => continue,
        };
        let flags = OFlag::O_PATH | OFlag::O_DIRECTORY;
        if let Ok(dir) = scoped_raw(target.root.as_fd(), parent, flags) {
            let _ = nix::sys::stat::utimensat(
                &dir,
                leaf,
                &timespec(*mtime),
                &timespec(*mtime),
                nix::sys::stat::UtimensatFlags::NoFollowSymlink,
            );
        }
    }
    Ok(stats)
}

/// A file written in full beside its name, which it takes once the stream says it is
/// whole.
struct Staged {
    parent: OwnedFd,
    staged: OsString,
    leaf: OsString,
    shown: PathBuf,
}

impl Staged {
    fn finish(self) -> Result<()> {
        let renamed = nix::fcntl::renameat(
            &self.parent,
            self.staged.as_os_str(),
            &self.parent,
            self.leaf.as_os_str(),
        );
        if let Err(e) = renamed {
            self.discard();
            return Err(nix_error(e, &self.shown));
        }
        Ok(())
    }

    fn discard(&self) {
        let _ = unlinkat(
            &self.parent,
            self.staged.as_os_str(),
            UnlinkatFlags::NoRemoveDir,
        );
    }
}

/// The name a file is written under until it is complete: beside its own, never the
/// name of anything the archive carries.
fn staged_leaf() -> OsString {
    // Not built on the leaf: a name near the longest a file system takes would leave no
    // room for the rest.
    OsString::from(format!(".nspawn-cp-{}", crate::store::unique_suffix()))
}

/// Makes room for a link: what is there goes, unless it is a directory.
fn replace_leaf(parent: &OwnedFd, leaf: &OsStr, shown: &Path) -> Result<()> {
    match unlinkat(parent, leaf, UnlinkatFlags::NoRemoveDir) {
        Ok(()) | Err(Errno::ENOENT) => Ok(()),
        Err(Errno::EISDIR) | Err(Errno::EPERM) if is_dir_at(parent, leaf) => {
            bail!("{} is a directory; not replacing it", shown.display())
        }
        Err(e) => Err(nix_error(e, shown)),
    }
}

fn is_dir_at(parent: &OwnedFd, leaf: &OsStr) -> bool {
    fstatat(parent, leaf, AtFlags::AT_SYMLINK_NOFOLLOW)
        .map(|st| is(st.st_mode, SFlag::S_IFDIR))
        .unwrap_or(false)
}

/// Runs blocking copy work on a thread of its own where a closed pipe is an error and
/// not SIGPIPE: the service must not die because a client went away.
pub async fn on_thread<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("nspawn-cp".to_string())
        .stack_size(COPY_STACK)
        .spawn(move || {
            block_sigpipe();
            let _ = tx.send(work());
        })
        .context("starting the copy")?;
    rx.await
        .map_err(|_| anyhow!("the copy ended without a result"))?
}

pub fn block_sigpipe() {
    use nix::sys::signal::{pthread_sigmask, SigSet, SigmaskHow, Signal};
    let mut set = SigSet::empty();
    set.add(Signal::SIGPIPE);
    let _ = pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&set), None);
}

/// A machine's root directory and the host UID that is root inside it.
pub struct Root {
    pub fd: OwnedFd,
    pub base: u32,
}

/// Writes from now on as `id` on this thread, with root's file capabilities kept, since
/// the machine's root may write where its owner alone could not (/root is 0550). An
/// idmapped mount (the tree of an mstack machine, a volume of a machine with private
/// users) only lets a user it maps create files, which host root is not; everywhere
/// else the files come out owned by `id` just the same. Only the copy's own thread is
/// affected.
fn write_as(id: u32) -> Result<()> {
    nix::unistd::setfsgid(Gid::from_raw(id));
    nix::unistd::setfsuid(Uid::from_raw(id));
    crate::nsenter::raise_file_capabilities().map_err(|e| {
        anyhow!(
            "keeping root's file capabilities for the copy: {}",
            e.desc()
        )
    })
}

/// The root of a running machine as its init sees it (every backend), or the tree of a
/// stopped overlay or flat one; a stopped mstack machine has no tree on the host.
pub async fn open_root(ctx: &Context, name: &str, report: Report<'_>) -> Result<Root> {
    validate_machine_name(name)?;
    let sd = ctx.sd().await?;
    crate::api::machines::refuse_foreign(sd, name).await?;
    let store = &ctx.store;
    if store.is_starting(name) {
        bail!("machine {name} is starting; wait for it");
    }
    if sd.machine_exists(name).await? {
        let leader = sd.machine_leader(name).await?;
        let proc_path = PathBuf::from(format!("/proc/{leader}"));
        let proc_dir = nix::fcntl::open(
            &proc_path,
            OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| nix_error(e, &proc_path))?;
        // The PID could have been another process's by now; the descriptor is only
        // trusted once the machine still names it.
        if sd.machine_leader(name).await? != leader {
            bail!("machine {name} changed while it was being opened; try again");
        }
        let fd = openat(
            &proc_dir,
            "root",
            OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| nix_error(e, &proc_path.join("root")))?;
        let seen = fstat(&fd).map_err(|e| nix_error(e, &proc_path))?.st_uid;
        let managed = store
            .load_image(name)?
            .is_some_and(|r| r.backend == BackendChoice::Mstack);
        if managed {
            // The tree is mountfsd's idmapped mount: the owner of / as seen through it is
            // the range nsresourced gave the machine, which is what root inside is.
            return Ok(Root { fd, base: seen });
        }
        let base = match sd.machine_uid_shift(name).await {
            Ok(shift) => {
                if shift != seen {
                    note(
                        report,
                        format!("note: {name}: machined shifts UIDs by {shift}, its root directory belongs to {seen}; using {shift}"),
                    );
                }
                shift
            }
            Err(_) => seen,
        };
        return Ok(Root { fd, base });
    }
    let record = store
        .load_image(name)?
        .ok_or_else(|| anyhow!("no machine or image named {name}"))?;
    let tree = store.machines_dir.join(name);
    match record.backend {
        BackendChoice::Mstack => bail!(
            "{name} is an mstack machine and not running; start it first (its root exists only while it runs)"
        ),
        // Mounted by its unit, which a stop leaves alone; mounted again if it went.
        BackendChoice::Overlay if !is_mountpoint(&tree)? => {
            sd.start_unit(&crate::unitname::mount_unit_for(&tree.to_string_lossy()))
                .await
                .with_context(|| format!("mounting the tree of {name}"))?;
        }
        _ => {}
    }
    let fd = nix::fcntl::open(
        &tree,
        OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| nix_error(e, &tree))?;
    // Root inside owns /: its owner is the shift, 0 before the first boot.
    let base = fstat(&fd).map_err(|e| nix_error(e, &tree))?.st_uid;
    Ok(Root { fd, base })
}

/// CopyFrom: `path` of the machine, packed into `out`.
pub async fn copy_from(
    ctx: &Context,
    name: &str,
    path: &str,
    out: OwnedFd,
    report: Report<'_>,
) -> Result<Stats> {
    let root = open_root(ctx, name, report).await?;
    let (source, _) = split_source(path);
    let relative = inside(&source);
    let (parent_path, leaf) = split_leaf(&relative);
    // The name the copy takes: the leaf, or for a path that ends in .. the directory it
    // comes to, the machine's name for its root.
    let top = if leaf == "." {
        let mut parts: Vec<&OsStr> = Vec::new();
        for component in relative.components() {
            match component {
                Component::Normal(part) => parts.push(part),
                Component::ParentDir => {
                    parts.pop();
                }
                _ => {}
            }
        }
        parts
            .last()
            .map_or_else(|| OsString::from(name), |p| p.to_os_string())
    } else {
        leaf.clone()
    };
    // A source ending in / or /. names a directory, a link to one followed as path
    // resolution follows it there: the directory is packed under the name given.
    let trailing = path.ends_with('/') || path.ends_with("/.");
    let (parent, leaf) = if trailing && leaf != "." {
        let dir = scoped(
            root.fd.as_fd(),
            &relative,
            OFlag::O_PATH | OFlag::O_DIRECTORY,
        )
        .map_err(|e| anyhow!("{name}:{source}: {e:#}"))?;
        (dir, OsString::from("."))
    } else {
        let parent = scoped(
            root.fd.as_fd(),
            &parent_path,
            OFlag::O_PATH | OFlag::O_DIRECTORY,
        )
        .map_err(|e| anyhow!("{name}:{source}: {e:#}"))?;
        (parent, leaf)
    };
    if let Err(e) = fstatat(&parent, leaf.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW) {
        bail!("{}", nix_error(e, Path::new(&format!("{name}:{source}"))));
    }
    let base = root.base;
    let stats = on_thread(move || {
        pack(
            parent.as_fd(),
            &leaf,
            Path::new(&top),
            Ids::Shift(base),
            File::from(out),
        )
    })
    .await?;
    for skipped in &stats.skipped {
        note(
            report,
            format!("note: {skipped} is not a file, directory or link; not copied"),
        );
    }
    Ok(stats)
}

/// CopyTo: a stream from `input` unpacked at `path` of the machine, by the rules of `plan`.
pub async fn copy_to(
    ctx: &Context,
    name: &str,
    path: &str,
    contents: bool,
    input: OwnedFd,
    report: Report<'_>,
) -> Result<Stats> {
    let root = open_root(ctx, name, report).await?;
    let trailing_slash = path.len() > 1 && path.ends_with('/');
    let relative = destination_in(root.fd.as_fd(), &inside(path.trim_end_matches('/')))?;
    let dst_kind = kind_in(root.fd.as_fd(), &relative)?;
    if dst_kind.is_none() {
        let (parent, _) = split_leaf(&relative);
        if kind_in(root.fd.as_fd(), &parent)? != Some(Kind::Directory) {
            bail!("{name}:/{}: no such directory", parent.display());
        }
    }
    let base = root.base;
    let label = format!("{name}:{path}");
    let stats = on_thread(move || {
        if base != 0 {
            write_as(base)?;
        }
        unpack(
            File::from(input),
            |is_dir, top| {
                let plan = plan(is_dir, top, contents, &relative, dst_kind, trailing_slash)
                    .map_err(|e| anyhow!("{label}: {e:#}"))?;
                Ok(Target {
                    root: root.fd,
                    base: plan.dir,
                    top: plan.top,
                })
            },
            Ids::Shift(base),
        )
    })
    .await?;
    for skipped in &stats.skipped {
        note(
            report,
            format!("note: {skipped} is not a file, directory or link; not copied"),
        );
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_kernel_s_own_file_systems_are_told_apart() {
        let open = |p: &str| {
            nix::fcntl::open(
                p,
                OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
                Mode::empty(),
            )
            .unwrap()
        };
        // Where the kernel's file systems sit depends on where the tests run: the root
        // of /sys is a tmpfs in a systemd-nspawn machine with a network of its own, with
        // sysfs mounted below it. The mount table says where they are.
        let mountinfo = fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
        for kind in ["proc", "sysfs"] {
            match mounted_at(&mountinfo, kind) {
                Some(point) => assert!(kernel_filesystem(&open(&point)), "{kind} at {point}"),
                None => eprintln!("no {kind} mounted here: not checked"),
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        assert!(!kernel_filesystem(&open(&tmp.path().to_string_lossy())));
    }

    /// The last mount point of a file system type in a /proc/PID/mountinfo, the one on top
    /// when several share a point; points with escaped characters are left out.
    fn mounted_at(mountinfo: &str, kind: &str) -> Option<String> {
        let mut top: std::collections::BTreeMap<&str, &str> = std::collections::BTreeMap::new();
        for line in mountinfo.lines() {
            let fields: Vec<&str> = line.split(' ').collect();
            let (Some(point), Some(dash)) = (fields.get(4), fields.iter().position(|f| *f == "-"))
            else {
                continue;
            };
            if let Some(fstype) = fields.get(dash + 1) {
                top.insert(point, fstype);
            }
        }
        top.into_iter()
            .find(|(point, fstype)| *fstype == kind && !point.contains('\\'))
            .map(|(point, _)| point.to_string())
    }

    #[test]
    fn mount_points_are_read_from_the_mount_table() {
        let table = "\
22 1 0:21 / /proc rw,nosuid shared:5 - proc proc rw
23 1 0:20 / /sys rw shared:6 - tmpfs tmpfs rw,mode=555
24 23 0:22 /kernel /sys/kernel rw - sysfs sysfs rw
25 1 0:23 / /mnt/a\\040b rw - sysfs sysfs rw
26 1 0:24 / /data rw - sysfs sysfs rw
27 1 0:25 / /data rw - ext4 /dev/vda rw
";
        assert_eq!(mounted_at(table, "proc").as_deref(), Some("/proc"));
        assert_eq!(mounted_at(table, "sysfs").as_deref(), Some("/sys/kernel"));
        assert_eq!(mounted_at(table, "tmpfs").as_deref(), Some("/sys"));
        assert_eq!(mounted_at(table, "cgroup2"), None);
    }
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn open_dir(path: &Path) -> OwnedFd {
        nix::fcntl::open(
            path,
            OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .unwrap()
    }

    #[test]
    fn endpoints_are_machine_or_local_paths() {
        let machine = |name: &str, path: &str| Endpoint::Machine {
            name: name.to_string(),
            path: path.to_string(),
        };
        assert_eq!(
            Endpoint::parse("web:/etc/hosts").unwrap(),
            machine("web", "/etc/hosts")
        );
        assert_eq!(
            Endpoint::parse("web:etc/hosts").unwrap(),
            machine("web", "/etc/hosts")
        );
        assert_eq!(
            Endpoint::parse("./a:b").unwrap(),
            Endpoint::Local(PathBuf::from("./a:b"))
        );
        assert_eq!(
            Endpoint::parse("/tmp/a:b").unwrap(),
            Endpoint::Local(PathBuf::from("/tmp/a:b"))
        );
        assert_eq!(
            Endpoint::parse("notes.txt").unwrap(),
            Endpoint::Local(PathBuf::from("notes.txt"))
        );
        assert_eq!(Endpoint::parse("a:b").unwrap(), machine("a", "/b"));
        for bad in ["web:", ":x", "bad name:/x", "-", "", "../x:y:z"] {
            if bad == "../x:y:z" {
                assert!(matches!(Endpoint::parse(bad), Ok(Endpoint::Local(_))));
                continue;
            }
            assert!(Endpoint::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn destinations_follow_the_cp_rules() {
        let p = |dir: &str, top: Option<&str>| Plan {
            dir: PathBuf::from(dir),
            top: top.map(OsString::from),
        };
        let name = OsStr::new("src");
        let dst = Path::new("/srv/dst");
        use Kind::*;
        // A file.
        assert_eq!(
            plan(false, name, false, dst, None, false).unwrap(),
            p("/srv", Some("dst"))
        );
        assert!(
            plan(false, name, false, dst, None, true).is_err(),
            "a missing DIR/"
        );
        assert_eq!(
            plan(false, name, false, dst, Some(Other), false).unwrap(),
            p("/srv", Some("dst"))
        );
        // A device is never replaced by the copy.
        let err = plan(
            false,
            name,
            false,
            Path::new("/dev/null"),
            Some(Device),
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("got a device"), "{err}");
        assert!(
            kind_in(open_dir(Path::new("/")).as_fd(), Path::new("dev/null"))
                .unwrap()
                .is_some_and(|k| k == Device)
        );
        assert!(plan(false, name, false, dst, Some(Other), true).is_err());
        assert_eq!(
            plan(false, name, false, dst, Some(Directory), false).unwrap(),
            p("/srv/dst", Some("src"))
        );
        // A directory.
        assert_eq!(
            plan(true, name, false, dst, None, false).unwrap(),
            p("/srv", Some("dst"))
        );
        assert_eq!(
            plan(true, name, true, dst, None, false).unwrap(),
            p("/srv", Some("dst"))
        );
        assert_eq!(
            plan(true, name, false, dst, Some(Directory), false).unwrap(),
            p("/srv/dst", Some("src"))
        );
        assert_eq!(
            plan(true, name, true, dst, Some(Directory), false).unwrap(),
            p("/srv/dst", None)
        );
        assert!(plan(true, name, false, dst, Some(Other), false).is_err());
        // Relative local names.
        assert_eq!(
            plan(false, name, false, Path::new("out"), None, false).unwrap(),
            p(".", Some("out"))
        );
        assert_eq!(split_source("/etc/."), ("/etc".to_string(), true));
        assert_eq!(split_source("/."), ("/".to_string(), true));
        assert_eq!(split_source("."), (".".to_string(), true));
        assert_eq!(split_source("/etc"), ("/etc".to_string(), false));
        // As filepath.Base reads the last element, trailing slashes left out.
        assert_eq!(split_source("/etc/./"), ("/etc".to_string(), true));
        assert_eq!(split_source("/etc/.//"), ("/etc".to_string(), true));
        assert_eq!(split_source("./"), (".".to_string(), true));
        assert_eq!(split_source("/etc/"), ("/etc/".to_string(), false));
        assert_eq!(
            split_leaf(Path::new("etc/hosts")),
            (PathBuf::from("etc"), OsString::from("hosts"))
        );
        assert_eq!(
            split_leaf(Path::new("")),
            (PathBuf::new(), OsString::from("."))
        );
        // A path that ends in .. is the directory it leads to, not the machine's root.
        assert_eq!(
            split_leaf(Path::new("srv/data/..")),
            (PathBuf::from("srv/data/.."), OsString::from("."))
        );
    }

    fn tree(root: &Path) {
        fs::create_dir_all(root.join("src/sub")).unwrap();
        fs::write(root.join("src/a.txt"), "alpha").unwrap();
        fs::set_permissions(root.join("src/a.txt"), fs::Permissions::from_mode(0o640)).unwrap();
        fs::write(root.join("src/sub/b.bin"), vec![7u8; 70_000]).unwrap();
        std::os::unix::fs::symlink("a.txt", root.join("src/rel")).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", root.join("src/abs")).unwrap();
        std::os::unix::fs::symlink("/nowhere", root.join("src/dangling")).unwrap();
        let sock = std::os::unix::net::UnixListener::bind(root.join("src/sock")).unwrap();
        drop(sock);
        let old = nix::sys::time::TimeVal::new(1_000_000, 0);
        nix::sys::stat::utimes(&root.join("src/a.txt"), &old, &old).unwrap();
    }

    #[test]
    fn a_tree_too_deep_is_refused_and_not_a_crash() {
        use nix::sys::stat::mkdirat;
        // As main does: the walk takes a descriptor per level.
        raise_open_files();
        let tmp = tempfile::tempdir().unwrap();
        let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC;
        fs::create_dir(tmp.path().join("deep")).unwrap();
        // deep/d/.../d, MAX_DEPTH - 1 directories below the top: as deep as the walk
        // goes. The last one stays open to add one level more.
        let mut last = open_dir(&tmp.path().join("deep"));
        for _ in 1..MAX_DEPTH {
            mkdirat(&last, "d", Mode::from_bits_truncate(0o755)).unwrap();
            last = openat(&last, "d", flags, Mode::empty()).unwrap();
        }
        let packing = |dir: PathBuf| {
            std::thread::Builder::new()
                .stack_size(COPY_STACK)
                .spawn(move || {
                    let parent = open_dir(&dir);
                    let top = Path::new("deep");
                    pack(
                        parent.as_fd(),
                        OsStr::new("deep"),
                        top,
                        Ids::Keep,
                        std::io::sink(),
                    )
                })
                .unwrap()
                .join()
                .unwrap()
        };
        let whole = packing(tmp.path().to_path_buf()).unwrap();
        assert_eq!(whole.entries, MAX_DEPTH as u64);
        mkdirat(&last, "d", Mode::from_bits_truncate(0o755)).unwrap();
        let e = packing(tmp.path().to_path_buf()).unwrap_err().to_string();
        assert!(e.contains("more than 2048 directories deep"), "{e}");
    }

    fn roundtrip(from: &Path, to: &Path, contents: bool, dst: &Path) -> Stats {
        let parent = open_dir(from);
        let mut stream = Vec::new();
        let packed = pack(
            parent.as_fd(),
            OsStr::new("src"),
            Path::new("src"),
            Ids::Keep,
            &mut stream,
        )
        .unwrap();
        assert!(
            packed.skipped.iter().any(|s| s.ends_with("sock")),
            "a socket is not copied"
        );
        let root = open_dir(to);
        let kind = kind_in(root.as_fd(), dst).unwrap();
        unpack(
            &stream[..],
            |is_dir, top| {
                assert!(is_dir);
                let plan = plan(is_dir, top, contents, dst, kind, false)?;
                Ok(Target {
                    root: root.try_clone().unwrap(),
                    base: plan.dir,
                    top: plan.top,
                })
            },
            Ids::Keep,
        )
        .unwrap()
    }

    #[test]
    fn a_stream_cut_short_leaves_the_file_that_was_there() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("f"), "old").unwrap();
        let mut stream = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(0o644);
        header.set_size(1000);
        stream
            .append_data(&mut header, "f", io::repeat(b'x').take(1000))
            .unwrap();
        let mut bytes = stream.into_inner().unwrap();
        // The header and half the data: the sender went away.
        bytes.truncate(512 + 500);
        let root = open_dir(tmp.path());
        let result = unpack(
            &bytes[..],
            |_, _| {
                Ok(Target {
                    root: root.try_clone().unwrap(),
                    base: PathBuf::new(),
                    top: Some("f".into()),
                })
            },
            Ids::Keep,
        );
        assert!(result.unwrap_err().to_string().contains("short"));
        assert_eq!(fs::read_to_string(tmp.path().join("f")).unwrap(), "old");
        assert_eq!(
            fs::read_dir(tmp.path()).unwrap().count(),
            1,
            "no staged file left"
        );
    }

    #[test]
    fn a_file_that_failed_to_read_never_takes_the_name() {
        struct Failing(usize);
        impl Read for Failing {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.0 == 0 {
                    return Err(io::Error::from_raw_os_error(nix::libc::EIO));
                }
                let n = buf.len().min(self.0);
                buf[..n].fill(b'a');
                self.0 -= n;
                Ok(n)
            }
        }
        // The sender fills the entry to its size and keeps the error.
        let mut exact = Exact {
            file: Failing(3),
            left: 10,
            failed: None,
        };
        let mut got = Vec::new();
        exact.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"aaa\0\0\0\0\0\0\0");
        assert!(exact.failed.is_some());
        // As pack writes it then: the whole entry, then a block that is no header.
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("f"), "old").unwrap();
        let mut stream = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(0o644);
        header.set_size(1000);
        stream
            .append_data(&mut header, "f", io::repeat(0).take(1000))
            .unwrap();
        stream.get_mut().extend_from_slice(&[0xff; 512]);
        let bytes = stream.into_inner().unwrap();
        let root = open_dir(tmp.path());
        let result = unpack(
            &bytes[..],
            |_, _| {
                Ok(Target {
                    root: root.try_clone().unwrap(),
                    base: PathBuf::new(),
                    top: Some("f".into()),
                })
            },
            Ids::Keep,
        );
        assert!(result.is_err());
        assert_eq!(fs::read_to_string(tmp.path().join("f")).unwrap(), "old");
        assert_eq!(
            fs::read_dir(tmp.path()).unwrap().count(),
            1,
            "no staged file left"
        );
    }

    #[test]
    fn a_directory_without_read_permission_keeps_its_time() {
        if nix::unistd::geteuid().is_root() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let mut stream = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Directory);
        header.set_mode(0o311);
        header.set_mtime(1_234_567);
        header.set_size(0);
        stream
            .append_data(&mut header, "drop", io::empty())
            .unwrap();
        let stream = stream.into_inner().unwrap();
        let root = open_dir(tmp.path());
        unpack(
            &stream[..],
            |_, _| {
                Ok(Target {
                    root: root.try_clone().unwrap(),
                    base: PathBuf::new(),
                    top: Some("drop".into()),
                })
            },
            Ids::Keep,
        )
        .unwrap();
        let meta = fs::metadata(tmp.path().join("drop")).unwrap();
        assert_eq!(meta.mtime(), 1_234_567);
        fs::set_permissions(tmp.path().join("drop"), fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn many_directories_keep_their_times() {
        let tmp = tempfile::tempdir().unwrap();
        let mut stream = tar::Builder::new(Vec::new());
        for path in std::iter::once("top".to_string()).chain((0..1500).map(|i| format!("top/d{i}")))
        {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Directory);
            header.set_mode(0o755);
            header.set_mtime(1_234_567);
            header.set_size(0);
            stream.append_data(&mut header, &path, io::empty()).unwrap();
        }
        let stream = stream.into_inner().unwrap();
        let root = open_dir(tmp.path());
        let stats = unpack(
            &stream[..],
            |_, _| {
                Ok(Target {
                    root: root.try_clone().unwrap(),
                    base: PathBuf::new(),
                    top: Some("top".into()),
                })
            },
            Ids::Keep,
        )
        .unwrap();
        assert_eq!(stats.entries, 1501);
        for dir in ["top", "top/d0", "top/d1499"] {
            assert_eq!(
                fs::metadata(tmp.path().join(dir)).unwrap().mtime(),
                1_234_567,
                "{dir}"
            );
        }
    }

    #[test]
    fn a_tree_goes_through_a_stream_as_it_was() {
        let tmp = tempfile::tempdir().unwrap();
        let from = tmp.path().join("from");
        let to = tmp.path().join("to");
        fs::create_dir_all(&to).unwrap();
        tree(&from);
        roundtrip(&from, &to, false, Path::new("copy"));
        let copy = to.join("copy");
        assert_eq!(fs::read_to_string(copy.join("a.txt")).unwrap(), "alpha");
        assert_eq!(fs::read(copy.join("sub/b.bin")).unwrap(), vec![7u8; 70_000]);
        let meta = fs::metadata(copy.join("a.txt")).unwrap();
        assert_eq!(meta.mode() & 0o7777, 0o640);
        assert_eq!(meta.mtime(), 1_000_000);
        assert_eq!(fs::read_link(copy.join("rel")).unwrap(), Path::new("a.txt"));
        assert_eq!(
            fs::read_link(copy.join("abs")).unwrap(),
            Path::new("/etc/passwd")
        );
        assert_eq!(
            fs::read_link(copy.join("dangling")).unwrap(),
            Path::new("/nowhere")
        );
        assert!(!copy.join("sock").exists());
        // Into an existing directory: nested under its own name.
        roundtrip(&from, &to, false, Path::new("copy"));
        assert!(to.join("copy/src/a.txt").exists());
        // Its contents: merged, a file replaced, a link replaced.
        fs::write(to.join("copy/a.txt"), "old").unwrap();
        roundtrip(&from, &to, true, Path::new("copy"));
        assert_eq!(fs::read_to_string(to.join("copy/a.txt")).unwrap(), "alpha");
    }

    #[test]
    fn a_stream_cannot_write_outside_its_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        fs::create_dir_all(root.join("dst")).unwrap();
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        // A link in the destination pointing out, absolute: scoped to the root.
        std::os::unix::fs::symlink(&outside, root.join("dst/out")).unwrap();
        let mirrored = root.join(outside.strip_prefix("/").unwrap());
        fs::create_dir_all(&mirrored).unwrap();
        let mut stream = Vec::new();
        {
            let mut b = tar::Builder::new(&mut stream);
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Directory);
            h.set_mode(0o755);
            h.set_size(0);
            b.append_data(&mut h, "top", io::empty()).unwrap();
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Regular);
            h.set_mode(0o644);
            h.set_size(1);
            b.append_data(&mut h, "top/x", &b"x"[..]).unwrap();
            b.finish().unwrap();
        }
        let fd = open_dir(&root);
        unpack(
            &stream[..],
            |_, _| {
                Ok(Target {
                    root: fd.try_clone().unwrap(),
                    base: PathBuf::from("dst/out"),
                    top: Some(OsString::from("top")),
                })
            },
            Ids::Keep,
        )
        .unwrap();
        assert!(
            !outside.join("top").exists(),
            "the link was followed out of the root"
        );
        assert!(
            mirrored.join("top/x").exists(),
            "the absolute link resolved inside the root instead"
        );

        // Paths that climb, and hard links, are refused.
        for (path, kind) in [
            ("../escape", tar::EntryType::Regular),
            ("top/link", tar::EntryType::Link),
        ] {
            let mut stream = Vec::new();
            {
                let mut b = tar::Builder::new(&mut stream);
                let mut h = tar::Header::new_gnu();
                h.set_entry_type(tar::EntryType::Directory);
                h.set_mode(0o755);
                h.set_size(0);
                b.append_data(&mut h, "top", io::empty()).unwrap();
                let mut h = tar::Header::new_gnu();
                h.set_entry_type(kind);
                h.set_mode(0o644);
                h.set_size(0);
                if kind == tar::EntryType::Link {
                    b.append_link(&mut h, path, "top").unwrap();
                } else {
                    // The tar crate refuses to write "..", so the name goes in raw.
                    h.as_old_mut().name[..path.len()].copy_from_slice(path.as_bytes());
                    h.set_cksum();
                    b.append(&h, io::empty()).unwrap();
                }
                b.finish().unwrap();
            }
            let result = unpack(
                &stream[..],
                |_, _| {
                    Ok(Target {
                        root: fd.try_clone().unwrap(),
                        base: PathBuf::from("dst"),
                        top: Some(OsString::from("top2")),
                    })
                },
                Ids::Keep,
            );
            assert!(result.is_err(), "{path}");
        }
        assert!(!tmp.path().join("escape").exists());
    }

    #[test]
    fn a_lookup_that_crossed_a_rename_is_tried_again() {
        let mut calls = 0;
        let answer = retry_on_eagain(|| {
            calls += 1;
            if calls < 3 {
                Err(Errno::EAGAIN)
            } else {
                Ok(calls)
            }
        });
        assert_eq!(answer, Ok(3));
        calls = 0;
        let answer: nix::Result<()> = retry_on_eagain(|| {
            calls += 1;
            Err(Errno::EAGAIN)
        });
        assert_eq!(answer, Err(Errno::EAGAIN));
        assert_eq!(calls, LOOKUP_ATTEMPTS);
        calls = 0;
        let answer: nix::Result<()> = retry_on_eagain(|| {
            calls += 1;
            Err(Errno::ENOENT)
        });
        assert_eq!(answer, Err(Errno::ENOENT));
        assert_eq!(calls, 1, "only EAGAIN is tried again");
    }

    #[test]
    fn lookups_stay_in_the_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        fs::create_dir_all(root.join("etc")).unwrap();
        fs::write(root.join("etc/hostname"), "inside").unwrap();
        std::os::unix::fs::symlink("/etc", root.join("etclink")).unwrap();
        let fd = open_dir(&root);
        match kind_in(fd.as_fd(), Path::new("etclink/hostname")) {
            Err(e) if e.to_string().contains("Linux 5.6") => return,
            other => assert_eq!(other.unwrap(), Some(Kind::Other)),
        }
        let file = scoped(fd.as_fd(), Path::new("etclink/hostname"), OFlag::O_RDONLY).unwrap();
        let mut text = String::new();
        File::from(file).read_to_string(&mut text).unwrap();
        assert_eq!(text, "inside", "an absolute link resolves inside the root");
        assert_eq!(
            kind_in(fd.as_fd(), Path::new("../../../etc")).unwrap(),
            Some(Kind::Directory),
            "climbing stops at the root"
        );
        assert_eq!(kind_in(fd.as_fd(), Path::new("nope")).unwrap(), None);
    }

    #[test]
    fn a_destination_link_stands_for_what_it_names() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        fs::create_dir_all(root.join("etc")).unwrap();
        fs::create_dir_all(root.join("run/systemd/resolve")).unwrap();
        fs::write(root.join("run/systemd/resolve/stub-resolv.conf"), "stub").unwrap();
        std::os::unix::fs::symlink(
            "../run/systemd/resolve/stub-resolv.conf",
            root.join("etc/resolv.conf"),
        )
        .unwrap();
        std::os::unix::fs::symlink("/nowhere/new", root.join("etc/dangling")).unwrap();
        std::os::unix::fs::symlink("loop", root.join("etc/loop")).unwrap();
        std::os::unix::fs::symlink("../../../../etc/resolv.conf", root.join("etc/up")).unwrap();
        // As docker's daemon gives LinkTarget: through the links on the way too.
        fs::create_dir_all(root.join("var/www/app/releases/5")).unwrap();
        fs::create_dir_all(root.join("var/www/app/shared/storage")).unwrap();
        std::os::unix::fs::symlink("releases/5", root.join("var/www/app/current")).unwrap();
        std::os::unix::fs::symlink(
            "../../shared/storage",
            root.join("var/www/app/releases/5/storage"),
        )
        .unwrap();
        let fd = open_dir(&root);
        let follow = |p: &str| destination_in(fd.as_fd(), Path::new(p));
        if matches!(follow("etc/resolv.conf"), Err(ref e) if e.to_string().contains("Linux 5.6")) {
            return;
        }
        assert_eq!(
            follow("etc/resolv.conf").unwrap(),
            Path::new("run/systemd/resolve/stub-resolv.conf")
        );
        // Created where it points, as docker does.
        assert_eq!(follow("etc/dangling").unwrap(), Path::new("nowhere/new"));
        assert_eq!(
            follow("etc/up").unwrap(),
            Path::new("run/systemd/resolve/stub-resolv.conf"),
            "climbing stops at the root"
        );
        assert_eq!(
            follow("var/www/app/current/storage").unwrap(),
            Path::new("var/www/app/shared/storage")
        );
        assert_eq!(follow("etc").unwrap(), Path::new("etc"));
        assert_eq!(follow("etc/absent").unwrap(), Path::new("etc/absent"));
        assert_eq!(follow("no/dir/x").unwrap(), Path::new("no/dir/x"));
        assert_eq!(follow("").unwrap(), Path::new(""));
        assert!(follow("etc/loop")
            .unwrap_err()
            .to_string()
            .contains("too many symlinks"));
        // On the host a relative path keeps the .. that leads out of it.
        let links = |p: &Path| {
            Ok(match p.to_str() {
                Some("a/link") => Some(PathBuf::from("../../b")),
                Some("abs") => Some(PathBuf::from("/srv/x")),
                _ => None,
            })
        };
        assert_eq!(
            follow_destination(Path::new("a/link"), false, links).unwrap(),
            Path::new("../b")
        );
        assert_eq!(
            follow_destination(Path::new("abs"), false, links).unwrap(),
            Path::new("/srv/x")
        );
        // Eleven links followed, as CopyInfoDestinationPath follows them; a twelfth is
        // too many.
        let chain = |length: usize| {
            move |p: &Path| {
                let n: usize = p
                    .to_str()
                    .unwrap()
                    .trim_start_matches('l')
                    .parse()
                    .unwrap_or(0);
                Ok((1..=length)
                    .contains(&n)
                    .then(|| PathBuf::from(format!("l{}", n + 1))))
            }
        };
        assert_eq!(
            follow_destination(Path::new("l1"), false, chain(11)).unwrap(),
            Path::new("l12")
        );
        assert!(follow_destination(Path::new("l1"), false, chain(12))
            .unwrap_err()
            .to_string()
            .contains("too many symlinks"));
    }

    #[test]
    fn owners_are_shifted_on_the_way_out() {
        assert_eq!(Ids::Keep.outward(1000), 1000);
        assert_eq!(Ids::Shift(100_000).outward(100_000), 0);
        assert_eq!(Ids::Shift(100_000).outward(100_033), 33);
        assert_eq!(Ids::Shift(100_000).outward(0), 65534);
        assert_eq!(Ids::Shift(100_000).outward(100_000 + 65536), 65534);
    }
}
