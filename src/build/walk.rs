//! Walking a directory tree the way a build context is packed: entries in lexical
//! order, depth first, symlinks reported and never followed, each with its metadata
//! (mode, owner, size, times, link target, device numbers, xattrs, hardlinks to an
//! earlier entry), filtered by include and exclude patterns such as those of a
//! .dockerignore, with optional symlinks to follow into the includes and a hook that
//! can rewrite or drop entries. A port of the walker and filter of tonistiigi/fsutil
//! (upstream/SOURCE names the version).
//!
//! An excluded directory is still entered when an exception (`!pattern`) could match
//! something below it; it is then reported only once something inside it is, right
//! before that entry, so that a packed context never holds an entry without its
//! parents. The same holds for directories that only lead to an include pattern.
//!
//! The filesystem errors of this module and of `copy` read as the reference's do
//! ("lstat PATH: no such file or directory"), when rendered with `{:#}`.
// The port keeps the reference's API whole; its tests use what the builder does not.
#![allow(dead_code)]

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File, Metadata};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{anyhow, bail, Context as _, Result};
use nix::errno::Errno as NixErrno;

use super::ignore::{contains_wildcards, glob_match_bytes, lexical, lossy, PatternMatcher};

pub(crate) const S_IFMT: u32 = 0o170000;
pub(crate) const S_IFSOCK: u32 = 0o140000;
pub(crate) const S_IFLNK: u32 = 0o120000;
pub(crate) const S_IFREG: u32 = 0o100000;
pub(crate) const S_IFBLK: u32 = 0o060000;
pub(crate) const S_IFDIR: u32 = 0o040000;
pub(crate) const S_IFCHR: u32 = 0o020000;
pub(crate) const S_IFIFO: u32 = 0o010000;

/// A failed filesystem call on one path, written as the reference writes it:
/// "OP PATH: REASON".
#[derive(Debug)]
pub struct PathError {
    pub op: &'static str,
    pub path: PathBuf,
    pub err: io::Error,
}

impl PathError {
    pub fn new(op: &'static str, path: impl Into<PathBuf>, err: io::Error) -> PathError {
        PathError {
            op,
            path: path.into(),
            err,
        }
    }
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {}: {}",
            self.op,
            self.path.display(),
            reason(&self.err)
        )
    }
}

// No source: the reason is already part of the message, and a source would print it
// twice in an error chain.
impl std::error::Error for PathError {}

/// A failed call on two paths (link, symlink): "OP OLD NEW: REASON".
#[derive(Debug)]
pub struct LinkError {
    pub op: &'static str,
    pub old: PathBuf,
    pub new: PathBuf,
    pub err: io::Error,
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {} {}: {}",
            self.op,
            self.old.display(),
            self.new.display(),
            reason(&self.err)
        )
    }
}

impl std::error::Error for LinkError {}

/// A failed system call reported without a path, as the reference reports a bare errno.
#[derive(Debug)]
pub struct Errno(pub io::Error);

impl fmt::Display for Errno {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&reason(&self.0))
    }
}

impl std::error::Error for Errno {}

/// A path that does not exist, from a check rather than a system call.
#[derive(Debug)]
pub struct NotExist;

impl fmt::Display for NotExist {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("file does not exist")
    }
}

impl std::error::Error for NotExist {}

/// The reason of an error in the reference's words: the C library's message with its
/// first letter in lower case, which is how the reference's runtime spells errno.
pub(crate) fn reason(err: &io::Error) -> String {
    let Some(code) = err.raw_os_error() else {
        return err.to_string();
    };
    let full = io::Error::from_raw_os_error(code).to_string();
    let text = full
        .strip_suffix(&format!(" (os error {code})"))
        .unwrap_or(&full);
    let mut chars = text.chars();
    match (chars.next(), chars.next()) {
        (Some(a), Some(b)) if a.is_ascii_uppercase() && b.is_ascii_lowercase() => {
            format!("{}{}", a.to_ascii_lowercase(), &text[1..])
        }
        _ => text.to_string(),
    }
}

fn io_errors(err: &anyhow::Error) -> impl Iterator<Item = &io::Error> {
    err.chain().filter_map(|cause| {
        if let Some(e) = cause.downcast_ref::<PathError>() {
            Some(&e.err)
        } else if let Some(e) = cause.downcast_ref::<LinkError>() {
            Some(&e.err)
        } else if let Some(e) = cause.downcast_ref::<Errno>() {
            Some(&e.0)
        } else {
            cause.downcast_ref::<io::Error>()
        }
    })
}

fn has_not_exist(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| cause.is::<NotExist>())
}

/// Whether the error says that a path does not exist (ENOENT).
pub(crate) fn is_not_found(err: &anyhow::Error) -> bool {
    has_not_exist(err)
        || io_errors(err).any(|e| {
            e.raw_os_error() == Some(NixErrno::ENOENT as i32) || e.kind() == io::ErrorKind::NotFound
        })
}

/// `is_not_found`, or a path component that is not a directory (ENOTDIR).
pub(crate) fn is_not_exist(err: &anyhow::Error) -> bool {
    is_not_found(err)
        || io_errors(err).any(|e| {
            e.raw_os_error() == Some(NixErrno::ENOTDIR as i32)
                || e.kind() == io::ErrorKind::NotADirectory
        })
}

fn is_permission(err: &anyhow::Error) -> bool {
    io_errors(err).any(|e| e.kind() == io::ErrorKind::PermissionDenied)
}

/// A string quoted the way the reference's `%q` quotes it.
pub(crate) fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{7}' => out.push_str("\\a"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{b}' => out.push_str("\\v"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A list of strings printed the way the reference's `%s` prints one: "[a b c]".
pub(crate) fn list(items: &[String]) -> String {
    format!("[{}]", items.join(" "))
}

pub(crate) fn bytes_path(bytes: &[u8]) -> PathBuf {
    PathBuf::from(OsStr::from_bytes(bytes))
}

pub(crate) fn path_bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_bytes()
}

/// What the walker records about an entry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stat {
    /// The path relative to the walked root, without a leading slash.
    pub path: PathBuf,
    /// st_mode: file type and permission bits. A socket is reported as a regular file
    /// of the same permissions, as the reference does since an archive has no sockets.
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    /// The size for anything but a directory (the target's length for a symlink).
    pub size: i64,
    /// Modification time in nanoseconds since the epoch.
    pub mod_time: i64,
    /// The target of a symlink, or for a file with several links the path of the first
    /// entry of the walk that is the same file.
    pub linkname: PathBuf,
    pub devmajor: i64,
    pub devminor: i64,
    pub xattrs: BTreeMap<OsString, Vec<u8>>,
}

impl Stat {
    pub fn file_type(&self) -> u32 {
        self.mode & S_IFMT
    }

    pub fn is_dir(&self) -> bool {
        self.file_type() == S_IFDIR
    }

    pub fn is_symlink(&self) -> bool {
        self.file_type() == S_IFLNK
    }

    pub fn is_regular(&self) -> bool {
        self.file_type() == S_IFREG
    }
}

// Keyed by device and inode, since inode numbers repeat across the filesystems a tree
// can span.
type Seen = HashMap<(u64, u64), PathBuf>;

fn mkstat(
    origpath: &Path,
    relpath: &Path,
    meta: &Metadata,
    seen: Option<&RefCell<Seen>>,
) -> Result<Stat> {
    let st_mode = meta.mode();
    let mut stat = Stat {
        path: relpath.to_path_buf(),
        mode: st_mode,
        uid: meta.uid(),
        gid: meta.gid(),
        mod_time: meta.mtime() * 1_000_000_000 + meta.mtime_nsec(),
        ..Stat::default()
    };
    if st_mode & S_IFMT != S_IFDIR {
        // The reference tests the type bits this way, which also takes in symlinks and
        // sockets; their device number is zero.
        if st_mode & S_IFBLK != 0 || st_mode & S_IFCHR != 0 {
            let rdev = meta.rdev();
            stat.devmajor = ((rdev >> 8) & 0xfff) as i64;
            stat.devminor = ((rdev & 0xff) | ((rdev >> 12) & 0xfff00)) as i64;
        }
        if let Some(seen) = seen {
            let key = (meta.dev(), meta.ino());
            let mut seen = seen.borrow_mut();
            let earlier = if meta.nlink() > 1 {
                seen.get(&key).cloned()
            } else {
                None
            };
            match earlier {
                Some(first) => stat.linkname = first,
                None => {
                    seen.insert(key, relpath.to_path_buf());
                }
            }
        }
        stat.size = meta.size() as i64;
        if st_mode & S_IFMT == S_IFLNK {
            stat.linkname =
                fs::read_link(origpath).map_err(|e| PathError::new("readlink", origpath, e))?;
        }
    }
    load_xattrs(origpath, &mut stat)?;
    if st_mode & S_IFMT == S_IFSOCK {
        stat.mode = S_IFREG | (st_mode & 0o7777);
    }
    Ok(stat)
}

fn load_xattrs(origpath: &Path, stat: &mut Stat) -> Result<()> {
    let names = match xattr::list(origpath) {
        Ok(names) => names,
        Err(e) if e.raw_os_error() == Some(NixErrno::EOPNOTSUPP as i32) => return Ok(()),
        Err(e) => {
            return Err(anyhow::Error::new(Errno(e))
                .context(format!("failed to xattr {}", origpath.display())))
        }
    };
    for name in names {
        if name.as_bytes().starts_with(b"com.apple.") {
            continue;
        }
        if let Ok(Some(value)) = xattr::get(origpath, &name) {
            stat.xattrs.insert(name, value);
        }
    }
    Ok(())
}

/// What a walk callback asks the walker to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Continue,
    /// For a directory, do not enter it; for anything else, skip the rest of the
    /// directory it is in.
    SkipDir,
}

/// An entry handed to a walk callback. Its `Stat` is computed when first asked for,
/// which is what keeps hardlinks pointing only at entries that were reported.
#[derive(Debug, Clone)]
pub struct DirEntry {
    inner: Rc<EntryInner>,
}

#[derive(Debug)]
struct EntryInner {
    path: PathBuf,
    origpath: Option<PathBuf>,
    is_dir: bool,
    is_symlink: bool,
    seen: Option<Rc<RefCell<Seen>>>,
    stat: RefCell<Option<Stat>>,
}

impl DirEntry {
    pub fn from_stat(stat: Stat) -> DirEntry {
        DirEntry {
            inner: Rc::new(EntryInner {
                path: stat.path.clone(),
                origpath: None,
                is_dir: stat.is_dir(),
                is_symlink: stat.is_symlink(),
                seen: None,
                stat: RefCell::new(Some(stat)),
            }),
        }
    }

    fn lazy(origpath: PathBuf, path: PathBuf, kind: RawKind, seen: Rc<RefCell<Seen>>) -> DirEntry {
        DirEntry {
            inner: Rc::new(EntryInner {
                path,
                origpath: Some(origpath),
                is_dir: kind.is_dir,
                is_symlink: kind.is_symlink,
                seen: Some(seen),
                stat: RefCell::new(None),
            }),
        }
    }

    /// The last element of the entry's path.
    pub fn name(&self) -> Vec<u8> {
        match self.inner.stat.borrow().as_ref() {
            Some(stat) => lexical::base(path_bytes(&stat.path)),
            None => lexical::base(path_bytes(&self.inner.path)),
        }
    }

    pub fn is_dir(&self) -> bool {
        match self.inner.stat.borrow().as_ref() {
            Some(stat) => stat.is_dir(),
            None => self.inner.is_dir,
        }
    }

    pub fn is_symlink(&self) -> bool {
        match self.inner.stat.borrow().as_ref() {
            Some(stat) => stat.is_symlink(),
            None => self.inner.is_symlink,
        }
    }

    pub fn info(&self) -> Result<Stat> {
        if let Some(stat) = self.inner.stat.borrow().as_ref() {
            return Ok(stat.clone());
        }
        let Some(origpath) = &self.inner.origpath else {
            bail!("{}: no metadata", self.inner.path.display());
        };
        let meta =
            fs::symlink_metadata(origpath).map_err(|e| PathError::new("lstat", origpath, e))?;
        let stat = mkstat(
            origpath,
            &self.inner.path,
            &meta,
            self.inner.seen.as_deref(),
        )?;
        *self.inner.stat.borrow_mut() = Some(stat.clone());
        Ok(stat)
    }
}

/// The callback of `Fs::walk`: the path relative to the root, the entry (absent only
/// when the error says it could not be read) and the error met on it, if any. An entry
/// can come twice, the second time with the error of reading the directory.
pub type WalkFn<'a> =
    dyn FnMut(&Path, Option<DirEntry>, Option<anyhow::Error>) -> Result<Step> + 'a;

/// A tree that can be walked and whose files can be opened.
pub trait Fs {
    /// Walks `target` (the root itself for "" or "/") and everything below it, the root
    /// excluded. A callback error stops the walk and is returned, except one saying that
    /// a path does not exist, which skips that path.
    fn walk(&mut self, target: &Path, f: &mut WalkFn<'_>) -> Result<()>;

    fn open(&self, path: &Path) -> Result<File>;
}

/// A directory of the host as an `Fs`.
#[derive(Debug, Clone)]
pub struct HostFs {
    root: PathBuf,
}

impl HostFs {
    /// The root is resolved through symlinks and must be a directory.
    pub fn new(root: &Path) -> Result<HostFs> {
        // The reference names the component that failed to resolve; canonicalize does
        // not tell which, so the root is named.
        let resolved = fs::canonicalize(root).map_err(|e| {
            let kind = e.kind();
            let inner = PathError::new("lstat", root, e).to_string();
            PathError::new("resolve", root, io::Error::new(kind, inner))
        })?;
        let meta = fs::metadata(&resolved).map_err(|e| PathError::new("stat", &resolved, e))?;
        if !meta.is_dir() {
            return Err(PathError::new(
                "stat",
                &resolved,
                io::Error::from_raw_os_error(NixErrno::ENOTDIR as i32),
            )
            .into());
        }
        Ok(HostFs { root: resolved })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct RawKind {
    is_dir: bool,
    is_symlink: bool,
}

impl From<fs::FileType> for RawKind {
    fn from(t: fs::FileType) -> RawKind {
        RawKind {
            is_dir: t.is_dir(),
            is_symlink: t.is_symlink(),
        }
    }
}

fn child_path(parent: &[u8], name: &[u8]) -> Vec<u8> {
    let mut p = parent.to_vec();
    if p.last() != Some(&b'/') {
        p.push(b'/');
    }
    p.extend_from_slice(name);
    p
}

fn child_rel(parent: &[u8], name: &[u8]) -> Vec<u8> {
    if parent == b"." {
        return name.to_vec();
    }
    child_path(parent, name)
}

pub(crate) type ReadDir = (Vec<(Vec<u8>, RawKind)>, Option<anyhow::Error>);

/// The entries of a directory sorted by name, and the error that cut the reading
/// short, if any.
pub(crate) fn read_dir_sorted(dir: &Path) -> ReadDir {
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => return (Vec::new(), Some(PathError::new("open", dir, e).into())),
    };
    let mut entries = Vec::new();
    let mut error = None;
    for entry in rd {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                error = Some(PathError::new("readdirent", dir, e).into());
                break;
            }
        };
        let kind = match entry.file_type() {
            Ok(t) => RawKind::from(t),
            // Gone between the listing and the lstat: as if it had never been there.
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => {
                error = Some(PathError::new("lstat", entry.path(), e).into());
                break;
            }
        };
        entries.push((entry.file_name().into_vec(), kind));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    (entries, error)
}

type RawWalkFn<'a> =
    dyn FnMut(&[u8], &[u8], Option<RawKind>, Option<anyhow::Error>) -> Result<Step> + 'a;

/// Depth-first walk in lexical order, the order of the reference's directory walk.
fn walk_dir(abs: &[u8], rel: &[u8], kind: RawKind, f: &mut RawWalkFn<'_>) -> Result<Step> {
    if f(abs, rel, Some(kind), None)? == Step::SkipDir {
        return Ok(if kind.is_dir {
            Step::Continue
        } else {
            Step::SkipDir
        });
    }
    if !kind.is_dir {
        return Ok(Step::Continue);
    }
    let (entries, err) = read_dir_sorted(&bytes_path(abs));
    if let Some(err) = err {
        if f(abs, rel, Some(kind), Some(err))? == Step::SkipDir {
            return Ok(Step::Continue);
        }
    }
    for (name, child_kind) in entries {
        let step = walk_dir(
            &child_path(abs, &name),
            &child_rel(rel, &name),
            child_kind,
            f,
        )?;
        if step == Step::SkipDir {
            break;
        }
    }
    Ok(Step::Continue)
}

impl Fs for HostFs {
    fn walk(&mut self, target: &Path, f: &mut WalkFn<'_>) -> Result<()> {
        let seen: Rc<RefCell<Seen>> = Rc::default();
        let root = path_bytes(&self.root).to_vec();
        let start = lexical::join(&[&root, path_bytes(target)]);
        let start_rel = lexical::rel(&root, &start).ok_or_else(|| {
            anyhow!(
                "Rel: can't make {} relative to {}",
                lossy(&start),
                lossy(&root)
            )
        })?;
        let mut raw = |abs: &[u8], rel: &[u8], kind: Option<RawKind>, err| -> Result<Step> {
            if rel == b"." {
                return Ok(Step::Continue);
            }
            let entry =
                kind.map(|k| DirEntry::lazy(bytes_path(abs), bytes_path(rel), k, seen.clone()));
            match f(&bytes_path(rel), entry, err) {
                Err(e) if is_not_exist(&e) => Ok(Step::SkipDir),
                other => other,
            }
        };
        let start_path = bytes_path(&start);
        let result = match fs::symlink_metadata(&start_path) {
            Err(e) => raw(
                &start,
                &start_rel,
                None,
                Some(PathError::new("lstat", &start_path, e).into()),
            ),
            Ok(meta) => walk_dir(&start, &start_rel, meta.file_type().into(), &mut raw),
        };
        result.map(drop)
    }

    fn open(&self, path: &Path) -> Result<File> {
        let p = bytes_path(&lexical::join(&[path_bytes(&self.root), path_bytes(path)]));
        File::open(&p).map_err(|e| PathError::new("open", p, e).into())
    }
}

/// What the map hook decides for an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapResult {
    /// Report the entry, with whatever the hook changed in its `Stat`.
    Keep,
    /// Do not report it, and go on.
    Exclude,
    /// Do not report it; do not enter it if it is a directory, or skip the rest of its
    /// directory if it is not.
    SkipDir,
}

pub type MapFn<'a> = Box<dyn FnMut(&mut Stat) -> MapResult + 'a>;

/// How a walk is filtered.
#[derive(Default)]
pub struct FilterOpt<'a> {
    /// Only paths matching one of these (or below a match) are reported.
    pub include_patterns: Vec<String>,
    /// Paths matching these are not reported (the patterns of an ignore file).
    pub exclude_patterns: Vec<String>,
    /// Paths whose symlinks, at any component, are followed and whose targets are
    /// added to the include patterns, the links themselves included. `Some(vec![])`
    /// differs from `None` as an empty list differs from a nil one in the reference.
    pub follow_paths: Option<Vec<String>>,
    /// Called for every entry that passes the patterns; it can change the `Stat`.
    pub map: Option<MapFn<'a>>,
}

/// An `Fs` seen through include and exclude patterns.
pub struct FilterFs<'a, F: Fs> {
    fs: F,
    include: Option<PatternMatcher>,
    exclude: Option<PatternMatcher>,
    only_prefix_includes: bool,
    only_prefix_exclude_exceptions: bool,
    map: Option<MapFn<'a>>,
}

const PATTERN_CHARS: &str = "*[]?^\\";

fn pattern_without_trailing_glob(p: &super::ignore::Pattern) -> &str {
    let s = p.as_str();
    let s = s.strip_suffix("/**").unwrap_or(s);
    s.strip_suffix("/*").unwrap_or(s)
}

impl<'a, F: Fs> FilterFs<'a, F> {
    pub fn new(mut fs: F, opt: FilterOpt<'a>) -> Result<FilterFs<'a, F>> {
        let mut include_patterns = opt.include_patterns;
        if let Some(follow) = &opt.follow_paths {
            if let Some(targets) = follow_links(&mut fs, follow)? {
                include_patterns.extend(targets);
                include_patterns = dedupe_paths(&include_patterns).unwrap_or_default();
            }
        }
        let mut include = None;
        let mut only_prefix_includes = true;
        if !include_patterns.is_empty() {
            let m = PatternMatcher::new(&include_patterns).map_err(|e| {
                anyhow::Error::new(e).context(format!(
                    "invalid includepatterns: {}",
                    list(&include_patterns)
                ))
            })?;
            only_prefix_includes = !m.patterns().iter().any(|p| {
                !p.exclusion()
                    && pattern_without_trailing_glob(p).contains(|c| PATTERN_CHARS.contains(c))
            });
            include = Some(m);
        }
        let mut exclude = None;
        let mut only_prefix_exclude_exceptions = true;
        if !opt.exclude_patterns.is_empty() {
            let m = PatternMatcher::new(&opt.exclude_patterns).map_err(|e| {
                anyhow::Error::new(e).context(format!(
                    "invalid excludepatterns: {}",
                    list(&opt.exclude_patterns)
                ))
            })?;
            only_prefix_exclude_exceptions = !m.patterns().iter().any(|p| {
                p.exclusion()
                    && pattern_without_trailing_glob(p).contains(|c| PATTERN_CHARS.contains(c))
            });
            exclude = Some(m);
        }
        Ok(FilterFs {
            fs,
            include,
            exclude,
            only_prefix_includes,
            only_prefix_exclude_exceptions,
            map: opt.map,
        })
    }
}

struct VisitedDir {
    entry: DirEntry,
    path_with_sep: Vec<u8>,
    include_info: super::ignore::MatchInfo,
    exclude_info: super::ignore::MatchInfo,
    called_fn: bool,
    skip_fn: bool,
}

/// Whether some pattern of the given kind, taken as a plain prefix, lies below `dir`.
fn prefix_below(m: &PatternMatcher, exclusions: bool, dir: &str) -> bool {
    let dir_slash = format!("{dir}/");
    m.patterns().iter().any(|p| {
        p.exclusion() == exclusions
            && format!("{}/", pattern_without_trailing_glob(p)).starts_with(&dir_slash)
    })
}

impl<F: Fs> Fs for FilterFs<'_, F> {
    fn walk(&mut self, target: &Path, f: &mut WalkFn<'_>) -> Result<()> {
        let FilterFs {
            fs,
            include,
            exclude,
            only_prefix_includes,
            only_prefix_exclude_exceptions,
            map,
        } = self;
        let tracking = include.is_some() || exclude.is_some();
        let mut parent_dirs: Vec<VisitedDir> = Vec::new();
        fs.walk(target, &mut |path, entry, walk_err| {
            let step = (|| -> Result<Step> {
                let is_dir = entry.as_ref().is_some_and(DirEntry::is_dir);
                let pbytes = path_bytes(path);
                let mut dir = None;
                if tracking {
                    while let Some(last) = parent_dirs.last() {
                        if pbytes.starts_with(&last.path_with_sep) {
                            break;
                        }
                        parent_dirs.pop();
                    }
                    if let (true, Some(entry)) = (is_dir, &entry) {
                        let mut path_with_sep = pbytes.to_vec();
                        path_with_sep.push(b'/');
                        dir = Some(VisitedDir {
                            entry: entry.clone(),
                            path_with_sep,
                            include_info: Default::default(),
                            exclude_info: Default::default(),
                            called_fn: false,
                            skip_fn: false,
                        });
                    }
                }
                let pstr = lossy(pbytes);
                let mut skip = false;
                if let Some(m) = include.as_ref() {
                    let parent = parent_dirs
                        .last()
                        .map(|d| d.include_info.clone())
                        .unwrap_or_default();
                    let (matched, info) = m
                        .matches_using_parent_results(&pstr, &parent)
                        .context("failed to match includepatterns")?;
                    if let Some(d) = dir.as_mut() {
                        d.include_info = info;
                    }
                    if !matched {
                        // Nothing below a directory can match when every include is a
                        // plain path that does not lead into it.
                        if is_dir && *only_prefix_includes && !prefix_below(m, false, &pstr) {
                            return Ok(Step::SkipDir);
                        }
                        skip = true;
                    }
                }
                if let Some(m) = exclude.as_ref() {
                    let parent = parent_dirs
                        .last()
                        .map(|d| d.exclude_info.clone())
                        .unwrap_or_default();
                    let (matched, info) = m
                        .matches_using_parent_results(&pstr, &parent)
                        .context("failed to match excludepatterns")?;
                    if let Some(d) = dir.as_mut() {
                        d.exclude_info = info;
                    }
                    if matched {
                        // An excluded directory is entered only when an exception could
                        // match below it.
                        if is_dir
                            && *only_prefix_exclude_exceptions
                            && (!m.exclusions() || !prefix_below(m, true, &pstr))
                        {
                            return Ok(Step::SkipDir);
                        }
                        skip = true;
                    }
                }
                if let Some(err) = walk_err {
                    // A directory nothing is taken from may be unreadable.
                    if skip && is_permission(&err) {
                        return Ok(Step::Continue);
                    }
                    return Err(err);
                }
                let result = (|| -> Result<Step> {
                    if skip {
                        return Ok(Step::Continue);
                    }
                    if let Some(d) = dir.as_mut() {
                        d.called_fn = true;
                    }
                    let entry = entry.ok_or_else(|| anyhow!("{}: no entry", path.display()))?;
                    let mut stat = entry.info()?;
                    if let Some(map) = map.as_mut() {
                        match map(&mut stat) {
                            MapResult::SkipDir => return Ok(Step::SkipDir),
                            MapResult::Exclude => return Ok(Step::Continue),
                            MapResult::Keep => {}
                        }
                    }
                    // Parents held back because nothing in them was reported yet.
                    for parent in parent_dirs.iter_mut() {
                        if parent.skip_fn {
                            return Ok(Step::SkipDir);
                        }
                        if parent.called_fn {
                            continue;
                        }
                        let mut parent_stat = parent.entry.info()?;
                        if let Some(map) = map.as_mut() {
                            match map(&mut parent_stat) {
                                MapResult::Exclude => continue,
                                MapResult::SkipDir => {
                                    parent.skip_fn = true;
                                    return Ok(Step::SkipDir);
                                }
                                MapResult::Keep => {}
                            }
                        }
                        parent.called_fn = true;
                        let ppath = parent_stat.path.clone();
                        if f(&ppath, Some(DirEntry::from_stat(parent_stat)), None)? == Step::SkipDir
                        {
                            parent.skip_fn = true;
                            return Ok(Step::SkipDir);
                        }
                    }
                    let spath = stat.path.clone();
                    f(&spath, Some(DirEntry::from_stat(stat)), None)
                })();
                if let Some(d) = dir {
                    parent_dirs.push(d);
                }
                result
            })();
            match step {
                Err(e) if is_not_exist(&e) => Ok(Step::SkipDir),
                other => other,
            }
        })
    }

    fn open(&self, path: &Path) -> Result<File> {
        let p = lossy(path_bytes(path));
        if let Some(m) = &self.include {
            if !m.matches_or_parent_matches(&p)? {
                return Err(anyhow::Error::new(NotExist).context(format!("open {p}")));
            }
        }
        if let Some(m) = &self.exclude {
            if m.matches_or_parent_matches(&p)? {
                return Err(anyhow::Error::new(NotExist).context(format!("open {p}")));
            }
        }
        self.fs.open(path)
    }
}

/// Walks `root` with `opt`, calling `f` with the `Stat` of every entry that passes, in
/// the reference's order (the root itself is not reported). A directory held back by
/// the patterns comes right before the first entry reported inside it. Errors stop the
/// walk, except those saying that a path does not exist, from the walk or from `f`,
/// which skip that path as `Step::SkipDir` would, and unreadable directories that are
/// excluded anyway.
pub fn walk(
    root: &Path,
    opt: FilterOpt<'_>,
    f: &mut dyn FnMut(&Stat) -> Result<Step>,
) -> Result<()> {
    let fs = HostFs::new(root)?;
    let mut filtered = FilterFs::new(fs, opt)?;
    filtered.walk(Path::new("/"), &mut |_, entry, err| {
        if let Some(err) = err {
            return Err(err);
        }
        let entry = entry.ok_or_else(|| anyhow!("walk: entry without metadata"))?;
        f(&entry.info()?)
    })
}

/// Resolves the symlinks met on the way to each of `paths` (a component of a path may
/// have glob characters), inside the root of `fs`, and returns every link and final
/// target, sorted and with paths below another dropped; `None` when the root itself is
/// one of them, which means no filter.
pub fn follow_links(fs: &mut dyn Fs, paths: &[String]) -> Result<Option<Vec<String>>> {
    let mut resolver = SymlinkResolver {
        resolved: HashSet::new(),
    };
    for p in paths {
        resolver.append(fs, p.as_bytes())?;
    }
    let mut res: Vec<String> = resolver
        .resolved
        .iter()
        .map(|r| lossy(r).into_owned())
        .collect();
    res.sort();
    Ok(dedupe_paths(&res))
}

struct SymlinkResolver {
    resolved: HashSet<Vec<u8>>,
}

impl SymlinkResolver {
    fn append(&mut self, fs: &mut dyn Fs, p: &[u8]) -> Result<()> {
        let mut p = lexical::join(&[b".", p]);
        let mut current = b".".to_vec();
        loop {
            let (first, rest) = match p.iter().position(|&b| b == b'/') {
                Some(i) => (p[..i].to_vec(), p[i + 1..].to_vec()),
                None => (p.clone(), Vec::new()),
            };
            current = lexical::join(&[&current, &first]);
            let targets = self.read_symlink(fs, &current, true)?;
            p = rest;
            if (p.is_empty() || !targets.is_empty()) && self.resolved.contains(&current) {
                return Ok(());
            }
            if !targets.is_empty() {
                self.resolved.insert(current.clone());
                for target in targets {
                    self.append(fs, &lexical::join(&[&target, &p]))?;
                }
                return Ok(());
            }
            if p.is_empty() {
                self.resolved.insert(current);
                return Ok(());
            }
        }
    }

    fn read_symlink(
        &mut self,
        fs: &mut dyn Fs,
        p: &[u8],
        allow_wildcard: bool,
    ) -> Result<Vec<Vec<u8>>> {
        let base = lexical::base(p);
        if allow_wildcard && contains_wildcards(&base) {
            let dir = lexical::dir(p);
            let entries = match read_dir(fs, &dir) {
                Ok(entries) => entries,
                Err(e) if is_not_found(&e) => return Ok(Vec::new()),
                Err(e) => return Err(e.context("readdir")),
            };
            let mut out = Vec::new();
            for entry in entries {
                let name = entry.name();
                if glob_match_bytes(&base, &name).unwrap_or(false) {
                    out.extend(self.read_symlink(fs, &lexical::join(&[&dir, &name]), false)?);
                }
            }
            return Ok(out);
        }
        let entry = match stat_file(fs, p) {
            Ok(Some(entry)) => entry,
            Ok(None) => return Ok(Vec::new()),
            Err(e) if is_not_found(&e) => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        if !entry.is_symlink() {
            return Ok(Vec::new());
        }
        let stat = entry.info()?;
        let link = lexical::clean(path_bytes(&stat.linkname));
        if lexical::is_abs(&link) {
            return Ok(vec![link]);
        }
        Ok(vec![lexical::join(&[
            b"/",
            &lexical::join(&[&lexical::dir(p), &link]),
        ])])
    }
}

/// The entry at `path` in `fs`, or `None` for the root.
fn stat_file(fs: &mut dyn Fs, path: &[u8]) -> Result<Option<DirEntry>> {
    let root = lexical::clean(path);
    if root == b"/" || root == b"." {
        return Ok(None);
    }
    let mut out = None;
    let root_path = bytes_path(&root);
    fs.walk(&root_path, &mut |p, entry, err| {
        if let Some(err) = err {
            return Err(err);
        }
        if p != root_path {
            bail!(
                "expected single entry {} but got {}",
                quote(&lossy(&root)),
                quote(&lossy(path_bytes(p)))
            );
        }
        let entry = entry.ok_or_else(|| anyhow!("walk: entry without metadata"))?;
        let is_dir = entry.is_dir();
        out = Some(entry);
        Ok(if is_dir {
            Step::SkipDir
        } else {
            Step::Continue
        })
    })?;
    match out {
        Some(entry) => Ok(Some(entry)),
        None => Err(anyhow::Error::new(NotExist).context(format!("readFile {}", lossy(&root)))),
    }
}

/// The entries of the directory at `path` in `fs`.
fn read_dir(fs: &mut dyn Fs, path: &[u8]) -> Result<Vec<DirEntry>> {
    let mut root = lexical::clean(path);
    let mut out: Option<Vec<DirEntry>> = None;
    if root == b"/" || root == b"." {
        root = b".".to_vec();
        out = Some(Vec::new());
    }
    let root_path = bytes_path(&root);
    fs.walk(&root_path, &mut |p, entry, err| {
        if let Some(err) = err {
            return Err(err);
        }
        let entry = entry.ok_or_else(|| anyhow!("walk: entry without metadata"))?;
        if p == root_path {
            if !entry.is_dir() {
                return Err(PathError::new(
                    "walk",
                    &root_path,
                    io::Error::from_raw_os_error(NixErrno::ENOTDIR as i32),
                )
                .into());
            }
            out = Some(Vec::new());
            return Ok(Step::Continue);
        }
        let Some(list) = out.as_mut() else {
            bail!(
                "expected to read parent entry {} before child {}",
                quote(&lossy(&root)),
                quote(&lossy(path_bytes(p)))
            );
        };
        let is_dir = entry.is_dir();
        list.push(entry);
        Ok(if is_dir {
            Step::SkipDir
        } else {
            Step::Continue
        })
    })?;
    match out {
        Some(list) => Ok(list),
        None if root != b"." => {
            Err(anyhow::Error::new(NotExist).context(format!("readDir {}", lossy(&root))))
        }
        None => Ok(Vec::new()),
    }
}

/// Drops the paths below an earlier one of a sorted list; `None` when "." is in it.
fn dedupe_paths(paths: &[String]) -> Option<Vec<String>> {
    let mut out: Vec<String> = Vec::with_capacity(paths.len());
    let mut last = String::new();
    for s in paths {
        if s == "." {
            return None;
        }
        if s.starts_with(&format!("{last}/")) {
            continue;
        }
        out.push(s.clone());
        last.clone_from(s);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    /// Builds a tree from lines "ADD PATH KIND [ARG]": KIND is file (ARG its content),
    /// dir, symlink (ARG the target), hardlink (ARG the existing path) or socket.
    fn tree(lines: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for line in lines {
            let parts: Vec<&str> = line.split_whitespace().collect();
            assert_eq!(parts[0], "ADD");
            let p = dir.path().join(parts[1]);
            match parts[2] {
                "dir" => {
                    fs::create_dir(&p).unwrap();
                    fs::set_permissions(&p, fs::Permissions::from_mode(0o700)).unwrap();
                }
                "file" => {
                    fs::write(&p, parts.get(3).unwrap_or(&"")).unwrap();
                    fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
                }
                "symlink" => symlink(parts[3], &p).unwrap(),
                "hardlink" => fs::hard_link(dir.path().join(parts[3]), &p).unwrap(),
                "socket" => {
                    // The listener is dropped, which leaves the socket file behind.
                    std::os::unix::net::UnixListener::bind(&p).unwrap();
                }
                other => panic!("unknown kind {other}"),
            }
        }
        dir
    }

    fn describe(out: &mut String, stat: &Stat) {
        let kind = if stat.is_symlink() {
            format!("symlink:{}", stat.linkname.display())
        } else if stat.is_dir() {
            "dir".to_string()
        } else {
            "file".to_string()
        };
        out.push_str(&format!("{kind} {}", stat.path.display()));
        if !stat.is_symlink() && !stat.linkname.as_os_str().is_empty() {
            out.push_str(&format!(" >{}", stat.linkname.display()));
        }
        out.push('\n');
    }

    fn walk_lines(root: &Path, opt: FilterOpt<'_>) -> Result<String> {
        let mut out = String::new();
        walk(root, opt, &mut |stat| {
            describe(&mut out, stat);
            Ok(Step::Continue)
        })?;
        Ok(out)
    }

    fn fs_walk_lines(fs: &mut dyn Fs, target: &str) -> Result<String> {
        let mut out = String::new();
        fs.walk(Path::new(target), &mut |_, entry, err| {
            if let Some(err) = err {
                return Err(err);
            }
            describe(&mut out, &entry.unwrap().info()?);
            Ok(Step::Continue)
        })?;
        Ok(out)
    }

    fn includes(patterns: &[&str]) -> FilterOpt<'static> {
        FilterOpt {
            include_patterns: patterns.iter().map(|s| s.to_string()).collect(),
            ..FilterOpt::default()
        }
    }

    fn excludes(patterns: &[&str]) -> FilterOpt<'static> {
        FilterOpt {
            exclude_patterns: patterns.iter().map(|s| s.to_string()).collect(),
            ..FilterOpt::default()
        }
    }

    fn follows(paths: &[&str]) -> FilterOpt<'static> {
        FilterOpt {
            follow_paths: Some(paths.iter().map(|s| s.to_string()).collect()),
            ..FilterOpt::default()
        }
    }

    fn trim_lines(expected: &str) -> String {
        let mut out = String::new();
        for line in expected.lines().map(str::trim).filter(|l| !l.is_empty()) {
            out.push_str(line);
            out.push('\n');
        }
        out
    }

    #[test]
    fn walker_simple() {
        let d = tree(&["ADD foo file", "ADD foo2 file"]);
        assert_eq!(
            walk_lines(d.path(), FilterOpt::default()).unwrap(),
            "file foo\nfile foo2\n"
        );
    }

    #[test]
    fn invalid_exclude_patterns() {
        let d = tree(&["ADD foo file data1"]);
        let fs = HostFs::new(d.path()).unwrap();
        let err = FilterFs::new(fs, excludes(&["!"])).err().unwrap();
        assert_eq!(
            format!("{err:#}"),
            "invalid excludepatterns: [!]: illegal exclusion pattern: \"!\""
        );
    }

    #[test]
    fn walker_include() {
        let d = tree(&["ADD bar dir", "ADD bar/foo file", "ADD foo2 file"]);
        let both = "dir bar\nfile bar/foo\n";
        for (pattern, want) in [
            ("bar", both),
            ("bar/foo", both),
            ("b*", both),
            ("bar/f*", both),
            ("bar/g*", ""),
            ("f*", "file foo2\n"),
            ("b*/f*", both),
            ("b*/foo", both),
            ("b*/", both),
        ] {
            assert_eq!(
                walk_lines(d.path(), includes(&[pattern])).unwrap(),
                want,
                "include {pattern:?}"
            );
        }
    }

    #[test]
    fn walker_include_return_skip_dir() {
        let d = tree(&[
            "ADD foo dir",
            "ADD foo/x dir",
            "ADD foo/y dir",
            "ADD foo/x/a.txt file",
            "ADD foo/y/b.txt file",
        ]);
        let mut found = Vec::new();
        walk(d.path(), includes(&["**/*.txt"]), &mut |stat| {
            found.push(stat.path.clone());
            Ok(Step::SkipDir)
        })
        .unwrap();
        assert_eq!(found, [PathBuf::from("foo")]);
    }

    #[test]
    fn walker_exclude() {
        let d = tree(&[
            "ADD bar file",
            "ADD foo dir",
            "ADD foo2 file",
            "ADD foo/bar2 file",
        ]);
        assert_eq!(
            walk_lines(d.path(), excludes(&["foo*", "!foo/bar2"])).unwrap(),
            "file bar\ndir foo\nfile foo/bar2\n"
        );
    }

    #[test]
    fn walker_follow_links() {
        let d = tree(&[
            "ADD bar file",
            "ADD foo dir",
            "ADD foo/l1 symlink /baz/one",
            "ADD foo/l2 symlink /baz/two",
            "ADD baz dir",
            "ADD baz/one file",
            "ADD baz/two symlink ../bax",
            "ADD bax file",
            "ADD bay file",
        ]);
        assert_eq!(
            walk_lines(d.path(), follows(&["foo/l*", "bar"])).unwrap(),
            "file bar\nfile bax\ndir baz\nfile baz/one\nsymlink:../bax baz/two\ndir foo\n\
             symlink:/baz/one foo/l1\nsymlink:/baz/two foo/l2\n"
        );
    }

    #[test]
    fn walker_follow_links_to_root() {
        let d = tree(&[
            "ADD foo symlink .",
            "ADD bar file",
            "ADD bax file",
            "ADD bay dir",
            "ADD bay/baz file",
        ]);
        assert_eq!(
            walk_lines(d.path(), follows(&["foo"])).unwrap(),
            "file bar\nfile bax\ndir bay\nfile bay/baz\nsymlink:. foo\n"
        );
    }

    #[test]
    fn walker_map() {
        let d = tree(&[
            "ADD bar file",
            "ADD foo dir",
            "ADD foo2 file",
            "ADD foo/bar2 file",
        ]);
        let opt = FilterOpt {
            map: Some(Box::new(|s: &mut Stat| {
                if s.path.as_os_str().as_bytes().starts_with(b"foo") {
                    s.path = PathBuf::from(format!("_{}", s.path.display()));
                    return MapResult::Keep;
                }
                MapResult::Exclude
            })),
            ..FilterOpt::default()
        };
        assert_eq!(
            walk_lines(d.path(), opt).unwrap(),
            "dir _foo\nfile _foo/bar2\nfile _foo2\n"
        );
    }

    #[test]
    fn walker_map_skip_dir() {
        let d = tree(&[
            "ADD excludeDir dir",
            "ADD excludeDir/a.txt file",
            "ADD includeDir dir",
            "ADD includeDir/a.txt file",
        ]);
        let mut walked = Vec::new();
        let out = {
            let opt = FilterOpt {
                map: Some(Box::new(|s: &mut Stat| {
                    walked.push(s.path.display().to_string());
                    let p = s.path.as_os_str().as_bytes();
                    if p.starts_with(b"excludeDir") {
                        return MapResult::SkipDir;
                    }
                    if p.starts_with(b"includeDir") {
                        return MapResult::Keep;
                    }
                    MapResult::Exclude
                })),
                ..FilterOpt::default()
            };
            walk_lines(d.path(), opt).unwrap()
        };
        assert_eq!(out, "dir includeDir\nfile includeDir/a.txt\n");
        assert_eq!(walked, ["excludeDir", "includeDir", "includeDir/a.txt"]);
    }

    #[test]
    fn walker_map_skip_dir_with_pattern() {
        let d = tree(&[
            "ADD x dir",
            "ADD x/a.txt file",
            "ADD y dir",
            "ADD y/b.txt file",
        ]);
        let opt = FilterOpt {
            include_patterns: vec!["**/*.txt".to_string()],
            map: Some(Box::new(|s: &mut Stat| {
                if lexical::base(s.path.as_os_str().as_bytes()) == b"x" {
                    return MapResult::SkipDir;
                }
                MapResult::Keep
            })),
            ..FilterOpt::default()
        };
        assert_eq!(walk_lines(d.path(), opt).unwrap(), "dir y\nfile y/b.txt\n");
    }

    #[test]
    fn walker_permission_denied() {
        // Root reads a directory whatever its mode.
        if nix::unistd::geteuid().is_root() {
            return;
        }
        let d = tree(&["ADD foo dir", "ADD foo/bar dir"]);
        let bar = d.path().join("foo/bar");
        fs::set_permissions(&bar, fs::Permissions::from_mode(0o000)).unwrap();
        let err = walk_lines(d.path(), FilterOpt::default()).unwrap_err();
        assert!(format!("{err:#}").contains("permission denied"), "{err:#}");
        assert_eq!(
            walk_lines(d.path(), excludes(&["**/bar"])).unwrap(),
            "dir foo\n"
        );
        assert_eq!(
            walk_lines(d.path(), excludes(&["**/bar", "!foo/bar/baz"])).unwrap(),
            "dir foo\n"
        );
        let err = walk_lines(d.path(), excludes(&["**/bar", "!foo/bar"])).unwrap_err();
        assert!(format!("{err:#}").contains("permission denied"), "{err:#}");
        assert_eq!(
            walk_lines(d.path(), includes(&["foo", "!**/bar"])).unwrap(),
            "dir foo\n"
        );
        fs::set_permissions(&bar, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn walker_doublestar_include() {
        let d = tree(&[
            "ADD a dir",
            "ADD a/b dir",
            "ADD a/b/baz dir",
            "ADD a/b/bar dir",
            "ADD a/b/bar/foo file",
            "ADD a/b/bar/fop file",
            "ADD bar dir",
            "ADD bar/foo file",
            "ADD baz dir",
            "ADD foo2 file",
            "ADD foo dir",
            "ADD foo/bar dir",
            "ADD foo/bar/bee file",
        ]);
        let cases: &[(&str, &str)] = &[
            (
                "**",
                "dir a\ndir a/b\ndir a/b/bar\nfile a/b/bar/foo\nfile a/b/bar/fop\ndir a/b/baz
                 dir bar\nfile bar/foo\ndir baz\ndir foo\ndir foo/bar\nfile foo/bar/bee
                 file foo2",
            ),
            (
                "**/bar",
                "dir a\ndir a/b\ndir a/b/bar\nfile a/b/bar/foo\nfile a/b/bar/fop\ndir bar
                 file bar/foo\ndir foo\ndir foo/bar\nfile foo/bar/bee",
            ),
            (
                "**/bar/foo",
                "dir a\ndir a/b\ndir a/b/bar\nfile a/b/bar/foo\ndir bar\nfile bar/foo",
            ),
            (
                "**/b*",
                "dir a\ndir a/b\ndir a/b/bar\nfile a/b/bar/foo\nfile a/b/bar/fop\ndir a/b/baz
                 dir bar\nfile bar/foo\ndir baz\ndir foo\ndir foo/bar\nfile foo/bar/bee",
            ),
            (
                "**/bar/f*",
                "dir a\ndir a/b\ndir a/b/bar\nfile a/b/bar/foo\nfile a/b/bar/fop\ndir bar
                 file bar/foo",
            ),
            ("**/bar/g*", ""),
            (
                "**/f*",
                "dir a\ndir a/b\ndir a/b/bar\nfile a/b/bar/foo\nfile a/b/bar/fop\ndir bar
                 file bar/foo\ndir foo\ndir foo/bar\nfile foo/bar/bee\nfile foo2",
            ),
            (
                "**/b*/f*",
                "dir a\ndir a/b\ndir a/b/bar\nfile a/b/bar/foo\nfile a/b/bar/fop\ndir bar
                 file bar/foo",
            ),
            (
                "**/b*/foo",
                "dir a\ndir a/b\ndir a/b/bar\nfile a/b/bar/foo\ndir bar\nfile bar/foo",
            ),
            ("**/foo/**", "dir foo\ndir foo/bar\nfile foo/bar/bee"),
            ("**/baz", "dir a\ndir a/b\ndir a/b/baz\ndir baz"),
        ];
        for (pattern, want) in cases {
            assert_eq!(
                walk_lines(d.path(), includes(&[pattern])).unwrap(),
                trim_lines(want),
                "include {pattern:?}"
            );
        }
    }

    #[test]
    fn fs_walk_targets() {
        let d = tree(&["ADD foo file", "ADD bar dir", "ADD bar/foo2 file"]);
        let mut fs = HostFs::new(d.path()).unwrap();
        assert_eq!(fs.root(), fs::canonicalize(d.path()).unwrap());
        assert_eq!(
            fs_walk_lines(&mut fs, "").unwrap(),
            "dir bar\nfile bar/foo2\nfile foo\n"
        );
        assert_eq!(fs_walk_lines(&mut fs, "foo").unwrap(), "file foo\n");
        assert_eq!(
            fs_walk_lines(&mut fs, "bar").unwrap(),
            "dir bar\nfile bar/foo2\n"
        );
    }

    #[test]
    fn fs_walk_nested() {
        let d = tree(&["ADD foo dir", "ADD foo/bar file"]);
        let host = || HostFs::new(d.path()).unwrap();
        let mut f2 = FilterFs::new(host(), excludes(&["foo", "!foo/bar"])).unwrap();
        assert_eq!(
            fs_walk_lines(&mut f2, "").unwrap(),
            "dir foo\nfile foo/bar\n"
        );
        let inner = FilterFs::new(host(), excludes(&["!foo/bar"])).unwrap();
        let mut f2 = FilterFs::new(inner, excludes(&["foo"])).unwrap();
        assert_eq!(fs_walk_lines(&mut f2, "").unwrap(), "");
        let inner = FilterFs::new(host(), excludes(&["foo"])).unwrap();
        let mut f2 = FilterFs::new(inner, excludes(&["!foo/bar"])).unwrap();
        assert_eq!(fs_walk_lines(&mut f2, "").unwrap(), "");
    }

    #[test]
    fn filtered_open() {
        let d = tree(&["ADD foo file", "ADD bar file"]);
        let mut f = FilterFs::new(HostFs::new(d.path()).unwrap(), excludes(&["bar"])).unwrap();
        assert_eq!(fs_walk_lines(&mut f, "").unwrap(), "file foo\n");
        let mut content = String::new();
        io::Read::read_to_string(&mut f.open(Path::new("foo")).unwrap(), &mut content).unwrap();
        assert_eq!(content, "");
        let err = f.open(Path::new("bar")).unwrap_err();
        assert!(is_not_found(&err));
        assert_eq!(format!("{err:#}"), "open bar: file does not exist");
    }

    #[test]
    fn filtered_open_wildcard() {
        let d = tree(&[
            "ADD baz file",
            "ADD bar dir",
            "ADD bar2 file",
            "ADD bar/foo file",
        ]);
        let f = FilterFs::new(HostFs::new(d.path()).unwrap(), includes(&["bar*"])).unwrap();
        assert!(is_not_found(&f.open(Path::new("baz")).unwrap_err()));
        f.open(Path::new("bar2")).unwrap();
        f.open(Path::new("bar/foo")).unwrap();
    }

    #[test]
    fn filtered_open_invert() {
        let d = tree(&[
            "ADD foo dir",
            "ADD foo/bar dir",
            "ADD foo/bar/baz dir",
            "ADD foo/bar/baz/x file",
            "ADD foo/bar/baz/y file",
        ]);
        let f = FilterFs::new(
            HostFs::new(d.path()).unwrap(),
            excludes(&["foo", "!foo/bar", "foo/bar/baz", "!foo/bar/baz/x"]),
        )
        .unwrap();
        f.open(Path::new("foo/bar/baz/x")).unwrap();
        assert!(is_not_found(
            &f.open(Path::new("foo/bar/baz/y")).unwrap_err()
        ));
    }

    fn follow(root: &Path, paths: &[&str]) -> Option<Vec<String>> {
        let mut fs = HostFs::new(root).unwrap();
        let paths: Vec<String> = paths.iter().map(|s| s.to_string()).collect();
        follow_links(&mut fs, &paths).unwrap()
    }

    fn strings(items: &[&str]) -> Option<Vec<String>> {
        Some(items.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn follow_links_chain() {
        let d = tree(&[
            "ADD dir dir",
            "ADD dir/foo file contents",
            "ADD dir/l1 symlink foo",
            "ADD l2 symlink dir/l1",
            "ADD bar file",
            "ADD baz file",
        ]);
        assert_eq!(
            follow(d.path(), &["l2", "bar"]),
            strings(&["bar", "dir/foo", "dir/l1", "l2"])
        );
    }

    #[test]
    fn follow_links_loop() {
        let d = tree(&[
            "ADD l1 symlink l1",
            "ADD l3 symlink l2",
            "ADD l2 symlink l3",
        ]);
        assert_eq!(
            follow(d.path(), &["l1", "l3"]),
            strings(&["l1", "l2", "l3"])
        );
    }

    #[test]
    fn follow_links_absolute() {
        let d = tree(&[
            "ADD dir dir",
            "ADD dir/l1 symlink /foo/bar/baz",
            "ADD foo dir",
            "ADD foo/bar symlink ../",
            "ADD baz file",
        ]);
        assert_eq!(
            follow(d.path(), &["dir/l1"]),
            strings(&["baz", "dir/l1", "foo/bar"])
        );
        // The same with a link that points above the root.
        let d = tree(&[
            "ADD dir dir",
            "ADD dir/l1 symlink /foo/bar/baz",
            "ADD foo dir",
            "ADD foo/bar symlink ../../../",
            "ADD baz file",
        ]);
        assert_eq!(
            follow(d.path(), &["dir/l1"]),
            strings(&["baz", "dir/l1", "foo/bar"])
        );
    }

    #[test]
    fn follow_links_not_exists() {
        let d = tree(&[]);
        assert_eq!(
            follow(d.path(), &["foo/bar/baz", "bar/baz"]),
            strings(&["bar/baz", "foo/bar/baz"])
        );
        assert_eq!(follow(d.path(), &["."]), None);
        assert_eq!(follow(d.path(), &["f*/foo/t*"]), strings(&["f*/foo/t*"]));
    }

    #[test]
    fn follow_links_normalized() {
        let d = tree(&[]);
        assert_eq!(
            follow(d.path(), &["foo/bar/baz", "foo/bar"]),
            strings(&["foo/bar"])
        );
        let d = tree(&[
            "ADD dir dir",
            "ADD dir/l1 symlink /foo",
            "ADD dir/l2 symlink /",
            "ADD foo dir",
            "ADD foo/bar file",
        ]);
        assert_eq!(
            follow(d.path(), &["dir/l1", "foo/bar"]),
            strings(&["dir/l1", "foo"])
        );
        assert_eq!(follow(d.path(), &["dir/l2", "foo", "foo/bar"]), None);
    }

    #[test]
    fn follow_links_wildcard() {
        let d = tree(&[
            "ADD dir dir",
            "ADD foo dir",
            "ADD dir/l1 symlink /foo/bar1",
            "ADD dir/l2 symlink /foo/bar2",
            "ADD dir/anotherlink symlink /foo/bar3",
            "ADD foo/bar2 symlink ../baz",
            "ADD foo/bar1 file",
            "ADD foo/bar3 file",
            "ADD baz file",
        ]);
        assert_eq!(
            follow(d.path(), &["dir/l*"]),
            strings(&["baz", "dir/l*", "foo/bar1", "foo/bar2"])
        );
        assert_eq!(follow(d.path(), &["dir"]), strings(&["dir"]));
        assert_eq!(
            follow(d.path(), &["dir", "dir/*link"]),
            strings(&["dir", "foo/bar3"])
        );
    }

    #[test]
    fn internal_read_file() {
        let d = tree(&["ADD dir dir", "ADD dir/foo1 file"]);
        let mut fs = HostFs::new(d.path()).unwrap();
        assert!(stat_file(&mut fs, b"/").unwrap().is_none());
        assert!(stat_file(&mut fs, b"").unwrap().is_none());
        let entry = stat_file(&mut fs, b"dir").unwrap().unwrap();
        assert_eq!(entry.name(), b"dir");
        assert!(entry.is_dir());
        let entry = stat_file(&mut fs, b"dir/foo1").unwrap().unwrap();
        assert_eq!(entry.name(), b"foo1");
        assert!(!entry.is_dir());
        assert!(is_not_found(&stat_file(&mut fs, b"dir/foo2").unwrap_err()));
        assert!(is_not_found(&stat_file(&mut fs, b"dir/x/y/z").unwrap_err()));
    }

    #[test]
    fn internal_read_dir() {
        let d = tree(&["ADD dir dir", "ADD dir/foo1 file"]);
        let mut fs = HostFs::new(d.path()).unwrap();
        let entries = read_dir(&mut fs, b"/").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name(), b"dir");
        assert!(entries[0].is_dir());
        let entries = read_dir(&mut fs, b"dir").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name(), b"foo1");
        assert!(is_not_found(&read_dir(&mut fs, b"dir2").unwrap_err()));
        assert!(is_not_found(&read_dir(&mut fs, b"dir/foo1").unwrap_err()));
    }

    #[test]
    fn stat_metadata_hardlinks_and_sockets() {
        let d = tree(&[
            "ADD a file hello",
            "ADD b hardlink a",
            "ADD c symlink a",
            "ADD d socket",
            "ADD e dir",
        ]);
        let mut stats = Vec::new();
        walk(d.path(), FilterOpt::default(), &mut |s| {
            stats.push(s.clone());
            Ok(Step::Continue)
        })
        .unwrap();
        let by_name: BTreeMap<String, Stat> = stats
            .into_iter()
            .map(|s| (s.path.display().to_string(), s))
            .collect();
        assert_eq!(by_name["a"].size, 5);
        assert_eq!(by_name["a"].mode, S_IFREG | 0o644);
        assert!(by_name["a"].is_regular() && by_name["d"].is_regular());
        assert!(by_name["a"].linkname.as_os_str().is_empty());
        // The second name of a file points at the first; its size stays, as in the
        // reference.
        assert_eq!(by_name["b"].linkname, PathBuf::from("a"));
        assert_eq!(by_name["b"].size, 5);
        assert_eq!(by_name["c"].linkname, PathBuf::from("a"));
        assert_eq!(by_name["c"].size, 1);
        assert_eq!(by_name["d"].file_type(), S_IFREG);
        assert_eq!(by_name["e"].mode & S_IFMT, S_IFDIR);
        assert_eq!(by_name["e"].size, 0);
        let meta = fs::symlink_metadata(d.path().join("a")).unwrap();
        assert_eq!(by_name["a"].uid, meta.uid());
        assert_eq!(
            by_name["a"].mod_time,
            meta.mtime() * 1_000_000_000 + meta.mtime_nsec()
        );
    }

    #[test]
    fn hardlink_to_an_excluded_file_is_not_a_link() {
        let d = tree(&["ADD a file x", "ADD b hardlink a"]);
        let out = walk_lines(d.path(), excludes(&["a"])).unwrap();
        assert_eq!(out, "file b\n");
    }

    #[test]
    fn stat_reads_xattrs() {
        let d = tree(&["ADD a file x"]);
        let p = d.path().join("a");
        // Not every filesystem of a test machine takes user xattrs.
        if xattr::set(&p, "user.test", b"v").is_err() {
            return;
        }
        let mut seen = None;
        walk(d.path(), FilterOpt::default(), &mut |s| {
            seen = Some(s.xattrs.clone());
            Ok(Step::Continue)
        })
        .unwrap();
        let xattrs = seen.unwrap();
        assert_eq!(
            xattrs.get(OsStr::new("user.test")).map(Vec::as_slice),
            Some(&b"v"[..])
        );
    }

    #[test]
    fn root_errors_read_as_the_reference() {
        let d = tree(&["ADD f file"]);
        let err = HostFs::new(&d.path().join("missing")).unwrap_err();
        let missing = d.path().join("missing");
        assert_eq!(
            format!("{err:#}"),
            format!(
                "resolve {}: lstat {}: no such file or directory",
                missing.display(),
                missing.display()
            )
        );
        let err = HostFs::new(&d.path().join("f")).unwrap_err();
        assert!(
            format!("{err:#}").ends_with("f: not a directory"),
            "{err:#}"
        );
    }

    #[test]
    fn errno_reasons_and_quoting() {
        assert_eq!(
            reason(&io::Error::from_raw_os_error(2)),
            "no such file or directory"
        );
        assert_eq!(
            reason(&io::Error::from_raw_os_error(13)),
            "permission denied"
        );
        assert_eq!(
            reason(&io::Error::from_raw_os_error(5)),
            "input/output error"
        );
        assert_eq!(
            reason(&io::Error::from_raw_os_error(40)),
            "too many levels of symbolic links"
        );
        assert_eq!(quote("a\"b\\c\n\u{1}"), "\"a\\\"b\\\\c\\n\\x01\"");
        assert_eq!(list(&["a".to_string(), "b".to_string()]), "[a b]");
    }
}
