//! The OCI layer tar of what an overlayfs upper directory changes over its lower layers,
//! byte for byte as the reference writes it, so with the same diff_id, and its gzip blob.
//!
//! The reference is overlay_linux.go walking the upper directory, diff.go and path.go
//! comparing the trees under an opaque directory, tar.go turning each change into tar
//! entries, and the parts of Go's archive/tar writer those rely on; upstream/SOURCE lists
//! the files and versions. Trees are read through [`Tree`] so that tests can describe
//! ownership, device nodes and trusted.* attributes, which an unprivileged user cannot
//! create.
// The port keeps the reference's API whole; its tests use what the builder does not.
#![allow(dead_code)]

use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{bail, Context, Result};
use flate2::write::GzEncoder;
use flate2::Compression;
use sha2::{Digest, Sha256};

/// Level of the gzip blob. The reference compresses with Go's gzip.DefaultCompression,
/// which compress/flate runs as level 6.
pub const GZIP_LEVEL: u32 = 6;

const CAPABILITY: &str = "security.capability";
const OPAQUE: [&str; 2] = ["trusted.overlay.opaque", "user.overlay.opaque"];
// The reference only looks at the trusted name of redirect and not at all at metacopy;
// both make the upper directory an incomplete record of the changes. A step's mount is
// no userxattr one: the user names are a step's own attributes, which the kernel does
// not read, and no reason to refuse its layer.
const REDIRECT: [&str; 1] = ["trusted.overlay.redirect"];
const METACOPY: [&str; 1] = ["trusted.overlay.metacopy"];

const S_IFMT: u32 = 0o170_000;
const S_IFSOCK: u32 = 0o140_000;
const S_IFLNK: u32 = 0o120_000;
const S_IFREG: u32 = 0o100_000;
const S_IFBLK: u32 = 0o060_000;
const S_IFDIR: u32 = 0o040_000;
const S_IFCHR: u32 = 0o020_000;
const S_IFIFO: u32 = 0o010_000;

/// What lstat says about an entry, as far as the differ and the tar writer care.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stat {
    pub dev: u64,
    pub ino: u64,
    /// st_mode, file type included.
    pub mode: u32,
    pub nlink: u64,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u64,
    pub size: u64,
    pub mtime: i64,
    pub mtime_nsec: i64,
}

impl From<&fs::Metadata> for Stat {
    fn from(m: &fs::Metadata) -> Self {
        Stat {
            dev: m.dev(),
            ino: m.ino(),
            mode: m.mode(),
            nlink: m.nlink(),
            uid: m.uid(),
            gid: m.gid(),
            rdev: m.rdev(),
            size: m.size(),
            mtime: m.mtime(),
            mtime_nsec: m.mtime_nsec(),
        }
    }
}

impl Stat {
    fn kind(&self) -> u32 {
        self.mode & S_IFMT
    }

    fn is_dir(&self) -> bool {
        self.kind() == S_IFDIR
    }

    fn is_regular(&self) -> bool {
        self.kind() == S_IFREG
    }

    fn is_symlink(&self) -> bool {
        self.kind() == S_IFLNK
    }

    /// overlayfs records a deletion as a character device 0:0.
    fn is_whiteout(&self) -> bool {
        self.kind() == S_IFCHR && self.rdev == 0
    }

    fn is_linked(&self) -> bool {
        !self.is_dir() && self.nlink > 1
    }
}

/// Read access to one directory tree: an overlayfs upper directory or a lower layer.
///
/// Paths are relative to the root of the tree, the empty path being the root. Errors keep
/// their kind: NotFound and NotADirectory steer the differ as ENOENT and ENOTDIR steer the
/// reference.
pub trait Tree {
    fn lstat(&self, path: &Path) -> io::Result<Stat>;
    /// The names in a directory, in any order.
    fn read_dir(&self, path: &Path) -> io::Result<Vec<OsString>>;
    fn read_link(&self, path: &Path) -> io::Result<PathBuf>;
    /// An extended attribute of the entry itself, never of a symlink's target; None when it
    /// is not set or the filesystem has no such attributes.
    fn xattr(&self, path: &Path, name: &str) -> io::Result<Option<Vec<u8>>>;
    fn open(&self, path: &Path) -> io::Result<Box<dyn Read + '_>>;
}

/// A directory on disk.
pub struct DirTree {
    root: PathBuf,
}

impl DirTree {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        DirTree { root: root.into() }
    }

    fn full(&self, path: &Path) -> PathBuf {
        join(&self.root, path)
    }
}

impl Tree for DirTree {
    fn lstat(&self, path: &Path) -> io::Result<Stat> {
        fs::symlink_metadata(self.full(path)).map(|m| Stat::from(&m))
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<OsString>> {
        fs::read_dir(self.full(path))?
            .map(|entry| entry.map(|e| e.file_name()))
            .collect()
    }

    fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        fs::read_link(self.full(path))
    }

    fn xattr(&self, path: &Path, name: &str) -> io::Result<Option<Vec<u8>>> {
        match xattr::get(self.full(path), name) {
            Err(e) if e.raw_os_error() == Some(nix::libc::EOPNOTSUPP) => Ok(None),
            other => other,
        }
    }

    fn open(&self, path: &Path) -> io::Result<Box<dyn Read + '_>> {
        Ok(Box::new(fs::File::open(self.full(path))?))
    }
}

/// How an entry differs from the lowers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    /// The same as in the lowers, but one of several hard links: reported so that the
    /// tar keeps the links together.
    Unmodified,
    Add,
    Modify,
    Delete,
}

/// What [`write_layer`] wrote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TarSummary {
    /// Bytes of the tar, the two end-of-archive blocks included.
    pub size: u64,
    /// Entries in the tar, PAX extended headers not counted.
    pub entries: u64,
}

/// The digests of a committed layer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Committed {
    /// "sha256:<hex>" of the uncompressed tar: the diff_id of the image config.
    pub diff_id: String,
    /// Bytes of the uncompressed tar.
    pub tar_size: u64,
    /// "sha256:<hex>" of the gzip blob: the layer digest of the manifest.
    pub digest: String,
    /// Bytes of the gzip blob.
    pub size: u64,
}

/// Calls `f` with each change of `upper` over `lowers` (topmost first, as in lowerdir=),
/// in the order of the walk, before the tar writer adds parent directories. A deletion
/// found under an opaque directory comes without metadata, as in the reference.
pub fn changes(
    upper: &dyn Tree,
    lowers: &[&dyn Tree],
    mut f: impl FnMut(ChangeKind, &Path, Option<&Stat>) -> Result<()>,
) -> Result<()> {
    let base = Merged::new(lowers);
    let view = View(upper);
    diff_upper(upper, &view, &base, &mut f)
}

/// Writes the layer tar of what `upper` changes over `lowers` (topmost first, as in
/// lowerdir=) to `out`.
pub fn write_layer<W: Write>(upper: &dyn Tree, lowers: &[&dyn Tree], out: W) -> Result<TarSummary> {
    let base = Merged::new(lowers);
    let view = View(upper);
    let mut writer = ChangeWriter::new(out, &view);
    diff_upper(upper, &view, &base, &mut |kind, path, stat| {
        writer.handle_change(kind, path, stat)
    })?;
    writer.tw.finish()
}

/// Writes the layer once: compressed to `blob`, and as a plain tar to `extract`.
///
/// The tar, and so the diff_id, is the reference's; the blob is not. flate2 and Go's
/// compress/flate emit different deflate streams at the same level (Go also ends every
/// stream with an empty stored block), so only the gzip header and trailer match.
pub fn commit<B: Write, T: Write>(
    upper: &dyn Tree,
    lowers: &[&dyn Tree],
    blob: B,
    extract: T,
) -> Result<Committed> {
    let mut tee = Tee {
        diff: Sha256::new(),
        size: 0,
        gzip: GzEncoder::new(Hashing::new(blob), Compression::new(GZIP_LEVEL)),
        extract,
    };
    write_layer(upper, lowers, &mut tee)?;
    let Tee {
        diff,
        size,
        gzip,
        mut extract,
    } = tee;
    extract
        .flush()
        .context("flushing the extracted copy of the layer")?;
    let mut blob = gzip
        .finish()
        .context("finishing the gzip stream of the layer")?;
    blob.inner.flush().context("flushing the layer blob")?;
    Ok(Committed {
        diff_id: sha256_id(diff),
        tar_size: size,
        digest: sha256_id(blob.hash),
        size: blob.size,
    })
}

fn sha256_id(hash: Sha256) -> String {
    format!("sha256:{}", hex::encode(hash.finalize()))
}

struct Hashing<W> {
    inner: W,
    hash: Sha256,
    size: u64,
}

impl<W: Write> Hashing<W> {
    fn new(inner: W) -> Self {
        Hashing {
            inner,
            hash: Sha256::new(),
            size: 0,
        }
    }
}

impl<W: Write> Write for Hashing<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hash.update(&buf[..n]);
        self.size += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

struct Tee<B: Write, T: Write> {
    diff: Sha256,
    size: u64,
    gzip: GzEncoder<Hashing<B>>,
    extract: T,
}

impl<B: Write, T: Write> Write for Tee<B, T> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.gzip.write_all(buf)?;
        self.extract.write_all(buf)?;
        self.diff.update(buf);
        self.size += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // A flush of the encoder itself would end a deflate block and change the blob.
        self.gzip.get_mut().flush()?;
        self.extract.flush()
    }
}

fn join(a: &Path, b: &Path) -> PathBuf {
    if a.as_os_str().is_empty() {
        b.to_path_buf()
    } else if b.as_os_str().is_empty() {
        a.to_path_buf()
    } else {
        a.join(b)
    }
}

fn shown(path: &Path) -> String {
    format!("/{}", path.display())
}

fn bytes_path(bytes: &[u8]) -> &Path {
    Path::new(OsStr::from_bytes(bytes))
}

fn not_found() -> io::Error {
    io::ErrorKind::NotFound.into()
}

fn is_opaque(tree: &dyn Tree, path: &Path) -> io::Result<bool> {
    for name in OPAQUE {
        if tree.xattr(path, name)?.as_deref() == Some(b"y".as_slice()) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The upper directory as the reference's mount of it (lowerdir=upper:empty) shows it: a
/// whiteout cannot be looked up, but only the root, the one directory both layers have,
/// leaves whiteouts out of its listing. overlayfs passes the listing of any other
/// directory through, so the reference fails on a whiteout under an opaque directory.
struct View<'a>(&'a dyn Tree);

impl Tree for View<'_> {
    fn lstat(&self, path: &Path) -> io::Result<Stat> {
        let stat = self.0.lstat(path)?;
        if stat.is_whiteout() {
            return Err(not_found());
        }
        Ok(stat)
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<OsString>> {
        let names = self.0.read_dir(path)?;
        if !path.as_os_str().is_empty() {
            return Ok(names);
        }
        let mut shown = Vec::new();
        for name in names {
            if !self.0.lstat(Path::new(&name))?.is_whiteout() {
                shown.push(name);
            }
        }
        Ok(shown)
    }

    fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        self.0.read_link(path)
    }

    fn xattr(&self, path: &Path, name: &str) -> io::Result<Option<Vec<u8>>> {
        self.0.xattr(path, name)
    }

    fn open(&self, path: &Path) -> io::Result<Box<dyn Read + '_>> {
        self.0.open(path)
    }
}

/// The lowers as the reference's mount of them shows them. One layer is bind-mounted, so
/// its whiteouts and opaque attributes mean nothing. Several are merged by overlayfs,
/// which lists a directory that only one layer contributes as that layer has it, whiteouts
/// included, though looking one of them up finds nothing.
struct Merged<'a> {
    layers: &'a [&'a dyn Tree],
    /// For each directory looked up, the layers whose directories merge into it.
    dirs: RefCell<HashMap<PathBuf, Rc<[usize]>>>,
}

struct Found {
    stat: Stat,
    /// The topmost layer that has the entry and provides its contents.
    top: Option<usize>,
    /// For a directory, the layers whose directories merge into it, from the top.
    layers: Rc<[usize]>,
}

impl<'a> Merged<'a> {
    fn new(layers: &'a [&'a dyn Tree]) -> Self {
        Merged {
            layers,
            dirs: RefCell::new(HashMap::new()),
        }
    }

    fn find(&self, path: &Path) -> io::Result<Found> {
        let Some(name) = path.file_name() else {
            // overlayfs merges the roots of all layers whatever their attributes.
            return Ok(match self.layers.first() {
                Some(top) => Found {
                    stat: top.lstat(path)?,
                    top: Some(0),
                    layers: (0..self.layers.len()).collect(),
                },
                None => Found {
                    stat: Stat {
                        mode: S_IFDIR | 0o700,
                        nlink: 2,
                        ..Stat::default()
                    },
                    top: None,
                    layers: Rc::from([]),
                },
            });
        };
        let parent = path.parent().unwrap_or_else(|| Path::new(""));
        let below = self.dir_layers(parent)?;
        let path = join(parent, Path::new(name));
        let mut top = None;
        let mut merged = Vec::new();
        for &layer in below.iter() {
            let stat = match self.layers[layer].lstat(&path) {
                Ok(stat) => stat,
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                    ) =>
                {
                    continue
                }
                Err(e) => return Err(e),
            };
            if stat.is_whiteout() {
                break;
            }
            match top {
                None => top = Some((stat, layer)),
                // Under a directory, a lower layer's non-directory ends the merge.
                Some(_) if !stat.is_dir() => break,
                Some(_) => {}
            }
            if !stat.is_dir() {
                break;
            }
            merged.push(layer);
            if is_opaque(self.layers[layer], &path)? {
                break;
            }
        }
        let (stat, top) = top.ok_or_else(not_found)?;
        Ok(Found {
            stat,
            top: Some(top),
            layers: merged.into(),
        })
    }

    fn dir_layers(&self, path: &Path) -> io::Result<Rc<[usize]>> {
        let cached = self.dirs.borrow().get(path).cloned();
        if let Some(layers) = cached {
            return Ok(layers);
        }
        let found = self.find(path)?;
        if !found.stat.is_dir() {
            return Err(io::ErrorKind::NotADirectory.into());
        }
        self.dirs
            .borrow_mut()
            .insert(path.to_path_buf(), found.layers.clone());
        Ok(found.layers)
    }

    fn top(&self, path: &Path) -> io::Result<&'a dyn Tree> {
        if let [only] = self.layers {
            return Ok(*only);
        }
        match self.find(path)?.top {
            Some(top) => Ok(self.layers[top]),
            None => Err(io::ErrorKind::InvalidInput.into()),
        }
    }
}

impl Tree for Merged<'_> {
    fn lstat(&self, path: &Path) -> io::Result<Stat> {
        if let [only] = self.layers {
            return only.lstat(path);
        }
        Ok(self.find(path)?.stat)
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<OsString>> {
        if let [only] = self.layers {
            return only.read_dir(path);
        }
        let layers = self.dir_layers(path)?;
        if let [layer] = layers[..] {
            return self.layers[layer].read_dir(path);
        }
        let mut seen = HashSet::new();
        let mut names = Vec::new();
        for &layer in layers.iter() {
            let tree = self.layers[layer];
            for name in tree.read_dir(path)? {
                if !seen.insert(name.clone()) {
                    continue;
                }
                if !tree.lstat(&join(path, Path::new(&name)))?.is_whiteout() {
                    names.push(name);
                }
            }
        }
        Ok(names)
    }

    fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        self.top(path)?.read_link(path)
    }

    fn xattr(&self, path: &Path, name: &str) -> io::Result<Option<Vec<u8>>> {
        if self.layers.is_empty() {
            return Ok(None);
        }
        self.top(path)?.xattr(path, name)
    }

    fn open(&self, path: &Path) -> io::Result<Box<dyn Read + '_>> {
        self.top(path)?.open(path)
    }
}

/// A depth-first walk in lexical order, as filepath.Walk does it: a directory's names are
/// read and sorted when the walk reaches it.
struct Walker<'a> {
    tree: &'a dyn Tree,
    root: PathBuf,
    stack: Vec<(PathBuf, std::vec::IntoIter<OsString>)>,
}

impl<'a> Walker<'a> {
    fn new(tree: &'a dyn Tree, root: &Path) -> Result<Self> {
        let stat = tree
            .lstat(root)
            .with_context(|| format!("looking at {}", shown(root)))?;
        let mut walker = Walker {
            tree,
            root: root.to_path_buf(),
            stack: Vec::new(),
        };
        if stat.is_dir() {
            walker.descend(PathBuf::new())?;
        }
        Ok(walker)
    }

    fn descend(&mut self, dir: PathBuf) -> Result<()> {
        let full = join(&self.root, &dir);
        let mut names = self
            .tree
            .read_dir(&full)
            .with_context(|| format!("reading the directory {}", shown(&full)))?;
        names.sort();
        self.stack.push((dir, names.into_iter()));
        Ok(())
    }

    /// The next entry, relative to the root of the walk.
    fn next(&mut self) -> Result<Option<(PathBuf, Stat)>> {
        loop {
            let Some((dir, names)) = self.stack.last_mut() else {
                return Ok(None);
            };
            let Some(name) = names.next() else {
                self.stack.pop();
                continue;
            };
            let path = join(dir, Path::new(&name));
            let full = join(&self.root, &path);
            let stat = self
                .tree
                .lstat(&full)
                .with_context(|| format!("looking at {}", shown(&full)))?;
            if stat.is_dir() {
                self.descend(path.clone())?;
            }
            return Ok(Some((path, stat)));
        }
    }

    /// Leaves out what is under the directory `next` has just returned.
    fn skip_dir(&mut self) {
        self.stack.pop();
    }
}

/// The order of two walks: by path with '/' sorting before any other byte.
fn directory_compare(a: &[u8], b: &[u8]) -> Ordering {
    let key = |c: u8| if c == b'/' { 0 } else { c };
    for (&x, &y) in a.iter().zip(b) {
        match key(x).cmp(&key(y)) {
            Ordering::Equal => {}
            other => return other,
        }
    }
    a.len().cmp(&b.len())
}

/// Whether the entry of `b` at `pb` is unchanged from the one of `a` at `pa`. Only when
/// both modification times have no nanoseconds, as after a tar round trip, does the
/// content decide.
fn same_entry(
    a: &dyn Tree,
    pa: &Path,
    sa: &Stat,
    b: &dyn Tree,
    pb: &Path,
    sb: &Stat,
) -> Result<bool> {
    if sa.dev == sb.dev && sa.ino == sb.ino {
        return Ok(true);
    }
    if sa.mode != sb.mode || sa.uid != sb.uid || sa.gid != sb.gid || sa.rdev != sb.rdev {
        return Ok(false);
    }
    let ca = a
        .xattr(pa, CAPABILITY)
        .with_context(|| format!("reading the capabilities of {}", shown(pa)))?;
    let cb = b
        .xattr(pb, CAPABILITY)
        .with_context(|| format!("reading the capabilities of {}", shown(pb)))?;
    if ca.unwrap_or_default() != cb.unwrap_or_default() {
        return Ok(false);
    }
    if sa.is_dir() {
        return Ok(true);
    }
    if sa.size != sb.size || sa.mtime != sb.mtime {
        return Ok(false);
    }
    if sa.mtime_nsec == 0 && sb.mtime_nsec == 0 {
        if sa.is_symlink() {
            let la = a
                .read_link(pa)
                .with_context(|| format!("reading the link {}", shown(pa)))?;
            let lb = b
                .read_link(pb)
                .with_context(|| format!("reading the link {}", shown(pb)))?;
            return Ok(la == lb);
        }
        if sa.size == 0 {
            return Ok(true);
        }
        return same_content(a, pa, b, pb);
    }
    Ok(sa.mtime_nsec == sb.mtime_nsec)
}

fn same_content(a: &dyn Tree, pa: &Path, b: &dyn Tree, pb: &Path) -> Result<bool> {
    let mut fa = a
        .open(pa)
        .with_context(|| format!("opening {}", shown(pa)))?;
    let mut fb = b
        .open(pb)
        .with_context(|| format!("opening {}", shown(pb)))?;
    let mut ba = vec![0u8; 32 * 1024];
    let mut bb = vec![0u8; 32 * 1024];
    loop {
        let na = read_full(&mut fa, &mut ba).with_context(|| format!("reading {}", shown(pa)))?;
        let nb = read_full(&mut fb, &mut bb).with_context(|| format!("reading {}", shown(pb)))?;
        if na != nb || ba[..na] != bb[..nb] {
            return Ok(false);
        }
        if na < ba.len() {
            return Ok(true);
        }
    }
}

fn read_full(r: &mut dyn Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

type ChangeFn<'f> = dyn FnMut(ChangeKind, &Path, Option<&Stat>) -> Result<()> + 'f;

/// The changes from `a` at `a_root` to `b` at `b_root`, found by walking both (diff.go).
/// A deleted directory is reported alone, without what was under it.
fn double_walk(
    a: &dyn Tree,
    a_root: &Path,
    b: &dyn Tree,
    b_root: &Path,
    f: &mut ChangeFn<'_>,
) -> Result<()> {
    let mut wa = Walker::new(a, a_root)?;
    let mut wb = Walker::new(b, b_root)?;
    let (mut f1, mut f2) = (None, None);
    let (mut done1, mut done2) = (false, false);
    let mut rmdir: Option<Vec<u8>> = None;
    loop {
        if f1.is_none() && !done1 {
            f1 = wa.next()?;
            done1 = f1.is_none();
        }
        if f2.is_none() && !done2 {
            f2 = wb.next()?;
            done2 = f2.is_none();
        }
        let mut kind = match (&f1, &f2) {
            (None, None) => break,
            (None, Some(_)) => ChangeKind::Add,
            (Some(_), None) => ChangeKind::Delete,
            (Some((p1, _)), Some((p2, _))) => {
                match directory_compare(p1.as_os_str().as_bytes(), p2.as_os_str().as_bytes()) {
                    Ordering::Less => ChangeKind::Delete,
                    Ordering::Greater => ChangeKind::Add,
                    Ordering::Equal => ChangeKind::Modify,
                }
            }
        };
        let (path, stat) = match kind {
            ChangeKind::Add => {
                rmdir = None;
                let (path, stat) = f2.take().expect("an entry to add");
                (path, Some(stat))
            }
            ChangeKind::Delete => {
                let (path, stat) = f1.take().expect("an entry to delete");
                let bytes = path.as_os_str().as_bytes();
                if rmdir.as_deref().is_some_and(|r| bytes.starts_with(r)) {
                    continue;
                }
                rmdir = stat.is_dir().then(|| [bytes, b"/"].concat());
                (path, None)
            }
            _ => {
                let (p1, s1) = f1.take().expect("a lower entry");
                let (p2, s2) = f2.take().expect("an upper entry");
                let same = same_entry(a, &join(a_root, &p1), &s1, b, &join(b_root, &p2), &s2)?;
                rmdir = (s1.is_dir() && !s2.is_dir())
                    .then(|| [p1.as_os_str().as_bytes(), b"/"].concat());
                if same {
                    if !s2.is_linked() {
                        continue;
                    }
                    kind = ChangeKind::Unmodified;
                }
                (p2, Some(s2))
            }
        };
        f(kind, &path, stat.as_ref())?;
    }
    Ok(())
}

fn has_xattr(tree: &dyn Tree, path: &Path, names: &[&str], nonempty: bool) -> Result<bool> {
    for &name in names {
        let value = tree
            .xattr(path, name)
            .with_context(|| format!("reading the attribute {name} of {}", shown(path)))?;
        if value.is_some_and(|v| !nonempty || !v.is_empty()) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether the upper directory at `path` hides what the lowers have there.
fn opaque_over(upper: &dyn Tree, base: &dyn Tree, path: &Path) -> Result<bool> {
    if !is_opaque(upper, path)
        .with_context(|| format!("reading the opaque attribute of {}", shown(path)))?
    {
        return Ok(false);
    }
    match base.lstat(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("looking up {} in the lowers", shown(path))),
    }
}

/// overlay_linux.go's Changes: the upper directory is the record of the changes, and only
/// an opaque directory needs both trees walked.
fn diff_upper(
    upper: &dyn Tree,
    view: &dyn Tree,
    base: &dyn Tree,
    f: &mut ChangeFn<'_>,
) -> Result<()> {
    let mut walk = Walker::new(upper, Path::new(""))?;
    while let Some((path, stat)) = walk.next()? {
        if stat.is_dir() && has_xattr(upper, &path, &REDIRECT, true)? {
            bail!(
                "{} is a renamed directory (overlayfs redirect_dir), which a layer cannot be made from",
                shown(&path)
            );
        }
        if stat.is_regular() && has_xattr(upper, &path, &METACOPY, false)? {
            bail!(
                "{} holds only metadata (overlayfs metacopy), which a layer cannot be made from",
                shown(&path)
            );
        }
        let kind = if stat.is_whiteout() {
            match base.lstat(&path) {
                Ok(_) => Some(ChangeKind::Delete),
                // Nothing to delete below: the whiteout is not part of the layer.
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("looking up {} in the lowers", shown(&path)))
                }
            }
        } else {
            match base.lstat(&path) {
                Ok(lower) => {
                    // A directory is copied up whenever something under it changes.
                    (!same_entry(base, &path, &lower, view, &path, &stat)?)
                        .then_some(ChangeKind::Modify)
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                    ) =>
                {
                    Some(ChangeKind::Add)
                }
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("looking up {} in the lowers", shown(&path)))
                }
            }
        };
        if let Some(kind) = kind {
            f(kind, &path, Some(&stat))?;
        }
        if stat.is_dir() && opaque_over(upper, base, &path)? {
            double_walk(base, &path, view, &path, &mut |kind, sub, stat| {
                f(kind, &join(&path, sub), stat)
            })?;
            walk.skip_dir();
        }
    }
    Ok(())
}

const TYPE_REG: u8 = b'0';
const TYPE_LINK: u8 = b'1';
const TYPE_SYMLINK: u8 = b'2';
const TYPE_CHAR: u8 = b'3';
const TYPE_BLOCK: u8 = b'4';
const TYPE_DIR: u8 = b'5';
const TYPE_FIFO: u8 = b'6';
const TYPE_XHEADER: u8 = b'x';

const BLOCK_SIZE: u64 = 512;
const NAME_SIZE: usize = 100;
const PREFIX_SIZE: usize = 155;
const MAX_SPECIAL_FILE_SIZE: usize = 1 << 20;
const WHITEOUT_PREFIX: &[u8] = b".wh.";
const PAX_SCHILY_XATTR: &str = "SCHILY.xattr.";
const PAX_BASIC_KEYS: [&str; 10] = [
    "path", "linkpath", "size", "uid", "gid", "uname", "gname", "mtime", "atime", "ctime",
];
/// The Unix second of Go's zero time.Time: a modification time in it truncates to the zero
/// time, which the writer stores as 0.
const GO_ZERO_TIME: i64 = -62_135_596_800;

/// The fields of a Go tar.Header that tar.go sets.
#[derive(Clone, Debug)]
struct Header {
    typeflag: u8,
    name: Vec<u8>,
    linkname: Vec<u8>,
    mode: i64,
    uid: i64,
    gid: i64,
    size: i64,
    /// In seconds; None is the zero time.
    mtime: Option<i64>,
    devmajor: i64,
    devminor: i64,
    /// PAXRecords.
    records: BTreeMap<Vec<u8>, Vec<u8>>,
    /// Format PAX, which tar.go asks for every entry but whiteouts.
    pax: bool,
}

impl Header {
    /// tar.go dates whiteouts at the epoch and leaves the format to the writer.
    fn whiteout(name: Vec<u8>) -> Self {
        Header {
            typeflag: TYPE_REG,
            name,
            linkname: Vec::new(),
            mode: 0,
            uid: 0,
            gid: 0,
            size: 0,
            mtime: Some(0),
            devmajor: 0,
            devminor: 0,
            records: BTreeMap::new(),
            pax: false,
        }
    }

    /// tar.FileInfoHeader with the device numbers tar.go adds and its truncation of the
    /// modification time to the second.
    fn from_stat(stat: &Stat, name: Vec<u8>, linkname: Vec<u8>) -> Result<Self> {
        let (typeflag, size) = match stat.kind() {
            S_IFREG => (TYPE_REG, stat.size as i64),
            S_IFDIR => (TYPE_DIR, 0),
            S_IFLNK => (TYPE_SYMLINK, 0),
            S_IFCHR => (TYPE_CHAR, 0),
            S_IFBLK => (TYPE_BLOCK, 0),
            S_IFIFO => (TYPE_FIFO, 0),
            _ => bail!("unknown file mode {:o}", stat.mode),
        };
        // The reference tests the type bits this loosely, which also catches directories,
        // symlinks and sockets; their rdev is 0.
        let (devmajor, devminor) = if stat.mode & S_IFBLK != 0 || stat.mode & S_IFCHR != 0 {
            (major(stat.rdev), minor(stat.rdev))
        } else {
            (0, 0)
        };
        Ok(Header {
            typeflag,
            name,
            linkname,
            mode: i64::from(stat.mode & 0o7777),
            uid: i64::from(stat.uid),
            gid: i64::from(stat.gid),
            size,
            mtime: (stat.mtime != GO_ZERO_TIME).then_some(stat.mtime),
            devmajor,
            devminor,
            records: BTreeMap::new(),
            pax: true,
        })
    }
}

fn major(dev: u64) -> i64 {
    (((dev & 0x0000_0000_000f_ff00) >> 8) | ((dev & 0xffff_f000_0000_0000) >> 32)) as u32 as i64
}

fn minor(dev: u64) -> i64 {
    ((dev & 0x0000_0000_0000_00ff) | ((dev & 0x0000_0fff_fff0_0000) >> 12)) as u32 as i64
}

fn whiteout_name(path: &[u8]) -> Vec<u8> {
    match path.iter().rposition(|&c| c == b'/') {
        Some(i) => [&path[..=i], WHITEOUT_PREFIX, &path[i + 1..]].concat(),
        None => [WHITEOUT_PREFIX, path].concat(),
    }
}

/// tar.go's ChangeWriter: each change becomes the entries of a layer.
struct ChangeWriter<'a, W: Write> {
    tw: TarWriter<W>,
    /// Where contents, link targets and attributes are read: the upper as the reference's
    /// view mount of it shows it.
    source: &'a dyn Tree,
    inode_src: HashMap<u64, Vec<u8>>,
    inode_refs: HashMap<u64, Vec<Vec<u8>>>,
    added_dirs: HashSet<Vec<u8>>,
}

impl<'a, W: Write> ChangeWriter<'a, W> {
    fn new(out: W, source: &'a dyn Tree) -> Self {
        ChangeWriter {
            tw: TarWriter::new(out),
            source,
            inode_src: HashMap::new(),
            inode_refs: HashMap::new(),
            added_dirs: HashSet::new(),
        }
    }

    fn handle_change(&mut self, kind: ChangeKind, path: &Path, stat: Option<&Stat>) -> Result<()> {
        let p = path.as_os_str().as_bytes();
        if kind == ChangeKind::Delete {
            let hdr = Header::whiteout(whiteout_name(p));
            self.include_parents(&hdr)?;
            return self.tw.write_header(&hdr);
        }
        let Some(stat) = stat else {
            bail!("no metadata for the change of {}", shown(path));
        };
        if stat.kind() == S_IFSOCK {
            return Ok(());
        }
        let linkname = if stat.is_symlink() {
            self.source
                .read_link(path)
                .with_context(|| format!("reading the link {}", shown(path)))?
                .into_os_string()
                .into_vec()
        } else {
            Vec::new()
        };
        let mut name = p.to_vec();
        if stat.is_dir() && !name.ends_with(b"/") {
            name.push(b'/');
        }
        let mut hdr = Header::from_stat(stat, name, linkname)
            .with_context(|| format!("archiving {}", shown(path)))?;

        let mut additional_links = Vec::new();
        if stat.is_linked() {
            if let Some(source) = self.inode_src.get(&stat.ino) {
                hdr.typeflag = TYPE_LINK;
                hdr.linkname.clone_from(source);
                hdr.size = 0;
            } else {
                if kind == ChangeKind::Unmodified {
                    self.inode_refs
                        .entry(stat.ino)
                        .or_default()
                        .push(hdr.name.clone());
                    return Ok(());
                }
                self.inode_src.insert(stat.ino, hdr.name.clone());
                additional_links = self.inode_refs.remove(&stat.ino).unwrap_or_default();
            }
        } else if kind == ChangeKind::Unmodified {
            return Ok(());
        }

        let capability = self
            .source
            .xattr(path, CAPABILITY)
            .with_context(|| format!("reading the capabilities of {}", shown(path)))?;
        if let Some(capability) = capability.filter(|c| !c.is_empty()) {
            hdr.records.insert(
                format!("{PAX_SCHILY_XATTR}{CAPABILITY}").into_bytes(),
                capability,
            );
        }

        self.include_parents(&hdr)?;
        self.tw.write_header(&hdr)?;
        if hdr.typeflag == TYPE_REG && hdr.size > 0 {
            self.copy_file(path, hdr.size as u64)?;
        }

        // Unmodified names of this inode seen before it: the reference links them here,
        // with the rest of this entry's header.
        let source = hdr.name.clone();
        for extra in additional_links {
            hdr.name = extra;
            hdr.typeflag = TYPE_LINK;
            hdr.linkname.clone_from(&source);
            hdr.size = 0;
            self.include_parents(&hdr)?;
            self.tw.write_header(&hdr)?;
        }
        Ok(())
    }

    /// Adds the parent directory of an entry before it, unless the tar already has it.
    fn include_parents(&mut self, hdr: &Header) -> Result<()> {
        let name = trim_trailing_slashes(&hdr.name);
        if let Some(i) = name.iter().rposition(|&c| c == b'/').filter(|&i| i > 0) {
            let parent = &name[..i];
            if !self.added_dirs.contains(parent) {
                self.added_dirs.insert(parent.to_vec());
                let parent = bytes_path(parent);
                // The reference uses stat, which only differs from lstat on a symlink, and
                // the parent of an entry of the walk is a directory.
                let stat = self
                    .source
                    .lstat(parent)
                    .with_context(|| format!("looking at {}", shown(parent)))?;
                self.handle_change(ChangeKind::Modify, parent, Some(&stat))?;
            }
        }
        if hdr.typeflag == TYPE_DIR {
            self.added_dirs.insert(name.to_vec());
        }
        Ok(())
    }

    fn copy_file(&mut self, path: &Path, size: u64) -> Result<()> {
        let mut file = self
            .source
            .open(path)
            .with_context(|| format!("opening {}", shown(path)))?;
        let mut buf = vec![0u8; 32 * 1024];
        let mut left = size;
        while left > 0 {
            let want = buf.len().min(usize::try_from(left).unwrap_or(usize::MAX));
            let n = read_full(&mut file, &mut buf[..want])
                .with_context(|| format!("reading {}", shown(path)))?;
            if n == 0 {
                bail!("{} is shorter than the {size} bytes it had", shown(path));
            }
            self.tw.put(&buf[..n])?;
            left -= n as u64;
        }
        if read_full(&mut file, &mut buf[..1])
            .with_context(|| format!("reading {}", shown(path)))?
            != 0
        {
            bail!("{} grew past the {size} bytes it had", shown(path));
        }
        self.tw.pad(size)
    }
}

/// The subset of Go's archive/tar writer that tar.go uses, with its choice between USTAR
/// and PAX headers and its field formatting.
struct TarWriter<W: Write> {
    out: W,
    size: u64,
    entries: u64,
}

enum Encoding {
    Ustar,
    Pax(BTreeMap<Vec<u8>, Vec<u8>>),
}

impl<W: Write> TarWriter<W> {
    fn new(out: W) -> Self {
        TarWriter {
            out,
            size: 0,
            entries: 0,
        }
    }

    fn put(&mut self, bytes: &[u8]) -> Result<()> {
        self.out.write_all(bytes).context("writing the layer tar")?;
        self.size += bytes.len() as u64;
        Ok(())
    }

    fn pad(&mut self, size: u64) -> Result<()> {
        let n = (BLOCK_SIZE - size % BLOCK_SIZE) % BLOCK_SIZE;
        self.put(&[0u8; BLOCK_SIZE as usize][..n as usize])
    }

    fn write_header(&mut self, hdr: &Header) -> Result<()> {
        let encoding = encoding(hdr).with_context(|| {
            format!(
                "encoding the tar header of {}",
                String::from_utf8_lossy(&hdr.name)
            )
        })?;
        match encoding {
            Encoding::Ustar => {
                let (prefix, name) = split_ustar_path(&hdr.name).unwrap_or((&[], &hdr.name));
                let mut blk = template(hdr, name, &hdr.linkname);
                format_string(&mut blk[345..500], prefix);
                finish_block(&mut blk);
                self.put(&blk)?;
            }
            Encoding::Pax(records) => {
                if !records.is_empty() {
                    let data: Vec<u8> = records
                        .iter()
                        .flat_map(|(k, v)| format_pax_record(k, v))
                        .collect();
                    if data.len() > MAX_SPECIAL_FILE_SIZE {
                        bail!(
                            "the PAX records of {} are too long",
                            String::from_utf8_lossy(&hdr.name)
                        );
                    }
                    self.write_raw_file(&pax_header_name(&hdr.name), &data, TYPE_XHEADER)?;
                }
                let mut blk = template(hdr, &to_ascii(&hdr.name), &to_ascii(&hdr.linkname));
                finish_block(&mut blk);
                self.put(&blk)?;
            }
        }
        self.entries += 1;
        Ok(())
    }

    fn write_raw_file(&mut self, name: &[u8], data: &[u8], flag: u8) -> Result<()> {
        let mut name = to_ascii(name);
        name.truncate(NAME_SIZE);
        let mut blk = [0u8; BLOCK_SIZE as usize];
        blk[156] = flag;
        format_string(&mut blk[0..100], trim_trailing_slashes(&name));
        format_octal(&mut blk[100..108], 0);
        format_octal(&mut blk[108..116], 0);
        format_octal(&mut blk[116..124], 0);
        format_octal(&mut blk[124..136], data.len() as i64);
        format_octal(&mut blk[136..148], 0);
        finish_block(&mut blk);
        self.put(&blk)?;
        self.put(data)?;
        self.pad(data.len() as u64)
    }

    fn finish(mut self) -> Result<TarSummary> {
        self.put(&[0u8; 2 * BLOCK_SIZE as usize])?;
        Ok(TarSummary {
            size: self.size,
            entries: self.entries,
        })
    }
}

/// allowedFormats in common.go, for the fields tar.go sets: USTAR when everything fits,
/// otherwise PAX with the records of what does not.
fn encoding(h: &Header) -> Result<Encoding> {
    let mut ustar = true;
    let mut pax = true;
    let mut prefer_pax = false;
    let mut records = BTreeMap::new();
    if !is_ascii(&h.name) || h.name.len() > NAME_SIZE {
        if split_ustar_path(&h.name).is_none() {
            ustar = false;
        }
        records.insert(b"path".to_vec(), h.name.clone());
    }
    if !is_ascii(&h.linkname) || h.linkname.len() > NAME_SIZE {
        ustar = false;
        records.insert(b"linkpath".to_vec(), h.linkname.clone());
    }
    let numbers = [
        (h.mode, 8, None),
        (h.uid, 8, Some("uid")),
        (h.gid, 8, Some("gid")),
        (h.size, 12, Some("size")),
        (h.devmajor, 8, None),
        (h.devminor, 8, None),
    ];
    for (value, width, key) in numbers {
        if !fits_in_octal(width, value) {
            ustar = false;
            match key {
                Some(key) => {
                    records.insert(key.as_bytes().to_vec(), value.to_string().into_bytes());
                }
                None => pax = false,
            }
        }
    }
    if let Some(mtime) = h.mtime {
        if !fits_in_octal(12, mtime) {
            ustar = false;
            prefer_pax = true;
            records.insert(b"mtime".to_vec(), mtime.to_string().into_bytes());
        }
    }
    if matches!(h.typeflag, TYPE_REG | TYPE_CHAR | TYPE_BLOCK | TYPE_FIFO) && h.name.ends_with(b"/")
    {
        bail!("a file name may not end in a slash");
    }
    if h.size < 0 {
        bail!("negative size");
    }
    if !h.records.is_empty() {
        for (k, v) in &h.records {
            let basic = PAX_BASIC_KEYS.iter().any(|b| b.as_bytes() == k.as_slice());
            if records.contains_key(k) || basic || k.starts_with(b"GNU.sparse.") {
                continue;
            }
            records.insert(k.clone(), v.clone());
        }
        ustar = false;
    }
    if let Some(k) = records.keys().find(|k| !valid_pax_record(k, &records[*k])) {
        bail!("invalid PAX record {}", String::from_utf8_lossy(k));
    }
    if h.pax && prefer_pax {
        ustar = false;
    }
    if ustar {
        Ok(Encoding::Ustar)
    } else if pax {
        Ok(Encoding::Pax(records))
    } else {
        bail!("neither USTAR nor PAX can encode the header");
    }
}

/// The V7 and USTAR fields every header of the writer fills in.
fn template(h: &Header, name: &[u8], linkname: &[u8]) -> [u8; BLOCK_SIZE as usize] {
    let mut blk = [0u8; BLOCK_SIZE as usize];
    blk[156] = h.typeflag;
    format_string(&mut blk[0..100], name);
    format_string(&mut blk[157..257], linkname);
    format_octal(&mut blk[100..108], h.mode);
    format_octal(&mut blk[108..116], h.uid);
    format_octal(&mut blk[116..124], h.gid);
    format_octal(&mut blk[124..136], h.size);
    format_octal(&mut blk[136..148], h.mtime.unwrap_or(0));
    // User and group names stay empty: the reference never looks them up.
    format_octal(&mut blk[329..337], h.devmajor);
    format_octal(&mut blk[337..345], h.devminor);
    blk
}

/// The USTAR magic, which PAX headers carry too, and the checksum.
fn finish_block(blk: &mut [u8; BLOCK_SIZE as usize]) {
    blk[257..263].copy_from_slice(b"ustar\0");
    blk[263..265].copy_from_slice(b"00");
    let sum: i64 = blk
        .iter()
        .enumerate()
        .map(|(i, &c)| i64::from(if (148..156).contains(&i) { b' ' } else { c }))
        .sum();
    format_octal(&mut blk[148..155], sum);
    blk[155] = b' ';
}

fn trim_trailing_slashes(s: &[u8]) -> &[u8] {
    let end = s.iter().rposition(|&c| c != b'/').map_or(0, |i| i + 1);
    &s[..end]
}

/// Go ranges over a string by UTF-8 rune: every byte of a multibyte sequence, and every
/// byte that is not valid UTF-8, counts as non-ASCII.
fn is_ascii(s: &[u8]) -> bool {
    s.iter().all(|&c| c != 0 && c < 0x80)
}

fn to_ascii(s: &[u8]) -> Vec<u8> {
    s.iter().copied().filter(|&c| c != 0 && c < 0x80).collect()
}

fn fits_in_octal(width: usize, x: i64) -> bool {
    x >= 0 && (width >= 22 || x < 1i64 << ((width - 1) * 3))
}

fn format_string(b: &mut [u8], s: &[u8]) {
    let n = s.len().min(b.len());
    b[..n].copy_from_slice(&s[..n]);
    if s.len() < b.len() {
        b[s.len()] = 0;
    }
    // A truncated name must not look like a directory to readers that ignore PAX.
    if s.len() > b.len() && b[b.len() - 1] == b'/' {
        let kept = trim_trailing_slashes(&s[..b.len() - 1]).len();
        b[kept] = 0;
    }
}

fn format_octal(b: &mut [u8], x: i64) {
    let x = if fits_in_octal(b.len(), x) { x } else { 0 };
    let digits = format!("{x:o}");
    let mut field = vec![b'0'; b.len().saturating_sub(digits.len() + 1)];
    field.extend_from_slice(digits.as_bytes());
    format_string(b, &field);
}

fn split_ustar_path(name: &[u8]) -> Option<(&[u8], &[u8])> {
    let mut length = name.len();
    if length <= NAME_SIZE || !is_ascii(name) {
        return None;
    }
    if length > PREFIX_SIZE + 1 {
        length = PREFIX_SIZE + 1;
    } else if name[length - 1] == b'/' {
        length -= 1;
    }
    let i = name[..length].iter().rposition(|&c| c == b'/')?;
    let suffix = name.len() - i - 1;
    if i == 0 || suffix > NAME_SIZE || suffix == 0 || i > PREFIX_SIZE {
        return None;
    }
    Some((&name[..i], &name[i + 1..]))
}

fn valid_pax_record(k: &[u8], v: &[u8]) -> bool {
    if k.is_empty() || k.contains(&b'=') {
        return false;
    }
    match k {
        b"path" | b"linkpath" | b"uname" | b"gname" => !v.contains(&0),
        _ => !k.contains(&0),
    }
}

/// "%d %s=%s\n", the length counting its own digits.
fn format_pax_record(k: &[u8], v: &[u8]) -> Vec<u8> {
    let record = |size: usize| [format!("{size} ").as_bytes(), k, b"=", v, b"\n"].concat();
    let mut size = k.len() + v.len() + 3;
    size += size.to_string().len();
    let first = record(size);
    if first.len() == size {
        first
    } else {
        record(first.len())
    }
}

/// path.Join(dir, "PaxHeaders.0", file) for a clean relative name.
fn pax_header_name(name: &[u8]) -> Vec<u8> {
    let (dir, file) = match name.iter().rposition(|&c| c == b'/') {
        Some(i) => (&name[..i], &name[i + 1..]),
        None => (&name[..0], name),
    };
    let dir = trim_trailing_slashes(dir);
    let mut out = Vec::with_capacity(name.len() + 14);
    if !dir.is_empty() {
        out.extend_from_slice(dir);
        out.push(b'/');
    }
    out.extend_from_slice(b"PaxHeaders.0");
    if !file.is_empty() {
        out.push(b'/');
        out.extend_from_slice(file);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::unix::fs::PermissionsExt;

    use nix::sys::stat::{mknod, utimensat, Mode, SFlag, UtimensatFlags};
    use nix::sys::time::TimeSpec;

    /// The scenarios the reference was run on. Each line is TREE PATH KIND [KEY=VALUE]...,
    /// with TREE upper or lower (a single layer) and KIND one of dir, file, symlink, fifo,
    /// char, block, socket, whiteout, link. In paths and in data, target and of values,
    /// {C*N} stands for N copies of C and %XX for the byte XX; mtime is SEC.NSEC as a
    /// timespec; opaque=y sets user.overlay.opaque and opaque=trusted the trusted name.
    const SPEC: &str = r"
scenario empty

scenario basic
lower etc dir
lower etc/hostname file data=old mtime=1600000000.100000000
lower etc/keep file data=keep
lower var dir
lower var/log dir
lower var/log/old file data=x
lower usr dir
lower usr/bin dir
lower usr/bin/tool file data=tool mode=0755
lower toplevel-gone dir
lower toplevel-gone/inner file data=i
upper etc dir mtime=1700000100.000000000
upper etc/hostname file data=newhost mtime=1700000001.200000000
upper etc/new file data=hello%0aworld%0a
upper var dir
upper var/log dir
upper var/log/never whiteout
upper var/log/old whiteout
upper usr dir
upper usr/bin dir
upper usr/bin/added file fill=1500 mode=0755
upper tmp dir mode=1777
upper sym symlink target=etc/hostname
upper fifo fifo mode=0640
upper null char dev=1:3 mode=0666
upper sda block dev=8:0 mode=0660
upper bigdev char dev=4095:1048575
upper sock socket
upper setuid file data=s mode=4755
upper setgid-dir dir mode=2775 uid=1000 gid=1000
upper owned file data=o uid=1000 gid=100 xattr=user.note:ignored
upper toplevel-gone whiteout

scenario names
upper {A*99} dir
upper {A*99}/{b*120} file data=x
upper {p*60} dir
upper {p*60}/{q*50} dir
upper {p*60}/{q*50}/{r*30} file data=split
upper {s*155} dir
upper {s*155}/{t*100} file data=edge
upper {u*120} file data=long
upper {x*101} dir
upper caf%c3%a9 file data=utf8
upper bad%ff%fename file data=invalid
upper longtarget symlink target={T*150}
upper utf8target symlink target=t%c3%a1rget
upper {L*120} file data=linked
upper short link of={L*120}

scenario longwhiteouts
lower {w*120} file data=w
lower {p*60} dir
lower {p*60}/{q*50} dir
lower {p*60}/{q*50}/gone file data=g
lower {p*60}/{q*50}/{g*96} file data=g
upper {w*120} whiteout
upper {p*60} dir
upper {p*60}/{q*50} dir
upper {p*60}/{q*50}/gone whiteout
upper {p*60}/{q*50}/{g*96} whiteout

scenario links
upper data dir
upper data/a file data=shared
upper data/b link of=data/a
upper data/c link of=data/a
upper other file data=x mode=0600
upper zz link of=other
upper b-dir dir
upper b-dir/orig file data=o
upper a-dir dir
upper a-dir/alias link of=b-dir/orig

scenario caps
upper ping file data=p mode=0755 cap=0100000200040000000000000000000000000000
upper pinglink link of=ping
upper plain file data=q xattr=user.foo:bar
upper {C*120} file data=c cap=0100000200040000000000000000000000000000
upper capuid file data=u uid=3000000 cap=0100000200040000000000000000000000000000

scenario numbers
upper biguid file data=u uid=3000000 gid=2097152
upper maxoctal file data=m uid=2097151 gid=2097151
upper negative file data=n mtime=-2.500000000
upper epoch file data=e mtime=0.000000000
upper future file data=f mtime=8589934592.000000000
upper lastoctal file data=g mtime=8589934591.999999999
upper bigdir dir uid=4000000

scenario unchanged
lower f-same file data=abc mtime=1650000000.123456789
upper f-same file data=abc mtime=1650000000.123456789
lower f-trunc-same file data=abc mtime=1650000000.000000000
upper f-trunc-same file data=abc mtime=1650000000.000000000
lower f-trunc-diff file data=abc mtime=1650000000.000000000
upper f-trunc-diff file data=abd mtime=1650000000.000000000
lower f-nsec-diff file data=abc mtime=1650000000.000000001
upper f-nsec-diff file data=abc mtime=1650000000.000000002
lower f-one-trunc file data=abc mtime=1650000000.000000000
upper f-one-trunc file data=abc mtime=1650000000.500000000
lower f-sec-diff file data=abc mtime=1650000000.500000000
upper f-sec-diff file data=abc mtime=1650000001.500000000
lower f-size file data=abc
upper f-size file data=abcd
lower f-mode file data=abc mode=0600
upper f-mode file data=abc mode=0644
lower f-owner file data=abc uid=1
upper f-owner file data=abc uid=2
lower f-empty file mtime=1650000000.000000000
upper f-empty file mtime=1650000000.000000000
lower f-cap file data=abc mtime=1650000000.000000000
upper f-cap file data=abc mtime=1650000000.000000000 cap=0100000200040000000000000000000000000000
lower f-big-same file fill=100000 mtime=1650000000.000000000
upper f-big-same file fill=100000 mtime=1650000000.000000000
lower l-same symlink target=abc mtime=1650000000.000000000
upper l-same symlink target=abc mtime=1650000000.000000000
lower l-diff symlink target=abc mtime=1650000000.000000000
upper l-diff symlink target=abd mtime=1650000000.000000000
lower l-nsec symlink target=abc mtime=1650000000.300000000
upper l-nsec symlink target=abd mtime=1650000000.300000000
lower d-mtime dir mtime=1.000000000
upper d-mtime dir mtime=2.000000000
lower d-mode dir mode=0755
upper d-mode dir mode=0700
lower d-owner dir gid=0
upper d-owner dir gid=5
lower c-same char dev=1:3
upper c-same char dev=1:3
lower c-rdev char dev=1:3
upper c-rdev char dev=1:5
lower fifo-same fifo
upper fifo-same fifo
lower f-type file data=x
upper f-type dir
upper f-type/child file data=c
lower d-type dir
lower d-type/child file data=c
upper d-type file data=d

scenario opaque
lower o dir
lower o/a file data=a
lower o/b dir
lower o/b/c file data=c
lower o/b/d dir
lower o/keep file data=k mtime=1650000000.123456789
lower o/mod file data=m
lower o/zlast file data=z
upper o dir opaque=y
upper o/keep file data=k mtime=1650000000.123456789
upper o/mod file data=mm
upper o/new file data=n
upper p dir opaque=y
upper p/x file data=x
upper p/never whiteout
lower q dir
lower q/old file data=o
upper q dir mode=0700 opaque=y
upper q/fresh file data=f
lower r dir
lower r/sub dir
lower r/sub/deep file data=d
lower r/sub-x file data=y
upper r dir opaque=y
lower s symlink target=o
upper s dir opaque=y
upper s/inner file data=i

scenario opaquelinks
lower o dir
lower o/l1 file data=L mtime=1650000000.100000000
lower o/l2 file data=L mtime=1650000000.100000000
lower o/m1 file data=M mtime=1650000000.100000000
upper o dir opaque=y
upper o/l1 file data=L mtime=1650000000.100000000
upper o/l2 link of=o/l1
upper o/l3 link of=o/l1
upper o/m0 file data=M mtime=1650000000.100000000
upper o/m1 link of=o/m0

scenario replace
lower f2d file data=x
upper f2d dir opaque=y
upper f2d/child file data=c
lower d2f dir
lower d2f/x file data=x
upper d2f file data=y

scenario upperonly
upper a dir
upper a/b dir mode=0700
upper a/b/c file data=c
upper a/sock socket
upper a/gone whiteout
upper a/opq dir opaque=y
upper a/opq/f file data=f
upper z file fill=70000

scenario parents
lower a dir
lower a/b dir mode=0750
lower a/b/c dir uid=7
upper a dir
upper a/b dir mode=0750
upper a/b/c dir uid=7
upper a/b/c/new file data=new
upper a/b/c/sock socket
upper d dir
upper d/sock socket

scenario opaquecaps
lower o dir
lower o/l1 file data=L mtime=1650000000.100000000 cap=0100000200040000000000000000000000000000
upper o dir opaque=y
upper o/l1 file data=L mtime=1650000000.100000000 cap=0100000200040000000000000000000000000000
upper o/l3 link of=o/l1

scenario edges
upper {n*100} file data=exactly-100
upper t100 symlink target={T*100}
upper t101 symlink target={T*101}
upper empty-file file
upper block512 file fill=512
lower keep dir
lower keep/deeper dir
lower keep/deeper/o dir
lower keep/deeper/o/gone file data=g
upper keep dir
upper keep/deeper dir
upper keep/deeper/o dir opaque=y
upper keep/deeper/o/new file data=n
lower caf%c3%a9-gone file data=x
upper caf%c3%a9-gone whiteout
lower {W*97} file data=w
upper {W*97} whiteout
";

    /// What the reference wrote for each scenario of SPEC: the sha256 and the size of the
    /// tar. It ran as root on tmpfs with the lower bind-mounted read-only, as a lower
    /// snapshot of one layer is, and the upper read through the overlay view WriteUpperdir
    /// mounts.
    const REFERENCE: [(&str, &str, u64); 15] = [
        (
            "empty",
            "5f70bf18a086007016e948b04aed3b82103a36bea41755b6cddfaf10ace3c6ef",
            1024,
        ),
        (
            "basic",
            "5ec0d7af853a03e07718b2079acdf5180f7f340fe91119b60844020ff61c4fa8",
            14336,
        ),
        (
            "names",
            "c33494cbe45758b5ac260c7d66d9302e8a6513676f2866adaf51bcbf135d0ce3",
            22528,
        ),
        (
            "longwhiteouts",
            "695dab11274d43a273359ed275bd29d09b382b83c8f3ce92fe1a6dd6409ea4c8",
            4608,
        ),
        (
            "links",
            "5e2924de77bbacaaab22001e08be3b3e17a2838e538066d5830b135a4eaed0c6",
            7680,
        ),
        (
            "caps",
            "fe1b1121c98b86ca7b5af7eaaeafa69a22cea2cce7f86b9cd6c49589a16d3f03",
            9728,
        ),
        (
            "numbers",
            "81fd90fcdd19b1952c0e0fc78ad345e929d85bce0979d2b5504b40836a60286a",
            11776,
        ),
        (
            "unchanged",
            "056fa9e2ac696b8e044d4f5ddc7e7da90e349bedf125ef418657bdfc8c7a17da",
            14848,
        ),
        (
            "opaque",
            "cc45e4698f54f461c9285a9addb1241a894a8c20662ac5ea001fb19708c7cce7",
            11776,
        ),
        (
            "opaquelinks",
            "caeb37674f865e00d3c906d9a2dc48440c08b439428bd1b57f95101ce39b5ed0",
            5120,
        ),
        (
            "replace",
            "d2884edaa744f88c58df0fb6f15ff830e2816d1f37cdf610d17d3ccff2c39d0e",
            3584,
        ),
        (
            "upperonly",
            "90893b7d2b0df96b2297d42205f6fc62ec4947653fa492b0102d018a71ecfe63",
            75264,
        ),
        (
            "parents",
            "45b971489c46a3f14a449cb5dcfb245706626c0ab419a8251bcd3c723cc462e8",
            4096,
        ),
        (
            "opaquecaps",
            "b8ad79e3535abe5c05bf700e24a180e28ba6745523f721d61b2c539994e24557",
            5120,
        ),
        (
            "edges",
            "2b6227e29a69271db54b4e60797f11a923c433b9a328c5875183dbbe9c85163c",
            11776,
        ),
    ];

    /// The blob the reference compresses an empty change set to, the well-known empty
    /// layer sha256:4f4fb700ef54461cfa02571ae0db9a0dc1e0cdb5577484a6d75e68dc38e8acc1.
    const REFERENCE_EMPTY_BLOB: [u8; 32] = [
        0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0x62, 0x18, 0x05, 0xa3, 0x60,
        0x14, 0x8c, 0x58, 0x00, 0x08, 0x00, 0x00, 0xff, 0xff, 0x2e, 0xaf, 0xb5, 0xef, 0x00, 0x04,
        0x00, 0x00,
    ];

    /// The blob flate2 compresses an empty change set to. Pinned so that a change of the
    /// compressor, which changes every layer digest, shows.
    const EMPTY_BLOB_DIGEST: &str =
        "sha256:acb5604b126ac894c1eb11c4575bf2072fea61232a888e453770c79d7ed56419";
    const EMPTY_BLOB_SIZE: u64 = 35;

    // ---- fake trees ----

    struct Inode {
        stat: Stat,
        data: Vec<u8>,
        target: Vec<u8>,
        xattrs: BTreeMap<String, Vec<u8>>,
    }

    struct MemTree {
        dev: u64,
        next_ino: u64,
        root: Inode,
        paths: BTreeMap<Vec<u8>, u64>,
        inodes: HashMap<u64, Inode>,
    }

    impl MemTree {
        fn new(dev: u64) -> Self {
            MemTree {
                dev,
                next_ino: 2,
                root: Inode {
                    stat: Stat {
                        dev,
                        ino: 1,
                        mode: S_IFDIR | 0o755,
                        nlink: 2,
                        ..Stat::default()
                    },
                    data: Vec::new(),
                    target: Vec::new(),
                    xattrs: BTreeMap::new(),
                },
                paths: BTreeMap::new(),
                inodes: HashMap::new(),
            }
        }

        fn insert(&mut self, path: &[u8], mut inode: Inode) {
            inode.stat.dev = self.dev;
            inode.stat.ino = self.next_ino;
            inode.stat.nlink = if inode.stat.is_dir() { 2 } else { 1 };
            self.next_ino += 1;
            self.paths.insert(path.to_vec(), inode.stat.ino);
            self.inodes.insert(inode.stat.ino, inode);
        }

        fn link(&mut self, path: &[u8], of: &[u8]) {
            let ino = self.paths[of];
            self.inodes.get_mut(&ino).unwrap().stat.nlink += 1;
            self.paths.insert(path.to_vec(), ino);
        }

        fn inode_mut(&mut self, path: &str) -> &mut Inode {
            let ino = self.paths[path.as_bytes()];
            self.inodes.get_mut(&ino).unwrap()
        }

        fn get(&self, path: &Path) -> io::Result<&Inode> {
            let bytes = path.as_os_str().as_bytes();
            if bytes.is_empty() {
                return Ok(&self.root);
            }
            for (i, _) in bytes.iter().enumerate().filter(|(_, &c)| c == b'/') {
                match self.paths.get(&bytes[..i]) {
                    None => return Err(not_found()),
                    Some(ino) if !self.inodes[ino].stat.is_dir() => {
                        return Err(io::ErrorKind::NotADirectory.into())
                    }
                    Some(_) => {}
                }
            }
            self.paths
                .get(bytes)
                .map(|ino| &self.inodes[ino])
                .ok_or_else(not_found)
        }
    }

    impl Tree for MemTree {
        fn lstat(&self, path: &Path) -> io::Result<Stat> {
            Ok(self.get(path)?.stat)
        }

        fn read_dir(&self, path: &Path) -> io::Result<Vec<OsString>> {
            if !self.get(path)?.stat.is_dir() {
                return Err(io::ErrorKind::NotADirectory.into());
            }
            let prefix = path.as_os_str().as_bytes();
            Ok(self
                .paths
                .keys()
                .filter_map(|k| {
                    if prefix.is_empty() {
                        Some(&k[..])
                    } else {
                        k.strip_prefix(prefix).and_then(|r| r.strip_prefix(b"/"))
                    }
                })
                .filter(|rest| !rest.contains(&b'/'))
                .map(|rest| OsString::from_vec(rest.to_vec()))
                .collect())
        }

        fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
            let inode = self.get(path)?;
            if !inode.stat.is_symlink() {
                return Err(io::ErrorKind::InvalidInput.into());
            }
            Ok(PathBuf::from(OsString::from_vec(inode.target.clone())))
        }

        fn xattr(&self, path: &Path, name: &str) -> io::Result<Option<Vec<u8>>> {
            Ok(self.get(path)?.xattrs.get(name).cloned())
        }

        fn open(&self, path: &Path) -> io::Result<Box<dyn Read + '_>> {
            let inode = self.get(path)?;
            if !inode.stat.is_regular() {
                return Err(io::ErrorKind::InvalidInput.into());
            }
            Ok(Box::new(&inode.data[..]))
        }
    }

    // ---- the spec language ----

    struct Entry {
        tree: String,
        path: Vec<u8>,
        kind: String,
        kv: BTreeMap<String, String>,
    }

    fn expand(s: &str) -> Vec<u8> {
        let mut text = String::new();
        let mut rest = s;
        while let Some(i) = rest.find('{') {
            text.push_str(&rest[..i]);
            let j = i + rest[i..].find('}').unwrap();
            let (c, n) = rest[i + 1..j].split_once('*').unwrap();
            text.push_str(&c.repeat(n.parse().unwrap()));
            rest = &rest[j + 1..];
        }
        text.push_str(rest);
        let bytes = text.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' && i + 2 < bytes.len() {
                out.push(u8::from_str_radix(&text[i + 1..i + 3], 16).unwrap());
                i += 3;
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        out
    }

    fn parse(spec: &str) -> Vec<(String, Vec<Entry>)> {
        let mut out: Vec<(String, Vec<Entry>)> = Vec::new();
        for line in spec.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields[0] == "scenario" {
                out.push((fields[1].to_string(), Vec::new()));
                continue;
            }
            if out.is_empty() {
                out.push((String::new(), Vec::new()));
            }
            let kv = fields[3..]
                .iter()
                .map(|f| {
                    let (k, v) = f.split_once('=').unwrap();
                    (k.to_string(), v.to_string())
                })
                .collect();
            out.last_mut().unwrap().1.push(Entry {
                tree: fields[0].to_string(),
                path: expand(fields[1]),
                kind: fields[2].to_string(),
                kv,
            });
        }
        if out.is_empty() {
            out.push((String::new(), Vec::new()));
        }
        out
    }

    fn content(e: &Entry) -> Vec<u8> {
        if let Some(data) = e.kv.get("data") {
            expand(data)
        } else if let Some(n) = e.kv.get("fill") {
            (0..n.parse::<usize>().unwrap())
                .map(|i| (i % 251) as u8)
                .collect()
        } else {
            Vec::new()
        }
    }

    fn mtime(e: &Entry) -> (i64, i64) {
        let s =
            e.kv.get("mtime")
                .map_or("1700000000.250000000", String::as_str);
        let (sec, nsec) = s.split_once('.').unwrap();
        assert_eq!(nsec.len(), 9, "mtime {s}");
        (sec.parse().unwrap(), nsec.parse().unwrap())
    }

    fn kind_bits(e: &Entry) -> (u32, u32) {
        match e.kind.as_str() {
            "dir" => (S_IFDIR, 0o755),
            "file" => (S_IFREG, 0o644),
            "symlink" => (S_IFLNK, 0o777),
            "fifo" => (S_IFIFO, 0o644),
            "char" => (S_IFCHR, 0o600),
            "block" => (S_IFBLK, 0o600),
            "socket" => (S_IFSOCK, 0o755),
            "whiteout" => (S_IFCHR, 0),
            other => panic!("unknown kind {other}"),
        }
    }

    fn perm(e: &Entry) -> u32 {
        let (_, default) = kind_bits(e);
        match (e.kind.as_str(), e.kv.get("mode")) {
            ("symlink" | "whiteout", _) | (_, None) => default,
            (_, Some(mode)) => u32::from_str_radix(mode, 8).unwrap(),
        }
    }

    fn number(e: &Entry, key: &str) -> u32 {
        e.kv.get(key).map_or(0, |v| v.parse().unwrap())
    }

    fn makedev(major: u64, minor: u64) -> u64 {
        ((major & 0xfff) << 8)
            | ((major & 0xffff_f000) << 32)
            | (minor & 0xff)
            | ((minor & 0xffff_ff00) << 12)
    }

    fn build(entries: &[&Entry], dev: u64) -> MemTree {
        let mut tree = MemTree::new(dev);
        for e in entries {
            if e.kind == "link" {
                tree.link(&e.path, &expand(&e.kv["of"]));
                continue;
            }
            let (kind, _) = kind_bits(e);
            let data = if kind == S_IFREG {
                content(e)
            } else {
                Vec::new()
            };
            let target = e.kv.get("target").map(|t| expand(t)).unwrap_or_default();
            let rdev = e.kv.get("dev").map_or(0, |d| {
                let (major, minor) = d.split_once(':').unwrap();
                makedev(major.parse().unwrap(), minor.parse().unwrap())
            });
            let size = match kind {
                S_IFREG => data.len(),
                S_IFLNK => target.len(),
                _ => 0,
            } as u64;
            let (mtime, mtime_nsec) = mtime(e);
            let mut xattrs = BTreeMap::new();
            if let Some(cap) = e.kv.get("cap") {
                xattrs.insert(CAPABILITY.to_string(), hex::decode(cap).unwrap());
            }
            if let Some(x) = e.kv.get("xattr") {
                let (k, v) = x.split_once(':').unwrap();
                xattrs.insert(k.to_string(), v.as_bytes().to_vec());
            }
            match e.kv.get("opaque").map(String::as_str) {
                Some("y") => {
                    xattrs.insert("user.overlay.opaque".into(), b"y".to_vec());
                }
                Some("trusted") => {
                    xattrs.insert("trusted.overlay.opaque".into(), b"y".to_vec());
                }
                Some(other) => panic!("opaque={other}"),
                None => {}
            }
            if let Some(r) = e.kv.get("redirect") {
                xattrs.insert("trusted.overlay.redirect".into(), r.as_bytes().to_vec());
            }
            if e.kv.contains_key("metacopy") {
                xattrs.insert("trusted.overlay.metacopy".into(), Vec::new());
            }
            if e.path == b"." {
                tree.root.xattrs = xattrs;
                continue;
            }
            let stat = Stat {
                mode: kind | perm(e),
                uid: number(e, "uid"),
                gid: number(e, "gid"),
                rdev,
                size,
                mtime,
                mtime_nsec,
                ..Stat::default()
            };
            tree.insert(
                &e.path,
                Inode {
                    stat,
                    data,
                    target,
                    xattrs,
                },
            );
        }
        tree
    }

    /// The trees of each scenario, by name; upper and lower always exist.
    fn scenarios(spec: &str, dev_base: u64) -> Vec<(String, BTreeMap<String, MemTree>)> {
        parse(spec)
            .into_iter()
            .map(|(name, entries)| {
                let mut names: Vec<&str> = vec!["upper", "lower"];
                for e in &entries {
                    if !names.contains(&e.tree.as_str()) {
                        names.push(&e.tree);
                    }
                }
                let trees = names
                    .iter()
                    .enumerate()
                    .map(|(i, tree)| {
                        let mine: Vec<&Entry> =
                            entries.iter().filter(|e| e.tree == *tree).collect();
                        (tree.to_string(), build(&mine, dev_base + i as u64))
                    })
                    .collect();
                (name, trees)
            })
            .collect()
    }

    fn trees(spec: &str) -> BTreeMap<String, MemTree> {
        scenarios(spec, 1).pop().unwrap().1
    }

    fn layer(upper: &dyn Tree, lowers: &[&dyn Tree]) -> Vec<u8> {
        let mut out = Vec::new();
        let summary = write_layer(upper, lowers, &mut out).unwrap();
        assert_eq!(summary.size, out.len() as u64);
        assert_eq!(summary.entries, listing(&out).len() as u64);
        out
    }

    fn scenario_layer(trees: &BTreeMap<String, MemTree>) -> Vec<u8> {
        layer(&trees["upper"], &[&trees["lower"]])
    }

    fn sha256_hex(data: &[u8]) -> String {
        hex::encode(Sha256::digest(data))
    }

    /// The entries of a tar as the reference's tests check them.
    fn listing(data: &[u8]) -> Vec<String> {
        let mut archive = tar::Archive::new(data);
        archive
            .entries()
            .unwrap()
            .map(|entry| {
                let mut entry = entry.unwrap();
                let header = entry.header().clone();
                let path = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
                let link = entry
                    .link_name_bytes()
                    .map(|l| String::from_utf8_lossy(&l).into_owned())
                    .unwrap_or_default();
                let mode = header.mode().unwrap();
                match header.entry_type() {
                    tar::EntryType::Directory => format!("dir {path} {mode:o}"),
                    tar::EntryType::Regular => {
                        let mut body = Vec::new();
                        entry.read_to_end(&mut body).unwrap();
                        let base = path.rsplit('/').next().unwrap();
                        if let Some(gone) = base.strip_prefix(".wh.") {
                            let dir = &path[..path.len() - base.len()];
                            format!("whiteout {dir}{gone}")
                        } else {
                            format!("file {path} {mode:o} {}", String::from_utf8_lossy(&body))
                        }
                    }
                    tar::EntryType::Link => format!("link {path} -> {link}"),
                    tar::EntryType::Symlink => format!("symlink {path} -> {link}"),
                    tar::EntryType::Char | tar::EntryType::Block => format!(
                        "dev {path} {}:{}",
                        header.device_major().unwrap().unwrap(),
                        header.device_minor().unwrap().unwrap()
                    ),
                    tar::EntryType::Fifo => format!("fifo {path}"),
                    other => format!("{other:?} {path}"),
                }
            })
            .collect()
    }

    fn kind_name(kind: ChangeKind) -> &'static str {
        match kind {
            ChangeKind::Unmodified => "unmodified",
            ChangeKind::Add => "add",
            ChangeKind::Modify => "modify",
            ChangeKind::Delete => "delete",
        }
    }

    /// The changes of the upper over the lower, checked as overlay_linux_test.go checks
    /// them: an entry that is not deleted carries the metadata of the upper.
    fn change_list(spec: &str) -> Vec<String> {
        let t = trees(spec);
        let upper = &t["upper"];
        let mut out = Vec::new();
        changes(upper, &[&t["lower"]], |kind, path, stat| {
            if kind != ChangeKind::Delete {
                assert_eq!(stat.copied(), View(upper).lstat(path).ok(), "{path:?}");
            }
            out.push(format!("{} /{}", kind_name(kind), path.display()));
            Ok(())
        })
        .unwrap();
        out
    }

    // ---- against the reference ----

    #[test]
    fn layers_match_the_reference() {
        let mut failures = Vec::new();
        let all = scenarios(SPEC, 1);
        assert_eq!(all.len(), REFERENCE.len());
        for (name, trees) in &all {
            let (_, digest, size) = REFERENCE.iter().find(|(n, _, _)| n == name).unwrap();
            let out = scenario_layer(trees);
            if sha256_hex(&out) != *digest || out.len() as u64 != *size {
                failures.push(format!(
                    "{name}: {} bytes, sha256:{}\n  {}",
                    out.len(),
                    sha256_hex(&out),
                    listing(&out).join("\n  ")
                ));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn output_does_not_depend_on_inode_numbers() {
        for ((name, a), (_, b)) in scenarios(SPEC, 1).iter().zip(&scenarios(SPEC, 500)) {
            assert_eq!(scenario_layer(a), scenario_layer(b), "{name}");
        }
    }

    #[test]
    fn output_is_deterministic() {
        let all = scenarios(SPEC, 1);
        for (name, trees) in &all {
            assert_eq!(scenario_layer(trees), scenario_layer(trees), "{name}");
            let mut blob1 = Vec::new();
            let mut blob2 = Vec::new();
            let c1 = commit(&trees["upper"], &[&trees["lower"]], &mut blob1, io::sink()).unwrap();
            let c2 = commit(&trees["upper"], &[&trees["lower"]], &mut blob2, io::sink()).unwrap();
            assert_eq!(c1, c2, "{name}");
            assert_eq!(blob1, blob2, "{name}");
        }
    }

    #[test]
    fn entries_of_the_reference_layers() {
        let all = scenarios(SPEC, 1);
        let get = |name: &str| {
            let trees = &all.iter().find(|(n, _)| n == name).unwrap().1;
            listing(&scenario_layer(trees))
        };
        assert_eq!(
            get("opaque"),
            [
                "dir o/ 755",
                "whiteout o/a",
                "whiteout o/b",
                "file o/mod 644 mm",
                "file o/new 644 n",
                "whiteout o/zlast",
                "dir p/ 755",
                "file p/x 644 x",
                "dir q/ 700",
                "file q/fresh 644 f",
                "whiteout q/old",
                "dir r/ 755",
                "whiteout r/sub",
                "whiteout r/sub-x",
                "dir s/ 755",
                "file s/inner 644 i",
            ]
        );
        assert_eq!(
            get("opaquelinks"),
            [
                "dir o/ 755",
                "file o/l3 644 L",
                "link o/l1 -> o/l3",
                "link o/l2 -> o/l3",
                "file o/m0 644 M",
                "link o/m1 -> o/m0",
            ]
        );
        assert_eq!(
            get("links"),
            [
                "dir a-dir/ 755",
                "file a-dir/alias 644 o",
                "dir b-dir/ 755",
                "link b-dir/orig -> a-dir/alias",
                "dir data/ 755",
                "file data/a 644 shared",
                "link data/b -> data/a",
                "link data/c -> data/a",
                "file other 600 x",
                "link zz -> other",
            ]
        );
    }

    #[test]
    fn empty_layer_is_the_reference_one() {
        let t = trees("");
        let mut blob = Vec::new();
        let mut extracted = Vec::new();
        let committed = commit(&t["upper"], &[&t["lower"]], &mut blob, &mut extracted).unwrap();
        assert_eq!(extracted, vec![0u8; 1024]);
        assert_eq!(
            committed.diff_id,
            "sha256:5f70bf18a086007016e948b04aed3b82103a36bea41755b6cddfaf10ace3c6ef"
        );
        assert_eq!(committed.tar_size, 1024);

        let mut reference = Vec::new();
        flate2::read::GzDecoder::new(&REFERENCE_EMPTY_BLOB[..])
            .read_to_end(&mut reference)
            .unwrap();
        assert_eq!(reference, extracted);
        assert_eq!(
            sha256_hex(&REFERENCE_EMPTY_BLOB),
            "4f4fb700ef54461cfa02571ae0db9a0dc1e0cdb5577484a6d75e68dc38e8acc1"
        );

        // Only the gzip header and trailer can be the reference's: see commit.
        assert_eq!(blob[..10], REFERENCE_EMPTY_BLOB[..10]);
        assert_eq!(blob[blob.len() - 8..], REFERENCE_EMPTY_BLOB[24..]);
        let mut ours = Vec::new();
        flate2::read::GzDecoder::new(&blob[..])
            .read_to_end(&mut ours)
            .unwrap();
        assert_eq!(ours, extracted);
        assert_eq!(committed.digest, format!("sha256:{}", sha256_hex(&blob)));
        assert_eq!(committed.size, blob.len() as u64);
        assert_eq!(
            (committed.digest.as_str(), committed.size),
            (EMPTY_BLOB_DIGEST, EMPTY_BLOB_SIZE)
        );
    }

    #[test]
    fn commit_tees_one_pass() {
        let all = scenarios(SPEC, 1);
        let trees = &all.iter().find(|(n, _)| n == "basic").unwrap().1;
        let tar = scenario_layer(trees);
        let mut blob = Vec::new();
        let mut extracted = Vec::new();
        let committed = commit(
            &trees["upper"],
            &[&trees["lower"]],
            &mut blob,
            &mut extracted,
        )
        .unwrap();
        assert_eq!(extracted, tar);
        assert_eq!(committed.diff_id, format!("sha256:{}", sha256_hex(&tar)));
        assert_eq!(committed.tar_size, tar.len() as u64);
        assert_eq!(committed.digest, format!("sha256:{}", sha256_hex(&blob)));
        assert_eq!(committed.size, blob.len() as u64);
        let mut decompressed = Vec::new();
        flate2::read::GzDecoder::new(&blob[..])
            .read_to_end(&mut decompressed)
            .unwrap();
        assert_eq!(decompressed, tar);
    }

    // ---- the lowers as overlayfs merges them ----

    /// Scenarios run by the reference with the lowers mounted as a lower snapshot is: one
    /// layer (lower) bind-mounted, several (lower0 the topmost, lower1, ...) as an
    /// upper-less overlay. "." is the root of a tree.
    const MERGE_SPEC: &str = r"
scenario merge
lower0 . dir opaque=trusted
lower0 a dir
lower0 a/x whiteout
lower0 b dir opaque=trusted
lower0 b/w file data=w
lower0 f file data=f
lower0 d dir
lower0 d/inner file data=i
lower0 gone whiteout
lower0 s symlink target=a
lower0 deep dir mode=0711
lower0 deep/er dir
lower0 deep/er/new file data=n
lower0 m dir
lower0 m/top file data=t
lower0 top-file file data=top
lower1 a dir mode=0700
lower1 a/x file data=x
lower1 a/y file data=y
lower1 b dir
lower1 b/z file data=z
lower1 f dir
lower1 f/under file data=u
lower1 d file data=was-a-file
lower1 gone dir
lower1 gone/deep file data=g
lower1 only file data=bottom
lower1 s dir
lower1 s/hidden file data=h
lower1 deep dir
lower1 deep/er dir mode=0700
lower1 deep/er/mid file data=e
lower1 m dir opaque=trusted
lower1 m/mid file data=m
lower1 w whiteout
lower1 top-file whiteout
lower2 deep dir
lower2 deep/er dir
lower2 deep/er/from2 file data=2
lower2 deep/from2 file data=2
lower2 m dir
lower2 m/low file data=l
lower2 w file data=w
lower2 top-file dir
lower2 top-file/x file data=x
lower2 d dir
lower2 d/below file data=b
upper a dir
upper a/x whiteout
upper a/y whiteout
upper b dir opaque=y
upper b/new file data=new
upper d dir opaque=y
upper d/fresh file data=fr
upper deep dir mode=0711
upper deep/er dir
upper deep/er/added file data=add
upper deep/er/from2 whiteout
upper gone whiteout
upper m dir
upper m/low whiteout
upper m/top file data=t2
upper only whiteout
upper s dir opaque=y
upper s/inside file data=in
upper w whiteout

scenario merged-opaque
lower1 o dir mode=0700
lower1 o/a file data=old
lower1 o/b dir
lower1 o/b/c file data=c
lower1 o/b/stale file data=s
lower1 o/zlast file data=z
lower1 q dir
lower1 q/old file data=o
lower1 r file data=was-a-file
lower1 s dir
lower1 s/x file data=x
lower1 junk dir
lower1 junk/f file data=f
lower0 o dir
lower0 o/a file data=a
lower0 o/b dir opaque=trusted
lower0 o/b/c file data=c
lower0 o/b/d dir
lower0 o/keep file data=k mtime=1650000000.123456789
lower0 o/mod file data=m
lower0 r dir
lower0 r/sub dir
lower0 r/sub/deep file data=d
lower0 r/sub-x file data=y
lower0 s symlink target=o
lower0 junk whiteout
upper o dir opaque=y
upper o/keep file data=k mtime=1650000000.123456789
upper o/mod file data=mm
upper o/new file data=n
upper p dir opaque=y
upper p/x file data=x
upper p/never whiteout
upper q dir mode=0700 opaque=y
upper q/fresh file data=f
upper r dir opaque=y
upper s dir opaque=y
upper s/inner file data=i

scenario single-lower-whiteout
lower x whiteout
lower y whiteout
lower d dir opaque=trusted
lower d/f file data=f
upper x whiteout
upper y file data=y
upper d dir
upper d/g file data=g

scenario single-lower-opaque-walk
lower o dir
lower o/w whiteout
lower o/f file data=f
upper o dir opaque=y
upper o/n file data=n

scenario two-lowers-merged-whiteout
lower0 o dir
lower0 o/w whiteout
lower1 o dir
lower1 o/w file data=w
lower1 o/f file data=f
upper o dir opaque=y
upper o/n file data=n

scenario two-lowers-unmerged-whiteout-lookup
lower0 a file data=a
lower1 o dir
lower1 o/w whiteout
lower1 o/f file data=f
upper o dir
upper o/w whiteout
upper o/f whiteout

scenario two-lowers-unmerged-whiteout
lower0 a file data=a
lower1 o dir
lower1 o/w whiteout
lower1 o/f file data=f
upper o dir opaque=y
upper o/n file data=n

scenario opaque-top-dir
lower0 o dir opaque=trusted
lower0 o/w whiteout
lower0 o/k file data=k
lower1 o dir
lower1 o/w file data=w

scenario dir-over-file
lower0 o dir
lower0 o/w whiteout
lower0 o/k file data=k
lower1 o file data=o

scenario merged-three
lower0 o dir
lower0 o/w whiteout
lower1 a file data=a
lower2 o dir
lower2 o/w file data=w
lower2 o/k file data=k
";

    /// The lowers of each scenario of MERGE_SPEC as the kernel listed their mount: path,
    /// st_mode, size, and the content of a file or the target of a symlink.
    const KERNEL_LISTINGS: &[(&str, &[&str])] = &[
        (
            "merge",
            &[
                "a 40755 0",
                "a/y 100644 1 y",
                "b 40755 0",
                "b/w 100644 1 w",
                "d 40755 0",
                "d/inner 100644 1 i",
                "deep 40711 0",
                "deep/er 40755 0",
                "deep/er/from2 100644 1 2",
                "deep/er/mid 100644 1 e",
                "deep/er/new 100644 1 n",
                "deep/from2 100644 1 2",
                "f 100644 1 f",
                "m 40755 0",
                "m/mid 100644 1 m",
                "m/top 100644 1 t",
                "only 100644 6 bottom",
                "s 120777 1 a",
                "top-file 100644 3 top",
            ],
        ),
        (
            "merged-opaque",
            &[
                "o 40755 0",
                "o/a 100644 1 a",
                "o/b 40755 0",
                "o/b/c 100644 1 c",
                "o/b/d 40755 0",
                "o/keep 100644 1 k",
                "o/mod 100644 1 m",
                "o/zlast 100644 1 z",
                "q 40755 0",
                "q/old 100644 1 o",
                "r 40755 0",
                "r/sub 40755 0",
                "r/sub/deep 100644 1 d",
                "r/sub-x 100644 1 y",
                "s 120777 1 o",
            ],
        ),
        (
            "single-lower-whiteout",
            &["d 40755 0", "d/f 100644 1 f", "x 20000 0", "y 20000 0"],
        ),
        (
            "single-lower-opaque-walk",
            &["o 40755 0", "o/f 100644 1 f", "o/w 20000 0"],
        ),
        (
            "two-lowers-merged-whiteout",
            &["o 40755 0", "o/f 100644 1 f"],
        ),
        (
            "two-lowers-unmerged-whiteout-lookup",
            &[
                "a 100644 1 a",
                "o 40755 0",
                "o/f 100644 1 f",
                "o/w listed-but-missing",
            ],
        ),
        (
            "two-lowers-unmerged-whiteout",
            &[
                "a 100644 1 a",
                "o 40755 0",
                "o/f 100644 1 f",
                "o/w listed-but-missing",
            ],
        ),
        (
            "opaque-top-dir",
            &["o 40755 0", "o/k 100644 1 k", "o/w listed-but-missing"],
        ),
        (
            "dir-over-file",
            &["o 40755 0", "o/k 100644 1 k", "o/w listed-but-missing"],
        ),
        (
            "merged-three",
            &["a 100644 1 a", "o 40755 0", "o/k 100644 1 k"],
        ),
    ];

    /// What the reference wrote for each scenario of MERGE_SPEC, None where it fails.
    const MERGE_REFERENCE: &[(&str, Option<(&str, u64)>)] = &[
        (
            "merge",
            Some((
                "d2eddb81374f83e0dfce54e36609cb257ebf1c97f7fde4dd913563d6fa10c63a",
                12288,
            )),
        ),
        (
            "merged-opaque",
            Some((
                "cc45e4698f54f461c9285a9addb1241a894a8c20662ac5ea001fb19708c7cce7",
                11776,
            )),
        ),
        (
            "single-lower-whiteout",
            Some((
                "76efcf0c0d7d60d8946f8311b7b50b0c82e7869bd0b57fd6ca0a900783a47ee1",
                4096,
            )),
        ),
        (
            "single-lower-opaque-walk",
            Some((
                "e6c92e3c47614f505b9ccbd0e060b990afb7fda98ed27f884d3f51824289768c",
                3584,
            )),
        ),
        (
            "two-lowers-merged-whiteout",
            Some((
                "1538195ee042a6f6715919e05155f5eb8594e061526bd50534e3db0c91214423",
                3072,
            )),
        ),
        (
            "two-lowers-unmerged-whiteout-lookup",
            Some((
                "69049cb1858f5d752b20a32c9a91e7c1ef9ba42374a081c841c97031c60e54e7",
                2048,
            )),
        ),
        ("two-lowers-unmerged-whiteout", None),
        (
            "opaque-top-dir",
            Some((
                "5f70bf18a086007016e948b04aed3b82103a36bea41755b6cddfaf10ace3c6ef",
                1024,
            )),
        ),
        (
            "dir-over-file",
            Some((
                "5f70bf18a086007016e948b04aed3b82103a36bea41755b6cddfaf10ace3c6ef",
                1024,
            )),
        ),
        (
            "merged-three",
            Some((
                "5f70bf18a086007016e948b04aed3b82103a36bea41755b6cddfaf10ace3c6ef",
                1024,
            )),
        ),
    ];

    fn lowers_of(trees: &BTreeMap<String, MemTree>) -> Vec<&dyn Tree> {
        if !trees.contains_key("lower0") {
            return vec![&trees["lower"]];
        }
        (0..)
            .map_while(|i| trees.get(&format!("lower{i}")))
            .map(|tree| tree as &dyn Tree)
            .collect()
    }

    /// The listing filepath.Walk makes of the mounted lowers.
    fn merged_listing(lowers: &[&dyn Tree]) -> Vec<String> {
        fn walk(merged: &Merged, dir: &Path, out: &mut Vec<String>) {
            let mut names = merged.read_dir(dir).unwrap();
            names.sort();
            for name in names {
                let path = join(dir, Path::new(&name));
                let stat = match merged.lstat(&path) {
                    Ok(stat) => stat,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        out.push(format!("{} listed-but-missing", path.display()));
                        continue;
                    }
                    Err(e) => panic!("{}: {e}", path.display()),
                };
                let (size, extra) = match stat.kind() {
                    S_IFREG => {
                        let mut data = String::new();
                        merged
                            .open(&path)
                            .unwrap()
                            .read_to_string(&mut data)
                            .unwrap();
                        (stat.size, data)
                    }
                    S_IFLNK => (
                        stat.size,
                        merged.read_link(&path).unwrap().display().to_string(),
                    ),
                    _ => (0, String::new()),
                };
                let line = format!("{} {:o} {size} {extra}", path.display(), stat.mode);
                out.push(line.trim_end().to_string());
                if stat.is_dir() {
                    walk(merged, &path, out);
                }
            }
        }
        let merged = Merged::new(lowers);
        let mut out = Vec::new();
        walk(&merged, Path::new(""), &mut out);
        out
    }

    #[test]
    fn merged_lowers_match_the_kernel() {
        let all = scenarios(MERGE_SPEC, 1);
        assert_eq!(all.len(), KERNEL_LISTINGS.len());
        assert_eq!(all.len(), MERGE_REFERENCE.len());
        for (name, trees) in &all {
            let lowers = lowers_of(trees);
            let (_, listing_want) = KERNEL_LISTINGS.iter().find(|(n, _)| n == name).unwrap();
            assert_eq!(merged_listing(&lowers), *listing_want, "{name}");
            let (_, want) = MERGE_REFERENCE.iter().find(|(n, _)| n == name).unwrap();
            match want {
                Some((digest, size)) => {
                    let out = layer(&trees["upper"], &lowers);
                    assert_eq!(
                        (sha256_hex(&out).as_str(), out.len() as u64),
                        (*digest, *size),
                        "{name}: {:#?}",
                        listing(&out)
                    );
                }
                None => {
                    let err = write_layer(&trees["upper"], &lowers, io::sink()).unwrap_err();
                    assert!(format!("{err:#}").contains("/o/w"), "{name}: {err:#}");
                }
            }
        }
        // Split in two layers, the lower of the scenario opaque gives the same layer.
        let (_, flat, _) = REFERENCE.iter().find(|(n, _, _)| *n == "opaque").unwrap();
        let (_, split) = MERGE_REFERENCE
            .iter()
            .find(|(n, _)| *n == "merged-opaque")
            .unwrap();
        assert_eq!(split.unwrap().0, *flat);
    }

    #[test]
    fn merged_lowers_fail_as_overlayfs_does() {
        let all = scenarios(MERGE_SPEC, 1);
        let lowers = lowers_of(&all[0].1);
        let merged = Merged::new(&lowers);
        let error = |p: &str| merged.lstat(Path::new(p)).unwrap_err().kind();
        assert_eq!(error("a/x"), io::ErrorKind::NotFound);
        assert_eq!(error("gone/deep"), io::ErrorKind::NotFound);
        assert_eq!(error("m/low"), io::ErrorKind::NotFound);
        assert_eq!(error("f/under"), io::ErrorKind::NotADirectory);
        assert_eq!(error("top-file/x"), io::ErrorKind::NotADirectory);
        assert_eq!(
            merged.read_dir(Path::new("f")).unwrap_err().kind(),
            io::ErrorKind::NotADirectory
        );
        assert_eq!(
            merged
                .xattr(Path::new("b"), "trusted.overlay.opaque")
                .unwrap(),
            Some(b"y".to_vec())
        );

        let none: [&dyn Tree; 0] = [];
        let empty = Merged::new(&none);
        assert!(empty.lstat(Path::new("")).unwrap().is_dir());
        assert!(empty.read_dir(Path::new("")).unwrap().is_empty());
        assert_eq!(
            empty.lstat(Path::new("x")).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(empty.xattr(Path::new(""), CAPABILITY).unwrap(), None);
    }

    // ---- overlay_linux_test.go ----

    #[test]
    fn simple_diff() {
        let spec = "
            lower etc dir
            lower etc/hosts file data=mydomain%2010.0.0.1
            lower etc/profile file data=PATH=/usr/bin
            lower etc/unchanged file data=PATH=/usr/bin
            lower etc/unexpected file data=#!/bin/sh
            upper etc dir mtime=1700000500.000000001
            upper etc/hosts file data=mydomain%2010.0.0.120 mtime=1700000500.000000002
            upper etc/profile file data=PATH=/usr/bin mode=0666 mtime=1700000500.000000003
            upper etc/unexpected whiteout
            upper root dir mode=0700
            upper root/.bashrc file data=PATH=/usr/sbin:/usr/bin
        ";
        assert_eq!(
            change_list(spec),
            [
                "modify /etc/hosts",
                "modify /etc/profile",
                "delete /etc/unexpected",
                "add /root",
                "add /root/.bashrc",
            ]
        );
    }

    #[test]
    fn rename_diff() {
        // With redirect_dir off, overlayfs renames a lower directory by copying it.
        let spec = "
            lower dir1 dir
            lower dir1/f1 file data=#####
            upper dir1 whiteout
            upper dir2 dir
            upper dir2/f1 file data=#####
        ";
        assert_eq!(
            change_list(spec),
            ["delete /dir1", "add /dir2", "add /dir2/f1"]
        );
    }

    #[test]
    fn empty_file_diff() {
        let spec = "
            lower etc dir
            lower etc/empty file mtime=1700000000.000000000
        ";
        assert!(change_list(spec).is_empty());
    }

    #[test]
    fn nested_deletion() {
        let spec = "
            lower d0 dir
            lower d1 dir
            lower d1/d2 dir
            lower d1/d2/f1 file data=mydomain%2010.0.0.1
            upper d0 whiteout
            upper d1 whiteout
        ";
        assert_eq!(change_list(spec), ["delete /d0", "delete /d1"]);
    }

    #[test]
    fn directory_replace() {
        let spec = "
            lower dir1 dir
            lower dir1/f1 file data=#####
            lower dir1/f2 dir
            lower dir1/f2/f3 file data=#!/bin/sh
            upper dir1 dir
            upper dir1/f11 file data=#New%20file%20here
            upper dir1/f2 file data=Now%20file mode=0666
        ";
        assert_eq!(change_list(spec), ["add /dir1/f11", "modify /dir1/f2"]);
    }

    #[test]
    fn remove_directory_tree() {
        let spec = "
            lower dir1 dir
            lower dir1/dir2 dir
            lower dir1/dir2/dir3 dir
            lower dir1/f1 file data=f1
            lower dir1/dir2/f2 file data=f2
            upper dir1 whiteout
        ";
        assert_eq!(change_list(spec), ["delete /dir1"]);
    }

    #[test]
    fn remove_directory_tree_with_dash() {
        let spec = "
            lower dir1 dir
            lower dir1/dir2 dir
            lower dir1/dir2/dir3 dir
            lower dir1/f1 file data=f1
            lower dir1/dir2/f2 file data=f2
            lower dir1-before dir
            lower dir1-before/f2 file data=f2
            upper dir1 whiteout
        ";
        assert_eq!(change_list(spec), ["delete /dir1"]);
    }

    #[test]
    fn file_replace() {
        // A directory made where a whiteout was is opaque.
        let spec = "
            lower dir1 file data=a%20file,%20not%20a%20directory
            upper dir1 dir opaque=trusted
            upper dir1/dir2 dir
            upper dir1/dir2/f1 file data=also%20a%20file
        ";
        assert_eq!(
            change_list(spec),
            ["modify /dir1", "add /dir1/dir2", "add /dir1/dir2/f1"]
        );
    }

    #[test]
    fn parent_directory_permission() {
        let spec = "
            lower dir1 dir mode=0700
            lower dir2 dir mode=0751
            lower dir3 dir mode=0777
            upper dir1 dir mode=0700
            upper dir1/d dir mode=0700
            upper dir1/d/f file data=irrelevant
            upper dir1/f file data=irrelevant
            upper dir2 dir mode=0751
            upper dir2/f file data=irrelevant
            upper dir3 dir mode=0777
            upper dir3/f file data=irrelevant
        ";
        assert_eq!(
            change_list(spec),
            [
                "add /dir1/d",
                "add /dir1/d/f",
                "add /dir1/f",
                "add /dir2/f",
                "add /dir3/f",
            ]
        );
    }

    #[test]
    fn update_with_same_time() {
        let spec = "
            lower file-modified-time file data=1 mtime=1700000000.000000005
            lower file-no-change file data=1 mtime=1700000000.000000005
            lower file-same-time file data=1 mtime=1700000000.000000005
            lower file-truncated-time-1 file data=1 mtime=1700000000.000000000
            lower file-truncated-time-2 file data=1 mtime=1700000000.000000000
            lower file-truncated-time-3 file data=1 mtime=1700000000.000000005
            upper file-modified-time file data=2 mtime=1700000000.000000006
            upper file-no-change file data=1 mtime=1700000000.000000005
            upper file-same-time file data=2 mtime=1700000000.000000005
            upper file-truncated-time-1 file data=1 mtime=1700000000.000000005
            upper file-truncated-time-2 file data=2 mtime=1700000000.000000000
            upper file-truncated-time-3 file data=1 mtime=1700000000.000000000
        ";
        assert_eq!(
            change_list(spec),
            [
                "modify /file-modified-time",
                "modify /file-truncated-time-1",
                "modify /file-truncated-time-2",
                "modify /file-truncated-time-3",
            ]
        );
    }

    #[test]
    fn lchtimes() {
        for mtime in ["0.000000000", "0.000000042"] {
            let spec = format!(
                "
                lower foo file data=foo
                lower lnk0 symlink target=/foo mtime={mtime}
                "
            );
            assert!(change_list(&spec).is_empty());
        }
    }

    // ---- tar_test.go, TestDiffTar: the change writer over the walk of two trees ----

    fn diff_tar(spec: &str) -> Vec<String> {
        let t = trees(spec);
        let mut out = Vec::new();
        let mut writer = ChangeWriter::new(&mut out, &t["b"]);
        double_walk(
            &t["a"],
            Path::new(""),
            &t["b"],
            Path::new(""),
            &mut |k, p, s| writer.handle_change(k, p, s),
        )
        .unwrap();
        writer.tw.finish().unwrap();
        listing(&out)
    }

    #[test]
    fn diff_tar_cases() {
        let cases: [(&str, &str, &[&str]); 8] = [
            ("empty diff", "a etc dir\nb etc dir", &[]),
            (
                "parent inclusion",
                "
                a d1 dir
                a d2 dir mode=0770
                b d1 dir
                b d1/d dir mode=0700
                b d2 dir mode=0770
                b d2/f file data=ok
                ",
                &[
                    "dir d1/ 755",
                    "dir d1/d/ 700",
                    "dir d2/ 770",
                    "file d2/f 644 ok",
                ],
            ),
            (
                "hardlink parent inclusion",
                "
                a d1 dir
                a d1/f1 file data=link%20me
                a d2 dir
                a d2/f1 file data=link%20me
                a d3 dir
                a d4 dir
                a d4/f1 file data=link%20me
                a d5 dir
                a d5/f1 file data=link%20me
                a d6 dir
                a d6/l1 link of=d1/f1
                a d6/l2 link of=d5/f1
                b d1 dir
                b d1/f1 file data=link%20me
                b d2 dir
                b d2/f1 file data=link%20me
                b d2/l1 link of=d1/f1
                b d3 dir
                b d4 dir
                b d4/f1 file data=link%20me
                b d3/l1 link of=d4/f1
                b d5 dir
                b d5/f1 file data=link%20me
                b d6 dir
                ",
                &[
                    "dir d2/ 755",
                    "file d2/l1 644 link me",
                    "dir d1/ 755",
                    "link d1/f1 -> d2/l1",
                    "dir d3/ 755",
                    "file d3/l1 644 link me",
                    "dir d4/ 755",
                    "link d4/f1 -> d3/l1",
                    "dir d6/ 755",
                    "whiteout d6/l1",
                    "whiteout d6/l2",
                ],
            ),
            (
                "update directory permission",
                "
                a d1 dir
                a d2 dir mode=0770
                b d1 dir mode=0777
                b d1/d dir mode=0700
                b d2 dir mode=0770
                b d2/f file data=ok
                ",
                &[
                    "dir d1/ 777",
                    "dir d1/d/ 700",
                    "dir d2/ 770",
                    "file d2/f 644 ok",
                ],
            ),
            (
                "hardlink updated parent",
                "
                a d1 dir
                a d1/f1 file data=link%20me
                a d2 dir
                a d2/f1 file data=link%20me
                a d3 dir
                a d3/f1 file data=link%20me
                a d4 dir
                b d1 dir mode=0777
                b d1/f1 file data=link%20me
                b d2 dir
                b d2/f1 file data=link%20me
                b d2/l1 link of=d1/f1
                b d3 dir
                b d3/f1 file data=link%20me
                b d4 dir mode=0777
                b d4/l1 link of=d3/f1
                ",
                &[
                    "dir d1/ 777",
                    "dir d2/ 755",
                    "file d2/l1 644 link me",
                    "link d1/f1 -> d2/l1",
                    "dir d4/ 777",
                    "file d4/l1 644 link me",
                    "dir d3/ 755",
                    "link d3/f1 -> d4/l1",
                ],
            ),
            (
                "whiteout includes parents",
                "
                a d1 dir
                a d1/f1 file data=content
                a d2 dir
                a d2/f1 file data=content
                a d2/f2 file data=content
                a d3 dir
                a d3/f1 file data=content
                a d4 dir
                a d4/f1 file data=content
                a d5 dir
                a d5/f1 file data=content
                b d1 dir
                b d2 dir
                b d2/f2 file data=content mode=0777
                b d3 dir
                b d3/f2 file data=content
                b d4 dir
                b d4/f0 file data=content
                ",
                &[
                    "dir d1/ 755",
                    "whiteout d1/f1",
                    "dir d2/ 755",
                    "whiteout d2/f1",
                    "file d2/f2 777 content",
                    "dir d3/ 755",
                    "whiteout d3/f1",
                    "file d3/f2 644 content",
                    "dir d4/ 755",
                    "file d4/f0 644 content",
                    "whiteout d4/f1",
                    "whiteout d5",
                ],
            ),
            (
                "whiteout parent removal",
                "
                a d1 dir
                a d2 dir
                a d2/f1 file data=content
                b d3 dir
                ",
                &["whiteout d1", "whiteout d2", "dir d3/ 755"],
            ),
            (
                "ignore sockets",
                "
                a f1 file data=content
                b f1 file data=content
                b f2 file data=content
                b s0 socket mode=0644
                b f3 file data=content
                ",
                &["file f2 644 content", "file f3 644 content"],
            ),
        ];
        for (name, spec, want) in cases {
            assert_eq!(diff_tar(spec), want, "{name}");
        }
    }

    // ---- archive/tar ----

    #[test]
    fn split_ustar_path_vectors() {
        let sr = |s: &str, n: usize| s.repeat(n);
        let vectors: Vec<(String, Option<(String, String)>)> = vec![
            (String::new(), None),
            ("abc".into(), None),
            ("\u{7528}\u{6236}\u{540d}".into(), None),
            (sr("a", NAME_SIZE), None),
            (sr("a", NAME_SIZE) + "/", None),
            (
                sr("a", NAME_SIZE) + "/a",
                Some((sr("a", NAME_SIZE), "a".into())),
            ),
            (sr("a", PREFIX_SIZE) + "/", None),
            (
                sr("a", PREFIX_SIZE) + "/a",
                Some((sr("a", PREFIX_SIZE), "a".into())),
            ),
            (sr("a", NAME_SIZE + 1), None),
            (
                sr("/", NAME_SIZE + 1),
                Some((sr("/", NAME_SIZE - 1), "/".into())),
            ),
            (
                sr("a", PREFIX_SIZE) + "/" + &sr("b", NAME_SIZE),
                Some((sr("a", PREFIX_SIZE), sr("b", NAME_SIZE))),
            ),
            (sr("a", PREFIX_SIZE) + "//" + &sr("b", NAME_SIZE), None),
            (
                sr("a/", NAME_SIZE),
                Some((sr("a/", 77) + "a", sr("a/", 22))),
            ),
        ];
        for (input, want) in vectors {
            let got = split_ustar_path(input.as_bytes()).map(|(p, s)| {
                (
                    String::from_utf8(p.to_vec()).unwrap(),
                    String::from_utf8(s.to_vec()).unwrap(),
                )
            });
            assert_eq!(got, want, "{input:?}");
        }
    }

    #[test]
    fn fits_in_octal_vectors() {
        let vectors: [(i64, usize, bool); 23] = [
            (-1, 1, false),
            (-1, 2, false),
            (-1, 3, false),
            (0, 1, true),
            (1, 1, false),
            (0, 2, true),
            (0o7, 2, true),
            (0o10, 2, false),
            (0, 4, true),
            (0o777, 4, true),
            (0o1000, 4, false),
            (0, 8, true),
            (0o7777777, 8, true),
            (0o10000000, 8, false),
            (0, 12, true),
            (0o77777777777, 12, true),
            (0o100000000000, 12, false),
            (i64::MAX, 22, true),
            (0o12345670123, 12, true),
            (0o1564164, 12, true),
            (-0o12345670123, 12, false),
            (-0o1564164, 12, false),
            (-1564164, 30, false),
        ];
        for (x, width, ok) in vectors {
            assert_eq!(fits_in_octal(width, x), ok, "{x} in {width}");
        }
    }

    #[test]
    fn format_octal_vectors() {
        let vectors: [(i64, &[u8]); 12] = [
            (0, b"0\0"),
            (7, b"7\0"),
            (0o77, b"77\0"),
            (0, b"0000000\0"),
            (0o123, b"0000123\0"),
            (0o7654321, b"7654321\0"),
            (0o7777777, b"7777777\0"),
            (0, b"00000000000\0"),
            (0o1234567, b"00001234567\0"),
            (0o76543210321, b"76543210321\0"),
            (0o77777777777, b"77777777777\0"),
            (i64::MAX, b"777777777777777777777\0"),
        ];
        for (x, want) in vectors {
            let mut got = vec![0xffu8; want.len()];
            format_octal(&mut got, x);
            assert_eq!(got, want, "{x:o}");
        }
        // A value that does not fit is written as zero, as in a PAX header's fields.
        let mut got = vec![0xffu8; 8];
        format_octal(&mut got, 0o10000000);
        assert_eq!(got, b"0000000\0");
    }

    #[test]
    fn format_pax_record_vectors() {
        let med = "CD".repeat(50);
        let long = "AB".repeat(100);
        let vectors: Vec<(String, String, Option<String>)> = vec![
            ("k".into(), "v".into(), Some("6 k=v\n".into())),
            (
                "path".into(),
                "/etc/hosts".into(),
                Some("19 path=/etc/hosts\n".into()),
            ),
            (
                "path".into(),
                long.clone(),
                Some(format!("210 path={long}\n")),
            ),
            (
                "path".into(),
                med.clone(),
                Some(format!("110 path={med}\n")),
            ),
            ("foo".into(), "ba".into(), Some("9 foo=ba\n".into())),
            ("foo".into(), "bar".into(), Some("11 foo=bar\n".into())),
            (
                "foo".into(),
                "b=\nar=\n==\0".into(),
                Some("18 foo=b=\nar=\n==\0\n".into()),
            ),
            (
                "foo".into(),
                "hello9 foo=ba\nworld".into(),
                Some("27 foo=hello9 foo=ba\nworld\n".into()),
            ),
            (
                "\u{263a}\u{263b}\u{2639}".into(),
                "\u{65e5}a\u{672c}b\u{8a9e}\u{e7}".into(),
                Some("27 \u{263a}\u{263b}\u{2639}=\u{65e5}a\u{672c}b\u{8a9e}\u{e7}\n".into()),
            ),
            (
                "xhello".into(),
                "\0world".into(),
                Some("17 xhello=\0world\n".into()),
            ),
            ("path".into(), "null\0".into(), None),
            ("null\0".into(), "value".into(), None),
            (
                "SCHILY.xattr.key".into(),
                "null\0".into(),
                Some("26 SCHILY.xattr.key=null\0\n".into()),
            ),
        ];
        for (k, v, want) in vectors {
            let got = valid_pax_record(k.as_bytes(), v.as_bytes())
                .then(|| String::from_utf8(format_pax_record(k.as_bytes(), v.as_bytes())).unwrap());
            assert_eq!(got, want, "{k:?}={v:?}");
        }
    }

    #[test]
    fn whiteout_header_bytes() {
        let mut out = Vec::new();
        let mut tw = TarWriter::new(&mut out);
        tw.write_header(&Header::whiteout(whiteout_name(b"a/b")))
            .unwrap();
        let mut want = [0u8; 512];
        want[..7].copy_from_slice(b"a/.wh.b");
        for field in [100..108, 108..116, 116..124, 329..337, 337..345] {
            want[field].copy_from_slice(b"0000000\0");
        }
        want[124..136].copy_from_slice(b"00000000000\0");
        want[136..148].copy_from_slice(b"00000000000\0");
        want[156] = b'0';
        want[257..265].copy_from_slice(b"ustar\x0000");
        let sum: u32 = want.iter().map(|&c| u32::from(c)).sum::<u32>() + 8 * u32::from(b' ');
        want[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        assert_eq!(out, want);
    }

    #[test]
    fn pax_header_names() {
        let cases: [(&[u8], &[u8]); 4] = [
            (b"x", b"PaxHeaders.0/x"),
            (b"x/", b"x/PaxHeaders.0"),
            (b"a/b/c", b"a/b/PaxHeaders.0/c"),
            (b"a/b/", b"a/b/PaxHeaders.0"),
        ];
        for (name, want) in cases {
            assert_eq!(pax_header_name(name), want);
        }
    }

    #[test]
    fn zero_time_is_written_as_the_epoch() {
        let stat = Stat {
            mode: S_IFREG | 0o644,
            mtime: GO_ZERO_TIME,
            mtime_nsec: 5,
            ..Stat::default()
        };
        let hdr = Header::from_stat(&stat, b"f".to_vec(), Vec::new()).unwrap();
        assert!(matches!(encoding(&hdr).unwrap(), Encoding::Ustar));
        let before = Stat {
            mtime: GO_ZERO_TIME - 1,
            ..stat
        };
        let hdr = Header::from_stat(&before, b"f".to_vec(), Vec::new()).unwrap();
        assert!(matches!(encoding(&hdr).unwrap(), Encoding::Pax(_)));
    }

    #[test]
    fn huge_files_get_a_size_record() {
        let stat = Stat {
            mode: S_IFREG | 0o644,
            size: 1 << 33,
            ..Stat::default()
        };
        let hdr = Header::from_stat(&stat, b"big".to_vec(), Vec::new()).unwrap();
        let Encoding::Pax(records) = encoding(&hdr).unwrap() else {
            panic!("USTAR for an 8 GiB file");
        };
        assert_eq!(records[b"size".as_slice()], b"8589934592");
        let fits = Stat {
            size: (1 << 33) - 1,
            ..stat
        };
        let hdr = Header::from_stat(&fits, b"big".to_vec(), Vec::new()).unwrap();
        assert!(matches!(encoding(&hdr).unwrap(), Encoding::Ustar));
    }

    // ---- what the upper directory cannot say ----

    fn layer_error(spec: &str) -> String {
        let t = trees(spec);
        let lowers: Vec<&dyn Tree> = t
            .iter()
            .filter(|(name, _)| name.as_str() != "upper")
            .map(|(_, tree)| tree as &dyn Tree)
            .collect();
        format!(
            "{:#}",
            write_layer(&t["upper"], &lowers, io::sink()).unwrap_err()
        )
    }

    #[test]
    fn redirected_directories_are_refused() {
        let err = layer_error("upper moved dir redirect=/old");
        assert!(err.contains("redirect_dir"), "{err}");
    }

    #[test]
    fn metacopy_files_are_refused() {
        let err = layer_error("upper f file data=x metacopy=y");
        assert!(err.contains("metacopy"), "{err}");
    }

    #[test]
    fn a_step_s_own_overlay_named_attributes_are_no_overlay_state() {
        for spec in [
            "upper f file data=x xattr=user.overlay.metacopy:y",
            "upper d dir xattr=user.overlay.redirect:/x",
        ] {
            let t = trees(spec);
            let lowers: Vec<&dyn Tree> = t
                .iter()
                .filter(|(name, _)| name.as_str() != "upper")
                .map(|(_, tree)| tree as &dyn Tree)
                .collect();
            write_layer(&t["upper"], &lowers, io::sink()).unwrap();
        }
    }

    #[test]
    fn whiteout_under_a_lower_file_is_an_error() {
        // ENOTDIR from the lowers is no "not found" for a whiteout in the reference.
        let err = layer_error(
            "
            lower x file data=x
            upper x dir
            upper x/y whiteout
            ",
        );
        assert!(err.contains("/x/y"), "{err}");
    }

    #[test]
    fn whiteouts_under_an_opaque_directory_fail_as_in_the_reference() {
        // With real mounts the reference fails on both: "lstat .../view/o/a: no such file
        // or directory". A directory the lowers lack is not treated as opaque, and its
        // whiteouts are left out as anywhere else.
        for gone in ["a", "b"] {
            let err = layer_error(&format!(
                "
                lower o dir
                lower o/a file data=a
                upper o dir opaque=y
                upper o/{gone} whiteout
                "
            ));
            assert!(err.contains(&format!("/o/{gone}")), "{err}");
        }
        let t = trees(
            "
            upper p dir opaque=y
            upper p/never whiteout
            upper p/x file data=x
            ",
        );
        let out = scenario_layer(&t);
        assert_eq!(
            (sha256_hex(&out).as_str(), out.len()),
            (
                "3039ddb090d9964c1ec768f28d66871382480d1f95ed399ae0ace6ed3a2b99f8",
                2560
            )
        );
    }

    #[test]
    fn files_that_change_size_are_errors() {
        for (data, word) in [(b"ab".as_slice(), "shorter"), (b"abcd", "grew")] {
            let mut t = trees("upper f file data=abc");
            t.get_mut("upper").unwrap().inode_mut("f").data = data.to_vec();
            let err = format!(
                "{:#}",
                write_layer(&t["upper"], &[&t["lower"]], io::sink()).unwrap_err()
            );
            assert!(err.contains(word), "{err}");
        }
    }

    // ---- real directories ----

    fn set_times(path: &Path, (sec, nsec): (i64, i64)) {
        let ts = TimeSpec::new(sec, nsec);
        utimensat(
            nix::fcntl::AT_FDCWD,
            path,
            &ts,
            &ts,
            UtimensatFlags::NoFollowSymlink,
        )
        .unwrap();
    }

    fn materialize(root: &Path, entries: &[&Entry]) {
        fs::create_dir_all(root).unwrap();
        let at = |p: &[u8]| root.join(bytes_path(p));
        for e in entries {
            let path = at(&e.path);
            match e.kind.as_str() {
                "dir" => fs::create_dir(&path).unwrap(),
                "file" => fs::write(&path, content(e)).unwrap(),
                "symlink" => {
                    std::os::unix::fs::symlink(bytes_path(&expand(&e.kv["target"])), &path).unwrap()
                }
                "fifo" => nix::unistd::mkfifo(&path, Mode::from_bits_truncate(0o600)).unwrap(),
                "socket" => drop(std::os::unix::net::UnixListener::bind(&path).unwrap()),
                "whiteout" => mknod(&path, SFlag::S_IFCHR, Mode::empty(), 0).unwrap(),
                "link" => fs::hard_link(at(&expand(&e.kv["of"])), &path).unwrap(),
                other => panic!("{other} cannot be made by an unprivileged test"),
            }
        }
        for e in entries
            .iter()
            .filter(|e| !matches!(e.kind.as_str(), "link" | "whiteout"))
        {
            let path = at(&e.path);
            if e.kind != "symlink" {
                fs::set_permissions(&path, fs::Permissions::from_mode(perm(e))).unwrap();
            }
            if e.kv.get("opaque").is_some_and(|o| o == "y") {
                xattr::set(&path, "user.overlay.opaque", b"y").unwrap();
            }
        }
        for e in entries
            .iter()
            .filter(|e| !matches!(e.kind.as_str(), "link" | "whiteout"))
        {
            set_times(&at(&e.path), mtime(e));
        }
    }

    #[test]
    fn dir_tree_reads_like_the_fake() {
        let tmp = tempfile::tempdir().unwrap();
        let probe = tmp.path().join("probe");
        fs::create_dir(&probe).unwrap();
        let whiteouts = mknod(&probe.join("w"), SFlag::S_IFCHR, Mode::empty(), 0).is_ok();
        let opaque = xattr::set(&probe, "user.overlay.opaque", b"y").is_ok();

        let spec = "
            lower etc dir
            lower etc/hostname file data=old mtime=1600000000.100000000
            lower etc/gone file data=g
            lower keep file data=k mtime=1650000000.000000000
            lower same-dir dir mode=0750
            lower o dir
            lower o/x file data=x
            upper etc dir mtime=1700000100.000000000
            upper etc/hostname file data=new
            upper etc/gone whiteout
            upper keep file data=k mtime=1650000000.000000000
            upper same-dir dir mode=0750 mtime=1700000200.000000000
            upper same-dir/new file fill=40000 mode=0600
            upper link1 file data=l
            upper link2 link of=link1
            upper sym symlink target=etc/hostname mtime=1650000000.000000000
            upper pipe fifo mode=0600
            upper sock socket
            upper o dir opaque=y
            upper o/y file data=y
        ";
        let uid = nix::unistd::geteuid().as_raw().to_string();
        let gid = nix::unistd::getegid().as_raw().to_string();
        let mut entries = parse(spec).pop().unwrap().1;
        entries.retain(|e| whiteouts || e.kind != "whiteout");
        for e in &mut entries {
            if !opaque {
                e.kv.remove("opaque");
            }
            if e.kind != "link" && e.kind != "whiteout" {
                e.kv.insert("uid".into(), uid.clone());
                e.kv.insert("gid".into(), gid.clone());
            }
        }
        let of =
            |tree: &str| -> Vec<&Entry> { entries.iter().filter(|e| e.tree == tree).collect() };
        materialize(&tmp.path().join("upper"), &of("upper"));
        materialize(&tmp.path().join("lower"), &of("lower"));
        let upper = DirTree::new(tmp.path().join("upper"));
        let lower = DirTree::new(tmp.path().join("lower"));
        let real = layer(&upper, &[&lower]);
        let fake = layer(&build(&of("upper"), 1), &[&build(&of("lower"), 2)]);
        assert_eq!(listing(&real), listing(&fake));
        assert_eq!(real, fake);
        assert_eq!(real, layer(&upper, &[&lower]));

        let entries = listing(&real);
        assert!(
            entries.contains(&"dir etc/ 755".to_string()),
            "{entries:#?}"
        );
        assert!(entries.contains(&"file etc/hostname 644 new".to_string()));
        assert!(entries.contains(&"link link2 -> link1".to_string()));
        assert!(entries.contains(&"dir same-dir/ 750".to_string()));
        assert!(!entries
            .iter()
            .any(|e| e.contains("keep") || e.contains("sock")));
        assert_eq!(
            entries.contains(&"whiteout etc/gone".to_string()),
            whiteouts
        );
        assert_eq!(entries.contains(&"whiteout o/x".to_string()), opaque);
    }

    #[test]
    fn dir_tree_errors_name_the_path() {
        let tmp = tempfile::tempdir().unwrap();
        let upper = DirTree::new(tmp.path().join("missing"));
        let err = format!("{:#}", write_layer(&upper, &[], io::sink()).unwrap_err());
        assert!(err.contains("looking at /"), "{err}");
        assert_eq!(
            DirTree::new(tmp.path())
                .xattr(Path::new(""), "user.nspawn.absent")
                .unwrap(),
            None
        );
    }
}
