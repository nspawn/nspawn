//! cp: files between the host and a machine, as a tar stream like docker's API. The
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
    /// a local path; a local path with a colon is written `./a:b`, as with docker.
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

/// A path as `cp` was given it: `DIR/.` means the contents of DIR, a trailing slash
/// that it must be a directory.
pub fn split_source(path: &str) -> (String, bool) {
    if let Some(stripped) = path.strip_suffix("/.") {
        let stripped = if stripped.is_empty() { "/" } else { stripped };
        return (stripped.to_string(), true);
    }
    if path == "." {
        return (".".to_string(), true);
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
        other => anyhow!("{}: {}", what.display(), other.desc()),
    }
}

fn scoped_raw(root: BorrowedFd<'_>, path: &Path, flags: OFlag) -> nix::Result<OwnedFd> {
    let lookup: &Path = if path.as_os_str().is_empty() {
        Path::new(".")
    } else {
        path
    };
    openat2(
        root,
        lookup,
        OpenHow::new()
            .flags(flags | OFlag::O_CLOEXEC)
            .resolve(ResolveFlag::RESOLVE_IN_ROOT | ResolveFlag::RESOLVE_NO_MAGICLINKS),
    )
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
            } else {
                Kind::Other
            }))
        }
        Err(Errno::ENOENT) => Ok(None),
        Err(e) => Err(nix_error(e, path)),
    }
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
        _ => (PathBuf::new(), OsString::from(".")),
    }
}

/// Reads exactly `left` bytes, zeros when the file shrank meanwhile: the header already
/// promised the size.
struct Exact {
    file: File,
    left: u64,
}

impl Read for Exact {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 {
            return Ok(0);
        }
        let want = buf.len().min(self.left.min(usize::MAX as u64) as usize);
        let mut n = self.file.read(&mut buf[..want])?;
        if n == 0 {
            buf[..want].fill(0);
            n = want;
        }
        self.left -= n as u64;
        Ok(n)
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
    pack_entry(&mut builder, parent, leaf, top, ids, &mut stats)?;
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
) -> Result<()> {
    let st = fstatat(dirfd, leaf, AtFlags::AT_SYMLINK_NOFOLLOW).map_err(|e| nix_error(e, path))?;
    let mut header = tar::Header::new_gnu();
    header.set_mode(st.st_mode & 0o7777);
    header.set_mtime(st.st_mtime.max(0) as u64);
    header.set_uid(u64::from(ids.outward(st.st_uid)));
    header.set_gid(u64::from(ids.outward(st.st_gid)));
    if is(st.st_mode, SFlag::S_IFDIR) {
        header.set_entry_type(tar::EntryType::Directory);
        header.set_size(0);
        builder
            .append_data(&mut header, path, io::empty())
            .with_context(|| format!("writing {}", path.display()))?;
        stats.entries += 1;
        let dir = openat(
            dirfd,
            leaf,
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| nix_error(e, path))?;
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
            pack_entry(builder, dir.as_fd(), &name, &path.join(&name), ids, stats)?;
        }
    } else if is(st.st_mode, SFlag::S_IFREG) {
        let file = openat(
            dirfd,
            leaf,
            OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| nix_error(e, path))?;
        let size = st.st_size.max(0) as u64;
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(size);
        builder
            .append_data(
                &mut header,
                path,
                Exact {
                    file: File::from(file),
                    left: size,
                },
            )
            .with_context(|| format!("writing {}", path.display()))?;
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
    let mut directories: Vec<(OwnedFd, u64)> = Vec::new();
    for entry in archive.entries().context("reading the stream")? {
        let mut entry = entry.context("reading the stream")?;
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
                        directories.push((dir, mtime));
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
                replace_leaf(&parent, &leaf, &shown)?;
                let file = openat(
                    &parent,
                    leaf.as_os_str(),
                    OFlag::O_WRONLY
                        | OFlag::O_CREAT
                        | OFlag::O_EXCL
                        | OFlag::O_NOFOLLOW
                        | OFlag::O_CLOEXEC,
                    Mode::from_bits_truncate(0o600),
                )
                .map_err(|e| nix_error(e, &shown))?;
                let mut file = File::from(file);
                let bytes = io::copy(&mut entry, &mut file)
                    .with_context(|| format!("writing {}", shown.display()))?;
                if let Some((uid, gid)) = owner {
                    fchown(&file, Some(uid), Some(gid)).map_err(|e| nix_error(e, &shown))?;
                }
                // After the chown, which clears setuid and setgid.
                fchmod(&file, mode).map_err(|e| nix_error(e, &shown))?;
                futimens(&file, &timespec(mtime), &timespec(mtime))
                    .map_err(|e| nix_error(e, &shown))?;
                stats.bytes += bytes;
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
    if target.is_none() {
        bail!("nothing arrived to copy");
    }
    // Last: writing into a directory changes its time.
    for (dir, mtime) in directories.iter().rev() {
        let _ = futimens(dir, &timespec(*mtime), &timespec(*mtime));
    }
    Ok(stats)
}

/// Makes room for a file or a link: what is there goes, unless it is a directory.
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
    let top = if leaf == "." {
        OsString::from(name)
    } else {
        leaf.clone()
    };
    let parent = scoped(
        root.fd.as_fd(),
        &parent_path,
        OFlag::O_PATH | OFlag::O_DIRECTORY,
    )
    .map_err(|e| anyhow!("{name}:{source}: {e:#}"))?;
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

/// CopyTo: a stream from `input` unpacked at `path` of the machine, by docker's rules.
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
    let relative = inside(path.trim_end_matches('/'));
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
    fn endpoints_read_like_docker() {
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
    fn destinations_follow_docker_cp() {
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
        assert_eq!(
            split_leaf(Path::new("etc/hosts")),
            (PathBuf::from("etc"), OsString::from("hosts"))
        );
        assert_eq!(
            split_leaf(Path::new("")),
            (PathBuf::new(), OsString::from("."))
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
    fn owners_are_shifted_on_the_way_out() {
        assert_eq!(Ids::Keep.outward(1000), 1000);
        assert_eq!(Ids::Shift(100_000).outward(100_000), 0);
        assert_eq!(Ids::Shift(100_000).outward(100_033), 33);
        assert_eq!(Ids::Shift(100_000).outward(0), 65534);
        assert_eq!(Ids::Shift(100_000).outward(100_000 + 65536), 65534);
    }
}
