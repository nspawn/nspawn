//! Copying files the way a build's COPY and ADD do: a source path (wildcards allowed)
//! inside a source root, to a destination path inside a destination root, with the
//! destination rules, include and exclude patterns, owner, mode, timestamps, hardlinks
//! and xattrs of the reference. A port of `docopy` from moby/buildkit's file backend
//! and of tonistiigi/fsutil's copy package, with the root-bound symlink resolution of
//! containerd/continuity (`root_path`) and the symbolic modes of tonistiigi/dchapes-mode
//! (`mode`); upstream/SOURCE names the versions.
//!
//! Every path a caller gives is resolved inside its root: symlinks are followed one
//! component at a time, an absolute target starts again at the root and `..` stops
//! there, so nothing a tree contains can lead a copy outside its roots.
//!
//! Unpacking archives (ADD of a local tar) is not part of this module: `CopyAction`
//! takes an `unpack` hook, called for every source before it is copied, which the
//! builder provides.
// The port keeps the reference's API whole; its tests use what the builder does not.
#![allow(dead_code)]

use std::collections::HashMap;
use std::fs::{self, DirBuilder, File, Metadata, Permissions};
use std::io;
use std::os::unix::fs::{lchown, symlink, DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context as _, Result};
use nix::errno::Errno as NixErrno;
use nix::fcntl::AT_FDCWD;
use nix::sys::stat::{mknod, utimensat, Mode, SFlag, UtimensatFlags};
use nix::sys::time::TimeSpec;

use super::ignore::{
    contains_wildcards, glob_match_bytes, lexical, lossy, MatchInfo, PatternMatcher,
};
use super::walk::{
    bytes_path, list, path_bytes, quote, read_dir_sorted, Errno, LinkError, PathError, S_IFBLK,
    S_IFCHR, S_IFDIR, S_IFIFO, S_IFLNK, S_IFMT, S_IFREG, S_IFSOCK,
};

const DEFAULT_DIRECTORY_MODE: u32 = 0o755;

/// More than this many symlinks on the way to a path is a loop.
#[derive(Debug)]
pub struct TooManyLinks;

impl std::fmt::Display for TooManyLinks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("too many links")
    }
}

impl std::error::Error for TooManyLinks {}

/// `path` joined to `root` with every symlink on the way resolved inside `root`: an
/// absolute target restarts at `root`, `..` never climbs above it, and a component
/// that does not exist ends the resolution as a plain name. An empty path is the root.
pub fn root_path(root: &Path, path: &str) -> Result<PathBuf> {
    Ok(bytes_path(&root_path_bytes(
        path_bytes(root),
        path.as_bytes(),
    )?))
}

fn root_path_bytes(root: &[u8], path: &[u8]) -> Result<Vec<u8>> {
    if path.is_empty() {
        return Ok(root.to_vec());
    }
    let mut path = path.to_vec();
    let mut links = 0usize;
    loop {
        let before = links;
        path = walk_links(root, &path, &mut links)?;
        if before == links {
            let rooted = lexical::join(&[b"/", &path]);
            if path == rooted {
                return Ok(lexical::join(&[root, &rooted]));
            }
            path = rooted;
        }
    }
}

/// The target of `path` if it is a symlink, or `path` itself made absolute.
fn walk_link(root: &[u8], path: &[u8], links: &mut usize) -> Result<(Vec<u8>, bool)> {
    if *links > 255 {
        return Err(TooManyLinks.into());
    }
    let path = lexical::join(&[b"/", path]);
    if path == b"/" {
        return Ok((path, false));
    }
    let real = bytes_path(&lexical::join(&[root, &path]));
    match fs::symlink_metadata(&real) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((path, false)),
        Err(e) => return Err(PathError::new("lstat", real, e).into()),
        Ok(meta) if !meta.file_type().is_symlink() => return Ok((path, false)),
        Ok(_) => {}
    }
    let target = fs::read_link(&real).map_err(|e| PathError::new("readlink", &real, e))?;
    *links += 1;
    Ok((path_bytes(&target).to_vec(), true))
}

fn walk_links(root: &[u8], path: &[u8], links: &mut usize) -> Result<Vec<u8>> {
    let (dir, file) = lexical::split(path);
    if dir.is_empty() {
        return Ok(walk_link(root, file, links)?.0);
    }
    if file.is_empty() {
        if dir == b"/" {
            return Ok(dir.to_vec());
        }
        return walk_links(root, &dir[..dir.len() - 1], links);
    }
    let newdir = walk_links(root, dir, links)?;
    let (newpath, is_link) = walk_link(root, &lexical::join(&[&newdir, file]), links)?;
    if !is_link || lexical::is_abs(&newpath) {
        return Ok(newpath);
    }
    Ok(lexical::join(&[&newdir, &newpath]))
}

/// A source path inside its root: every symlink resolved when `follow_links`, else all
/// but the last component.
fn root_path_follow(root: &[u8], p: &[u8], follow_links: bool) -> Result<Vec<u8>> {
    let p = lexical::join(&[b"/", p]);
    if p == b"/" {
        return Ok(root.to_vec());
    }
    if follow_links {
        return root_path_bytes(root, &p);
    }
    let (dir, file) = lexical::split(&p);
    let parent = root_path_bytes(root, dir)?;
    Ok(lexical::join(&[&parent, file]))
}

/// A user and group, as numbers: names are resolved by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct User {
    pub uid: u32,
    pub gid: u32,
}

/// Decides the owner of a created path from the owner of its source (`None` for a
/// directory created on the way, which has no source). `Ok(None)` leaves it alone.
pub type Chowner<'a> = &'a dyn Fn(Option<User>) -> Result<Option<User>>;

/// Called with the destination, the source, the xattr name ("" when listing failed)
/// and the error; what it returns ends the copy of that entry's xattrs.
pub type XattrErrorHandler<'a> = &'a dyn Fn(&Path, &Path, &str, anyhow::Error) -> Result<()>;

/// Called for every path the copy adds, relative to the destination root and with a
/// leading slash, with the metadata of its source.
pub type ChangeFn<'a> = &'a mut dyn FnMut(&Path, &Metadata) -> Result<()>;

/// Sets the owner of `p` (not following a symlink) to what `f` decides from `old`.
pub fn chown(p: &Path, old: Option<User>, f: Option<Chowner<'_>>) -> Result<()> {
    let Some(f) = f else {
        return Ok(());
    };
    if let Some(user) = f(old)? {
        lchown(p, Some(user.uid), Some(user.gid)).map_err(|e| PathError::new("lchown", p, e))?;
    }
    Ok(())
}

/// Sets both times of `p` (not following a symlink) to `tm`, if given.
pub fn utimes(p: &Path, tm: Option<TimeSpec>) -> Result<()> {
    let Some(tm) = tm else {
        return Ok(());
    };
    utimensat(AT_FDCWD, p, &tm, &tm, UtimensatFlags::NoFollowSymlink)
        .map_err(|e| anyhow::Error::new(Errno(e.into())))
        .with_context(|| format!("failed to utime {}", p.display()))
}

/// Creates `path` and its missing parents with the permission bits of `perm` (set
/// explicitly, so the umask does not apply), each owned as `user` decides from no
/// source and with both times set to `tm`. Returns the directories created, parents
/// first.
pub fn mkdir_all(
    path: &Path,
    perm: u32,
    user: Option<Chowner<'_>>,
    tm: Option<TimeSpec>,
) -> Result<Vec<PathBuf>> {
    mkdir_all_bytes(path_bytes(path), perm, user, tm)
}

fn mkdir_all_bytes(
    path: &[u8],
    perm: u32,
    user: Option<Chowner<'_>>,
    tm: Option<TimeSpec>,
) -> Result<Vec<PathBuf>> {
    let p = bytes_path(path);
    if let Ok(meta) = fs::metadata(&p) {
        if meta.is_dir() {
            return Ok(Vec::new());
        }
        return Err(PathError::new(
            "mkdir",
            p,
            io::Error::from_raw_os_error(NixErrno::ENOTDIR as i32),
        )
        .into());
    }
    let mut i = path.len();
    while i > 0 && path[i - 1] == b'/' {
        i -= 1;
    }
    let mut j = i;
    while j > 0 && path[j - 1] != b'/' {
        j -= 1;
    }
    let mut created = Vec::new();
    if j > 1 {
        created = mkdir_all_bytes(&path[..j - 1], perm, user, tm)?;
    }
    if fs::symlink_metadata(&p).is_ok_and(|m| m.is_dir()) {
        return Ok(created);
    }
    if let Err(e) = DirBuilder::new().mode(perm & 0o777).create(&p) {
        // "foo/." and the like: the directory exists after all.
        if fs::symlink_metadata(&p).is_ok_and(|m| m.is_dir()) {
            return Ok(created);
        }
        return Err(PathError::new("mkdir", p, e).into());
    }
    // The kernel gives a directory made under a setgid one the setgid bit; the chmod that
    // takes the umask out keeps it, as the reference's mkdir under a zero umask does.
    let setgid = p
        .parent()
        .and_then(|d| fs::metadata(d).ok())
        .map_or(0, |m| m.permissions().mode() & 0o2000);
    fs::set_permissions(&p, Permissions::from_mode((perm & 0o777) | setgid))
        .map_err(|e| PathError::new("chmod", &p, e))?;
    created.push(p.clone());
    chown(&p, None, user)?;
    utimes(&p, tm)?;
    Ok(created)
}

/// Gives the directories created on the way their timestamp back, deepest first, once
/// what was copied into them changed it.
fn fix_created_parent_dirs(dirs: &[PathBuf], tm: Option<TimeSpec>) -> Result<()> {
    for d in dirs.iter().rev() {
        utimes(d, tm)?;
    }
    Ok(())
}

/// The options of `copy`.
#[derive(Default)]
pub struct CopyInfo<'a> {
    /// Owner of every copied entry, from the owner of its source. Without it the
    /// source's owner is kept, and directories created on the way are left as created.
    pub chown: Option<Chowner<'a>>,
    /// Both times of every copied entry and created directory; without it the
    /// source's access and modification times are kept.
    pub utime: Option<TimeSpec>,
    /// `src` may hold glob characters; it is an error when nothing matches.
    pub allow_wildcards: bool,
    /// Permission bits (07777) for every copied entry, and for directories created on
    /// the way (0755 without it).
    pub mode: Option<u32>,
    /// A chmod(1) mode, octal or symbolic, applied to every copied entry; it takes
    /// precedence over `mode` for copied entries.
    pub mode_str: Option<String>,
    /// What to do with an xattr that cannot be copied; without it, fail.
    pub xattr_error_handler: Option<XattrErrorHandler<'a>>,
    /// Copy what a source directory holds into the destination rather than the
    /// directory itself, when the destination exists.
    pub copy_dir_contents: bool,
    /// Follow a symlink that is the source itself (those on the way are always
    /// resolved inside the root).
    pub follow_links: bool,
    /// Only entries matching one of these, relative to the source, are copied; the
    /// directories leading to them are created as they are met.
    pub include_patterns: Vec<String>,
    /// Entries matching these are not copied, even when included.
    pub exclude_patterns: Vec<String>,
    /// A source entry replaces whatever is at its destination, whatever its type;
    /// directories are still merged into directories.
    pub always_replace_existing_dest_paths: bool,
    pub change_fn: Option<ChangeFn<'a>>,
}

struct ParentDir {
    src: Vec<u8>,
    dst: Vec<u8>,
    copied: bool,
}

struct Copier<'a> {
    chown: Option<Chowner<'a>>,
    utime: Option<TimeSpec>,
    mode: Option<u32>,
    mode_set: Option<mode::Set>,
    inodes: HashMap<(u64, u64), Vec<u8>>,
    xattr_error_handler: Option<XattrErrorHandler<'a>>,
    include: Option<PatternMatcher>,
    exclude: Option<PatternMatcher>,
    parent_dirs: Vec<ParentDir>,
    change_fn: Option<ChangeFn<'a>>,
    root: Vec<u8>,
    always_replace: bool,
}

fn lstat(p: &[u8]) -> Result<Metadata, PathError> {
    let path = bytes_path(p);
    fs::symlink_metadata(&path).map_err(|e| PathError::new("lstat", path, e))
}

/// Copies `src` inside `src_root` to `dst` inside `dst_root` as the reference's copy
/// does: a directory is copied with everything below it (into `dst` when `dst` does not
/// exist, below `dst` under its own name when it does, unless `copy_dir_contents`); a
/// file lands at `dst`, or inside it when `dst` is a directory. A `dst` ending in "/"
/// or "/." is created as a directory first. Symlinks are copied as symlinks.
pub fn copy(
    src_root: &Path,
    src: &str,
    dst_root: &Path,
    dst: &str,
    ci: CopyInfo<'_>,
) -> Result<()> {
    copy_bytes(
        path_bytes(src_root),
        src.as_bytes(),
        path_bytes(dst_root),
        dst.as_bytes(),
        ci,
    )
}

fn copy_bytes(
    src_root: &[u8],
    src: &[u8],
    dst_root: &[u8],
    dst: &[u8],
    ci: CopyInfo<'_>,
) -> Result<()> {
    let utime = ci.utime;
    let mut created: Vec<Vec<PathBuf>> = Vec::new();
    let result = copy_sources(src_root, src, dst_root, dst, ci, &mut created);
    // The reference defers these, so they run whatever the outcome, and their own
    // errors are dropped.
    for dirs in created.iter().rev() {
        let _ = fix_created_parent_dirs(dirs, utime);
    }
    result
}

fn copy_sources(
    src_root: &[u8],
    src: &[u8],
    dst_root: &[u8],
    dst: &[u8],
    ci: CopyInfo<'_>,
    created: &mut Vec<Vec<PathBuf>>,
) -> Result<()> {
    let (dir, file) = lexical::split(dst);
    let ensure = if !file.is_empty() && file != b"." {
        dir
    } else {
        dst
    };
    if !ensure.is_empty() {
        let ensure = root_path_bytes(dst_root, ensure)?;
        let perm = ci.mode.unwrap_or(DEFAULT_DIRECTORY_MODE);
        created.push(mkdir_all_bytes(&ensure, perm, ci.chown, ci.utime)?);
    }
    let mode_set = match ci.mode_str.as_deref() {
        Some(s) if !s.is_empty() => Some(mode::parse_with_umask(s, 0)?),
        _ => None,
    };
    let dst = root_path_bytes(dst_root, &lexical::clean(dst))?;
    let mut c = Copier::new(dst_root, &ci, mode_set)?;
    c.change_fn = ci.change_fn;
    let srcs = if ci.allow_wildcards {
        let matches = resolve_wildcards_bytes(src_root, src, ci.follow_links)?;
        if matches.is_empty() {
            bail!("no matches found: {}", lossy(src));
        }
        matches
    } else {
        vec![src.to_vec()]
    };
    for s in srcs {
        let followed = root_path_follow(src_root, &s, ci.follow_links)?;
        let (target, dirs) = c.prepare_target_dir(&followed, &s, &dst, ci.copy_dir_contents)?;
        created.push(dirs);
        c.copy(
            &followed,
            b"",
            &target,
            false,
            &MatchInfo::default(),
            &MatchInfo::default(),
        )?;
    }
    Ok(())
}

impl<'a> Copier<'a> {
    fn new(root: &[u8], ci: &CopyInfo<'a>, mode_set: Option<mode::Set>) -> Result<Copier<'a>> {
        let include = if ci.include_patterns.is_empty() {
            None
        } else {
            Some(PatternMatcher::new(&ci.include_patterns).map_err(|e| {
                anyhow::Error::new(e).context(format!(
                    "invalid includepatterns: {}",
                    list(&ci.include_patterns)
                ))
            })?)
        };
        let exclude = if ci.exclude_patterns.is_empty() {
            None
        } else {
            Some(PatternMatcher::new(&ci.exclude_patterns).map_err(|e| {
                anyhow::Error::new(e).context(format!(
                    "invalid excludepatterns: {}",
                    list(&ci.exclude_patterns)
                ))
            })?)
        };
        Ok(Copier {
            chown: ci.chown,
            utime: ci.utime,
            mode: ci.mode,
            mode_set,
            inodes: HashMap::new(),
            xattr_error_handler: ci.xattr_error_handler,
            include,
            exclude,
            parent_dirs: Vec::new(),
            change_fn: None,
            root: root.to_vec(),
            always_replace: ci.always_replace_existing_dest_paths,
        })
    }

    fn dir_mode(&self) -> u32 {
        self.mode.unwrap_or(DEFAULT_DIRECTORY_MODE)
    }

    fn prepare_target_dir(
        &self,
        src_followed: &[u8],
        src: &[u8],
        dest: &[u8],
        copy_dir_contents: bool,
    ) -> Result<(Vec<u8>, Vec<PathBuf>)> {
        let fi_src = lstat(src_followed)?;
        let dest_path = bytes_path(dest);
        let fi_dest = match fs::metadata(&dest_path) {
            Ok(meta) => Some(meta),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(anyhow::Error::new(PathError::new("stat", dest_path, e))
                    .context("failed to lstat destination path"))
            }
        };
        let mut dest = dest.to_vec();
        let src_is_dir = fi_src.is_dir();
        if (!copy_dir_contents && src_is_dir && fi_dest.is_some())
            || (!src_is_dir && fi_dest.as_ref().is_some_and(Metadata::is_dir))
        {
            dest = lexical::join(&[&dest, &lexical::base(src)]);
        }
        let target = if copy_dir_contents && src_is_dir && fi_dest.is_none() {
            dest.clone()
        } else {
            lexical::dir(&dest)
        };
        let created = mkdir_all_bytes(&target, self.dir_mode(), self.chown, self.utime)?;
        Ok((dest, created))
    }

    fn include(&self, path: &str, parent: &MatchInfo) -> Result<(bool, MatchInfo)> {
        match &self.include {
            None => Ok((true, MatchInfo::default())),
            Some(m) => Ok(m
                .matches_using_parent_results(path, parent)
                .context("failed to match includepatterns")?),
        }
    }

    fn exclude(&self, path: &str, parent: &MatchInfo) -> Result<(bool, MatchInfo)> {
        match &self.exclude {
            None => Ok((false, MatchInfo::default())),
            Some(m) => Ok(m
                .matches_using_parent_results(path, parent)
                .context("failed to match excludepatterns")?),
        }
    }

    fn exclusions(&self) -> bool {
        self.exclude
            .as_ref()
            .is_some_and(PatternMatcher::exclusions)
    }

    /// Copies `src` to `target`; `src_components` is the path of `src` relative to the
    /// source being copied, which the patterns match.
    fn copy(
        &mut self,
        src: &[u8],
        src_components: &[u8],
        target: &[u8],
        overwrite_target_metadata: bool,
        parent_include: &MatchInfo,
        parent_exclude: &MatchInfo,
    ) -> Result<()> {
        // The patterns are checked before the lstat so that an excluded path that
        // cannot be read is never touched.
        let mut include = true;
        let mut excluded = false;
        let mut include_info = MatchInfo::default();
        let mut exclude_info = MatchInfo::default();
        if !src_components.is_empty() {
            let comp = lossy(src_components);
            let (matched, info) = self.include(&comp, parent_include)?;
            include = matched;
            include_info = info;
            let (matched, info) = self.exclude(&comp, parent_exclude)?;
            exclude_info = info;
            if matched {
                include = false;
                excluded = true;
            }
            // Nothing below can come back without include patterns or exceptions.
            if !include && self.include.is_none() && !self.exclusions() {
                return Ok(());
            }
        }

        let fi = lstat(src)
            .map_err(anyhow::Error::new)
            .with_context(|| format!("failed to stat {}", lossy(src)))?;
        if !include && !fi.is_dir() {
            return Ok(());
        }
        let target_path = bytes_path(target);
        let target_fi = match fs::symlink_metadata(&target_path) {
            Ok(meta) => Some(meta),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(anyhow::Error::new(PathError::new("lstat", &target_path, e)))
                    .with_context(|| format!("failed to stat {}", lossy(target)))
            }
        };
        if include {
            self.remove_target_if_needed(&target_path, &fi, target_fi.as_ref())?;
            self.create_parent_dirs(src, overwrite_target_metadata)?;
        }
        if !fi.is_dir() {
            if !include {
                return Ok(());
            }
            ensure_empty_file_target(&target_path)?;
        }

        let mut copy_file_info = include;
        let mut restore_timestamp = false;
        let mut notify = true;
        let kind = fi.mode() & S_IFMT;
        if kind == S_IFDIR {
            let created = self.copy_directory(
                src,
                src_components,
                target,
                &fi,
                overwrite_target_metadata,
                include,
                excluded,
                &include_info,
                &exclude_info,
            )?;
            // A directory that was already there keeps its metadata, but gets its
            // timestamp back after what was copied into it changed it.
            if !overwrite_target_metadata {
                copy_file_info = created;
                restore_timestamp = !created;
            }
            notify = false;
        } else if kind == S_IFREG {
            match get_link_source(target, &fi, &mut self.inodes) {
                Some(link) => {
                    let old = bytes_path(&link);
                    fs::hard_link(&old, &target_path)
                        .map_err(|err| LinkError {
                            op: "link",
                            old,
                            new: target_path.clone(),
                            err,
                        })
                        .context("failed to create hard link")?;
                }
                None => copy_file(src, target).context("failed to copy files")?,
            }
        } else if kind == S_IFLNK {
            let link = fs::read_link(bytes_path(src))
                .map_err(|e| PathError::new("readlink", bytes_path(src), e))
                .with_context(|| format!("failed to read link: {}", lossy(src)))?;
            symlink(&link, &target_path)
                .map_err(|err| LinkError {
                    op: "symlink",
                    old: link.clone(),
                    new: target_path.clone(),
                    err,
                })
                .with_context(|| format!("failed to create symlink: {}", lossy(target)))?;
        } else if kind == S_IFBLK || kind == S_IFCHR || kind == S_IFIFO || kind == S_IFSOCK {
            copy_device(&target_path, &fi).context("failed to create device")?;
        }

        if copy_file_info {
            self.copy_file_info(&fi, &target_path)
                .context("failed to copy file info")?;
            copy_xattrs(&target_path, &bytes_path(src), self.xattr_error_handler)
                .context("failed to copy xattrs")?;
        } else if restore_timestamp && target_fi.is_some() {
            self.copy_file_timestamp(&fi, &target_path)
                .context("failed to restore file timestamp")?;
        }
        if notify {
            self.notify_change(target, &fi)?;
        }
        Ok(())
    }

    fn notify_change(&mut self, target: &[u8], fi: &Metadata) -> Result<()> {
        let Some(f) = self.change_fn.as_mut() else {
            return Ok(());
        };
        let rel = target.strip_prefix(self.root.as_slice()).unwrap_or(target);
        let path = bytes_path(&lexical::clean(rel));
        f(&path, fi).context("failed to notify file change")
    }

    fn remove_target_if_needed(
        &self,
        target: &Path,
        src_fi: &Metadata,
        target_fi: Option<&Metadata>,
    ) -> Result<()> {
        if !self.always_replace {
            return Ok(());
        }
        let Some(target_fi) = target_fi else {
            return Ok(());
        };
        if src_fi.is_dir() && target_fi.is_dir() {
            // Directories are merged, not replaced.
            return Ok(());
        }
        let removed = if target_fi.is_dir() {
            fs::remove_dir_all(target)
        } else {
            fs::remove_file(target)
        };
        removed.map_err(|e| PathError::new("unlinkat", target, e).into())
    }

    /// Creates the directories held back on the way to an entry that matched an include
    /// pattern.
    fn create_parent_dirs(&mut self, src: &[u8], overwrite_target_metadata: bool) -> Result<()> {
        for i in 0..self.parent_dirs.len() {
            if self.parent_dirs[i].copied {
                continue;
            }
            let src_path = bytes_path(&self.parent_dirs[i].src);
            let dst_path = bytes_path(&self.parent_dirs[i].dst);
            // The reference names the entry being copied, not the parent, here.
            let fi = fs::metadata(&src_path)
                .map_err(|e| PathError::new("stat", &src_path, e))
                .with_context(|| format!("failed to stat {}", lossy(src)))?;
            if !fi.is_dir() {
                bail!("{} is not a directory", src_path.display());
            }
            let created = copy_directory_only(&dst_path, &fi, overwrite_target_metadata)?;
            if created {
                self.copy_file_info(&fi, &dst_path)
                    .context("failed to copy file info")?;
                copy_xattrs(&dst_path, &src_path, self.xattr_error_handler)
                    .context("failed to copy xattrs")?;
            }
            self.parent_dirs[i].copied = true;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn copy_directory(
        &mut self,
        src: &[u8],
        src_components: &[u8],
        dst: &[u8],
        stat: &Metadata,
        overwrite_target_metadata: bool,
        include: bool,
        excluded: bool,
        include_info: &MatchInfo,
        exclude_info: &MatchInfo,
    ) -> Result<bool> {
        if !stat.is_dir() {
            bail!("source is not directory");
        }
        let mut created = false;
        let mut parent = ParentDir {
            src: src.to_vec(),
            dst: dst.to_vec(),
            copied: false,
        };
        // A directory that only leads to included entries is created when the first of
        // them is met, so that a/*/c does not create a/b without a/b/c.
        if include {
            created = copy_directory_only(&bytes_path(dst), stat, overwrite_target_metadata)?;
            if created || overwrite_target_metadata {
                self.notify_change(dst, stat)?;
            }
            parent.copied = true;
        }
        self.parent_dirs.push(parent);
        let result = self.copy_children(
            src,
            src_components,
            dst,
            excluded,
            include_info,
            exclude_info,
        );
        self.parent_dirs.pop();
        result.map(|()| created)
    }

    fn copy_children(
        &mut self,
        src: &[u8],
        src_components: &[u8],
        dst: &[u8],
        excluded: bool,
        include_info: &MatchInfo,
        exclude_info: &MatchInfo,
    ) -> Result<()> {
        // An excluded directory is read only when an exception could bring back
        // something below it.
        if excluded && !self.exclusions() {
            return Ok(());
        }
        let (entries, err) = read_dir_sorted(&bytes_path(src));
        if let Some(err) = err {
            return Err(err.context(format!("failed to read {}", lossy(src))));
        }
        for (name, _) in entries {
            self.copy(
                &lexical::join(&[src, &name]),
                &lexical::join(&[src_components, &name]),
                &lexical::join(&[dst, &name]),
                true,
                include_info,
                exclude_info,
            )?;
        }
        Ok(())
    }

    fn copy_file_info(&self, fi: &Metadata, name: &Path) -> Result<()> {
        let old = User {
            uid: fi.uid(),
            gid: fi.gid(),
        };
        let keep = |u: Option<User>| -> Result<Option<User>> { Ok(u) };
        let owner: Chowner<'_> = match self.chown {
            Some(f) => f,
            None => &keep,
        };
        chown(name, Some(old), Some(owner))
            .with_context(|| format!("failed to chown {}", name.display()))?;
        let mut m = fi.mode() & 0o7777;
        if let Some(set) = &self.mode_set {
            m = set.apply(fi.mode()) & 0o7777;
        } else if let Some(mode) = self.mode {
            m = mode & 0o7777;
        }
        if fi.mode() & S_IFMT != S_IFLNK {
            fs::set_permissions(name, Permissions::from_mode(m))
                .map_err(|e| PathError::new("chmod", name, e))
                .with_context(|| format!("failed to chmod {}", name.display()))?;
        }
        self.copy_file_timestamp(fi, name)
    }

    fn copy_file_timestamp(&self, fi: &Metadata, name: &Path) -> Result<()> {
        if self.utime.is_some() {
            return utimes(name, self.utime);
        }
        let atime = TimeSpec::new(fi.atime(), fi.atime_nsec());
        let mtime = TimeSpec::new(fi.mtime(), fi.mtime_nsec());
        utimensat(
            AT_FDCWD,
            name,
            &atime,
            &mtime,
            UtimensatFlags::NoFollowSymlink,
        )
        .map_err(|e| anyhow::Error::new(Errno(e.into())))
        .with_context(|| format!("failed to utime {}", name.display()))
    }
}

fn copy_directory_only(
    dst: &Path,
    stat: &Metadata,
    overwrite_target_metadata: bool,
) -> Result<bool> {
    match fs::symlink_metadata(dst) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            DirBuilder::new()
                .mode(stat.mode() & 0o7777)
                .create(dst)
                .map_err(|e| PathError::new("mkdir", dst, e))
                .with_context(|| format!("failed to mkdir {}", dst.display()))?;
            Ok(true)
        }
        Err(e) => Err(PathError::new("lstat", dst, e).into()),
        Ok(st) if !st.is_dir() => bail!("cannot copy to non-directory: {}", dst.display()),
        Ok(_) => {
            if overwrite_target_metadata {
                fs::set_permissions(dst, Permissions::from_mode(stat.mode() & 0o7777))
                    .map_err(|e| PathError::new("chmod", dst, e))
                    .with_context(|| format!("failed to chmod on {}", dst.display()))?;
            }
            Ok(false)
        }
    }
}

fn ensure_empty_file_target(dst: &Path) -> Result<()> {
    match fs::symlink_metadata(dst) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow::Error::new(PathError::new("lstat", dst, e)))
            .context("failed to lstat file target"),
        Ok(meta) if meta.is_dir() => {
            bail!("cannot replace to directory {} with file", dst.display())
        }
        Ok(_) => fs::remove_file(dst).map_err(|e| PathError::new("remove", dst, e).into()),
    }
}

/// For a file with several links, the destination of the first copy of it, which the
/// next ones are linked to. Files are told apart by device and inode, since inode
/// numbers repeat across the filesystems a source can span.
fn get_link_source(
    name: &[u8],
    fi: &Metadata,
    inodes: &mut HashMap<(u64, u64), Vec<u8>>,
) -> Option<Vec<u8>> {
    if fi.is_dir() || fi.nlink() <= 1 {
        return None;
    }
    let key = (fi.dev(), fi.ino());
    if let Some(first) = inodes.get(&key) {
        return Some(first.clone());
    }
    inodes.insert(key, name.to_vec());
    None
}

fn copy_file(source: &[u8], target: &[u8]) -> Result<()> {
    let source = bytes_path(source);
    let target = bytes_path(target);
    let mut src = File::open(&source)
        .map_err(|e| PathError::new("open", &source, e))
        .with_context(|| format!("failed to open source {}", source.display()))?;
    let mut dst = File::create(&target)
        .map_err(|e| PathError::new("open", &target, e))
        .with_context(|| format!("failed to open target {}", target.display()))?;
    copy_file_content(&mut dst, &mut src)
}

/// Between two files the standard library copies in the kernel (copy_file_range) and
/// falls back to reading and writing where the kernel cannot, which is what the
/// reference does by hand.
fn copy_file_content(dst: &mut File, src: &mut File) -> Result<()> {
    io::copy(src, dst)
        .map_err(|e| anyhow::Error::new(Errno(e)))
        .context("copy file range failed")?;
    Ok(())
}

fn copy_device(dst: &Path, fi: &Metadata) -> Result<()> {
    let mut mode = fi.mode();
    let kind = mode & S_IFMT;
    let rdev = if kind == S_IFBLK || kind == S_IFCHR {
        fi.rdev()
    } else {
        0
    };
    // A socket is copied as an empty file of the same permissions; every other type,
    // block devices included, keeps its own.
    if kind == S_IFSOCK {
        mode &= !S_IFMT;
    }
    mknod(
        dst,
        SFlag::from_bits_truncate(mode & S_IFMT),
        Mode::from_bits_truncate(mode & 0o7777),
        rdev,
    )
    .map_err(|e| anyhow::Error::new(Errno(e.into())))
}

fn copy_xattrs(dst: &Path, src: &Path, handler: Option<XattrErrorHandler<'_>>) -> Result<()> {
    let fail = |key: &str, err: anyhow::Error| -> Result<()> {
        match handler {
            Some(h) => h(dst, src, key, err),
            None => Err(err),
        }
    };
    let keys = match xattr::list(src) {
        Ok(keys) => keys,
        Err(e) => {
            return fail(
                "",
                anyhow::Error::new(Errno(e))
                    .context(format!("failed to list xattrs on {}", src.display())),
            )
        }
    };
    // The first failure ends the copy of the xattrs even when the handler forgives it.
    for key in keys {
        let name = lossy(path_bytes(Path::new(&key))).into_owned();
        let data = match xattr::get(src, &key) {
            Ok(Some(data)) => data,
            Ok(None) => {
                let e = io::Error::from_raw_os_error(NixErrno::ENODATA as i32);
                return fail(
                    &name,
                    anyhow::Error::new(Errno(e)).context(format!(
                        "failed to get xattr {} on {}",
                        quote(&name),
                        src.display()
                    )),
                );
            }
            Err(e) => {
                return fail(
                    &name,
                    anyhow::Error::new(Errno(e)).context(format!(
                        "failed to get xattr {} on {}",
                        quote(&name),
                        src.display()
                    )),
                )
            }
        };
        if let Err(e) = xattr::set(dst, &key, &data) {
            return fail(
                &name,
                anyhow::Error::new(Errno(e)).context(format!(
                    "failed to set xattr {} on {}",
                    quote(&name),
                    dst.display()
                )),
            );
        }
    }
    Ok(())
}

/// The paths, relative to `root`, that `src` (which may hold glob characters) matches.
/// The part of `src` before the first component with a glob character is resolved
/// inside `root` (its last component followed only when `follow_links`); the rest is
/// matched against every path below it, a matched directory not being entered. Without
/// glob characters the result is `src` cleaned.
pub fn resolve_wildcards(root: &Path, src: &str, follow_links: bool) -> Result<Vec<PathBuf>> {
    Ok(
        resolve_wildcards_bytes(path_bytes(root), src.as_bytes(), follow_links)?
            .iter()
            .map(|m| bytes_path(m))
            .collect(),
    )
}

fn resolve_wildcards_bytes(root: &[u8], src: &[u8], follow_links: bool) -> Result<Vec<Vec<u8>>> {
    let (d1, d2) = split_wildcards(src);
    if d2.is_empty() {
        return Ok(vec![d1]);
    }
    let base = root_path_follow(root, &d1, follow_links)?;
    let mut matches = Vec::new();
    let meta = lstat(&base)?;
    match_below(&base, &base, &d2, &meta, &mut matches)?;
    matches
        .iter()
        .map(|m| {
            lexical::rel(root, m)
                .ok_or_else(|| anyhow!("Rel: can't make {} relative to {}", lossy(m), lossy(root)))
        })
        .collect()
}

fn split_wildcards(p: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let joined = lexical::join(&[p]);
    let mut p1: Vec<&[u8]> = Vec::new();
    let mut p2: Vec<&[u8]> = Vec::new();
    let mut found = false;
    for part in joined.split(|&b| b == b'/') {
        if !found && contains_wildcards(part) {
            found = true;
        }
        let part: &[u8] = if part.is_empty() { b"/" } else { part };
        if found {
            p2.push(part);
        } else {
            p1.push(part);
        }
    }
    (lexical::join(&p1), lexical::join(&p2))
}

/// The reference's walk of the base directory: lexical order, symlinks not followed,
/// the whole tree read (a pattern only matches as many components as it has, but
/// nothing stops the walk earlier), an unreadable directory an error.
fn match_below(
    base: &[u8],
    path: &[u8],
    comp: &[u8],
    meta: &Metadata,
    out: &mut Vec<Vec<u8>>,
) -> Result<()> {
    let rel = lexical::rel(base, path).unwrap_or_default();
    let is_base = rel == b".";
    let matched = !is_base && glob_match_bytes(comp, &rel).unwrap_or(false);
    if !meta.is_dir() {
        if matched {
            out.push(path.to_vec());
        }
        return Ok(());
    }
    let (entries, err) = read_dir_sorted(&bytes_path(path));
    if let Some(err) = err {
        return Err(err);
    }
    if matched {
        out.push(path.to_vec());
        return Ok(());
    }
    for (name, _) in entries {
        let child = lexical::join(&[path, &name]);
        let child_meta = lstat(&child)?;
        match_below(base, &child, comp, &child_meta, out)?;
    }
    Ok(())
}

/// Called for every source before it is copied, when set: it unpacks the source if it
/// is an archive and says so with `Ok(true)`, which skips the copy of that source.
/// Paths in the request are as the copy sees them; the hook resolves them inside their
/// roots with `root_path`.
pub type UnpackFn<'a> = &'a dyn Fn(&UnpackRequest<'_>) -> Result<bool>;

pub struct UnpackRequest<'r> {
    pub src_root: &'r Path,
    /// A source path inside `src_root`: the cleaned source, absolute, or a match of its
    /// wildcard, relative to `src_root`.
    pub src: &'r Path,
    pub dest_root: &'r Path,
    /// The cleaned destination path inside `dest_root`, its trailing slash kept.
    pub dest: &'r Path,
    pub owner: Option<User>,
    /// Both times of what is created, in nanoseconds since the epoch.
    pub timestamp: Option<i64>,
}

/// One copy of a build step, as the reference's file backend receives it.
#[derive(Default)]
pub struct CopyAction<'a> {
    /// The source inside the source root; a trailing "/." copies a directory's contents.
    pub src: String,
    /// The destination inside the destination root; a trailing "/" or "/." makes it a
    /// directory that the source goes into.
    pub dest: String,
    pub owner: Option<User>,
    /// Permission bits (07777) for what is copied.
    pub mode: Option<u32>,
    /// A chmod(1) mode for what is copied; it takes precedence over `mode`.
    pub mode_str: Option<String>,
    /// Follow the source when it is a symlink.
    pub follow_symlink: bool,
    /// Copy what a source directory holds rather than the directory.
    pub dir_copy_contents: bool,
    /// Without it, the parent of the destination must exist.
    pub create_dest_path: bool,
    pub allow_wildcard: bool,
    /// A wildcard that matches nothing copies nothing instead of failing.
    pub allow_empty_wildcard: bool,
    pub include_patterns: Vec<String>,
    pub exclude_patterns: Vec<String>,
    /// Paths of the source root that must exist (for the `--parents` form).
    pub required_paths: Vec<String>,
    /// Both times of what is created, in nanoseconds since the epoch; the sources'
    /// times are kept without it.
    pub timestamp: Option<i64>,
    pub always_replace_existing_dest_paths: bool,
    /// The hook point of ADD's archive unpacking.
    pub unpack: Option<UnpackFn<'a>>,
    /// Where xattrs that cannot be copied are reported; to stderr without it. They
    /// never fail the copy.
    pub xattr_errors: Option<&'a dyn Fn(&anyhow::Error)>,
}

impl CopyAction<'_> {
    /// The flags that COPY and ADD give a source from the build context or a stage.
    pub fn for_instruction(src: &CopySource, dest: &str) -> CopyAction<'static> {
        CopyAction {
            src: src.src.clone(),
            dest: dest.to_string(),
            follow_symlink: true,
            dir_copy_contents: true,
            create_dest_path: true,
            allow_wildcard: true,
            allow_empty_wildcard: true,
            include_patterns: src.include_patterns.clone(),
            required_paths: src.required_paths.clone(),
            ..CopyAction::default()
        }
    }

    pub fn set_chmod(&mut self, chmod: Chmod) {
        match chmod {
            Chmod::Octal(m) => self.mode = Some(m),
            Chmod::Symbolic(s) => self.mode_str = Some(s),
        }
    }
}

fn timespec_from_nanos(ts: i64) -> TimeSpec {
    TimeSpec::new(ts.div_euclid(1_000_000_000), ts.rem_euclid(1_000_000_000))
}

/// "/" joined to `s` and cleaned, keeping a trailing "/" or "/.".
fn clean_path(s: &str) -> Vec<u8> {
    let mut s2 = lexical::join(&[b"/", s.as_bytes()]);
    if s.ends_with("/.") {
        if s2 != b"/" {
            s2.push(b'/');
        }
        s2.push(b'.');
    } else if s.ends_with('/') && s2 != b"/" {
        s2.push(b'/');
    }
    s2
}

/// Runs one copy action from `src_root` to `dest_root`. Errors name paths inside the
/// roots, not on the host, wherever the reference does.
pub fn docopy(src_root: &Path, dest_root: &Path, action: &CopyAction<'_>) -> Result<()> {
    let src = path_bytes(src_root);
    let dest = path_bytes(dest_root);
    for required in &action.required_paths {
        let p = root_path_follow(src, required.as_bytes(), action.follow_symlink)?;
        if fs::symlink_metadata(bytes_path(&p)).is_err() {
            bail!("{}: not found", quote(required));
        }
    }
    let src_path = clean_path(&action.src);
    let dest_path = clean_path(&action.dest);
    if !action.create_dest_path {
        let p = root_path_bytes(dest, &lexical::join(&[b"/", action.dest.as_bytes()]))?;
        lstat(&lexical::dir(&p))
            .map_err(anyhow::Error::new)
            .with_context(|| format!("failed to stat {}", action.dest))?;
    }
    docopy_matches(src, dest, &src_path, &dest_path, action).map_err(|mut err| {
        if let Some(pe) = err.downcast_mut::<PathError>() {
            let mut p = path_bytes(&pe.path);
            p = p.strip_prefix(src).unwrap_or(p);
            p = p.strip_prefix(dest).unwrap_or(p);
            pe.path = bytes_path(p);
        }
        err
    })
}

fn docopy_matches(
    src: &[u8],
    dest: &[u8],
    src_path: &[u8],
    dest_path: &[u8],
    action: &CopyAction<'_>,
) -> Result<()> {
    let owner = action.owner;
    let chowner = move |old: Option<User>| -> Result<Option<User>> { Ok(owner.or(old)) };
    let report = |_dst: &Path, _src: &Path, _key: &str, err: anyhow::Error| -> Result<()> {
        match action.xattr_errors {
            Some(f) => f(&err),
            None => eprintln!("{err:#}"),
        }
        Ok(())
    };
    let matches = if !action.allow_wildcard {
        vec![src_path.to_vec()]
    } else {
        let m = resolve_wildcards_bytes(src, src_path, action.follow_symlink)?;
        if m.is_empty() {
            if action.allow_empty_wildcard {
                return Ok(());
            }
            bail!("{} not found", lossy(src_path));
        }
        m
    };
    let utime = action.timestamp.map(timespec_from_nanos);
    for s in matches {
        if let Some(unpack) = action.unpack {
            let request = UnpackRequest {
                src_root: &bytes_path(src),
                src: &bytes_path(&s),
                dest_root: &bytes_path(dest),
                dest: &bytes_path(dest_path),
                owner: action.owner,
                timestamp: action.timestamp,
            };
            if unpack(&request)? {
                continue;
            }
        }
        let (mode, mode_str) = match action.mode_str.as_deref() {
            Some(s) if !s.is_empty() => (None, Some(s.to_string())),
            _ => (action.mode, None),
        };
        let ci = CopyInfo {
            chown: Some(&chowner),
            utime,
            mode,
            mode_str,
            xattr_error_handler: Some(&report),
            copy_dir_contents: action.dir_copy_contents,
            follow_links: action.follow_symlink,
            include_patterns: action.include_patterns.clone(),
            exclude_patterns: action.exclude_patterns.clone(),
            always_replace_existing_dest_paths: action.always_replace_existing_dest_paths,
            ..CopyInfo::default()
        };
        copy_bytes(src, &s, dest, dest_path, ci)?;
    }
    Ok(())
}

/// A `--chmod` value: octal, or symbolic as chmod(1) takes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Chmod {
    Octal(u32),
    Symbolic(String),
}

/// Reads a `--chmod` value as the reference's front end does.
pub fn parse_chmod(s: &str) -> Result<Chmod> {
    let non_octal = || {
        anyhow!("invalid chmod parameter: '{s}'. it should be octal string and between 0 and 07777")
    };
    if let Some(p) = parse_octal_u32(s) {
        if p > 0o7777 {
            return Err(non_octal());
        }
        return Ok(Chmod::Octal(p));
    }
    match mode::parse(s) {
        Ok(_) => Ok(Chmod::Symbolic(s.to_string())),
        // A value that looks like a number gets the message of the octal form.
        Err(mode::ModeError::Num { .. }) => Err(non_octal()),
        Err(e) => Err(e.into()),
    }
}

fn parse_octal_u32(s: &str) -> Option<u32> {
    if s.is_empty() {
        return None;
    }
    let mut n: u32 = 0;
    for c in s.chars() {
        let d = c.to_digit(8)?;
        n = n.checked_mul(8)?.checked_add(d)?;
    }
    Some(n)
}

/// A source of COPY or ADD from the build context or a stage, as the front end turns
/// it into a copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopySource {
    pub src: String,
    pub include_patterns: Vec<String>,
    pub required_paths: Vec<String>,
}

/// The source of a COPY or ADD instruction. With `parents` (COPY --parents), the part
/// before a "/./" pivot (or "/" without one) becomes the source and the rest an
/// include pattern, so that the path below the pivot is recreated at the destination;
/// a source without glob characters must then exist.
pub fn copy_source(src: &str, parents: bool) -> CopySource {
    let normalize = |p: &str| -> String {
        if p.is_empty() {
            "/".to_string()
        } else if p.starts_with('/') {
            p.to_string()
        } else {
            lexical::join_str(&["/", p])
        }
    };
    let mut src = src.to_string();
    let mut include_patterns = Vec::new();
    let mut required_paths = Vec::new();
    if parents {
        let pattern = match src.split_once("/./") {
            Some((parent, pattern)) => {
                let pattern = pattern.to_string();
                src = parent.to_string();
                pattern
            }
            None => std::mem::replace(&mut src, "/".to_string()),
        };
        let pattern = normalize(&pattern);
        include_patterns.push(pattern.strip_prefix('/').unwrap_or(&pattern).to_string());
        if !contains_wildcards(src.as_bytes()) && !contains_wildcards(pattern.as_bytes()) {
            required_paths.push(normalize(&lexical::join_str(&[&src, &pattern])));
        }
    }
    CopySource {
        src: normalize(&src),
        include_patterns,
        required_paths,
    }
}

/// The destination of a COPY or ADD in a stage whose working directory is `workdir`:
/// absolute and cleaned, a trailing "/" kept, "/." kept, and "." or "" being the
/// working directory itself as a directory.
pub fn dest_path(workdir: &str, dest: &str) -> String {
    let dest = if !dest.starts_with('/') && (dest.is_empty() || dest == ".") {
        "./"
    } else {
        dest
    };
    let mut p = if dest.starts_with('/') {
        lexical::clean_str(dest)
    } else {
        lexical::join_str(&["/", workdir, dest])
    };
    if dest.ends_with('/') {
        if !p.ends_with('/') {
            p.push('/');
        }
    } else if dest.ends_with("/.") {
        if p != "/" {
            p.push('/');
        }
        p.push('.');
    }
    p
}

/// Symbolic and octal modes as chmod(1) takes them, applied to permission bits: a port
/// of BSD setmode(3) and getmode(3) by way of tonistiigi/dchapes-mode.
pub mod mode {
    use std::fmt;

    const IS_UID: u16 = 0o4000;
    const IS_GID: u16 = 0o2000;
    const IS_TXT: u16 = 0o1000;
    const I_RWXU: u16 = 0o700;
    const I_RUSER: u16 = 0o400;
    const I_WUSER: u16 = 0o200;
    const I_XUSER: u16 = 0o100;
    const I_RWXG: u16 = 0o070;
    const I_RGROUP: u16 = 0o040;
    const I_WGROUP: u16 = 0o020;
    const I_XGROUP: u16 = 0o010;
    const I_RWXO: u16 = 0o007;
    const I_ROTHER: u16 = 0o004;
    const I_WOTHER: u16 = 0o002;
    const I_XOTHER: u16 = 0o001;
    const STANDARD_BITS: u16 = IS_UID | IS_GID | I_RWXU | I_RWXG | I_RWXO;

    const CMD2_CLEAR: u8 = 1;
    const CMD2_SET: u8 = 2;
    const CMD2_GBITS: u8 = 4;
    const CMD2_OBITS: u8 = 8;
    const CMD2_UBITS: u8 = 16;

    /// A mode that does not parse, with the reference's messages.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum ModeError {
        Syntax,
        /// A mode starting with a digit that is not an octal number that fits.
        Num {
            input: String,
            out_of_range: bool,
        },
    }

    impl fmt::Display for ModeError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                ModeError::Syntax => f.write_str("invalid syntax"),
                ModeError::Num {
                    input,
                    out_of_range,
                } => write!(
                    f,
                    "strconv.ParseInt: parsing {}: {}",
                    super::quote(input),
                    if *out_of_range {
                        "value out of range"
                    } else {
                        "invalid syntax"
                    }
                ),
            }
        }
    }

    impl std::error::Error for ModeError {}

    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    struct BitCmd {
        cmd: u8,
        cmd2: u8,
        bits: u16,
    }

    /// A parsed mode: the changes it makes to permission bits.
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub struct Set {
        cmds: Vec<BitCmd>,
    }

    pub fn parse(s: &str) -> Result<Set, ModeError> {
        parse_with_umask(s, 0)
    }

    /// The octal form as the reference reads it: 16-bit signed, base 8.
    fn parse_octal(s: &str) -> Result<i64, ModeError> {
        let num = |out_of_range| ModeError::Num {
            input: s.to_string(),
            out_of_range,
        };
        let mut n: u64 = 0;
        for c in s.bytes() {
            let d = match c {
                b'0'..=b'7' => (c - b'0') as u64,
                _ => return Err(num(false)),
            };
            n = n * 8 + d;
            if n > u16::MAX as u64 {
                return Err(num(true));
            }
        }
        if n >= 1 << 15 {
            return Err(num(true));
        }
        Ok(n as i64)
    }

    /// Parses a mode; `umask` limits what a clause without a who (u, g, o, a) sets.
    pub fn parse_with_umask(s: &str, umask: u32) -> Result<Set, ModeError> {
        let mut m = Set::default();
        if s.is_empty() {
            return Err(ModeError::Syntax);
        }
        if s.as_bytes()[0].is_ascii_digit() {
            let v = parse_octal(s)?;
            if v & !((STANDARD_BITS | IS_TXT) as i64) != 0 {
                return Err(ModeError::Syntax);
            }
            m.addcmd(b'=', STANDARD_BITS | IS_TXT, v as u16, 0);
            return Ok(m);
        }
        let mask: u16 = !(umask as u16);
        let mut s = s.as_bytes();
        let mut equal_op_done = false;
        'clauses: loop {
            let mut who: u16 = 0;
            loop {
                let Some(&c) = s.first() else {
                    return Err(ModeError::Syntax);
                };
                match c {
                    b'a' => who |= STANDARD_BITS,
                    b'u' => who |= IS_UID | I_RWXU,
                    b'g' => who |= IS_GID | I_RWXG,
                    b'o' => who |= I_RWXO,
                    _ => break,
                }
                s = &s[1..];
            }
            loop {
                let op = s[0];
                s = &s[1..];
                match op {
                    b'+' | b'-' => {}
                    b'=' => equal_op_done = false,
                    _ => return Err(ModeError::Syntax),
                }
                who &= !IS_TXT;
                let mut perm: u16 = 0;
                let mut perm_x: u16 = 0;
                loop {
                    let b = s.first().copied().unwrap_or(0);
                    match b {
                        b'r' => perm |= I_RUSER | I_RGROUP | I_ROTHER,
                        b's' => {
                            // Set-id means nothing for "other" alone.
                            if who == 0 || who & !I_RWXO != 0 {
                                perm |= IS_UID | IS_GID;
                            }
                        }
                        b't' => {
                            if who == 0 || who & !I_RWXO != 0 {
                                who |= IS_TXT;
                                perm |= IS_TXT;
                            }
                        }
                        b'w' => perm |= I_WUSER | I_WGROUP | I_WOTHER,
                        b'X' => {
                            if op != b'-' {
                                perm_x = I_XUSER | I_XGROUP | I_XOTHER;
                            } else {
                                perm |= I_XUSER | I_XGROUP | I_XOTHER;
                            }
                        }
                        b'x' => perm |= I_XUSER | I_XGROUP | I_XOTHER,
                        b'u' | b'g' | b'o' => {
                            // Copying from a class flushes what was gathered so far.
                            if perm != 0 {
                                m.addcmd(op, who, perm, mask);
                                perm = 0;
                            }
                            if op == b'=' {
                                equal_op_done = true;
                            }
                            if perm_x != 0 {
                                m.addcmd(b'X', who, perm_x, mask);
                                perm_x = 0;
                            }
                            m.addcmd(b, who, op as u16, mask);
                        }
                        _ => {
                            if perm != 0 || (op == b'=' && !equal_op_done) {
                                if op == b'=' {
                                    equal_op_done = true;
                                }
                                m.addcmd(op, who, perm, mask);
                            }
                            if perm_x != 0 {
                                m.addcmd(b'X', who, perm_x, mask);
                            }
                            break;
                        }
                    }
                    s = &s[1..];
                }
                match s.first() {
                    None => break 'clauses,
                    // Another operator for the same who, as in "u+r-w".
                    Some(&c) if c != b',' => continue,
                    Some(_) => {
                        s = &s[1..];
                        continue 'clauses;
                    }
                }
            }
        }
        m.compress();
        Ok(m)
    }

    impl Set {
        fn addcmd(&mut self, op: u8, who: u16, oparg: u16, mask: u16) {
            let mut c = BitCmd::default();
            match op {
                b'=' | b'+' | b'-' | b'X' => {
                    let mut op = op;
                    if op == b'=' {
                        c.cmd = b'-';
                        c.bits = if who != 0 { who } else { STANDARD_BITS };
                        self.cmds.push(c);
                        op = b'+';
                    }
                    c.cmd = op;
                    c.bits = if who != 0 { who & oparg } else { mask & oparg };
                }
                b'u' | b'g' | b'o' => {
                    c.cmd = op;
                    if who != 0 {
                        if who & I_RUSER != 0 {
                            c.cmd2 |= CMD2_UBITS;
                        }
                        if who & I_RGROUP != 0 {
                            c.cmd2 |= CMD2_GBITS;
                        }
                        if who & I_ROTHER != 0 {
                            c.cmd2 |= CMD2_OBITS;
                        }
                        c.bits = !0;
                    } else {
                        c.cmd2 = CMD2_UBITS | CMD2_GBITS | CMD2_OBITS;
                        c.bits = mask;
                    }
                    match oparg as u8 {
                        b'+' => c.cmd2 |= CMD2_SET,
                        b'-' => c.cmd2 |= CMD2_CLEAR,
                        b'=' => c.cmd2 |= CMD2_SET | CMD2_CLEAR,
                        _ => {}
                    }
                }
                _ => return,
            }
            self.cmds.push(c);
        }

        /// Merges each run of `+`, `-` and `X` into at most one of each.
        fn compress(&mut self) {
            let mut out = Vec::with_capacity(self.cmds.len());
            let mut i = 0;
            while i < self.cmds.len() {
                let c = self.cmds[i];
                if !b"+-X".contains(&c.cmd) {
                    out.push(c);
                    i += 1;
                    continue;
                }
                let (mut setbits, mut clrbits, mut xbits) = (0u16, 0u16, 0u16);
                while i < self.cmds.len() {
                    let c = self.cmds[i];
                    match c.cmd {
                        b'-' => {
                            clrbits |= c.bits;
                            setbits &= !c.bits;
                            xbits &= !c.bits;
                        }
                        b'+' => {
                            setbits |= c.bits;
                            clrbits &= !c.bits;
                            xbits &= !c.bits;
                        }
                        b'X' => xbits |= c.bits & !setbits,
                        _ => break,
                    }
                    i += 1;
                }
                for (cmd, bits) in [(b'-', clrbits), (b'+', setbits), (b'X', xbits)] {
                    if bits != 0 {
                        out.push(BitCmd { cmd, cmd2: 0, bits });
                    }
                }
            }
            self.cmds = out;
        }

        /// Applies the changes to an st_mode: its permission bits (07777) change, its
        /// file type stays, and decides what `X` does.
        pub fn apply(&self, st_mode: u32) -> u32 {
            let is_dir = st_mode & super::S_IFMT == super::S_IFDIR;
            let omode = (st_mode & 0o7777) as u16;
            let mut newmode = omode;
            for c in &self.cmds {
                match c.cmd {
                    b'u' | b'g' | b'o' => {
                        let value = match c.cmd {
                            b'u' => (newmode & I_RWXU) >> 6,
                            b'g' => (newmode & I_RWXG) >> 3,
                            _ => newmode & I_RWXO,
                        };
                        if c.cmd2 & CMD2_CLEAR != 0 {
                            let clrval = if c.cmd2 & CMD2_SET != 0 {
                                I_RWXO
                            } else {
                                value
                            };
                            if c.cmd2 & CMD2_UBITS != 0 {
                                newmode &= !((clrval << 6) & c.bits);
                            }
                            if c.cmd2 & CMD2_GBITS != 0 {
                                newmode &= !((clrval << 3) & c.bits);
                            }
                            if c.cmd2 & CMD2_OBITS != 0 {
                                newmode &= !(clrval & c.bits);
                            }
                        }
                        if c.cmd2 & CMD2_SET != 0 {
                            if c.cmd2 & CMD2_UBITS != 0 {
                                newmode |= (value << 6) & c.bits;
                            }
                            if c.cmd2 & CMD2_GBITS != 0 {
                                newmode |= (value << 3) & c.bits;
                            }
                            if c.cmd2 & CMD2_OBITS != 0 {
                                newmode |= value & c.bits;
                            }
                        }
                    }
                    b'+' => newmode |= c.bits,
                    b'-' => newmode &= !c.bits,
                    b'X' if omode & (I_XUSER | I_XGROUP | I_XOTHER) != 0 || is_dir => {
                        newmode |= c.bits;
                    }
                    _ => {}
                }
            }
            (st_mode & !0o7777) | newmode as u32
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::os::unix::fs::symlink as mksymlink;

    fn is_root() -> bool {
        nix::unistd::geteuid().is_root()
    }

    /// Steps that build a tree, as the reference's tests describe theirs.
    enum Op<'a> {
        Dir(&'a str, u32),
        File(&'a str, &'a [u8], u32),
        Symlink(&'a str, &'a str),
        Link(&'a str, &'a str),
    }

    fn apply(root: &Path, ops: &[Op<'_>]) {
        for op in ops {
            match *op {
                Op::Dir(p, mode) => {
                    let p = root.join(p.trim_start_matches('/'));
                    fs::create_dir_all(&p).unwrap();
                    fs::set_permissions(&p, Permissions::from_mode(mode)).unwrap();
                }
                Op::File(p, data, mode) => {
                    let p = root.join(p.trim_start_matches('/'));
                    fs::write(&p, data).unwrap();
                    fs::set_permissions(&p, Permissions::from_mode(mode)).unwrap();
                }
                Op::Symlink(target, p) => {
                    let p = root.join(p.trim_start_matches('/'));
                    if let Some(parent) = p.parent() {
                        fs::create_dir_all(parent).unwrap();
                    }
                    mksymlink(target, &p).unwrap();
                }
                Op::Link(old, new) => {
                    fs::hard_link(
                        root.join(old.trim_start_matches('/')),
                        root.join(new.trim_start_matches('/')),
                    )
                    .unwrap();
                }
            }
        }
    }

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    /// Everything below `root` that a copy should reproduce: type, permission bits,
    /// owner, content or link target, and which paths are the same file.
    fn snapshot(root: &Path) -> (BTreeMap<String, String>, Vec<Vec<String>>) {
        let mut entries = BTreeMap::new();
        let mut inodes: BTreeMap<u64, Vec<String>> = BTreeMap::new();
        fn visit(
            root: &Path,
            dir: &Path,
            entries: &mut BTreeMap<String, String>,
            inodes: &mut BTreeMap<u64, Vec<String>>,
        ) {
            let mut names: Vec<_> = fs::read_dir(dir)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            names.sort();
            for name in names {
                let p = dir.join(&name);
                let meta = fs::symlink_metadata(&p).unwrap();
                let rel = p.strip_prefix(root).unwrap().display().to_string();
                let kind = meta.mode() & S_IFMT;
                let what = if kind == S_IFLNK {
                    format!("symlink {}", fs::read_link(&p).unwrap().display())
                } else if kind == S_IFDIR {
                    format!("dir {:o}", meta.mode() & 0o7777)
                } else {
                    let data = fs::read(&p).unwrap_or_default();
                    format!("file {:o} {:?}", meta.mode() & 0o7777, data)
                };
                entries.insert(rel.clone(), format!("{what} {}:{}", meta.uid(), meta.gid()));
                if kind == S_IFREG && meta.nlink() > 1 {
                    inodes.entry(meta.ino()).or_default().push(rel);
                }
                if kind == S_IFDIR {
                    visit(root, &p, entries, inodes);
                }
            }
        }
        visit(root, root, &mut entries, &mut inodes);
        let mut links: Vec<Vec<String>> = inodes.into_values().collect();
        links.sort();
        (entries, links)
    }

    fn assert_same_tree(a: &Path, b: &Path) {
        assert_eq!(snapshot(a), snapshot(b));
    }

    #[derive(Default)]
    struct Changes(Vec<String>);

    impl Changes {
        fn record(&mut self) -> impl FnMut(&Path, &Metadata) -> Result<()> + '_ {
            |p, _| {
                self.0.push(format!("add:{}", p.display()));
                Ok(())
            }
        }

        fn sorted(&self) -> String {
            let mut v = self.0.clone();
            v.sort();
            v.join(",")
        }
    }

    fn copy_tree_with_changes(t1: &Path, t2: &Path) -> String {
        let mut ch = Changes::default();
        let mut rec = ch.record();
        copy(
            t1,
            "/.",
            t2,
            "/",
            CopyInfo {
                change_fn: Some(&mut rec),
                ..CopyInfo::default()
            },
        )
        .unwrap();
        drop(rec);
        ch.sorted()
    }

    #[test]
    fn copy_directory() {
        let t1 = tmp();
        let t2 = tmp();
        apply(
            t1.path(),
            &[
                Op::Dir("/etc/", 0o755),
                Op::File("/etc/hosts", b"localhost 127.0.0.1", 0o644),
                Op::Link("/etc/hosts", "/etc/hosts.allow"),
                Op::Dir("/usr/local/lib", 0o755),
                Op::File("/usr/local/lib/libnothing.so", &[0, 0], 0o755),
                Op::Symlink("libnothing.so", "/usr/local/lib/libnothing.so.2"),
                Op::Dir("/home", 0o755),
            ],
        );
        // /usr and /usr/local come from create_dir_all with the umask; the copy keeps
        // whatever they got.
        let changes = copy_tree_with_changes(t1.path(), t2.path());
        assert_eq!(
            changes,
            "add:/etc,add:/etc/hosts,add:/etc/hosts.allow,add:/home,add:/usr,add:/usr/local,\
             add:/usr/local/lib,add:/usr/local/lib/libnothing.so,add:/usr/local/lib/libnothing.so.2"
        );
        assert_same_tree(t1.path(), t2.path());
    }

    // A symlink copied before its target must not be followed by later operations.
    #[test]
    fn copy_directory_with_local_symlink() {
        let t1 = tmp();
        let t2 = tmp();
        apply(
            t1.path(),
            &[
                Op::File("nothing.txt", &[0, 0], 0o755),
                Op::Symlink("nothing.txt", "link-no-nothing.txt"),
            ],
        );
        assert_eq!(
            copy_tree_with_changes(t1.path(), t2.path()),
            "add:/link-no-nothing.txt,add:/nothing.txt"
        );
        assert_same_tree(t1.path(), t2.path());
    }

    #[test]
    fn copy_to_work_dir() {
        let t1 = tmp();
        apply(t1.path(), &[Op::File("foo.txt", b"contents", 0o755)]);
        let t2 = tmp();
        copy(
            t1.path(),
            "foo.txt",
            t2.path(),
            "foo.txt",
            CopyInfo::default(),
        )
        .unwrap();
        assert_same_tree(t1.path(), t2.path());
    }

    #[test]
    fn copy_single_file() {
        let t1 = tmp();
        apply(t1.path(), &[Op::File("foo.txt", b"contents", 0o755)]);
        let t2 = tmp();
        copy(t1.path(), "foo.txt", t2.path(), "/", CopyInfo::default()).unwrap();
        assert_same_tree(t1.path(), t2.path());
        let t3 = tmp();
        copy(
            t1.path(),
            "foo.txt",
            t3.path(),
            "foo.txt",
            CopyInfo::default(),
        )
        .unwrap();
        assert_same_tree(t1.path(), t3.path());
        let t4 = tmp();
        copy(
            t1.path(),
            "foo.txt",
            t4.path(),
            "foo2.txt",
            CopyInfo::default(),
        )
        .unwrap();
        assert!(t4.path().join("foo2.txt").exists());
        let mut ch = Changes::default();
        let mut rec = ch.record();
        copy(
            t1.path(),
            "foo.txt",
            t4.path(),
            "a/b/c/foo2.txt",
            CopyInfo {
                change_fn: Some(&mut rec),
                ..CopyInfo::default()
            },
        )
        .unwrap();
        drop(rec);
        assert!(t4.path().join("a/b/c/foo2.txt").exists());
        assert_eq!(ch.sorted(), "add:/a/b/c/foo2.txt");
    }

    #[test]
    fn copy_override_file() {
        let t1 = tmp();
        apply(t1.path(), &[Op::File("foo.txt", b"contents", 0o755)]);
        let t2 = tmp();
        copy(
            t1.path(),
            "foo.txt",
            t2.path(),
            "foo.txt",
            CopyInfo::default(),
        )
        .unwrap();
        assert_same_tree(t1.path(), t2.path());
        copy(
            t1.path(),
            "foo.txt",
            t2.path(),
            "foo.txt",
            CopyInfo::default(),
        )
        .unwrap();
        assert_same_tree(t1.path(), t2.path());
        copy(t1.path(), "/.", t2.path(), "/", CopyInfo::default()).unwrap();
        assert_same_tree(t1.path(), t2.path());
    }

    #[test]
    fn copy_directory_basename() {
        let t1 = tmp();
        apply(
            t1.path(),
            &[
                Op::Dir("foo", 0o755),
                Op::Dir("foo/bar", 0o755),
                Op::File("foo/bar/baz.txt", b"contents", 0o755),
            ],
        );
        let t2 = tmp();
        let mut ch = Changes::default();
        let mut rec = ch.record();
        copy(
            t1.path(),
            "foo",
            t2.path(),
            "foo",
            CopyInfo {
                change_fn: Some(&mut rec),
                ..CopyInfo::default()
            },
        )
        .unwrap();
        drop(rec);
        assert_same_tree(t1.path(), t2.path());
        assert_eq!(ch.sorted(), "add:/foo,add:/foo/bar,add:/foo/bar/baz.txt");

        let mut ch = Changes::default();
        let mut rec = ch.record();
        copy(
            t1.path(),
            "foo",
            t2.path(),
            "foo",
            CopyInfo {
                copy_dir_contents: true,
                change_fn: Some(&mut rec),
                ..CopyInfo::default()
            },
        )
        .unwrap();
        drop(rec);
        assert_eq!(ch.sorted(), "add:/foo/bar,add:/foo/bar/baz.txt");
        assert_same_tree(t1.path(), t2.path());
    }

    #[test]
    fn copy_wildcards() {
        let t1 = tmp();
        apply(
            t1.path(),
            &[
                Op::File("foo.txt", b"foo-contents", 0o755),
                Op::File("foo.go", b"go-contents", 0o755),
                Op::File("bar.txt", b"bar-contents", 0o755),
            ],
        );
        let t2 = tmp();
        assert!(copy(t1.path(), "foo*", t2.path(), "/", CopyInfo::default()).is_err());
        let wild = || CopyInfo {
            allow_wildcards: true,
            ..CopyInfo::default()
        };
        copy(t1.path(), "foo*", t2.path(), "/", wild()).unwrap();
        assert!(t2.path().join("foo.txt").exists());
        assert!(t2.path().join("foo.go").exists());
        assert!(!t2.path().join("bar.txt").exists());
        let t3 = tmp();
        copy(t1.path(), "bar*", t3.path(), "foo.txt", wild()).unwrap();
        assert_eq!(
            fs::read(t3.path().join("foo.txt")).unwrap(),
            b"bar-contents"
        );
        let err = copy(t1.path(), "baz*", t3.path(), "/", wild()).unwrap_err();
        assert_eq!(format!("{err:#}"), "no matches found: baz*");
    }

    #[test]
    fn copy_existing_dir_dest() {
        // Giving files to another owner takes root.
        if !is_root() {
            return;
        }
        let t1 = tmp();
        apply(
            t1.path(),
            &[
                Op::Dir("dir", 0o755),
                Op::File("dir/foo.txt", b"foo-contents", 0o644),
                Op::File("dir/bar.txt", b"bar-contents", 0o644),
            ],
        );
        let t2 = tmp();
        apply(
            t2.path(),
            &[
                Op::Dir("dir", 0o700),
                Op::File("dir/bar.txt", b"old-bar-contents", 0o600),
            ],
        );
        for x in ["dir", "dir/bar.txt"] {
            lchown(t2.path().join(x), Some(1), Some(1)).unwrap();
        }
        copy(
            t1.path(),
            "dir",
            t2.path(),
            "dir",
            CopyInfo {
                copy_dir_contents: true,
                ..CopyInfo::default()
            },
        )
        .unwrap();
        let st = fs::symlink_metadata(t2.path().join("dir")).unwrap();
        assert_eq!(st.mode() & 0o777, 0o700);
        assert_eq!((st.uid(), st.gid()), (1, 1));
        assert!(t2.path().join("dir/foo.txt").exists());
        let st = fs::symlink_metadata(t2.path().join("dir/bar.txt")).unwrap();
        assert_eq!(st.mode() & 0o777, 0o644);
        assert_eq!((st.uid(), st.gid()), (0, 0));
        assert_eq!(
            fs::read(t2.path().join("dir/bar.txt")).unwrap(),
            b"bar-contents"
        );
    }

    #[test]
    fn copy_directory_chmod_chown() {
        // Giving files to another owner takes root.
        if !is_root() {
            return;
        }
        let t1 = tmp();
        apply(
            t1.path(),
            &[
                Op::Dir("dir", 0o755),
                Op::File("dir/foo.txt", b"foo-contents", 0o644),
                Op::Dir("dir/sub2", 0o755),
            ],
        );
        let t2 = tmp();
        let owner =
            |_: Option<User>| -> Result<Option<User>> { Ok(Some(User { uid: 100, gid: 200 })) };
        copy(
            t1.path(),
            "dir",
            t2.path(),
            "dest/sub",
            CopyInfo {
                copy_dir_contents: true,
                mode: Some(0o700),
                chown: Some(&owner),
                ..CopyInfo::default()
            },
        )
        .unwrap();
        for p in ["dest/sub", "dest/sub/foo.txt", "dest/sub/sub2"] {
            let st = fs::symlink_metadata(t2.path().join(p)).unwrap();
            assert_eq!((st.uid(), st.gid()), (100, 200), "{p}");
            assert_eq!(st.mode() & 0o777, 0o700, "{p}");
        }
    }

    fn mtime(p: &Path) -> (i64, i64) {
        let st = fs::metadata(p).unwrap();
        (st.mtime(), st.mtime_nsec())
    }

    #[test]
    fn copy_directory_contents_timestamp() {
        let t1 = tmp();
        let t2 = tmp();
        apply(
            t1.path(),
            &[
                Op::Dir("/foo/", 0o755),
                Op::Dir("/foo/bar", 0o755),
                Op::File("/foo/bar/baz", b"", 0o644),
            ],
        );
        copy(
            t1.path(),
            "/foo",
            t2.path(),
            "/foo",
            CopyInfo {
                copy_dir_contents: true,
                utime: Some(TimeSpec::new(0, 0)),
                ..CopyInfo::default()
            },
        )
        .unwrap();
        assert_same_tree(t1.path(), t2.path());
        for s in ["foo/bar/baz", "foo/bar", "foo"] {
            assert_eq!(mtime(&t2.path().join(s)), (0, 0), "{s}");
        }
    }

    #[test]
    fn copy_symlinks() {
        let t1 = tmp();
        apply(
            t1.path(),
            &[
                Op::Dir("testdir", 0o755),
                Op::File("testdir/foo.txt", b"foo-contents", 0o644),
                Op::Symlink("foo.txt", "testdir/link2"),
                Op::Symlink("/testdir", "link"),
            ],
        );
        let t2 = tmp();
        copy(
            t1.path(),
            "link/link2",
            t2.path(),
            "foo",
            CopyInfo {
                follow_links: true,
                ..CopyInfo::default()
            },
        )
        .unwrap();
        let st = fs::symlink_metadata(t2.path().join("foo")).unwrap();
        assert_eq!(st.mode() & 0o777, 0o644);
        assert!(!st.file_type().is_symlink());
        assert_eq!(fs::read(t2.path().join("foo")).unwrap(), b"foo-contents");
        let t3 = tmp();
        copy(
            t1.path(),
            "link/link2",
            t3.path(),
            "foo",
            CopyInfo::default(),
        )
        .unwrap();
        let st = fs::symlink_metadata(t3.path().join("foo")).unwrap();
        assert!(st.file_type().is_symlink());
        assert_eq!(
            fs::read_link(t3.path().join("foo")).unwrap(),
            Path::new("foo.txt")
        );
    }

    #[test]
    fn copy_with_always_replace_existing_dest_paths() {
        let dest = tmp();
        apply(
            dest.path(),
            &[
                Op::Dir("root", 0o755),
                Op::Dir("root/overwritedir", 0o755),
                Op::File("root/overwritedir/subfile", b"", 0o755),
                Op::File("root/overwritefile", b"", 0o755),
                Op::Symlink("dir", "root/overwritesymlink"),
                Op::Dir("root/dir", 0o755),
                Op::File("root/dir/dirfile1", b"", 0o755),
                Op::Dir("root/dir/overwritesubdir", 0o755),
                Op::File("root/dir/overwritesubfile", b"", 0o755),
                Op::Symlink("dirfile1", "root/dir/overwritesymlink"),
            ],
        );
        let src = tmp();
        apply(
            src.path(),
            &[
                Op::Dir("root", 0o755),
                Op::File("root/overwritedir", b"", 0o755),
                Op::Dir("root/overwritefile", 0o755),
                Op::File("root/overwritefile/foo", b"", 0o755),
                Op::Dir("root/overwritesymlink", 0o755),
                Op::Dir("root/dir", 0o755),
                Op::File("root/dir/dirfile2", b"", 0o755),
                Op::File("root/dir/overwritesubdir", b"", 0o755),
                Op::Dir("root/dir/overwritesubfile", 0o755),
                Op::Dir("root/dir/overwritesymlink", 0o755),
            ],
        );
        let expected = tmp();
        apply(
            expected.path(),
            &[
                Op::Dir("root", 0o755),
                Op::File("root/overwritedir", b"", 0o755),
                Op::Dir("root/overwritefile", 0o755),
                Op::File("root/overwritefile/foo", b"", 0o755),
                Op::Dir("root/overwritesymlink", 0o755),
                Op::Dir("root/dir", 0o755),
                Op::File("root/dir/dirfile1", b"", 0o755),
                Op::File("root/dir/dirfile2", b"", 0o755),
                Op::File("root/dir/overwritesubdir", b"", 0o755),
                Op::Dir("root/dir/overwritesubfile", 0o755),
                Op::Dir("root/dir/overwritesymlink", 0o755),
            ],
        );
        copy(
            src.path(),
            "root",
            dest.path(),
            "root",
            CopyInfo {
                always_replace_existing_dest_paths: true,
                copy_dir_contents: true,
                ..CopyInfo::default()
            },
        )
        .unwrap();
        assert_same_tree(dest.path(), expected.path());
    }

    #[test]
    fn copy_include_exclude() {
        let t1 = tmp();
        apply(
            t1.path(),
            &[
                Op::Dir("bar", 0o755),
                Op::File("bar/foo", b"foo-contents", 0o755),
                Op::Dir("bar/baz", 0o755),
                Op::File("bar/baz/foo3", b"foo3-contents", 0o755),
                Op::File("foo2", b"foo2-contents", 0o755),
            ],
        );
        type Case<'a> = (&'a [&'a str], &'a [&'a str], &'a [&'a str], &'a str);
        let cases: &[Case<'_>] = &[
            (
                &["bar"],
                &[],
                &["bar", "bar/foo", "bar/baz", "bar/baz/foo3"],
                "add:/bar,add:/bar/baz,add:/bar/baz/foo3,add:/bar/foo",
            ),
            (
                &["*"],
                &[],
                &["bar", "bar/foo", "bar/baz", "bar/baz/foo3", "foo2"],
                "add:/bar,add:/bar/baz,add:/bar/baz/foo3,add:/bar/foo,add:/foo2",
            ),
            (&["bar/foo"], &[], &["bar", "bar/foo"], "add:/bar/foo"),
            (
                &["bar", "!bar/foo"],
                &[],
                &["bar", "bar/baz", "bar/baz/foo3"],
                "add:/bar,add:/bar/baz,add:/bar/baz/foo3",
            ),
            (
                &["bar/foo", "foo*"],
                &[],
                &["bar", "bar/foo", "foo2"],
                "add:/bar/foo,add:/foo2",
            ),
            (
                &["b*"],
                &[],
                &["bar", "bar/foo", "bar/baz", "bar/baz/foo3"],
                "add:/bar,add:/bar/baz,add:/bar/baz/foo3,add:/bar/foo",
            ),
            (&["bar/f*"], &[], &["bar", "bar/foo"], ""),
            (&["bar/g*"], &[], &[], ""),
            (&["b*/f*"], &[], &["bar", "bar/foo"], ""),
            (&["b*/foo"], &[], &["bar", "bar/foo"], ""),
            (
                &["b*/"],
                &[],
                &["bar", "bar/foo", "bar/baz", "bar/baz/foo3"],
                "",
            ),
            (
                &["bar/*/foo3"],
                &[],
                &["bar", "bar/baz", "bar/baz/foo3"],
                "",
            ),
            (
                &[],
                &["bar*", "!bar/baz"],
                &["bar", "bar/baz", "bar/baz/foo3", "foo2"],
                "",
            ),
            (
                &[],
                &["**", "!bar/baz"],
                &["bar", "bar/baz", "bar/baz/foo3"],
                "",
            ),
            (
                &[],
                &["**", "!bar/baz", "bar/baz/foo3"],
                &["bar", "bar/baz"],
                "",
            ),
            (&["bar"], &["bar/baz"], &["bar", "bar/foo"], ""),
            (&["**/foo3"], &[], &["bar", "bar/baz", "bar/baz/foo3"], ""),
            (
                &["**/baz"],
                &[],
                &["bar", "bar/baz", "bar/baz/foo3"],
                "add:/bar/baz,add:/bar/baz/foo3",
            ),
            (
                &["**/bar"],
                &[],
                &["bar", "bar/foo", "bar/baz", "bar/baz/foo3"],
                "add:/bar,add:/bar/baz,add:/bar/baz/foo3,add:/bar/foo",
            ),
            (
                &["bar"],
                &["**/foo3"],
                &["bar", "bar/foo", "bar/baz"],
                "add:/bar,add:/bar/baz,add:/bar/foo",
            ),
            (
                &[],
                &["bar/baz"],
                &["bar", "bar/foo", "foo2"],
                "add:/bar,add:/bar/foo,add:/foo2",
            ),
        ];
        for &(inc, exc, want, want_changes) in cases {
            let t2 = tmp();
            let mut ch = Changes::default();
            let mut rec = ch.record();
            copy(
                t1.path(),
                "/",
                t2.path(),
                "/",
                CopyInfo {
                    include_patterns: inc.iter().map(|s| s.to_string()).collect(),
                    exclude_patterns: exc.iter().map(|s| s.to_string()).collect(),
                    change_fn: Some(&mut rec),
                    ..CopyInfo::default()
                },
            )
            .unwrap();
            drop(rec);
            let got: Vec<&str> = [
                "bar",
                "bar/foo",
                "bar/baz",
                "bar/baz/asdf",
                "bar/baz/asdf/x",
                "bar/baz/foo3",
                "foo2",
            ]
            .into_iter()
            .filter(|p| t2.path().join(p).exists())
            .collect();
            assert_eq!(got, want, "include {inc:?} exclude {exc:?}");
            if !want_changes.is_empty() {
                assert_eq!(ch.sorted(), want_changes, "include {inc:?} exclude {exc:?}");
            }
        }
    }

    #[test]
    fn copy_file_with_path_timestamp() {
        let t1 = tmp();
        let t2 = tmp();
        apply(
            t1.path(),
            &[Op::Dir("/foo/", 0o755), Op::File("/foo/bar", b"", 0o644)],
        );
        copy(
            t1.path(),
            "/foo/bar",
            t2.path(),
            "/test1/test2/test3/bar",
            CopyInfo {
                copy_dir_contents: true,
                utime: Some(TimeSpec::new(0, 0)),
                ..CopyInfo::default()
            },
        )
        .unwrap();
        for s in [
            "test1/test2/test3/bar",
            "test1/test2/test3",
            "test1/test2",
            "test1",
        ] {
            assert_eq!(mtime(&t2.path().join(s)), (0, 0), "path: {s}");
        }
    }

    #[test]
    fn copy_devices_and_fifo() {
        // Device nodes take root.
        if !is_root() {
            return;
        }
        let t1 = tmp();
        let p = |n: &str| t1.path().join(n);
        mknod(
            &p("char"),
            SFlag::S_IFCHR,
            Mode::from_bits_truncate(0o444),
            nix::sys::stat::makedev(1, 9),
        )
        .unwrap();
        mknod(
            &p("block"),
            SFlag::S_IFBLK,
            Mode::from_bits_truncate(0o441),
            nix::sys::stat::makedev(3, 2),
        )
        .unwrap();
        mknod(
            &p("socket"),
            SFlag::S_IFSOCK,
            Mode::from_bits_truncate(0o555),
            0,
        )
        .unwrap();
        nix::unistd::mkfifo(&p("fifo"), Mode::from_bits_truncate(0o555)).unwrap();
        let t2 = tmp();
        copy(t1.path(), ".", t2.path(), ".", CopyInfo::default()).unwrap();
        let st = |n: &str| fs::symlink_metadata(t2.path().join(n)).unwrap();
        assert_eq!(st("char").mode() & S_IFMT, S_IFCHR);
        assert_eq!(st("char").mode() & 0o777, 0o444);
        assert_eq!(st("char").rdev(), nix::sys::stat::makedev(1, 9));
        assert_eq!(st("block").mode() & S_IFMT, S_IFBLK);
        assert_eq!(st("block").mode() & 0o777, 0o441);
        assert_eq!(st("fifo").mode() & S_IFMT, S_IFIFO);
        assert_eq!(st("fifo").mode() & 0o777, 0o555);
        assert_ne!(st("socket").mode() & S_IFMT, S_IFSOCK);
        assert_eq!(st("socket").mode() & 0o777, 0o555);
    }

    #[test]
    fn copy_fifo_and_socket_unprivileged() {
        let t1 = tmp();
        nix::unistd::mkfifo(&t1.path().join("fifo"), Mode::from_bits_truncate(0o640)).unwrap();
        std::os::unix::net::UnixListener::bind(t1.path().join("socket")).unwrap();
        fs::set_permissions(t1.path().join("socket"), Permissions::from_mode(0o751)).unwrap();
        let t2 = tmp();
        copy(t1.path(), ".", t2.path(), ".", CopyInfo::default()).unwrap();
        let st = |n: &str| fs::symlink_metadata(t2.path().join(n)).unwrap();
        assert_eq!(st("fifo").mode() & S_IFMT, S_IFIFO);
        assert_eq!(st("fifo").mode() & 0o777, 0o640);
        assert_eq!(st("socket").mode() & S_IFMT, S_IFREG);
        assert_eq!(st("socket").mode() & 0o777, 0o751);
    }

    #[test]
    fn copy_setuid() {
        // Device nodes take root.
        if !is_root() {
            return;
        }
        let t1 = tmp();
        mknod(
            &t1.path().join("char"),
            SFlag::S_IFCHR,
            Mode::from_bits_truncate(0o444),
            nix::sys::stat::makedev(1, 9),
        )
        .unwrap();
        let t2 = tmp();
        copy(t1.path(), ".", t2.path(), ".", CopyInfo::default()).unwrap();
        assert_eq!(
            fs::symlink_metadata(t2.path().join("char")).unwrap().mode() & 0o7777,
            0o444
        );
        let t3 = tmp();
        copy(
            t1.path(),
            ".",
            t3.path(),
            ".",
            CopyInfo {
                mode: Some(0o444 | 0o4000),
                ..CopyInfo::default()
            },
        )
        .unwrap();
        assert_eq!(
            fs::symlink_metadata(t3.path().join("char")).unwrap().mode() & 0o7777,
            0o4444
        );
    }

    #[test]
    fn copy_mode_text_format() {
        let t1 = tmp();
        apply(
            t1.path(),
            &[
                Op::File("file", b"hello", 0o644),
                Op::File("executable_file", b"world", 0o755),
                Op::Dir("dir", 0o750),
                Op::Dir("restricted_dir", 0o700),
            ],
        );
        let cases: &[(&str, u32, u32, u32, u32)] = &[
            ("go-w", 0o644, 0o755, 0o750, 0o700),
            ("u+x", 0o744, 0o755, 0o750, 0o700),
            ("g-rwx", 0o604, 0o705, 0o700, 0o700),
            ("o+r", 0o644, 0o755, 0o754, 0o704),
            ("a-x", 0o644, 0o644, 0o640, 0o600),
            ("o-rwx,g+x", 0o650, 0o750, 0o750, 0o710),
            ("a+X", 0o644, 0o755, 0o751, 0o711),
            ("u=rwX,go=rX", 0o644, 0o755, 0o755, 0o755),
            ("u=rX,go=r", 0o444, 0o544, 0o544, 0o544),
            ("u-x,u+w", 0o644, 0o655, 0o650, 0o600),
            ("u+x,o+x", 0o745, 0o755, 0o751, 0o701),
            ("g+rw,o+rw", 0o666, 0o777, 0o776, 0o766),
            ("a=r", 0o444, 0o444, 0o444, 0o444),
            ("u=rwx,g=,o=", 0o700, 0o700, 0o700, 0o700),
            ("o-rwx", 0o640, 0o750, 0o750, 0o700),
            ("g-r,a+x", 0o715, 0o715, 0o711, 0o711),
            ("u+rw,g+r,o-x,o+w", 0o646, 0o756, 0o752, 0o742),
        ];
        for &(mode_str, file, exec, dir, restricted) in cases {
            let t2 = tmp();
            copy(
                t1.path(),
                ".",
                t2.path(),
                ".",
                CopyInfo {
                    mode_str: Some(mode_str.to_string()),
                    ..CopyInfo::default()
                },
            )
            .unwrap();
            let perm = |n: &str| fs::symlink_metadata(t2.path().join(n)).unwrap().mode() & 0o777;
            assert_eq!(perm("file"), file, "{mode_str} file");
            assert_eq!(perm("executable_file"), exec, "{mode_str} executable");
            assert_eq!(perm("dir"), dir, "{mode_str} dir");
            assert_eq!(
                perm("restricted_dir"),
                restricted,
                "{mode_str} restricted dir"
            );
        }
    }

    // The process umask is left alone here: tests run in threads of one process, and
    // the reference's switch to skip the chmod after mkdir is not ported.
    #[test]
    fn mkdir_all_ignores_umask() {
        for perm in [0o777, 0o755] {
            let dir = tmp();
            let path = dir.path().join("a/b/c");
            let created = mkdir_all(&path, perm, None, None).unwrap();
            assert_eq!(created.len(), 3);
            for p in created {
                assert_eq!(
                    fs::metadata(&p).unwrap().mode() & 0o777,
                    perm,
                    "{}",
                    p.display()
                );
            }
        }
    }

    #[test]
    fn mkdir_all_existing_path() {
        let dir = tmp();
        let existing = dir.path().join("existing");
        DirBuilder::new().mode(0o700).create(&existing).unwrap();
        fs::set_permissions(&existing, Permissions::from_mode(0o700)).unwrap();
        let new_path = existing.join("new");
        let created = mkdir_all(&new_path.join("."), 0o770, None, None).unwrap();
        assert_eq!(created, std::slice::from_ref(&new_path));
        assert_eq!(fs::metadata(&existing).unwrap().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(&new_path).unwrap().mode() & 0o777, 0o770);
        let file = dir.path().join("file");
        fs::write(&file, b"").unwrap();
        let err = mkdir_all(&file, 0o755, None, None).unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            format!("mkdir {}: not a directory", file.display())
        );
    }

    // Symlink(old, new) of the resolver tests creates the parents of new.
    fn link(root: &Path, old: &str, new: &str) {
        let p = root.join(new);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        mksymlink(old, p).unwrap();
    }

    #[test]
    fn root_path_resolution() {
        struct Check<'a> {
            links: &'a [(&'a str, &'a str)],
            dirs: &'a [&'a str],
            scope: &'a str,
            unresolved: &'a str,
            expected: Option<&'a str>,
        }
        let check = |links, scope, unresolved, expected| Check {
            links,
            dirs: &[],
            scope,
            unresolved,
            expected: Some(expected),
        };
        let checks = [
            check(&[("/b", "fs/a/d")], "", "fs/a/d/c/data", "b/c/data"),
            check(&[("a", "fs/i")], "", "fs/i", "fs/a"),
            check(&[("realdir", "linkdir")], "linkdir", "foo/bar", "foo/bar"),
            check(&[("/b", "fs/a/d")], "", "fs/a/d", "b"),
            check(&[("../b", "fs/a/e")], "", "fs/a/e/c/data", "fs/b/c/data"),
            check(&[("../b", "fs/a/e")], "fs/a", "e", "b"),
            check(&[("../../../../test", "fs/a/f")], "", "fs/a/f", "test"),
            check(&[("../../../../test", "fs/a/f")], "fs", "a/f", "test"),
            check(
                &[
                    ("../g", "fs/b/h"),
                    ("../../../../../../../../../../../../root", "fs/g"),
                ],
                "",
                "fs/b/h",
                "root",
            ),
            check(&[("../i/a", "fs/j/k")], "fs/j", "k", "i/a"),
            check(&[("/", "foo")], "", "foo", ""),
            check(&[("/../../", "foo")], "", "foo", ""),
            check(&[("../../", "foo")], "", "foo", ""),
            check(
                &[("baz/target", "bar/foo")],
                "",
                "bar/foo",
                "bar/baz/target",
            ),
            check(
                &[("root2", "root"), ("../bar", "root2/foo")],
                "root",
                "foo",
                "bar",
            ),
            Check {
                links: &[
                    ("root2", "root"),
                    ("r/s", "root/a"),
                    ("../root/t", "root/r"),
                    ("/../u", "root/root/t/s/b"),
                    (".", "root/u/c"),
                    ("../v", "root/u/x/y"),
                    ("/../w", "root/u/v"),
                ],
                dirs: &["root2"],
                scope: "root",
                unresolved: "a/b/c/x/y/z",
                expected: Some("w/z"),
            },
            check(
                &[("/", "root/slash"), ("/idontexist/../slash", "root/sym")],
                "root",
                "sym/file",
                "file",
            ),
            check(
                &[("/foo/bar", "root/sym"), ("/sym/../baz", "root/hello")],
                "root",
                "hello",
                "foo/baz",
            ),
            Check {
                links: &[("foo", "root/foo")],
                dirs: &[],
                scope: "root",
                unresolved: "foo",
                expected: None,
            },
            Check {
                links: &[("foo", "bar"), ("bar", "foo")],
                dirs: &[],
                scope: "",
                unresolved: "foo",
                expected: None,
            },
            Check {
                links: &[
                    ("baz", "root/bar"),
                    ("../bak", "root/baz"),
                    ("/bar", "root/bak"),
                ],
                dirs: &[],
                scope: "root",
                unresolved: "bar",
                expected: None,
            },
        ];
        for c in checks {
            let dir = tmp();
            for d in c.dirs {
                fs::create_dir_all(dir.path().join(d)).unwrap();
            }
            for (old, new) in c.links {
                link(dir.path(), old, new);
            }
            let root = bytes_path(&lexical::join(&[
                path_bytes(dir.path()),
                c.scope.as_bytes(),
            ]));
            let got = root_path(&root, c.unresolved);
            match c.expected {
                Some(want) => {
                    let want = bytes_path(&lexical::join(&[path_bytes(&root), want.as_bytes()]));
                    assert_eq!(got.unwrap(), want, "{}", c.unresolved);
                }
                None => {
                    let err = got.unwrap_err();
                    assert!(err.is::<TooManyLinks>(), "{err:#}");
                    assert_eq!(format!("{err:#}"), "too many links");
                }
            }
        }
    }

    #[test]
    fn root_path_root_scope_and_empty() {
        let dir = tmp();
        let expected = fs::canonicalize(dir.path()).unwrap();
        assert_eq!(
            root_path(Path::new("/"), &expected.display().to_string()).unwrap(),
            expected
        );
        assert_eq!(root_path(dir.path(), "").unwrap(), dir.path());
    }

    #[test]
    fn copy_stays_inside_the_roots() {
        let parent = tmp();
        let src = parent.path().join("src");
        let dst = parent.path().join("dst");
        let victim = parent.path().join("victim");
        for d in [&src, &dst, &victim] {
            fs::create_dir(d).unwrap();
        }
        fs::write(victim.join("secret"), b"host").unwrap();
        fs::write(src.join("data"), b"inside").unwrap();
        // Links in both trees that would lead to the victim if followed on the host.
        mksymlink(&victim, src.join("abs")).unwrap();
        mksymlink("../victim", src.join("rel")).unwrap();
        mksymlink(&victim, dst.join("abs")).unwrap();
        mksymlink("../../victim", dst.join("rel")).unwrap();
        let run = |s: &str, d: &str| {
            let action = CopyAction {
                src: s.to_string(),
                dest: d.to_string(),
                follow_symlink: true,
                allow_wildcard: true,
                create_dest_path: true,
                ..CopyAction::default()
            };
            docopy(&src, &dst, &action)
        };
        // Sources: the links resolve inside the source root, where nothing is there.
        assert!(run("abs/secret", "/out").is_err());
        assert!(run("rel/secret", "/out").is_err());
        assert!(run("../victim/secret", "/out").is_err());
        assert!(run("../../victim/*", "/out/").is_err());
        assert!(!dst.join("out").exists());
        // Destinations: everything lands inside the destination root.
        run("data", "abs/x").unwrap();
        run("data", "rel/y").unwrap();
        run("data", "../../victim/z").unwrap();
        run("data", "/../../../victim/w").unwrap();
        let mut left: Vec<_> = fs::read_dir(&victim)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        left.sort();
        assert_eq!(left, ["secret"]);
        assert_eq!(
            fs::read(dst.join(victim.strip_prefix("/").unwrap()).join("x")).unwrap(),
            b"inside"
        );
        assert_eq!(fs::read(dst.join("victim/z")).unwrap(), b"inside");
        assert_eq!(fs::read(dst.join("victim/w")).unwrap(), b"inside");
        assert_eq!(fs::read(dst.join("victim/y")).unwrap(), b"inside");
    }

    #[test]
    fn docopy_destination_rules() {
        let src = tmp();
        apply(
            src.path(),
            &[
                Op::File("a.txt", b"a", 0o644),
                Op::Dir("dir", 0o755),
                Op::File("dir/b.txt", b"b", 0o644),
                Op::Dir("existing", 0o755),
            ],
        );
        let dst = tmp();
        apply(dst.path(), &[Op::Dir("existing", 0o755)]);
        let run = |s: &str, d: &str| {
            let cs = copy_source(s, false);
            let action = CopyAction::for_instruction(&cs, &dest_path("/", d));
            docopy(src.path(), dst.path(), &action)
        };
        // No trailing slash, nothing there: the file is the destination.
        run("a.txt", "/f").unwrap();
        assert_eq!(fs::read(dst.path().join("f")).unwrap(), b"a");
        // A trailing slash makes a directory.
        run("a.txt", "/d/").unwrap();
        assert_eq!(fs::read(dst.path().join("d/a.txt")).unwrap(), b"a");
        // An existing directory takes the file in.
        run("a.txt", "/existing").unwrap();
        assert_eq!(fs::read(dst.path().join("existing/a.txt")).unwrap(), b"a");
        // A directory source copies its contents.
        run("dir", "/c").unwrap();
        assert_eq!(fs::read(dst.path().join("c/b.txt")).unwrap(), b"b");
        run("dir", "/existing").unwrap();
        assert_eq!(fs::read(dst.path().join("existing/b.txt")).unwrap(), b"b");
        // A relative destination is relative to the working directory.
        let action =
            CopyAction::for_instruction(&copy_source("a.txt", false), &dest_path("/work", "."));
        docopy(src.path(), dst.path(), &action).unwrap();
        assert_eq!(fs::read(dst.path().join("work/a.txt")).unwrap(), b"a");
        // A wildcard that matches nothing copies nothing.
        run("*.none", "/x/").unwrap();
        assert!(!dst.path().join("x").exists());
        // Without a wildcard, a missing source fails, its path named inside the root.
        let err = run("missing", "/x").unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "lstat /missing: no such file or directory"
        );
    }

    #[test]
    fn docopy_flags() {
        let src = tmp();
        apply(src.path(), &[Op::File("a.txt", b"a", 0o600)]);
        let dst = tmp();
        let mut action = CopyAction {
            src: "nothing*".to_string(),
            dest: "/x".to_string(),
            allow_wildcard: true,
            create_dest_path: true,
            ..CopyAction::default()
        };
        let err = docopy(src.path(), dst.path(), &action).unwrap_err();
        assert_eq!(format!("{err:#}"), "/nothing* not found");
        action.src = "a.txt".to_string();
        action.dest = "/no/parent".to_string();
        action.create_dest_path = false;
        let err = docopy(src.path(), dst.path(), &action).unwrap_err();
        assert!(
            format!("{err:#}").starts_with("failed to stat /no/parent: lstat "),
            "{err:#}"
        );
        action.create_dest_path = true;
        action.timestamp = Some(1_500_000_000_123_456_789);
        action.set_chmod(parse_chmod("750").unwrap());
        docopy(src.path(), dst.path(), &action).unwrap();
        let st = fs::metadata(dst.path().join("no/parent")).unwrap();
        assert_eq!(st.mode() & 0o777, 0o750);
        assert_eq!((st.mtime(), st.mtime_nsec()), (1_500_000_000, 123_456_789));
        // The numeric mode also gives the directories created on the way their bits.
        assert_eq!(
            fs::metadata(dst.path().join("no")).unwrap().mode() & 0o777,
            0o750
        );
        assert_eq!(mtime(&dst.path().join("no")), (1_500_000_000, 123_456_789));
        // The unpack hook sees every source and can take it over.
        let seen = std::cell::RefCell::new(Vec::new());
        let hook = |r: &UnpackRequest<'_>| -> Result<bool> {
            assert_eq!(r.src_root, src.path());
            assert_eq!(r.dest_root, dst.path());
            assert_eq!(r.owner, None);
            assert_eq!(r.timestamp, Some(1_500_000_000_123_456_789));
            seen.borrow_mut()
                .push((r.src.to_path_buf(), r.dest.to_path_buf()));
            Ok(true)
        };
        action.dest = "/unpacked/".to_string();
        action.unpack = Some(&hook);
        docopy(src.path(), dst.path(), &action).unwrap();
        assert!(!dst.path().join("unpacked").exists());
        assert_eq!(
            seen.borrow().as_slice(),
            [(PathBuf::from("/a.txt"), PathBuf::from("/unpacked/"))]
        );
    }

    #[test]
    fn copy_parents() {
        let src = tmp();
        apply(
            src.path(),
            &[
                Op::Dir("a/b/c", 0o755),
                Op::File("a/b/c/file", b"x", 0o644),
                Op::File("a/b/other", b"y", 0o644),
                Op::Dir("x/y", 0o755),
                Op::File("x/y/z.txt", b"z", 0o644),
                Op::File("x/y/w.md", b"w", 0o644),
            ],
        );
        let cs = copy_source("a/./b/c", true);
        assert_eq!(
            cs,
            CopySource {
                src: "/a".to_string(),
                include_patterns: vec!["b/c".to_string()],
                required_paths: vec!["/a/b/c".to_string()],
            }
        );
        let dst = tmp();
        docopy(
            src.path(),
            dst.path(),
            &CopyAction::for_instruction(&cs, "/out/"),
        )
        .unwrap();
        assert_eq!(fs::read(dst.path().join("out/b/c/file")).unwrap(), b"x");
        assert!(!dst.path().join("out/b/other").exists());

        let cs = copy_source("x/y/*.txt", true);
        assert_eq!(cs.src, "/");
        assert_eq!(cs.include_patterns, ["x/y/*.txt"]);
        assert!(cs.required_paths.is_empty());
        docopy(
            src.path(),
            dst.path(),
            &CopyAction::for_instruction(&cs, "/p/"),
        )
        .unwrap();
        assert_eq!(fs::read(dst.path().join("p/x/y/z.txt")).unwrap(), b"z");
        assert!(!dst.path().join("p/x/y/w.md").exists());

        let cs = copy_source("a/./nope", true);
        let err = docopy(
            src.path(),
            dst.path(),
            &CopyAction::for_instruction(&cs, "/q/"),
        )
        .unwrap_err();
        assert_eq!(format!("{err:#}"), "\"/a/nope\": not found");
    }

    #[test]
    fn dest_paths() {
        for (workdir, dest, want) in [
            ("/", "/foo", "/foo"),
            ("/", "/foo/", "/foo/"),
            ("/w", "foo", "/w/foo"),
            ("/w", "foo/", "/w/foo/"),
            ("/w", ".", "/w/"),
            ("/w", "", "/w/"),
            ("/", ".", "/"),
            ("/w", "foo/.", "/w/foo/."),
            ("/", "/.", "/."),
            ("/w", "../x//y/", "/x/y/"),
        ] {
            assert_eq!(dest_path(workdir, dest), want, "{workdir} {dest}");
        }
        assert_eq!(clean_path("/foo/."), b"/foo/.");
        assert_eq!(clean_path("foo/"), b"/foo/");
        assert_eq!(clean_path("/"), b"/");
        assert_eq!(clean_path("."), b"/");
    }

    #[test]
    fn chmod_values() {
        assert_eq!(parse_chmod("0755").unwrap(), Chmod::Octal(0o755));
        assert_eq!(parse_chmod("4755").unwrap(), Chmod::Octal(0o4755));
        assert_eq!(
            parse_chmod("u+x").unwrap(),
            Chmod::Symbolic("u+x".to_string())
        );
        for bad in ["64a", "10000", "0999"] {
            assert_eq!(
                format!("{:#}", parse_chmod(bad).unwrap_err()),
                format!("invalid chmod parameter: '{bad}'. it should be octal string and between 0 and 07777")
            );
        }
        assert_eq!(
            format!("{:#}", parse_chmod("u+z").unwrap_err()),
            "invalid syntax"
        );
    }

    #[test]
    fn mode_parse_octal() {
        for i in 0..=0o7777u32 {
            mode::parse(&format!("0{i:o}")).unwrap();
        }
    }

    #[test]
    fn mode_parse_errors() {
        for input in ["", ",", "q+r", "017777", "z", "u", "g", "o", "go"] {
            assert_eq!(
                mode::parse(input),
                Err(mode::ModeError::Syntax),
                "{input:?}"
            );
        }
        assert_eq!(
            mode::parse("01000000000000000007777")
                .unwrap_err()
                .to_string(),
            "strconv.ParseInt: parsing \"01000000000000000007777\": value out of range"
        );
        assert_eq!(
            mode::parse("9").unwrap_err().to_string(),
            "strconv.ParseInt: parsing \"9\": invalid syntax"
        );
    }

    #[test]
    fn mode_apply_table() {
        const DIR: u32 = S_IFDIR;
        let cases: &[(u32, u32, u32, &str)] = &[
            (0o777, 0o000, 0, "="),
            (0o777, 0o777, 0, "+"),
            (0o421, 0o421, 0, "+"),
            (0o777, 0o777, 0, "-"),
            (0o421, 0o421, 0, "-"),
            (0o421, 0o1421, 0, "+t"),
            (0o1421, 0o421, 0, "-t"),
            (0o000, 0o400, 0, "u+r"),
            (0o000, 0o200, 0, "u+w"),
            (0o000, 0o100, 0, "u+x"),
            (0o000, 0o000, 0, "u+X"),
            (0o001, 0o101, 0, "u+X"),
            (DIR, DIR | 0o100, 0, "u+X"),
            (0o000, 0o4000, 0, "u+s"),
            (0o000, 0o040, 0, "g+r"),
            (0o000, 0o020, 0, "g+w"),
            (0o000, 0o010, 0, "g+x"),
            (0o000, 0o000, 0, "g+X"),
            (0o100, 0o110, 0, "g+X"),
            (DIR, DIR | 0o010, 0, "g+X"),
            (0o000, 0o2000, 0, "g+s"),
            (0o000, 0o004, 0, "o+r"),
            (0o000, 0o002, 0, "o+w"),
            (0o000, 0o001, 0, "o+x"),
            (0o000, 0o000, 0, "o+X"),
            (0o100, 0o101, 0, "o+X"),
            (DIR, DIR | 0o001, 0, "o+X"),
            (0o000, 0o000, 0, "o+s"),
            (0o000, 0o421, 0, "0421"),
            (0o000, 0o7421, 0, "07421"),
            (0o000, 0o444, 0, "+r"),
            (0o000, 0o222, 0, "+w"),
            (0o000, 0o111, 0, "+x"),
            (0o000, 0o000, 0, "+X"),
            (0o010, 0o111, 0, "+X"),
            (DIR, DIR | 0o111, 0, "+X"),
            (0o000, 0o6000, 0, "+s"),
            (0o000, 0o440, 0o034, "+r"),
            (0o000, 0o220, 0o052, "+w"),
            (0o000, 0o110, 0o061, "+x"),
            (0o777, 0o333, 0, "-r"),
            (0o777, 0o555, 0, "-w"),
            (0o777, 0o666, 0, "-x"),
            (0o777, 0o666, 0, "-X"),
            (DIR | 0o777, DIR | 0o666, 0, "-X"),
            (0o6777, 0o777, 0, "-s"),
            (0o777, 0o337, 0o037, "-r"),
            (0o777, 0o557, 0o057, "-w"),
            (0o777, 0o667, 0o067, "-x"),
            (0o777, 0o377, 0, "u-r"),
            (0o777, 0o577, 0, "u-w"),
            (0o777, 0o677, 0, "u-x"),
            (0o777, 0o677, 0, "u-X"),
            (DIR | 0o777, DIR | 0o677, 0, "u-X"),
            (0o4777, 0o777, 0, "u-s"),
            (0o000, 0o600, 0, "u=rw"),
            (0o777, 0o677, 0, "u=rw"),
            (0o777, 0o077, 0, "u="),
            (0o000, 0o666, 0, "=rw"),
            (0o777, 0o666, 0, "=rw"),
            (0o042, 0o442, 0, "u=g"),
            (0o742, 0o442, 0, "u=g"),
            (0o722, 0o022, 0, "u=g-w"),
            (0o742, 0o442, 0, "u=g-w"),
            (0o762, 0o462, 0, "u=g-w"),
            (0o772, 0o572, 0, "u=g-w"),
            (0o021, 0o121, 0, "u+o"),
            (0o021, 0o221, 0, "u+g"),
            (0o021, 0o321, 0, "u+go"),
            (0o011, 0o111, 0o037, "u+uo"),
            (0o111, 0o011, 0o037, "-u"),
            (0o111, 0o011, 0o037, "-Xu"),
            (0o111, 0o011, 0o037, "-uX"),
            (0o000, 0o764, 0, "a=r,ug=rw,u+x"),
            (0o777, 0o764, 0, "a=r,ug=rw,u+x"),
            (0o000, 0o1624, 0, "o+r,g+w,a+t,g-s,u+og"),
            (0o2100, 0o1724, 0, "o+r,g+w,a+t,g-s,u+og"),
            (DIR | 0o6000, DIR | 0o5624, 0, "o+r,g+w,a+t,g-s,u+og"),
            (0o777, 0o644, 0, "644"),
            (0o777, 0o755, 0, "go-w"),
            (0o666, 0o664, 0o002, "=rw,+X"),
            (0o766, 0o775, 0o002, "=rw,+X"),
            (0o766, 0o765, 0o012, "=rw,+X"),
            (0o000, 0o755, 0, "u=rwx,go=rx"),
            (0o000, 0o755, 0, "u=rwx,go=u-w"),
            (0o777, 0o700, 0, "go="),
            (0o770, 0o755, 0, "go=u-w"),
            (0o670, 0o644, 0, "go=u-w"),
            (0o570, 0o555, 0, "go=u-w"),
            (0o370, 0o311, 0, "go=u-w"),
            (0o444, 0o644, 0, "u=rwX,go=rX"),
            (DIR | 0o442, DIR | 0o755, 0, "u=rwX,go=rX"),
            (0o641, 0o755, 0, "u=rwX,go=rX"),
            (0o777, 0o766, 0, "go-X"),
            (0o424, 0o000, 0, "=X"),
            (0o421, 0o111, 0, "=X"),
            (0o421, 0o555, 0, "=rX"),
        ];
        for &(omode, nmode, umask, s) in cases {
            let set = mode::parse_with_umask(s, umask).unwrap();
            assert_eq!(
                set.apply(omode),
                nmode,
                "{s:?} umask {umask:o} on {omode:o}"
            );
        }
    }

    // What chmod(1) does with the symbolic modes of the reference's front end tests,
    // on a file created 0644 and a directory 0500.
    #[test]
    fn mode_matches_chmod_command() {
        for (src, mode_str, want) in [
            (0o644, "go-w", 0o644),
            (0o644, "o-rwx,g+x", 0o650),
            (0o644, "u=rwx,g=,o=", 0o700),
            (0o644, "u+rw,g+r,o-x,o+w", 0o646),
            (S_IFDIR | 0o500, "a+X", S_IFDIR | 0o511),
            (S_IFDIR | 0o500, "g+rw,o+rw", S_IFDIR | 0o566),
            (S_IFDIR | 0o500, "u=rwX,go=rX", S_IFDIR | 0o755),
        ] {
            assert_eq!(
                mode::parse(mode_str).unwrap().apply(src),
                want,
                "{mode_str}"
            );
        }
    }

    #[test]
    fn copy_preserves_hardlinks_and_xattrs() {
        let t1 = tmp();
        apply(
            t1.path(),
            &[
                Op::Dir("d", 0o755),
                Op::File("d/a", b"x", 0o640),
                Op::Link("d/a", "d/b"),
            ],
        );
        let has_xattrs = xattr::set(t1.path().join("d/a"), "user.k", b"v").is_ok();
        let t2 = tmp();
        copy(t1.path(), "d", t2.path(), "e", CopyInfo::default()).unwrap();
        let a = fs::metadata(t2.path().join("e/a")).unwrap();
        let b = fs::metadata(t2.path().join("e/b")).unwrap();
        assert_eq!(a.ino(), b.ino());
        if has_xattrs {
            assert_eq!(
                xattr::get(t2.path().join("e/a"), "user.k")
                    .unwrap()
                    .as_deref(),
                Some(&b"v"[..])
            );
        }
    }

    #[test]
    fn copy_keeps_source_times() {
        let t1 = tmp();
        apply(
            t1.path(),
            &[Op::File("f", b"x", 0o644), Op::Dir("d", 0o755)],
        );
        let old = TimeSpec::new(1_000_000_000, 5);
        for p in ["f", "d"] {
            utimes(&t1.path().join(p), Some(old)).unwrap();
        }
        let t2 = tmp();
        copy(t1.path(), "/", t2.path(), "/", CopyInfo::default()).unwrap();
        assert_eq!(mtime(&t2.path().join("f")), (1_000_000_000, 5));
        assert_eq!(mtime(&t2.path().join("d")), (1_000_000_000, 5));
    }

    #[test]
    fn resolve_wildcards_matches() {
        let t1 = tmp();
        apply(
            t1.path(),
            &[
                Op::Dir("a/b", 0o755),
                Op::File("a/b/x.txt", b"", 0o644),
                Op::File("a/y.txt", b"", 0o644),
                Op::Dir("a/c.txt", 0o755),
                Op::File("a/c.txt/z.txt", b"", 0o644),
            ],
        );
        let got = resolve_wildcards(t1.path(), "/a/*.txt", true).unwrap();
        assert_eq!(got, [PathBuf::from("a/c.txt"), PathBuf::from("a/y.txt")]);
        let got = resolve_wildcards(t1.path(), "a/*/*.txt", false).unwrap();
        assert_eq!(
            got,
            [PathBuf::from("a/b/x.txt"), PathBuf::from("a/c.txt/z.txt")]
        );
        assert_eq!(
            resolve_wildcards(t1.path(), "a//b/../b", true).unwrap(),
            [PathBuf::from("a/b")]
        );
        let err = resolve_wildcards(t1.path(), "nope/*", true).unwrap_err();
        assert!(
            format!("{err:#}").ends_with("nope: no such file or directory"),
            "{err:#}"
        );
    }
}
