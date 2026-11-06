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
use nix::sys::stat::{fchmod, fstat, fstatat, futimens, mkdirat, utimensat, Mode, SFlag};
use nix::sys::stat::{FileStat, UtimensatFlags};
use nix::sys::time::TimeSpec;
use nix::unistd::{fchown, fchownat, linkat, symlinkat, unlinkat, Gid, Uid, UnlinkatFlags};

use crate::api::{note, Context, FailureKind, Report};
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

/// The rules for the destination of a copy. `contents` is a source written `DIR/.`,
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

/// How many links a destination is followed through after the first, as
/// CopyInfoDestinationPath counts them.
const DESTINATION_LINKS: usize = 10;

/// How many links resolving a path inside a machine follows, as FollowSymlinkInScope.
const SCOPED_LINKS: usize = 255;

/// The destination of a copy on the command line's side: a final link stands for what it
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

/// The destination of a copy inside a machine: a final link stands for what it names,
/// resolved all the way inside the root as LinkTarget resolves it, links on the way
/// included, so the copy is written there, whether that exists or not,
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

    /// The host ID a file written for `id` inside the machine gets: the same with no
    /// shift, beyond the machine's 65536 IDs nobody's; none with `Keep`, which writes
    /// everything as whoever runs this.
    fn inward(self, id: u64) -> Option<u32> {
        match self {
            Ids::Keep => None,
            Ids::Shift(0) => Some(u32::try_from(id).unwrap_or(65534)),
            Ids::Shift(base) if id < 65536 => Some(base + id as u32),
            Ids::Shift(base) => Some(base + 65534),
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

/// `error`, a lookup's failure with `e`, as a path that is not there when it is one: the
/// text stays.
fn missing(e: Errno, error: anyhow::Error) -> anyhow::Error {
    if e == Errno::ENOENT {
        FailureKind::NoSuchPath.error(format!("{error:#}"))
    } else {
        error
    }
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
    pack_as(parent, leaf, top, Naming::Normalized, ids, out)
}

/// How the entries of a stream are named.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Naming {
    /// As the tar crate writes a path: `.` components left out, a directory without a
    /// slash after it.
    Normalized,
    /// Byte for byte as the top and the tree give them, a directory with a slash after
    /// it, as the engine names what it archives: a top named `.` makes `./` and
    /// `./NAME`, one named `/` makes `/` and `/NAME`, which the other way cannot write.
    Literal,
}

fn pack_as<W: Write>(
    parent: BorrowedFd<'_>,
    leaf: &OsStr,
    top: &Path,
    naming: Naming,
    ids: Ids,
    out: W,
) -> Result<Stats> {
    let mut builder = tar::Builder::new(out);
    let mut stats = Stats::default();
    let walk = Walk { naming, ids };
    if let Err(e) = pack_entry(&mut builder, parent, leaf, top, walk, &mut stats, 0) {
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

/// How a tree is packed: how its entries are named and whose they are.
#[derive(Debug, Clone, Copy)]
struct Walk {
    naming: Naming,
    ids: Ids,
}

fn pack_entry<W: Write>(
    builder: &mut tar::Builder<W>,
    dirfd: BorrowedFd<'_>,
    leaf: &OsStr,
    path: &Path,
    walk: Walk,
    stats: &mut Stats,
    depth: usize,
) -> Result<()> {
    let ids = walk.ids;
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
        append_entry(builder, &mut header, path, None, io::empty(), walk.naming)
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
            if let Err(e) = pack_entry(builder, dir.as_fd(), &name, &child, walk, stats, depth + 1)
            {
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
        append_entry(builder, &mut header, path, None, &mut data, walk.naming)
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
        let target = Path::new(&target);
        append_entry(
            builder,
            &mut header,
            path,
            Some(target),
            io::empty(),
            walk.naming,
        )
        .with_context(|| format!("writing {}", path.display()))?;
        stats.entries += 1;
    } else {
        stats.skipped.push(path.display().to_string());
    }
    Ok(())
}

/// The length of a header's name and link name fields.
const NAME_FIELD: usize = 100;

/// Appends an entry named `path`, with a link's target, as `naming` names it.
fn append_entry<W: Write, R: Read>(
    builder: &mut tar::Builder<W>,
    header: &mut tar::Header,
    path: &Path,
    link: Option<&Path>,
    data: R,
    naming: Naming,
) -> io::Result<()> {
    match (naming, link) {
        (Naming::Normalized, None) => builder.append_data(header, path, data),
        (Naming::Normalized, Some(target)) => builder.append_link(header, path, target),
        (Naming::Literal, link) => {
            let mut name = path.as_os_str().as_bytes().to_vec();
            if header.entry_type().is_dir() && !name.ends_with(b"/") {
                name.push(b'/');
            }
            if let Some(target) = link {
                let target = target.as_os_str().as_bytes();
                if target.len() > NAME_FIELD {
                    append_long(builder, tar::EntryType::GNULongLink, target)?;
                }
                set_field(&mut header.as_old_mut().linkname, target);
            }
            if name.len() > NAME_FIELD {
                append_long(builder, tar::EntryType::GNULongName, &name)?;
            }
            set_field(&mut header.as_old_mut().name, &name);
            header.set_cksum();
            builder.append(header, data)
        }
    }
}

/// A GNU entry that carries the whole name or link target of the entry after it, whose
/// own field holds what fits.
fn append_long<W: Write>(
    builder: &mut tar::Builder<W>,
    kind: tar::EntryType,
    bytes: &[u8],
) -> io::Result<()> {
    let mut header = tar::Header::new_gnu();
    set_field(&mut header.as_old_mut().name, b"././@LongLink");
    header.set_mode(0o644);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_size(bytes.len() as u64 + 1);
    header.set_entry_type(kind);
    header.set_cksum();
    builder.append(&header, bytes.chain(&[0u8][..]))
}

fn set_field(field: &mut [u8], bytes: &[u8]) {
    field.fill(0);
    let n = bytes.len().min(field.len());
    field[..n].copy_from_slice(&bytes[..n]);
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
                let staged = Staged::new(parent, leaf, shown);
                let size = entry.header().entry_size().unwrap_or(0);
                stats.bytes += staged.write(&mut entry, size, owner, mode, mtime)?;
                pending = Some(staged);
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
    fn new(parent: OwnedFd, leaf: OsString, shown: PathBuf) -> Self {
        Staged {
            parent,
            staged: staged_leaf(),
            leaf,
            shown,
        }
    }

    /// Writes `size` bytes of `data` under the staged name, then gives the file its
    /// owner, mode and time; what it wrote goes again when that fails. Written beside its
    /// name and renamed over it once complete, so that a copy cut short leaves the file
    /// that was there, not half of the new one.
    fn write(
        &self,
        data: &mut dyn Read,
        size: u64,
        owner: Option<(Uid, Gid)>,
        mode: Mode,
        mtime: u64,
    ) -> Result<u64> {
        let shown = &self.shown;
        let file = openat(
            &self.parent,
            self.staged.as_os_str(),
            OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::from_bits_truncate(0o600),
        )
        .map_err(|e| nix_error(e, shown))?;
        let mut file = File::from(file);
        let written: Result<u64> = (|| {
            let bytes = io::copy(data, &mut file)
                .with_context(|| format!("writing {}", shown.display()))?;
            // A stream cut short ends an entry early without an error: the file that was
            // there stays.
            if bytes != size {
                bail!(
                    "{}: the stream ended {} bytes short",
                    shown.display(),
                    size - bytes.min(size)
                );
            }
            if let Some((uid, gid)) = owner {
                fchown(&file, Some(uid), Some(gid)).map_err(|e| nix_error(e, shown))?;
            }
            // After the chown, which clears setuid and setgid.
            fchmod(&file, mode).map_err(|e| nix_error(e, shown))?;
            futimens(&file, &timespec(mtime), &timespec(mtime)).map_err(|e| nix_error(e, shown))?;
            Ok(bytes)
        })();
        if written.is_err() {
            self.discard();
        }
        written
    }

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
    /// For a machine that does not run, where its volumes go, relative to the root as
    /// written and with the tree's links followed, and each volume's target: they are
    /// mounted only while it runs, and the tree has a bare directory there.
    pub unmounted: Vec<(PathBuf, String)>,
}

impl Root {
    /// Refuses `relative` (to the root, as given) when it is at or below a volume the
    /// machine mounts only while it runs.
    fn refuse_unmounted(&self, label: &str, relative: &Path) -> Result<()> {
        if self.unmounted.is_empty() {
            return Ok(());
        }
        let resolved = resolve_in(self.fd.as_fd(), relative).ok();
        let covered = self.unmounted.iter().find(|(at, _)| {
            relative.starts_with(at) || resolved.as_ref().is_some_and(|r| r.starts_with(at))
        });
        match covered {
            Some((_, target)) => Err(FailureKind::Invalid.error(format!(
                "{label}: the volume at {target} is mounted only while the machine runs; start it first"
            ))),
            None => Ok(()),
        }
    }
}

/// Where the volumes of a machine that does not run go in its tree (`Root::unmounted`).
fn unmounted_volumes(
    root: BorrowedFd<'_>,
    volumes: &[crate::volume::Volume],
) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    for volume in volumes {
        let written = inside(&volume.target);
        if let Ok(resolved) = resolve_in(root, &written) {
            if resolved != written {
                out.push((resolved, volume.target.clone()));
            }
        }
        out.push((written, volume.target.clone()));
    }
    out
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
            return Ok(Root {
                fd,
                base: seen,
                unmounted: Vec::new(),
            });
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
        return Ok(Root {
            fd,
            base,
            unmounted: Vec::new(),
        });
    }
    let record = store.load_image(name)?.ok_or_else(|| {
        crate::api::FailureKind::NoSuchMachine.error(format!("no machine or image named {name}"))
    })?;
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
    let unmounted = unmounted_volumes(fd.as_fd(), &record.volumes);
    Ok(Root {
        fd,
        base,
        unmounted,
    })
}

/// CopyFrom: `path` of the machine, packed into `out`; its top entry named `entry_name`
/// when one is given (`check_entry_name`), else as `cp` names it.
pub async fn copy_from(
    ctx: &Context,
    name: &str,
    path: &str,
    entry_name: Option<&str>,
    out: OwnedFd,
    report: Report<'_>,
) -> Result<Stats> {
    let root = open_root(ctx, name, report).await?;
    let (source, _) = split_source(path);
    let relative = inside(&source);
    root.refuse_unmounted(&format!("{name}:{source}"), &relative)?;
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
        let dir = scoped_raw(
            root.fd.as_fd(),
            &relative,
            OFlag::O_PATH | OFlag::O_DIRECTORY,
        )
        .map_err(|e| missing(e, anyhow!("{name}:{source}: {:#}", nix_error(e, &relative))))?;
        (dir, OsString::from("."))
    } else {
        let parent = scoped_raw(
            root.fd.as_fd(),
            &parent_path,
            OFlag::O_PATH | OFlag::O_DIRECTORY,
        )
        .map_err(|e| {
            missing(
                e,
                anyhow!("{name}:{source}: {:#}", nix_error(e, &parent_path)),
            )
        })?;
        (parent, leaf)
    };
    if let Err(e) = fstatat(&parent, leaf.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW) {
        let error = nix_error(e, Path::new(&format!("{name}:{source}")));
        return Err(missing(e, anyhow!("{error}")));
    }
    let (top, naming) = match entry_name {
        Some(given) => (OsString::from(given), Naming::Literal),
        None => (top, Naming::Normalized),
    };
    let base = root.base;
    let stats = on_thread(move || {
        pack_as(
            parent.as_fd(),
            &leaf,
            Path::new(&top),
            naming,
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
    root.refuse_unmounted(&format!("{name}:{path}"), &relative)?;
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

/// A name CopyFrom's option `name` gives the top entry: `.`, `/`, or one name of a path
/// that is neither `..` nor holds a slash or NUL.
pub fn check_entry_name(name: &str) -> Result<()> {
    let plain = !name.is_empty() && name != ".." && !name.contains(['/', '\0']);
    if plain || name == "." || name == "/" {
        return Ok(());
    }
    Err(FailureKind::Invalid.error(format!(
        "option name {name:?}: expected one name of a path, . or /"
    )))
}

/// What StatPath says of a path inside a machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathStat {
    /// The path's last element (`base_name`).
    pub name: String,
    pub size: u64,
    /// st_mode: the file's type and permissions.
    pub mode: u32,
    /// Unix nanoseconds.
    pub mtime: i64,
    /// Where a link leads inside the machine, every link on the way followed, as an
    /// absolute path; empty for anything but a link.
    pub link_target: String,
}

/// The last element of a path as filepath.Base reads it: slashes at the end do not
/// count, the root is `/` and an empty path `.`.
pub fn base_name(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".to_string();
    }
    trimmed.rsplit('/').next().unwrap_or(trimmed).to_string()
}

/// `path` beneath `root` as lstat sees it there: its last component is not followed,
/// unless the path ends in a slash, `.` or `..`, which the kernel resolves through it.
/// `label` names the path in errors.
pub fn stat_in(root: BorrowedFd<'_>, path: &str, label: &str) -> Result<PathStat> {
    let relative = inside(path);
    let shown = Path::new(label);
    let last = path.rsplit('/').next().unwrap_or_default();
    let st = if matches!(last, "" | "." | "..") {
        let fd = scoped_raw(root, &relative, OFlag::O_PATH)
            .map_err(|e| missing(e, nix_error(e, shown)))?;
        fstat(&fd)
    } else {
        let (parent, leaf) = split_leaf(&relative);
        let dir = scoped_raw(root, &parent, OFlag::O_PATH | OFlag::O_DIRECTORY)
            .map_err(|e| missing(e, nix_error(e, shown)))?;
        fstatat(&dir, leaf.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW)
    }
    .map_err(|e| missing(e, nix_error(e, shown)))?;
    let link_target = if is(st.st_mode, SFlag::S_IFLNK) {
        let resolved = resolve_in(root, &relative).map_err(|e| anyhow!("{label}: {e:#}"))?;
        Path::new("/").join(resolved).display().to_string()
    } else {
        String::new()
    };
    Ok(PathStat {
        name: base_name(path),
        size: st.st_size.max(0) as u64,
        mode: st.st_mode,
        mtime: mtime_nanos(&st),
        link_target,
    })
}

fn mtime_nanos(st: &FileStat) -> i64 {
    st.st_mtime
        .saturating_mul(1_000_000_000)
        .saturating_add(st.st_mtime_nsec)
}

/// StatPath: `path` of the machine (running, or a stopped overlay or flat one), not
/// followed when it is a link.
pub async fn stat_path(ctx: &Context, name: &str, path: &str) -> Result<PathStat> {
    if path.is_empty() {
        return Err(FailureKind::Invalid.error("an empty path"));
    }
    let quiet = |_: crate::api::Event| {};
    let root = open_root(ctx, name, &quiet).await?;
    let label = format!("{name}:{path}");
    root.refuse_unmounted(&label, &inside(path))?;
    let path = path.to_string();
    tokio::task::spawn_blocking(move || stat_in(root.fd.as_fd(), &path, &label))
        .await
        .context("the stat stopped")?
}

/// The owner USER[:GROUP] names inside a machine, as numbers, read as the engine reads
/// the user a copy's files get: each part a name of /etc/passwd or /etc/group or a
/// number; a user's group is the one passwd gives it, 0 for a number passwd does not
/// list, and a part left empty is root's.
pub fn resolve_owner(passwd: &str, group: &str, spec: &str) -> Result<(u32, u32)> {
    fn entries(text: &str) -> impl Iterator<Item = Vec<&str>> {
        text.lines()
            .filter(|line| !line.starts_with('#'))
            .map(|line| line.split(':').collect::<Vec<&str>>())
            .filter(|fields| fields.len() >= 3)
    }
    // As the engine tells a number from a name: one that fits a signed 32-bit integer.
    let number = |s: &str| s.parse::<u32>().ok().filter(|n| *n <= i32::MAX as u32);
    let (user, wanted_group) = match spec.split_once(':') {
        Some((user, group)) => (user, group),
        None => (spec, ""),
    };
    let (mut uid, mut gid) = (0, 0);
    if !user.is_empty() {
        let found = match number(user) {
            Some(n) => entries(passwd).find(|f| f.len() >= 4 && number(f[2]) == Some(n)),
            None => entries(passwd).find(|f| f.len() >= 4 && f[0] == user),
        };
        (uid, gid) = match (found, number(user)) {
            (Some(f), _) => (
                f[2].parse()
                    .map_err(|_| anyhow!("user {user}: a bad uid in /etc/passwd"))?,
                f[3].parse()
                    .map_err(|_| anyhow!("user {user}: a bad gid in /etc/passwd"))?,
            ),
            (None, Some(n)) => (n, 0),
            (None, None) => bail!("no user {user} in the machine's /etc/passwd"),
        };
    }
    if !wanted_group.is_empty() {
        gid = match number(wanted_group) {
            Some(n) => n,
            None => entries(group)
                .find(|f| f[0] == wanted_group)
                .and_then(|f| f[2].parse().ok())
                .ok_or_else(|| anyhow!("no group {wanted_group} in the machine's /etc/group"))?,
        };
    }
    Ok((uid, gid))
}

/// A text file of the machine, read through its root; empty when it is not there. The
/// machine may have put a fifo or a terminal there: only a regular file is read, and
/// opening never waits for a writer.
fn text_in(root: BorrowedFd<'_>, path: &str) -> Result<String> {
    const MAX: u64 = 16 << 20;
    let flags = OFlag::O_RDONLY | OFlag::O_NONBLOCK | OFlag::O_NOCTTY;
    let file = match scoped_raw(root, Path::new(path), flags) {
        Ok(fd) => File::from(fd),
        Err(Errno::ENOENT) => return Ok(String::new()),
        Err(e) => return Err(nix_error(e, Path::new(path))),
    };
    let st = fstat(&file).map_err(|e| nix_error(e, Path::new(path)))?;
    if !is(st.st_mode, SFlag::S_IFREG) {
        return Err(FailureKind::Invalid.error(format!("/{path} is not a regular file")));
    }
    let mut bytes = Vec::new();
    file.take(MAX)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading /{path}"))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Why the machine's settings keep a copy from writing in `dir` (relative to its root):
/// a volume mounted read-only over it, or the read-only root where no volume is.
fn read_only_setting(
    volumes: &[crate::volume::Volume],
    root_read_only: bool,
    dir: &Path,
) -> Option<String> {
    let at = Path::new("/").join(dir);
    let volume = volumes
        .iter()
        .filter(|v| at.starts_with(&v.target))
        .max_by_key(|v| Path::new(&v.target).components().count());
    match volume {
        Some(v) if v.read_only => Some(format!("the volume at {} is mounted read-only", v.target)),
        Some(_) => None,
        None if root_read_only => Some("the machine's root is read-only".to_string()),
        None => None,
    }
}

/// CopyTo's extract, checked before any of its stream is read: the directory it writes
/// in, and the owner its files get.
pub struct Extraction {
    root: Root,
    /// Relative to the root, every link followed.
    dir: PathBuf,
    label: String,
    owner: Option<(u32, u32)>,
    no_overwrite_dir_non_dir: bool,
}

/// Checks an extract at `path` of the machine: an existing directory once its links are
/// followed (`NoSuchPath` when there is nothing, `Invalid` for anything else) that
/// neither the machine's settings nor its mounts keep read-only. `owner`, USER[:GROUP]
/// of the machine (`resolve_owner`), is every file's owner when given and not empty.
pub async fn prepare_extract(
    ctx: &Context,
    name: &str,
    path: &str,
    owner: Option<&str>,
    no_overwrite_dir_non_dir: bool,
    report: Report<'_>,
) -> Result<Extraction> {
    if path.is_empty() {
        return Err(FailureKind::Invalid.error("an empty path"));
    }
    let root = open_root(ctx, name, report).await?;
    let label = format!("{name}:{path}");
    let fd = root.fd.as_fd();
    let dir = resolve_in(fd, &inside(path)).map_err(|e| anyhow!("{label}: {e:#}"))?;
    root.refuse_unmounted(&label, &dir)?;
    match kind_in(fd, &dir).map_err(|e| anyhow!("{label}: {e:#}"))? {
        Some(Kind::Directory) => {}
        Some(_) => {
            return Err(
                FailureKind::Invalid.error(format!("{label}: extraction point is not a directory"))
            )
        }
        None => {
            return Err(FailureKind::NoSuchPath.error(format!("{label}: no such file or directory")))
        }
    }
    if let Some(record) = ctx.store.load_image(name)? {
        if let Some(why) = read_only_setting(&record.volumes, record.tuning.read_only, &dir) {
            return Err(FailureKind::Invalid.error(format!("{label}: {why}")));
        }
    }
    let at = scoped(fd, &dir, OFlag::O_PATH | OFlag::O_DIRECTORY)?;
    let read_only = nix::sys::statvfs::fstatvfs(&at)
        .is_ok_and(|s| s.flags().contains(nix::sys::statvfs::FsFlags::ST_RDONLY));
    if read_only {
        return Err(FailureKind::Invalid.error(format!("{label}: read-only file system")));
    }
    let owner = match owner.filter(|o| !o.is_empty()) {
        Some(spec) => {
            // Off the service's workers: the files are the machine's, on whatever it
            // mounted there.
            let copy = root.fd.try_clone().context("keeping the machine's root")?;
            let (passwd, group) = tokio::task::spawn_blocking(move || {
                Ok::<_, anyhow::Error>((
                    text_in(copy.as_fd(), "etc/passwd")?,
                    text_in(copy.as_fd(), "etc/group")?,
                ))
            })
            .await
            .context("reading the machine's users")??;
            let ids = resolve_owner(&passwd, &group, spec)
                .map_err(|e| FailureKind::Invalid.error(format!("{label}: {e:#}")))?;
            Some(ids)
        }
        None => None,
    };
    Ok(Extraction {
        root,
        dir,
        label,
        owner,
        no_overwrite_dir_non_dir,
    })
}

impl Extraction {
    /// The stream from `input`, plain or compressed, written beneath the directory
    /// (`extract`).
    pub async fn run(self, input: OwnedFd, report: Report<'_>) -> Result<Stats> {
        let Extraction {
            root,
            dir,
            label,
            owner,
            no_overwrite_dir_non_dir,
        } = self;
        let base = root.base;
        let stats = on_thread(move || {
            if base != 0 {
                write_as(base)?;
            }
            let reader = decompressed(File::from(input))?;
            let options = ExtractOptions {
                owner,
                no_overwrite_dir_non_dir,
                unmounted: root.unmounted.clone(),
            };
            extract(reader, root.fd.as_fd(), &dir, Ids::Shift(base), options)
                .with_context(|| label.clone())
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
}

/// How `extract` writes over what is there.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtractOptions {
    /// Every entry's owner inside the machine, instead of the one its header gives.
    pub owner: Option<(u32, u32)>,
    /// A directory never takes the place of anything else, nor anything else of a
    /// directory.
    pub no_overwrite_dir_non_dir: bool,
    /// Where no entry may go: the volumes of a machine that does not run
    /// (`Root::unmounted`).
    pub unmounted: Vec<(PathBuf, String)>,
}

/// The stream as it came, or decompressed when its first bytes say gzip, bzip2, xz or
/// zstd, as the engine takes an archive to extract.
fn decompressed(mut input: File) -> Result<Box<dyn Read>> {
    use crate::build::untar::{detect, Compression};
    // Read whole, not as the first read of a pipe happens to cut it.
    let mut head = [0u8; 8];
    let mut got = 0;
    while got < head.len() {
        match input.read(&mut head[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e).context("reading the stream"),
        }
    }
    let kind = detect(&head[..got]);
    let reader = io::BufReader::new(io::Cursor::new(head[..got].to_vec()).chain(input));
    Ok(match kind {
        Compression::None => Box::new(reader),
        Compression::Gzip => Box::new(flate2::read::MultiGzDecoder::new(reader)),
        Compression::Bzip2 => Box::new(bzip2::read::MultiBzDecoder::new(reader)),
        Compression::Zstd => Box::new(
            zstd::stream::read::Decoder::with_buffer(reader).context("reading the stream")?,
        ),
        // Its decoder only writes: it runs on a thread of its own, into a pipe.
        Compression::Xz => {
            let (read, write) = nix::unistd::pipe2(OFlag::O_CLOEXEC).context("creating a pipe")?;
            let mut reader = reader;
            let decoder = std::thread::Builder::new()
                .name("nspawn-xz".to_string())
                .spawn(move || {
                    block_sigpipe();
                    let mut out = File::from(write);
                    lzma_rs::xz_decompress(&mut reader, &mut out).map_err(|e| format!("{e:?}"))
                })
                .context("starting the xz decoder")?;
            Box::new(Decoded {
                pipe: File::from(read),
                decoder: Some(decoder),
            })
        }
    })
}

/// What a decoder on a thread of its own writes; its failure ends the stream with an
/// error, never as a clean end.
struct Decoded {
    pipe: File,
    decoder: Option<std::thread::JoinHandle<std::result::Result<(), String>>>,
}

impl Read for Decoded {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.pipe.read(buf)?;
        if n == 0 {
            if let Some(decoder) = self.decoder.take() {
                match decoder.join() {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => return Err(io::Error::other(format!("xz: {e}"))),
                    Err(_) => return Err(io::Error::other("the xz decoder stopped")),
                }
            }
        }
        Ok(n)
    }
}

/// The names an entry's path takes beneath a directory, cleaned as filepath.Clean
/// cleans them: `.` gone, each `..` taking the name before it; None when a `..` would
/// climb above the directory. With `rooted`, a path that starts with a slash is rooted
/// at the directory, and `..` stops there.
fn beneath(path: &Path, rooted: bool) -> Option<Vec<OsString>> {
    let stops = rooted && path.has_root();
    let mut names: Vec<OsString> = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => names.push(name.to_os_string()),
            Component::ParentDir => {
                let above = names.pop().is_none();
                if above && !stops {
                    return None;
                }
            }
            _ => {}
        }
    }
    Some(names)
}

/// Whether a link at `parent` (names relative to the root) to `target` leads above `dir`
/// once joined and cleaned, as the engine refuses such a link: an absolute target never
/// does, since the engine joins it below the link's directory too.
fn link_leaves(dir: &Path, parent: &Path, target: &Path) -> bool {
    if target.has_root() {
        return false;
    }
    let floor: Vec<Component> = dir.components().collect();
    let mut at: Vec<Component> = parent.components().collect();
    for component in target.components() {
        match component {
            Component::ParentDir => {
                at.pop();
            }
            Component::Normal(_) => at.push(component),
            _ => {}
        }
    }
    !at.starts_with(&floor)
}

/// What an entry of a stream to extract makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Made {
    Directory,
    File,
    Symlink,
    HardLink,
}

/// Writes the entries of a tar stream beneath `dir` (relative to `root`, every link of
/// it followed) as the engine extracts an archive into a container: each entry by its
/// own name, a leading slash dropped and a name that climbs above `dir` refused, the
/// directories it implies made with mode 0755 and root's owner, owners and modes from the
/// headers (owners with `Ids::Shift`, the option's owner instead when given), what is
/// there replaced, a directory merged with one of the stream. Sockets, devices and fifos
/// are left out, as `unpack` leaves them.
pub fn extract<R: Read>(
    reader: R,
    root: BorrowedFd<'_>,
    dir: &Path,
    ids: Ids,
    options: ExtractOptions,
) -> Result<Stats> {
    let mut archive = tar::Archive::new(reader);
    let mut stats = Stats::default();
    let mut directories: Vec<(PathBuf, u64)> = Vec::new();
    let mut pending: Option<Staged> = None;
    let root_owner = ids
        .inward(0)
        .map(|id| (Uid::from_raw(id), Gid::from_raw(id)));
    let owner_of = |header: &tar::Header| -> Option<(Uid, Gid)> {
        let (uid, gid) = match options.owner {
            Some((uid, gid)) => (u64::from(uid), u64::from(gid)),
            None => (header.uid().unwrap_or(0), header.gid().unwrap_or(0)),
        };
        Some((
            Uid::from_raw(ids.inward(uid)?),
            Gid::from_raw(ids.inward(gid)?),
        ))
    };
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
        let kind = entry.header().entry_type();
        if kind.is_pax_global_extensions() {
            continue;
        }
        let raw = entry.path_bytes().into_owned();
        let name = Path::new(OsStr::from_bytes(&raw));
        let mut names = beneath(name, true).ok_or_else(|| {
            anyhow!(
                "{}: the name leads out of the directory it is extracted into",
                name.display()
            )
        })?;
        let Some(leaf) = names.pop() else {
            // The directory itself, which stays as it is.
            if kind.is_dir() {
                continue;
            }
            bail!(
                "{}: only a directory can stand for the directory the stream is extracted into",
                name.display()
            );
        };
        // An old header's type 0 is a directory when its name ends in a slash.
        let old_directory = entry.header().as_old().linkflag[0] == 0 && raw.ends_with(b"/");
        let made = match kind {
            tar::EntryType::Directory => Made::Directory,
            tar::EntryType::Regular if old_directory => Made::Directory,
            tar::EntryType::Regular | tar::EntryType::Continuous | tar::EntryType::GNUSparse => {
                Made::File
            }
            tar::EntryType::Symlink => Made::Symlink,
            tar::EntryType::Link => Made::HardLink,
            _ => {
                let at = dir.join(names.iter().collect::<PathBuf>()).join(&leaf);
                stats
                    .skipped
                    .push(Path::new("/").join(at).display().to_string());
                continue;
            }
        };
        let lexical = dir.join(names.iter().collect::<PathBuf>()).join(&leaf);
        if let Some((_, target)) = options
            .unmounted
            .iter()
            .find(|(at, _)| lexical.starts_with(at))
        {
            bail!(
                "{}: the volume at {target} is mounted only while the machine runs; start it first",
                Path::new("/").join(&lexical).display()
            );
        }
        let parent_path = implied_directories(root, dir, &names, root_owner)?;
        let shown = Path::new("/").join(parent_path.join(&leaf));
        if let Some((_, target)) = options
            .unmounted
            .iter()
            .find(|(at, _)| parent_path.join(&leaf).starts_with(at))
        {
            bail!(
                "{}: the volume at {target} is mounted only while the machine runs; start it first",
                shown.display()
            );
        }
        let parent = scoped(root, &parent_path, OFlag::O_PATH | OFlag::O_DIRECTORY)?;
        let existing = fstatat(&parent, leaf.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW)
            .ok()
            .map(|st| is(st.st_mode, SFlag::S_IFDIR));
        let is_dir = made == Made::Directory;
        if options.no_overwrite_dir_non_dir {
            match existing {
                Some(true) if !is_dir => bail!(
                    "cannot overwrite directory {} with a non-directory",
                    shown.display()
                ),
                Some(false) if is_dir => bail!(
                    "cannot overwrite non-directory {} with a directory",
                    shown.display()
                ),
                _ => {}
            }
        }
        match (existing, made) {
            (Some(true), Made::Directory) | (None, _) => {}
            // Renamed over it once written.
            (Some(false), Made::File) => {}
            (Some(true), _) => remove_tree(parent.as_fd(), &leaf, &shown, 0)?,
            (Some(false), _) => {
                match unlinkat(&parent, leaf.as_os_str(), UnlinkatFlags::NoRemoveDir) {
                    Ok(()) | Err(Errno::ENOENT) => {}
                    Err(e) => return Err(nix_error(e, &shown)),
                }
            }
        }
        let header = entry.header();
        let mode = Mode::from_bits_truncate(header.mode().unwrap_or(0o644) & 0o7777);
        let mtime = header.mtime().unwrap_or(0);
        let owner = owner_of(header);
        match made {
            Made::Directory => {
                if existing != Some(true) {
                    mkdirat(&parent, leaf.as_os_str(), Mode::from_bits_truncate(0o700))
                        .map_err(|e| nix_error(e, &shown))?;
                }
                let made_dir = openat(
                    &parent,
                    leaf.as_os_str(),
                    OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|e| nix_error(e, &shown))?;
                if let Some((uid, gid)) = owner {
                    fchown(&made_dir, Some(uid), Some(gid)).map_err(|e| nix_error(e, &shown))?;
                }
                fchmod(&made_dir, mode).map_err(|e| nix_error(e, &shown))?;
                directories.push((parent_path.join(&leaf), mtime));
            }
            Made::File => {
                let size = entry.size();
                let staged = Staged::new(parent, leaf, shown);
                stats.bytes += staged.write(&mut entry, size, owner, mode, mtime)?;
                pending = Some(staged);
            }
            Made::Symlink => {
                let target = entry
                    .link_name_bytes()
                    .ok_or_else(|| anyhow!("{}: a link without a target", shown.display()))?
                    .into_owned();
                let target = Path::new(OsStr::from_bytes(&target));
                if link_leaves(dir, &parent_path, target) {
                    bail!(
                        "{} -> {}: the link leads out of the directory it is extracted into",
                        shown.display(),
                        target.display()
                    );
                }
                symlinkat(target, &parent, leaf.as_os_str()).map_err(|e| nix_error(e, &shown))?;
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
                let _ = utimensat(
                    &parent,
                    leaf.as_os_str(),
                    &timespec(mtime),
                    &timespec(mtime),
                    UtimensatFlags::NoFollowSymlink,
                );
            }
            Made::HardLink => {
                let target = entry
                    .link_name_bytes()
                    .ok_or_else(|| anyhow!("{}: a link without a target", shown.display()))?
                    .into_owned();
                let target = Path::new(OsStr::from_bytes(&target));
                // Joined below the directory whatever it starts with, as the engine
                // joins it.
                let source = beneath(target, false)
                    .filter(|names| !names.is_empty())
                    .ok_or_else(|| {
                        anyhow!(
                            "{} => {}: the link leads out of the directory it is extracted into",
                            shown.display(),
                            target.display()
                        )
                    })?;
                let source = dir.join(source.iter().collect::<PathBuf>());
                let (source_parent, source_leaf) = split_leaf(&source);
                let from = scoped(root, &source_parent, OFlag::O_PATH | OFlag::O_DIRECTORY)?;
                linkat(
                    &from,
                    source_leaf.as_os_str(),
                    &parent,
                    leaf.as_os_str(),
                    AtFlags::empty(),
                )
                .map_err(|e| nix_error(e, &shown))?;
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
        }
        stats.entries += 1;
    }
    // The stream ended cleanly after it.
    if let Some(staged) = pending.take() {
        staged.finish()?;
    }
    // What follows the archive's end (the padding of a record) is read too, so that its
    // sender does not find the stream closed under it.
    let _ = io::copy(&mut archive.into_inner().take(1 << 20), &mut io::sink());
    // Last: writing into a directory changes its time. Set through its parent, which
    // needs no read permission on it.
    for (path, mtime) in directories.iter().rev() {
        let (parent, leaf) = match (path.parent(), path.file_name()) {
            (Some(parent), Some(leaf)) => (parent, leaf),
            _ => continue,
        };
        let flags = OFlag::O_PATH | OFlag::O_DIRECTORY;
        if let Ok(at) = scoped_raw(root, parent, flags) {
            let _ = utimensat(
                &at,
                leaf,
                &timespec(*mtime),
                &timespec(*mtime),
                UtimensatFlags::NoFollowSymlink,
            );
        }
    }
    Ok(stats)
}

/// The directory `names` lead to below `dir`, each one that is not there made with mode
/// 0755 and `owner`, as the engine makes the directories an archive implies without
/// carrying them.
fn implied_directories(
    root: BorrowedFd<'_>,
    dir: &Path,
    names: &[OsString],
    owner: Option<(Uid, Gid)>,
) -> Result<PathBuf> {
    let flags = OFlag::O_PATH | OFlag::O_DIRECTORY;
    let whole = dir.join(names.iter().collect::<PathBuf>());
    match scoped_raw(root, &whole, flags) {
        Ok(_) => return Ok(whole),
        Err(Errno::ENOENT) => {}
        Err(e) => return Err(nix_error(e, &Path::new("/").join(&whole))),
    }
    let mut path = dir.to_path_buf();
    for name in names {
        let next = path.join(name);
        let shown = Path::new("/").join(&next);
        match scoped_raw(root, &next, flags) {
            Ok(_) => {}
            Err(Errno::ENOENT) => {
                let at = scoped(root, &path, flags)?;
                mkdirat(&at, name.as_os_str(), Mode::from_bits_truncate(0o700))
                    .map_err(|e| nix_error(e, &shown))?;
                let made = openat(
                    &at,
                    name.as_os_str(),
                    OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|e| nix_error(e, &shown))?;
                if let Some((uid, gid)) = owner {
                    fchown(&made, Some(uid), Some(gid)).map_err(|e| nix_error(e, &shown))?;
                }
                fchmod(&made, Mode::from_bits_truncate(0o755)).map_err(|e| nix_error(e, &shown))?;
            }
            Err(e) => return Err(nix_error(e, &shown)),
        }
        path = next;
    }
    Ok(path)
}

/// Removes the directory `leaf` of `parent` and everything in it, for an entry of
/// another kind to take its place: links are removed, never followed, and a file system
/// mounted anywhere in it stops the removal there.
fn remove_tree(parent: BorrowedFd<'_>, leaf: &OsStr, shown: &Path, depth: usize) -> Result<()> {
    if depth >= MAX_DEPTH {
        bail!(
            "{}: more than {MAX_DEPTH} directories deep; not removed",
            shown.display()
        );
    }
    let dir = retry_on_eagain(|| {
        openat2(
            parent,
            leaf,
            OpenHow::new()
                .flags(OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC)
                .resolve(
                    ResolveFlag::RESOLVE_BENEATH
                        | ResolveFlag::RESOLVE_NO_XDEV
                        | ResolveFlag::RESOLVE_NO_SYMLINKS
                        | ResolveFlag::RESOLVE_NO_MAGICLINKS,
                ),
        )
    })
    .map_err(|e| match e {
        Errno::EXDEV => anyhow!(
            "{}: a file system is mounted there; not replacing it",
            shown.display()
        ),
        e => nix_error(e, shown),
    })?;
    let listing = dir
        .try_clone()
        .context("duplicating a directory descriptor")?;
    let mut names: Vec<OsString> = Vec::new();
    for entry in nix::dir::Dir::from_fd(listing)
        .map_err(|e| nix_error(e, shown))?
        .iter()
    {
        let entry = entry.map_err(|e| nix_error(e, shown))?;
        let name = entry.file_name().to_bytes();
        if name != b"." && name != b".." {
            names.push(OsStr::from_bytes(name).to_os_string());
        }
    }
    for name in names {
        let child = shown.join(&name);
        match fstatat(&dir, name.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(st) if is(st.st_mode, SFlag::S_IFDIR) => {
                remove_tree(dir.as_fd(), &name, &child, depth + 1)?
            }
            Ok(_) => match unlinkat(&dir, name.as_os_str(), UnlinkatFlags::NoRemoveDir) {
                Ok(()) | Err(Errno::ENOENT) => {}
                Err(e) => return Err(nix_error(e, &child)),
            },
            Err(Errno::ENOENT) => {}
            Err(e) => return Err(nix_error(e, &child)),
        }
    }
    match unlinkat(parent, leaf, UnlinkatFlags::RemoveDir) {
        Ok(()) | Err(Errno::ENOENT) => Ok(()),
        Err(e) => Err(nix_error(e, shown)),
    }
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
        // LinkTarget goes through the links on the way too.
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
        // Created where it points.
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

    #[test]
    fn owners_are_shifted_on_the_way_in() {
        assert_eq!(Ids::Keep.inward(1000), None);
        assert_eq!(Ids::Shift(0).inward(1000), Some(1000));
        assert_eq!(Ids::Shift(0).inward(u64::from(u32::MAX) + 1), Some(65534));
        assert_eq!(Ids::Shift(100_000).inward(0), Some(100_000));
        assert_eq!(Ids::Shift(100_000).inward(33), Some(100_033));
        assert_eq!(Ids::Shift(100_000).inward(70_000), Some(165_534));
    }

    /// An entry written as a client writes it, its name and link name byte for byte:
    /// absolute names and type `\0` included, which the tar crate itself never writes.
    fn raw_entry(
        stream: &mut tar::Builder<Vec<u8>>,
        name: &str,
        kind: u8,
        link: &str,
        data: &[u8],
    ) {
        let mut h = tar::Header::new_gnu();
        h.set_mode(if kind == b'5' { 0o750 } else { 0o640 });
        h.set_uid(u64::from(nix::unistd::getuid().as_raw()));
        h.set_gid(u64::from(nix::unistd::getgid().as_raw()));
        h.set_mtime(1_000_000);
        h.set_size(data.len() as u64);
        h.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
        h.as_old_mut().linkname[..link.len()].copy_from_slice(link.as_bytes());
        h.as_old_mut().linkflag[0] = kind;
        h.set_cksum();
        stream.append(&h, data).unwrap();
    }

    fn extracted(root: &Path, dir: &str, stream: &[u8], options: ExtractOptions) -> Result<Stats> {
        let fd = open_dir(root);
        extract(stream, fd.as_fd(), Path::new(dir), Ids::Keep, options)
    }

    fn stream_of(entries: &[(&str, u8, &str, &[u8])]) -> Vec<u8> {
        let mut stream = tar::Builder::new(Vec::new());
        for (name, kind, link, data) in entries {
            raw_entry(&mut stream, name, *kind, link, data);
        }
        stream.into_inner().unwrap()
    }

    #[test]
    fn users_are_read_from_a_regular_file_only() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("etc")).unwrap();
        let fd = open_dir(root);
        assert_eq!(text_in(fd.as_fd(), "etc/passwd").unwrap(), "");
        fs::write(root.join("etc/group"), "app:x:7:\n").unwrap();
        assert_eq!(text_in(fd.as_fd(), "etc/group").unwrap(), "app:x:7:\n");
        // A fifo with no writer would hold an open for reading forever.
        nix::unistd::mkfifo(&root.join("etc/passwd"), Mode::from_bits_truncate(0o600)).unwrap();
        let e = text_in(fd.as_fd(), "etc/passwd").unwrap_err();
        assert_eq!(crate::api::failure::kind_of(&e), Some(FailureKind::Invalid));
        assert!(
            e.to_string().contains("/etc/passwd is not a regular file"),
            "{e}"
        );
    }

    #[test]
    fn nothing_goes_where_a_volume_is_mounted_only_while_the_machine_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("data")).unwrap();
        fs::create_dir_all(root.join("srv")).unwrap();
        std::os::unix::fs::symlink("/data", root.join("link")).unwrap();
        let fd = open_dir(root);
        let volume = |target: &str| crate::volume::Volume {
            source: "v".to_string(),
            target: target.to_string(),
            read_only: false,
        };
        let unmounted = unmounted_volumes(fd.as_fd(), &[volume("/data"), volume("/link/sub")]);
        assert!(unmounted.contains(&(PathBuf::from("data"), "/data".to_string())));
        // A target written through a link of the tree goes where the link leads.
        assert!(unmounted.contains(&(PathBuf::from("data/sub"), "/link/sub".to_string())));
        let options = ExtractOptions {
            unmounted: unmounted.clone(),
            ..ExtractOptions::default()
        };
        for name in ["data/f", "data/", "srv/../data/f"] {
            let kind = if name.ends_with('/') { b'5' } else { b'0' };
            let e = extracted(
                root,
                "",
                &stream_of(&[(name, kind, "", b"x")]),
                options.clone(),
            )
            .unwrap_err()
            .to_string();
            assert!(
                e.contains("the volume at /data is mounted only while"),
                "{name}: {e}"
            );
        }
        assert!(!root.join("data/f").exists());
        extracted(root, "", &stream_of(&[("srv/f", b'0', "", b"x")]), options).unwrap();
        assert!(root.join("srv/f").exists());
        let checked = Root {
            fd: open_dir(root),
            base: 0,
            unmounted,
        };
        for path in ["data", "data/x", "link/x"] {
            let e = checked
                .refuse_unmounted("web:/x", Path::new(path))
                .unwrap_err();
            assert_eq!(
                crate::api::failure::kind_of(&e),
                Some(FailureKind::Invalid),
                "{path}"
            );
        }
        for path in ["", "srv", "database"] {
            checked.refuse_unmounted("web:/x", Path::new(path)).unwrap();
        }
    }

    #[test]
    fn an_extract_writes_each_entry_by_its_own_name() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("dst")).unwrap();
        // Many top names, a directory the stream implies without carrying it, and a type
        // `\0` file named absolutely, as Testcontainers writes one.
        let stream = stream_of(&[
            ("a.txt", b'0', "", b"alpha"),
            ("b/", b'5', "", b""),
            ("b/c.txt", b'0', "", b"gamma"),
            ("d/e/f.txt", b'0', "", b"phi"),
            ("/abs/x", 0, "", b"x"),
            ("./dot", b'0', "", b"."),
            ("olddir/", 0, "", b""),
            ("/../up", b'0', "", b"rooted"),
            ("rel", b'2', "a.txt", b""),
            ("hard", b'1', "b/c.txt", b""),
            ("fifo", b'6', "", b""),
        ]);
        let stats = extracted(root, "dst", &stream, ExtractOptions::default()).unwrap();
        let dst = root.join("dst");
        assert_eq!(fs::read_to_string(dst.join("a.txt")).unwrap(), "alpha");
        assert_eq!(fs::read_to_string(dst.join("b/c.txt")).unwrap(), "gamma");
        assert_eq!(fs::read_to_string(dst.join("d/e/f.txt")).unwrap(), "phi");
        assert_eq!(fs::read_to_string(dst.join("abs/x")).unwrap(), "x");
        assert_eq!(fs::read_to_string(dst.join("dot")).unwrap(), ".");
        assert!(dst.join("olddir").is_dir());
        assert_eq!(fs::read_to_string(dst.join("up")).unwrap(), "rooted");
        assert_eq!(fs::read_link(dst.join("rel")).unwrap(), Path::new("a.txt"));
        let (c, hard) = (
            fs::metadata(dst.join("b/c.txt")).unwrap(),
            fs::metadata(dst.join("hard")).unwrap(),
        );
        assert_eq!(c.ino(), hard.ino(), "a hard link to what the stream wrote");
        // The modes and times of the headers; implied directories get 0755.
        let a = fs::metadata(dst.join("a.txt")).unwrap();
        assert_eq!(a.mode() & 0o7777, 0o640);
        assert_eq!(a.mtime(), 1_000_000);
        assert_eq!(fs::metadata(dst.join("b")).unwrap().mode() & 0o7777, 0o750);
        assert_eq!(fs::metadata(dst.join("b")).unwrap().mtime(), 1_000_000);
        assert_eq!(fs::metadata(dst.join("d")).unwrap().mode() & 0o7777, 0o755);
        assert_eq!(
            fs::metadata(dst.join("d/e")).unwrap().mode() & 0o7777,
            0o755
        );
        assert!(!dst.join("fifo").exists());
        assert_eq!(stats.skipped, vec!["/dst/fifo".to_string()]);
        assert_eq!(stats.entries, 10);
        assert!(!root.join("up").exists());
    }

    #[test]
    fn an_extract_at_the_root_takes_absolute_names() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // A directory made at /, as a client asks for one before it copies into it; the
        // root itself, as `.` or `/`, stays as it is.
        let stream = stream_of(&[
            ("./", b'5', "", b""),
            ("/", b'5', "", b""),
            ("/var/run/act/", b'5', "", b""),
            ("/run/secrets/docker/config.json", b'0', "", b"{}"),
        ]);
        let mode = fs::metadata(root).unwrap().mode();
        extracted(root, "", &stream, ExtractOptions::default()).unwrap();
        assert!(root.join("var/run/act").is_dir());
        assert_eq!(
            fs::read_to_string(root.join("run/secrets/docker/config.json")).unwrap(),
            "{}"
        );
        assert_eq!(fs::metadata(root).unwrap().mode(), mode);
        // Nothing but a directory names the directory itself.
        let file = stream_of(&[("./", b'0', "", b"")]);
        assert!(extracted(root, "", &file, ExtractOptions::default()).is_err());
        // An empty stream writes nothing and is no error.
        let empty = tar::Builder::new(Vec::new()).into_inner().unwrap();
        assert_eq!(
            extracted(root, "", &empty, ExtractOptions::default())
                .unwrap()
                .entries,
            0
        );
        assert!(extracted(root, "", &[], ExtractOptions::default()).is_ok());
    }

    #[test]
    fn an_extract_never_leaves_its_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        fs::create_dir_all(root.join("srv/dst")).unwrap();
        let refused = [
            ("../escape", b'0', "", &b"x"[..]),
            ("a/../../escape", b'0', "", b"x"),
            // A link that leads above the directory, relative, and a hard link from
            // above it.
            ("up", b'2', "../../etc", b""),
            ("sub/up", b'2', "../../escape", b""),
            ("hard", b'1', "../escape", b""),
            ("hard", b'1', "/../escape", b""),
        ];
        for entry in refused {
            let stream = stream_of(&[entry]);
            let e = extracted(&root, "srv/dst", &stream, ExtractOptions::default())
                .expect_err(entry.0)
                .to_string();
            assert!(e.contains("leads out of the directory"), "{e}");
        }
        assert!(!root.join("srv/escape").exists());
        assert!(!tmp.path().join("escape").exists());
        // An absolute link is joined below the link's directory by the engine, and a
        // relative one that climbs and comes back stays inside: both are written.
        let stream = stream_of(&[
            ("abs", b'2', "/etc", b""),
            ("back", b'2', "../dst/abs", b""),
        ]);
        extracted(&root, "srv/dst", &stream, ExtractOptions::default()).unwrap();
        assert_eq!(
            fs::read_link(root.join("srv/dst/abs")).unwrap(),
            Path::new("/etc")
        );
        // A link written by the stream leads, for the entries after it, where the
        // machine resolves it: inside the root, never the host's /etc.
        fs::create_dir_all(root.join("etc")).unwrap();
        let stream = stream_of(&[
            ("into", b'2', "/etc", b""),
            ("into/written", b'0', "", b"inside"),
        ]);
        let fd = open_dir(&root);
        if matches!(kind_in(fd.as_fd(), Path::new("etc")), Err(ref e) if e.to_string().contains("Linux 5.6"))
        {
            return;
        }
        extracted(&root, "srv/dst", &stream, ExtractOptions::default()).unwrap();
        assert_eq!(
            fs::read_to_string(root.join("etc/written")).unwrap(),
            "inside"
        );
        assert!(!Path::new("/etc/written").exists());
    }

    #[test]
    fn a_directory_and_a_file_replace_each_other_unless_told_not_to() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let dst = root.join("dst");
        let reset = || {
            let _ = fs::remove_dir_all(&dst);
            fs::create_dir_all(dst.join("dir/deep/er")).unwrap();
            fs::write(dst.join("dir/deep/er/f"), "x").unwrap();
            std::os::unix::fs::symlink("/nowhere", dst.join("dir/link")).unwrap();
            fs::write(dst.join("file"), "old").unwrap();
        };
        let file_over_dir = stream_of(&[("dir", b'0', "", b"new")]);
        let dir_over_file = stream_of(&[("file/", b'5', "", b"")]);
        let keep = ExtractOptions {
            no_overwrite_dir_non_dir: true,
            ..ExtractOptions::default()
        };
        reset();
        let e = extracted(root, "dst", &file_over_dir, keep.clone())
            .unwrap_err()
            .to_string();
        assert!(e.contains("cannot overwrite directory /dst/dir"), "{e}");
        let e = extracted(root, "dst", &dir_over_file, keep.clone())
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("cannot overwrite non-directory /dst/file"),
            "{e}"
        );
        assert!(dst.join("dir/deep/er/f").exists());
        assert_eq!(fs::read_to_string(dst.join("file")).unwrap(), "old");
        // A file over a file, and a directory into a directory, are fine either way.
        let merge = stream_of(&[
            ("file", b'0', "", b"new"),
            ("dir/", b'5', "", b""),
            ("dir/more", b'0', "", b"m"),
        ]);
        extracted(root, "dst", &merge, keep).unwrap();
        assert_eq!(fs::read_to_string(dst.join("file")).unwrap(), "new");
        assert!(dst.join("dir/deep/er/f").exists() && dst.join("dir/more").exists());
        // The directory merged takes the header's mode, as the engine applies it.
        assert_eq!(
            fs::metadata(dst.join("dir")).unwrap().mode() & 0o7777,
            0o750
        );
        // Without the option what is there makes way, a whole tree included.
        reset();
        extracted(root, "dst", &file_over_dir, ExtractOptions::default()).unwrap();
        assert_eq!(fs::read_to_string(dst.join("dir")).unwrap(), "new");
        extracted(root, "dst", &dir_over_file, ExtractOptions::default()).unwrap();
        assert!(dst.join("file").is_dir());
        // A link over a directory, and a file over a link, which is replaced, not
        // followed.
        reset();
        let link_over = stream_of(&[("dir", b'2', "file", b""), ("dir2", b'0', "", b"")]);
        extracted(root, "dst", &link_over, ExtractOptions::default()).unwrap();
        assert_eq!(fs::read_link(dst.join("dir")).unwrap(), Path::new("file"));
        let file_over_link = stream_of(&[("dir", b'0', "", b"plain")]);
        extracted(root, "dst", &file_over_link, ExtractOptions::default()).unwrap();
        assert!(!fs::symlink_metadata(dst.join("dir")).unwrap().is_symlink());
        assert_eq!(fs::read_to_string(dst.join("file")).unwrap(), "old");
    }

    #[test]
    fn owners_come_from_the_headers_or_the_option() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("dst")).unwrap();
        let me = (
            nix::unistd::getuid().as_raw(),
            nix::unistd::getgid().as_raw(),
        );
        let fd = open_dir(root);
        let run = |stream: &[u8], owner: Option<(u32, u32)>| {
            extract(
                stream,
                fd.as_fd(),
                Path::new("dst"),
                Ids::Shift(0),
                ExtractOptions {
                    owner,
                    ..ExtractOptions::default()
                },
            )
        };
        // Headers that name this user: chowned to it, which any user may do.
        let mine = stream_of(&[("mine", b'0', "", b"m"), ("mylink", b'2', "mine", b"")]);
        run(&mine, None).unwrap();
        let meta = fs::metadata(root.join("dst/mine")).unwrap();
        assert_eq!((meta.uid(), meta.gid()), me);
        if nix::unistd::geteuid().is_root() {
            return;
        }
        // A header that names root: only root may hand a file to it, so the header was
        // read; the option takes its place.
        let mut stream = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Regular);
        h.set_mode(0o644);
        h.set_size(1);
        h.set_uid(0);
        h.set_gid(0);
        stream.append_data(&mut h, "roots", &b"r"[..]).unwrap();
        let stream = stream.into_inner().unwrap();
        assert!(run(&stream, None).is_err());
        assert!(!root.join("dst/roots").exists(), "the staged file went");
        run(&stream, Some(me)).unwrap();
        let meta = fs::metadata(root.join("dst/roots")).unwrap();
        assert_eq!((meta.uid(), meta.gid()), me);
    }

    #[test]
    fn the_owner_option_is_read_as_the_engine_reads_a_user() {
        let passwd = "root:x:0:0::/root:/bin/sh\n# comment\nwww-data:x:33:33::/var/www:/bin/false\napp:x:1000:1001::/:/bin/sh\n";
        let group = "root:x:0:\nwww-data:x:33:\nstaff:x:50:app\n";
        let owner = |spec: &str| resolve_owner(passwd, group, spec);
        assert_eq!(owner("www-data").unwrap(), (33, 33));
        assert_eq!(owner("app").unwrap(), (1000, 1001));
        assert_eq!(owner("33").unwrap(), (33, 33), "a uid passwd lists");
        assert_eq!(owner("12345").unwrap(), (12345, 0), "a uid it does not");
        assert_eq!(owner("app:staff").unwrap(), (1000, 50));
        assert_eq!(owner("1000:7").unwrap(), (1000, 7));
        assert_eq!(owner(":33").unwrap(), (0, 33));
        assert_eq!(owner("33:").unwrap(), (33, 33));
        assert!(owner("nobody-here")
            .unwrap_err()
            .to_string()
            .contains("no user nobody-here"));
        assert!(owner("app:nogroup")
            .unwrap_err()
            .to_string()
            .contains("no group nogroup"));
        // Too large for the engine's number: a name, which nobody has.
        assert!(owner("4294967295").is_err());
    }

    #[test]
    fn a_compressed_stream_is_extracted_too() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("dst")).unwrap();
        let plain = stream_of(&[("f", b'0', "", b"content")]);
        let gzip = {
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(&plain).unwrap();
            e.finish().unwrap()
        };
        let bzip2 = {
            let mut e = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
            e.write_all(&plain).unwrap();
            e.finish().unwrap()
        };
        let zstd = zstd::encode_all(&plain[..], 3).unwrap();
        let mut xz = Vec::new();
        lzma_rs::xz_compress(&mut &plain[..], &mut xz).unwrap();
        let fd = open_dir(root);
        for (kind, bytes) in [
            ("plain", plain.clone()),
            ("gzip", gzip),
            ("bzip2", bzip2),
            ("zstd", zstd),
            ("xz", xz.clone()),
        ] {
            let _ = fs::remove_file(root.join("dst/f"));
            let (read, write) = nix::unistd::pipe2(OFlag::O_CLOEXEC).unwrap();
            let writer = std::thread::spawn(move || File::from(write).write_all(&bytes));
            let reader = decompressed(File::from(read)).unwrap();
            extract(
                reader,
                fd.as_fd(),
                Path::new("dst"),
                Ids::Keep,
                ExtractOptions::default(),
            )
            .unwrap();
            writer.join().unwrap().unwrap();
            assert_eq!(
                fs::read_to_string(root.join("dst/f")).unwrap(),
                "content",
                "{kind}"
            );
        }
        // A broken xz stream is an error, not a clean end.
        xz.truncate(xz.len() / 2);
        let (read, write) = nix::unistd::pipe2(OFlag::O_CLOEXEC).unwrap();
        let writer = std::thread::spawn(move || File::from(write).write_all(&xz));
        let reader = decompressed(File::from(read)).unwrap();
        let result = extract(
            reader,
            fd.as_fd(),
            Path::new("dst"),
            Ids::Keep,
            ExtractOptions::default(),
        );
        let _ = writer.join();
        assert!(result.is_err());
    }

    #[test]
    fn names_beneath_a_directory_are_cleaned_as_the_engine_cleans_them() {
        let names = |p: &str, rooted: bool| {
            beneath(Path::new(p), rooted).map(|n| {
                n.iter()
                    .map(|s| s.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/")
            })
        };
        assert_eq!(names("a/./b/", true).as_deref(), Some("a/b"));
        assert_eq!(names("/a/b", true).as_deref(), Some("a/b"));
        assert_eq!(names("a/../b", true).as_deref(), Some("b"));
        assert_eq!(names("/../../b", true).as_deref(), Some("b"));
        assert_eq!(names("./", true).as_deref(), Some(""));
        assert_eq!(names("../b", true), None);
        assert_eq!(names("a/../../b", true), None);
        // A hard link's target is joined below the directory, leading slash or not.
        assert_eq!(names("/a", false).as_deref(), Some("a"));
        assert_eq!(names("/../a", false), None);
        let leaves = |parent: &str, target: &str| {
            link_leaves(Path::new("srv/dst"), Path::new(parent), Path::new(target))
        };
        assert!(!leaves("srv/dst", "a"));
        assert!(!leaves("srv/dst/sub", "../a"));
        assert!(!leaves("srv/dst", "../dst/a"));
        assert!(!leaves("srv/dst", "/etc"));
        assert!(leaves("srv/dst", "../a"));
        assert!(leaves("srv/dst/sub", "../../a"));
        assert!(leaves("srv/dst", ".."));
    }

    #[test]
    fn read_only_settings_keep_an_extract_out() {
        let volume = |target: &str, read_only: bool| crate::volume::Volume {
            source: "data".to_string(),
            target: target.to_string(),
            read_only,
        };
        let volumes = [volume("/data", false), volume("/data/ro", true)];
        let why = |dir: &str, root_ro: bool| read_only_setting(&volumes, root_ro, Path::new(dir));
        assert_eq!(why("srv", false), None);
        assert_eq!(
            why("srv", true).as_deref(),
            Some("the machine's root is read-only")
        );
        assert_eq!(
            why("data/x", true),
            None,
            "a writable volume over a read-only root"
        );
        assert_eq!(
            why("data/ro/x", false).as_deref(),
            Some("the volume at /data/ro is mounted read-only")
        );
        assert_eq!(
            why("data/rox", false),
            None,
            "a name that only starts the same"
        );
        assert_eq!(
            why("", true).as_deref(),
            Some("the machine's root is read-only")
        );
    }

    #[test]
    fn a_stat_says_what_lstat_says_inside_the_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        fs::create_dir_all(root.join("etc/sub")).unwrap();
        fs::write(root.join("etc/hosts"), "127.0.0.1 localhost\n").unwrap();
        fs::set_permissions(root.join("etc/hosts"), fs::Permissions::from_mode(0o640)).unwrap();
        let when = nix::sys::time::TimeVal::new(1_000_000, 250_000);
        nix::sys::stat::utimes(&root.join("etc/hosts"), &when, &when).unwrap();
        std::os::unix::fs::symlink("/etc/sub", root.join("etc/abs")).unwrap();
        std::os::unix::fs::symlink("hosts", root.join("etc/rel")).unwrap();
        std::os::unix::fs::symlink("/nowhere/x", root.join("dangling")).unwrap();
        let fd = open_dir(&root);
        let stat = |p: &str| stat_in(fd.as_fd(), p, &format!("web:{p}"));
        if matches!(stat("/etc"), Err(ref e) if e.to_string().contains("Linux 5.6")) {
            return;
        }
        let file = stat("/etc/hosts").unwrap();
        assert_eq!(file.name, "hosts");
        assert_eq!(file.size, 20);
        assert_eq!(file.mode, 0o100640);
        assert_eq!(file.mtime, 1_000_000_250_000_000);
        assert_eq!(file.link_target, "");
        let dir = stat("/etc").unwrap();
        assert_eq!((dir.name.as_str(), dir.mode & 0o170000), ("etc", 0o040000));
        // A link is the link itself, with where it leads inside the root.
        let abs = stat("/etc/abs").unwrap();
        assert_eq!(abs.mode & 0o170000, 0o120000);
        assert_eq!(abs.link_target, "/etc/sub");
        assert_eq!(stat("etc/rel").unwrap().link_target, "/etc/hosts");
        assert_eq!(stat("/dangling").unwrap().link_target, "/nowhere/x");
        // A slash or a dot at the end goes through it.
        let through = stat("/etc/abs/").unwrap();
        assert_eq!(
            (through.name.as_str(), through.mode & 0o170000),
            ("abs", 0o040000)
        );
        assert_eq!(through.link_target, "");
        let dot = stat("/etc/abs/.").unwrap();
        assert_eq!((dot.name.as_str(), dot.mode & 0o170000), (".", 0o040000));
        let top = stat("/").unwrap();
        assert_eq!((top.name.as_str(), top.mode & 0o170000), ("/", 0o040000));
        // Missing: NoSuchPath, the path named.
        for missing in ["/nope", "/nope/deeper", "/dangling/."] {
            let e = stat(missing).unwrap_err();
            assert_eq!(
                crate::api::kind_of(&e),
                Some(FailureKind::NoSuchPath),
                "{missing}"
            );
            assert_eq!(
                e.to_string(),
                format!("web:{missing}: no such file or directory")
            );
        }
        let e = stat("/etc/hosts/").unwrap_err();
        assert_eq!(
            crate::api::kind_of(&e),
            None,
            "not a directory is no missing path"
        );
    }

    #[test]
    fn base_names_are_read_as_filepath_base_reads_them() {
        for (path, name) in [
            ("/etc/hosts", "hosts"),
            ("/etc/", "etc"),
            ("/etc//", "etc"),
            ("/etc/.", "."),
            ("/", "/"),
            ("//", "/"),
            ("", "."),
            ("hosts", "hosts"),
            ("/a/..", ".."),
        ] {
            assert_eq!(base_name(path), name, "{path}");
        }
        for good in ["etc", ".", "/", "a b", "..."] {
            assert!(check_entry_name(good).is_ok(), "{good}");
        }
        for bad in ["", "..", "a/b", "/etc", "a\0b"] {
            let e = check_entry_name(bad).unwrap_err();
            assert_eq!(crate::api::kind_of(&e), Some(FailureKind::Invalid), "{bad}");
        }
    }

    /// The names and link names of a stream, as a reader takes them.
    fn names_in(stream: &[u8]) -> Vec<(String, String)> {
        let mut archive = tar::Archive::new(stream);
        archive
            .entries()
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                let link = e
                    .link_name_bytes()
                    .map(|l| String::from_utf8_lossy(&l).into_owned())
                    .unwrap_or_default();
                (String::from_utf8_lossy(&e.path_bytes()).into_owned(), link)
            })
            .collect()
    }

    #[test]
    fn a_top_entry_takes_the_name_it_is_given() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("a.txt"), "alpha").unwrap();
        let long = "n".repeat(150);
        fs::write(src.join("sub").join(&long), "long").unwrap();
        let far = format!("/{}", "t".repeat(120));
        std::os::unix::fs::symlink(&far, src.join("link")).unwrap();
        let packed = |top: &str, naming: Naming| {
            let dir = open_dir(&src);
            let mut stream = Vec::new();
            pack_as(
                dir.as_fd(),
                OsStr::new("."),
                Path::new(top),
                naming,
                Ids::Keep,
                &mut stream,
            )
            .unwrap();
            stream
        };
        let link = ("link".to_string(), far.clone());
        let entries = |top: &str, prefix: &str| {
            vec![
                (top.to_string(), String::new()),
                (format!("{prefix}a.txt"), String::new()),
                (format!("{prefix}link"), far.clone()),
                (format!("{prefix}sub/"), String::new()),
                (format!("{prefix}sub/{long}"), String::new()),
            ]
        };
        // As the engine names `DIR/.`, `/` and `DIR`.
        assert_eq!(names_in(&packed(".", Naming::Literal)), entries("./", "./"));
        assert_eq!(names_in(&packed("/", Naming::Literal)), entries("/", "/"));
        assert_eq!(
            names_in(&packed("etc", Naming::Literal)),
            entries("etc/", "etc/")
        );
        // As `cp` names them, which the option leaves alone.
        let cp = names_in(&packed("src", Naming::Normalized));
        assert_eq!(cp[0], ("src".to_string(), String::new()));
        assert_eq!(cp[2], (format!("src/{}", link.0), far.clone()));
        assert_eq!(cp[3].0, "src/sub");
        // What the literal names carry comes back whole.
        let out = tmp.path().join("out");
        fs::create_dir_all(&out).unwrap();
        let stream = packed(".", Naming::Literal);
        extracted(&out, "", &stream, ExtractOptions::default()).unwrap();
        assert_eq!(
            fs::read_to_string(out.join("sub").join(&long)).unwrap(),
            "long"
        );
        assert_eq!(fs::read_link(out.join("link")).unwrap(), Path::new(&far));
    }
}
