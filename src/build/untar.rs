//! Unpacking a tar into a directory as a build's ADD of a local archive unpacks it: a
//! port of `Unpack` and `createTarFile` of moby/go-archive, run as buildkit's file
//! backend runs them, through chrootarchive with the destination as the root. The names
//! in the archive and every symlink on their way are resolved inside the destination.
//! upstream/SOURCE names the version.

use std::collections::BTreeMap;
use std::fs::{self, DirBuilder, Metadata, OpenOptions, Permissions};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{lchown, symlink, DirBuilderExt, OpenOptionsExt, PermissionsExt};

use anyhow::{anyhow, bail, Context as _, Result};
use nix::fcntl::AT_FDCWD;
use nix::sys::stat::{makedev, mknod, utimensat, Mode, SFlag, UtimensatFlags};
use nix::sys::time::TimeSpec;

use super::copy::{root_path_bytes, User};
use super::ignore::{lexical, lossy};
use super::walk::{bytes_path, path_bytes, quote, Errno, PathError};

/// The mode of a parent directory the archive does not list.
const IMPLIED_DIRECTORY_MODE: u32 = 0o755;
const PAX_SCHILY_XATTR: &[u8] = b"SCHILY.xattr.";
/// archive/tar's ErrHeader.
const INVALID_HEADER: &str = "archive/tar: invalid tar header";

/// A numeric field of a header as archive/tar's parseNumeric reads it: base-256 when
/// its first bit is set, else octal, NULs and spaces around it ignored and nothing at
/// all being 0.
fn numeric(field: &[u8]) -> Result<i64> {
    if field.first().is_some_and(|&b| b & 0x80 != 0) {
        let inv = if field[0] & 0x40 != 0 { 0xff } else { 0 };
        let mut x: u64 = 0;
        for (i, &c) in field.iter().enumerate() {
            let mut c = c ^ inv;
            if i == 0 {
                c &= 0x7f;
            }
            if x >> 56 > 0 {
                bail!(INVALID_HEADER);
            }
            x = x << 8 | u64::from(c);
        }
        if x >> 63 > 0 {
            bail!(INVALID_HEADER);
        }
        let x = x as i64;
        return Ok(if inv == 0xff { !x } else { x });
    }
    let start = field.iter().position(|&b| b != b' ' && b != 0);
    let end = field.iter().rposition(|&b| b != b' ' && b != 0);
    let (Some(start), Some(end)) = (start, end) else {
        return Ok(0);
    };
    let text = &field[start..=end];
    let text = text.split(|&b| b == 0).next().unwrap_or_default();
    std::str::from_utf8(text)
        .ok()
        .and_then(|t| u64::from_str_radix(t, 8).ok())
        .map(|x| x as i64)
        .ok_or_else(|| anyhow!(INVALID_HEADER))
}

/// The fields of a header an entry is made from.
struct Fields {
    mode: u32,
    uid: i64,
    gid: i64,
    mtime: i64,
    devmajor: i64,
    devminor: i64,
}

impl Fields {
    fn of(header: &tar::Header) -> Result<Fields> {
        let b = header.as_bytes();
        // V7 has no device numbers.
        let ustar = header.as_ustar().is_some() || header.as_gnu().is_some();
        let dev = |range: std::ops::Range<usize>| -> Result<i64> {
            if ustar {
                numeric(&b[range])
            } else {
                Ok(0)
            }
        };
        Ok(Fields {
            mode: numeric(&b[100..108])? as u32,
            uid: numeric(&b[108..116])?,
            gid: numeric(&b[116..124])?,
            mtime: numeric(&b[136..148])?,
            devmajor: dev(329..337)?,
            devminor: dev(337..345)?,
        })
    }
}

/// The compression of an archive, by its magic as go-archive's Detect tells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    None,
    Gzip,
    Bzip2,
    Xz,
    Zstd,
}

pub fn detect(head: &[u8]) -> Compression {
    if head.starts_with(b"BZh") {
        Compression::Bzip2
    } else if head.starts_with(&[0x1f, 0x8b, 0x08]) {
        Compression::Gzip
    } else if head.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0x00]) {
        Compression::Xz
    } else if head.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) || skippable_zstd_frame(head) {
        Compression::Zstd
    } else {
        Compression::None
    }
}

/// A zstd skippable frame, magic 0x184D2A50 to 0x184D2A5F, which a stream may start with.
fn skippable_zstd_frame(head: &[u8]) -> bool {
    head.len() >= 8
        && u32::from_le_bytes([head[0], head[1], head[2], head[3]]) & 0xFFFF_FFF0 == 0x184D_2A50
}

/// Both times of an entry, in nanoseconds since the epoch, bounded as boundTime bounds
/// them.
#[derive(Debug, Clone, Copy)]
struct Times {
    atime: i64,
    mtime: i64,
}

impl Times {
    /// Sets them on `path` itself, a symlink included: what the reference follows to is
    /// resolved inside the root before.
    fn utimens(&self, path: &[u8]) -> Result<()> {
        let spec =
            |ns: i64| TimeSpec::new(ns.div_euclid(1_000_000_000), ns.rem_euclid(1_000_000_000));
        let path = bytes_path(path);
        utimensat(
            AT_FDCWD,
            &path,
            &spec(self.atime),
            &spec(self.mtime),
            UtimensatFlags::NoFollowSymlink,
        )
        .map_err(|e| PathError::new("chtimes", &path, e.into()).into())
    }
}

/// Unpacks `entries` into `dest`, a directory, as the reference's Untar into a chroot
/// of it: an entry replaces what is at its path unless both are directories, parents the
/// archive does not list are made 0755, owners are the headers' unless `chown` names
/// one, the xattrs of the PAX records are set where the filesystem takes them, and the
/// directories get their times once everything in them is written.
pub fn unpack<'a, R: Read + 'a>(
    entries: impl Iterator<Item = io::Result<tar::Entry<'a, R>>>,
    dest: &std::path::Path,
    chown: Option<User>,
) -> Result<()> {
    let root = lexical::clean(path_bytes(dest));
    // By name: an entry after one may have put a symlink at its path, which is followed
    // inside the root.
    let mut dirs: Vec<(Vec<u8>, Times)> = Vec::new();
    let mut ignored: Vec<String> = Vec::new();
    for entry in entries {
        let mut entry = entry?;
        let typeflag = entry.header().as_bytes()[156];
        if typeflag == b'g' {
            continue;
        }
        let raw = entry.path_bytes().into_owned();
        let start = raw.iter().position(|&b| b != b'/').unwrap_or(raw.len());
        let name = lexical::clean(&raw[start..]);
        if name == b"." {
            continue;
        }
        if name == b".." || name.starts_with(b"../") {
            bail!("invalid entry name {}", quote(&lossy(&raw)));
        }
        // The legacy regular type with a trailing slash is a directory, as archive/tar
        // reads it.
        let typeflag = match typeflag {
            0 if raw.ends_with(b"/") => b'5',
            0 => b'0',
            t => t,
        };
        let dst = resolve(&root, &name)?;
        if let Ok(meta) = fs::symlink_metadata(bytes_path(&dst)) {
            if !(meta.is_dir() && typeflag == b'5') {
                remove_all(&dst, &meta)?;
            }
        }
        implied_directories(&root, &dst)?;
        let times = create(
            &mut entry,
            typeflag,
            &root,
            &name,
            &dst,
            chown,
            &mut ignored,
        )?;
        if typeflag == b'5' {
            dirs.push((name, times));
        }
    }
    if !ignored.is_empty() {
        eprintln!(
            "ignored xattrs in archive: underlying filesystem doesn't support them: {}",
            ignored.join("; ")
        );
    }
    for (name, times) in dirs {
        times.utimens(&root_path_bytes(&root, &lexical::join(&[b"/", &name]))?)?;
    }
    Ok(())
}

/// The path of `name` inside `root`, every symlink of its parent followed inside the
/// root and its last component not, as resolveArchivePath resolves it.
fn resolve(root: &[u8], name: &[u8]) -> Result<Vec<u8>> {
    let (parent, base) = lexical::split(name);
    if parent.is_empty() {
        return Ok(lexical::join(&[root, name]));
    }
    let parent = root_path_bytes(root, &lexical::join(&[b"/", parent]))?;
    Ok(lexical::join(&[&parent, base]))
}

/// The parents of `dst` that are missing, made 0755 whatever the umask, as
/// createImpliedDirectories makes them.
fn implied_directories(root: &[u8], dst: &[u8]) -> Result<()> {
    let parent = lexical::dir(dst);
    if parent == root {
        return Ok(());
    }
    match fs::symlink_metadata(bytes_path(&parent)) {
        Ok(_) => return Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(PathError::new("lstat", bytes_path(&parent), e).into()),
    }
    let rel = parent.strip_prefix(root).unwrap_or(&parent);
    let mut cur = root.to_vec();
    for c in rel.split(|&b| b == b'/').filter(|c| !c.is_empty()) {
        cur = lexical::join(&[&cur, c]);
        let path = bytes_path(&cur);
        match DirBuilder::new().mode(IMPLIED_DIRECTORY_MODE).create(&path) {
            Ok(()) => fs::set_permissions(&path, Permissions::from_mode(IMPLIED_DIRECTORY_MODE))
                .map_err(|e| PathError::new("chmod", &path, e))?,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                if !fs::metadata(&path).is_ok_and(|m| m.is_dir()) {
                    let enotdir = io::Error::from_raw_os_error(nix::libc::ENOTDIR);
                    return Err(PathError::new("mkdir", &path, enotdir).into());
                }
            }
            Err(e) => return Err(PathError::new("mkdir", &path, e).into()),
        }
    }
    Ok(())
}

fn remove_all(path: &[u8], meta: &Metadata) -> Result<()> {
    let path = bytes_path(path);
    let removed = if meta.is_dir() {
        fs::remove_dir_all(&path)
    } else {
        fs::remove_file(&path)
    };
    removed.map_err(|e| PathError::new("unlinkat", &path, e).into())
}

/// The target of a hard link inside `root`, an absolute one taken from the root, as
/// resolveHardlinkTarget takes it.
fn hardlink_target(root: &[u8], linkname: &[u8]) -> Result<Vec<u8>> {
    let mut cleaned = lexical::clean(linkname);
    if cleaned.starts_with(b"/") {
        let start = linkname
            .iter()
            .position(|&b| b != b'/')
            .unwrap_or(linkname.len());
        cleaned = lexical::clean(&linkname[start..]);
    }
    if cleaned == b"." || cleaned == b".." || cleaned.starts_with(b"../") {
        bail!("invalid hardlink target {}", quote(&lossy(linkname)));
    }
    resolve(root, &cleaned)
}

/// The PAX records of an entry, the last of a key winning.
fn records<R: Read>(entry: &mut tar::Entry<'_, R>) -> Result<BTreeMap<Vec<u8>, Vec<u8>>> {
    let mut out = BTreeMap::new();
    if let Some(extensions) = entry.pax_extensions()? {
        for extension in extensions {
            let extension = extension?;
            out.insert(
                extension.key_bytes().to_vec(),
                extension.value_bytes().to_vec(),
            );
        }
    }
    Ok(out)
}

/// A PAX time, "SECONDS[.FRACTION]", in nanoseconds, as parsePAXTime reads it.
fn pax_time(value: &[u8]) -> Result<i128> {
    let text = std::str::from_utf8(value).map_err(|_| anyhow!(INVALID_HEADER))?;
    let (secs, frac) = text.split_once('.').unwrap_or((text, ""));
    let s: i64 = secs.parse().map_err(|_| anyhow!(INVALID_HEADER))?;
    let s = i128::from(s) * 1_000_000_000;
    if frac.is_empty() {
        return Ok(s);
    }
    if !frac.bytes().all(|b| b.is_ascii_digit()) {
        bail!(INVALID_HEADER);
    }
    let digits: String = frac.chars().chain(std::iter::repeat('0')).take(9).collect();
    let n: i128 = digits.parse().map_err(|_| anyhow!(INVALID_HEADER))?;
    Ok(if secs.starts_with('-') { s - n } else { s + n })
}

/// The times an entry gives its path: the modification time, and the later of it and
/// the access time; outside what a timespec of nanoseconds holds, the epoch.
fn times(
    header: &tar::Header,
    fields: &Fields,
    records: &BTreeMap<Vec<u8>, Vec<u8>>,
) -> Result<Times> {
    let record = |key: &[u8]| records.get(key).filter(|v| !v.is_empty());
    let mtime = match record(b"mtime") {
        Some(v) => pax_time(v)?,
        None => i128::from(fields.mtime) * 1_000_000_000,
    };
    let atime = match record(b"atime") {
        Some(v) => Some(pax_time(v)?),
        None => header
            .as_gnu()
            .filter(|gnu| gnu.atime[0] != 0)
            .and_then(|gnu| numeric(&gnu.atime).ok())
            .map(|t| i128::from(t) * 1_000_000_000),
    };
    let bound = |t: i128| i64::try_from(t).ok().filter(|&t| t >= 0).unwrap_or(0);
    Ok(Times {
        atime: bound(atime.map_or(mtime, |a| a.max(mtime))),
        mtime: bound(mtime),
    })
}

/// One entry at `dst`, which nothing is at unless both are directories: made, owned,
/// its xattrs set, its mode and then its times given, as createTarFile does it.
fn create<R: Read>(
    entry: &mut tar::Entry<'_, R>,
    typeflag: u8,
    root: &[u8],
    name: &[u8],
    dst: &[u8],
    chown: Option<User>,
    ignored: &mut Vec<String>,
) -> Result<Times> {
    let records = records(entry)?;
    let header = entry.header().clone();
    let fields = Fields::of(&header)?;
    let mode = fields.mode;
    let path = bytes_path(dst);
    let hardlink = match typeflag {
        b'1' => Some(hardlink_target(
            root,
            &entry.link_name_bytes().unwrap_or_default(),
        )?),
        _ => None,
    };
    match typeflag {
        b'5' => {
            if !fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir()) {
                DirBuilder::new()
                    .mode(mode & 0o777)
                    .create(&path)
                    .map_err(|e| PathError::new("mkdir", &path, e))?;
            }
        }
        b'0' => {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(mode & 0o777)
                .custom_flags(nix::libc::O_NOFOLLOW)
                .open(&path)
                .map_err(|e| PathError::new("open", &path, e))?;
            io::copy(entry, &mut file).with_context(|| format!("writing {}", path.display()))?;
        }
        b'3' | b'4' | b'6' => {
            let kind = match typeflag {
                b'3' => SFlag::S_IFCHR,
                b'4' => SFlag::S_IFBLK,
                _ => SFlag::S_IFIFO,
            };
            let (major, minor) = match (
                u32::try_from(fields.devmajor),
                u32::try_from(fields.devminor),
            ) {
                (Ok(major), Ok(minor)) => (major, minor),
                _ => bail!(
                    "device number {}:{} for {} out of range: invalid archive",
                    fields.devmajor,
                    fields.devminor,
                    quote(&lossy(name))
                ),
            };
            mknod(
                &path,
                kind,
                Mode::from_bits_truncate(mode & 0o7777),
                makedev(major.into(), minor.into()),
            )
            .map_err(|e| PathError::new("mknodat", &path, e.into()))?;
        }
        b'1' => {
            let target = bytes_path(hardlink.as_deref().unwrap_or_default());
            fs::hard_link(&target, &path).map_err(|e| PathError::new("linkat", &path, e))?;
        }
        b'2' => {
            let target = bytes_path(&entry.link_name_bytes().unwrap_or_default());
            symlink(&target, &path).map_err(|e| PathError::new("symlinkat", &path, e))?;
        }
        t => bail!("unhandled tar header type {t}"),
    }

    let (uid, gid) = match chown {
        Some(user) => (i64::from(user.uid), i64::from(user.gid)),
        None => (fields.uid, fields.gid),
    };
    let owned = match (u32::try_from(uid), u32::try_from(gid)) {
        (Ok(u), Ok(g)) => lchown(&path, Some(u), Some(g)),
        _ => Err(io::Error::from_raw_os_error(nix::libc::EINVAL)),
    };
    if let Err(e) = owned {
        bail!(
            "failed to Lchown {} for UID {}, GID {}: lchown {}: {}",
            quote(&lossy(name)),
            fields.uid,
            fields.gid,
            lossy(name),
            Errno(e)
        );
    }

    let shown = format!("/{}", lossy(name));
    for (key, value) in &records {
        let Some(xattr) = key.strip_prefix(PAX_SCHILY_XATTR) else {
            continue;
        };
        if let Err(e) = xattr::set(&path, std::ffi::OsStr::from_bytes(xattr), value) {
            let forgiven = matches!(
                e.raw_os_error(),
                Some(nix::libc::ENOTSUP) | Some(nix::libc::EPERM)
            );
            let text = format!(
                "lsetxattr {shown}: xattr {}: {}",
                quote(&lossy(xattr)),
                Errno(e)
            );
            if forgiven {
                ignored.push(text);
                continue;
            }
            bail!(text);
        }
    }

    // No mode or times through a symlink, a hard link to one included; the mode after
    // the owner, whose change clears the set-id bits.
    let through = match &hardlink {
        Some(target) => {
            fs::symlink_metadata(bytes_path(target)).is_ok_and(|m| !m.file_type().is_symlink())
        }
        None => typeflag != b'2',
    };
    if through {
        fs::set_permissions(&path, Permissions::from_mode(mode & 0o7777))
            .map_err(|e| PathError::new("fchmodat2", &path, e))?;
    }
    let times = times(&header, &fields, &records)?;
    if typeflag == b'2' || through {
        times.utimens(dst)?;
    }
    Ok(times)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    fn me() -> (u64, u64) {
        (
            u64::from(nix::unistd::getuid().as_raw()),
            u64::from(nix::unistd::getgid().as_raw()),
        )
    }

    /// An entry of `tar_of`: name, typeflag, mode, mtime, data or link target, PAX
    /// records.
    type Entry<'a> = (&'a str, u8, u32, u64, &'a [u8], &'a [(&'a str, &'a [u8])]);

    /// A tar of `entries`, owned by whoever runs the tests.
    fn tar_of(entries: &[Entry<'_>]) -> Vec<u8> {
        let (uid, gid) = me();
        let mut b = tar::Builder::new(Vec::new());
        for (name, typeflag, mode, mtime, data, records) in entries {
            if !records.is_empty() {
                let body: Vec<u8> = records
                    .iter()
                    .flat_map(|(k, v)| crate::build::layer::format_pax_record(k.as_bytes(), v))
                    .collect();
                let mut h = tar::Header::new_ustar();
                h.set_entry_type(tar::EntryType::XHeader);
                h.set_size(body.len() as u64);
                h.set_mode(0o644);
                b.append_data(&mut h, "PaxHeader", &body[..]).unwrap();
            }
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::new(*typeflag));
            h.set_mode(*mode);
            h.set_uid(uid);
            h.set_gid(gid);
            h.set_mtime(*mtime);
            match typeflag {
                b'1' | b'2' => {
                    h.set_size(0);
                    b.append_link(&mut h, name, std::str::from_utf8(data).unwrap())
                        .unwrap();
                }
                _ => {
                    h.set_size(data.len() as u64);
                    b.append_data(&mut h, name, *data).unwrap();
                }
            }
        }
        b.into_inner().unwrap()
    }

    fn unpack_into(dest: &std::path::Path, tar: &[u8]) -> Result<()> {
        let mut archive = tar::Archive::new(tar);
        unpack(archive.entries()?, dest, None)
    }

    #[test]
    fn an_entry_replaces_what_is_there_unless_both_are_directories() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        fs::create_dir_all(d.join("usr/lib")).unwrap();
        symlink("usr/lib", d.join("lib")).unwrap();
        fs::write(d.join("was-a-file"), b"x").unwrap();
        fs::create_dir_all(d.join("was-a-dir/inner")).unwrap();
        fs::create_dir(d.join("stays")).unwrap();
        fs::write(d.join("stays/old"), b"old").unwrap();
        let tar = tar_of(&[
            ("lib/libfoo.so", b'0', 0o755, 7, b"elf", &[]),
            ("was-a-file/", b'5', 0o750, 7, b"", &[]),
            ("was-a-dir", b'0', 0o600, 7, b"now a file", &[]),
            ("stays/", b'5', 0o700, 7, b"", &[]),
            ("stays/new", b'0', 0o644, 7, b"new", &[]),
        ]);
        unpack_into(d, &tar).unwrap();
        // Through the symlink of its parent, which stays.
        assert_eq!(fs::read(d.join("usr/lib/libfoo.so")).unwrap(), b"elf");
        assert!(fs::symlink_metadata(d.join("lib"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(d.join("was-a-file").is_dir());
        assert_eq!(fs::read(d.join("was-a-dir")).unwrap(), b"now a file");
        // Merged: what was in it stays, the mode is the archive's.
        assert_eq!(fs::read(d.join("stays/old")).unwrap(), b"old");
        assert_eq!(fs::read(d.join("stays/new")).unwrap(), b"new");
        let mode = fs::metadata(d.join("stays")).unwrap().mode() & 0o7777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn a_directory_entry_takes_the_place_of_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        fs::create_dir_all(d.join("usr/lib")).unwrap();
        symlink("usr/lib", d.join("lib")).unwrap();
        let tar = tar_of(&[
            ("lib/", b'5', 0o755, 7, b"", &[]),
            ("lib/libfoo.so", b'0', 0o755, 7, b"elf", &[]),
        ]);
        unpack_into(d, &tar).unwrap();
        assert!(fs::symlink_metadata(d.join("lib")).unwrap().is_dir());
        assert!(d.join("lib/libfoo.so").is_file());
        assert!(!d.join("usr/lib/libfoo.so").exists());
    }

    #[test]
    fn an_absolute_symlink_on_the_way_stays_inside_the_destination() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join("dest");
        fs::create_dir_all(d.join("real")).unwrap();
        symlink("/real", d.join("abs")).unwrap();
        symlink("../../../..", d.join("up")).unwrap();
        let tar = tar_of(&[
            ("abs/a", b'0', 0o644, 7, b"a", &[]),
            ("up/b", b'0', 0o644, 7, b"b", &[]),
        ]);
        unpack_into(&d, &tar).unwrap();
        assert_eq!(fs::read(d.join("real/a")).unwrap(), b"a");
        assert_eq!(fs::read(d.join("b")).unwrap(), b"b");
        assert!(!dir.path().join("b").exists());
    }

    #[test]
    fn the_times_of_a_directory_a_symlink_replaced_stay_inside() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join("dest");
        let outside = dir.path().join("outside");
        fs::create_dir_all(d.join("inside")).unwrap();
        fs::create_dir(&outside).unwrap();
        let before = fs::metadata(&outside).unwrap().mtime();
        let target = outside.to_str().unwrap().as_bytes().to_vec();
        // An absolute target is inside the root, where this one is not there: the times
        // fail, as the reference's do.
        let tar = tar_of(&[
            ("d/", b'5', 0o755, 1000, b"", &[]),
            ("d", b'2', 0o777, 7, &target, &[]),
        ]);
        assert!(unpack_into(&d, &tar).is_err());
        assert_eq!(fs::metadata(&outside).unwrap().mtime(), before);
        let tar = tar_of(&[
            ("e/", b'5', 0o755, 2000, b"", &[]),
            ("e", b'2', 0o777, 7, b"/inside", &[]),
        ]);
        unpack_into(&d, &tar).unwrap();
        assert_eq!(fs::metadata(d.join("inside")).unwrap().mtime(), 2000);
    }

    #[test]
    fn a_parent_the_archive_does_not_list_is_made_and_one_there_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        fs::create_dir(d.join("tmp")).unwrap();
        fs::set_permissions(d.join("tmp"), Permissions::from_mode(0o1777)).unwrap();
        let tar = tar_of(&[
            ("tmp/foo", b'0', 0o644, 7, b"", &[]),
            ("new/deep/file", b'0', 0o644, 7, b"", &[]),
        ]);
        unpack_into(d, &tar).unwrap();
        assert_eq!(fs::metadata(d.join("tmp")).unwrap().mode() & 0o7777, 0o1777);
        for p in ["new", "new/deep"] {
            assert_eq!(
                fs::metadata(d.join(p)).unwrap().mode() & 0o7777,
                0o755,
                "{p}"
            );
        }
    }

    #[test]
    fn modes_times_and_special_files_are_the_archive_s() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let tar = tar_of(&[
            ("t/", b'5', 0o755, 1000, b"", &[]),
            ("t/inner/", b'5', 0o711, 2000, b"", &[]),
            ("t/inner/zero", b'0', 0o4755, 0, b"x", &[]),
            ("t/nanos", b'0', 0o644, 5, b"", &[("mtime", b"1234.5678")]),
            ("t/fifo", b'6', 0o640, 3000, b"", &[]),
            ("t/link", b'2', 0o777, 4000, b"inner/zero", &[]),
            ("t/hard", b'1', 0o4755, 0, b"/t/inner/zero", &[]),
        ]);
        unpack_into(d, &tar).unwrap();
        let meta = |p: &str| fs::symlink_metadata(d.join(p)).unwrap();
        // The directories after what went into them.
        assert_eq!(meta("t").mtime(), 1000);
        assert_eq!(meta("t/inner").mtime(), 2000);
        assert_eq!(meta("t/inner").mode() & 0o7777, 0o711);
        // A time of 0 stays 0, the set-id bit is kept.
        assert_eq!(meta("t/inner/zero").mtime(), 0);
        assert_eq!(meta("t/inner/zero").mode() & 0o7777, 0o4755);
        assert_eq!(
            (meta("t/nanos").mtime(), meta("t/nanos").mtime_nsec()),
            (1234, 567_800_000)
        );
        assert!(meta("t/fifo").file_type().is_fifo());
        assert_eq!(meta("t/fifo").mode() & 0o7777, 0o640);
        assert_eq!(meta("t/link").mtime(), 4000);
        assert_eq!(meta("t/hard").ino(), meta("t/inner/zero").ino());
    }

    #[test]
    fn xattrs_are_set_where_the_filesystem_takes_them() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let probe = d.join("probe");
        fs::write(&probe, b"").unwrap();
        let user_xattrs = xattr::set(&probe, "user.probe", b"1").is_ok();
        // security.capability needs CAP_SETFCAP: without it, EPERM, which is let go.
        let tar = tar_of(&[(
            "f",
            b'0',
            0o755,
            7,
            b"",
            &[
                ("SCHILY.xattr.user.nspawn", b"yes"),
                ("SCHILY.xattr.security.capability", b"\x01\x00\x00\x02\x00\x20\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00"),
            ],
        )]);
        unpack_into(d, &tar).unwrap();
        if user_xattrs {
            assert_eq!(
                xattr::get(d.join("f"), "user.nspawn").unwrap().as_deref(),
                Some(&b"yes"[..])
            );
        }
    }

    #[test]
    fn a_name_out_of_the_destination_or_an_unknown_type_fails() {
        let dir = tempfile::tempdir().unwrap();
        // Written by hand: tar-rs refuses to write such a name.
        let mut h = tar::Header::new_gnu();
        h.as_mut_bytes()[..9].copy_from_slice(b"../escape");
        h.set_size(0);
        h.set_mode(0o644);
        h.set_cksum();
        let mut b = tar::Builder::new(Vec::new());
        b.append(&h, &b""[..]).unwrap();
        let tar = b.into_inner().unwrap();
        let err = unpack_into(dir.path(), &tar).unwrap_err();
        assert_eq!(err.to_string(), "invalid entry name \"../escape\"");
        assert!(!dir.path().join("../escape").exists());

        let tar = tar_of(&[("contiguous", b'7', 0o644, 7, b"", &[])]);
        let err = unpack_into(dir.path(), &tar).unwrap_err();
        assert_eq!(err.to_string(), "unhandled tar header type 55");

        let tar = tar_of(&[("hard", b'1', 0o644, 7, b"../../x", &[])]);
        let err = unpack_into(dir.path(), &tar).unwrap_err();
        assert_eq!(err.to_string(), "invalid hardlink target \"../../x\"");
    }

    #[test]
    fn a_device_is_made_with_its_numbers() {
        if !nix::unistd::getuid().is_root() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Char);
        h.set_mode(0o666);
        h.set_device_major(1).unwrap();
        h.set_device_minor(3).unwrap();
        h.set_size(0);
        b.append_data(&mut h, "null", &b""[..]).unwrap();
        unpack_into(dir.path(), &b.into_inner().unwrap()).unwrap();
        let meta = fs::symlink_metadata(dir.path().join("null")).unwrap();
        assert!(meta.file_type().is_char_device());
        assert_eq!(meta.rdev(), makedev(1, 3));
    }

    #[test]
    fn numeric_fields_are_read_as_archive_tar_reads_them() {
        assert_eq!(numeric(b"0000644\0").unwrap(), 0o644);
        assert_eq!(numeric(b"  644 \0").unwrap(), 0o644);
        assert_eq!(numeric(b"\0\0\0\0\0\0\0\0").unwrap(), 0);
        assert_eq!(numeric(b"        ").unwrap(), 0);
        assert_eq!(numeric(&[0x80, 0, 0, 0, 0, 0, 0x01, 0x00]).unwrap(), 256);
        assert_eq!(
            numeric(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]).unwrap(),
            -1
        );
        assert!(numeric(b"0009\0").is_err());
    }

    #[test]
    fn compressions_are_told_by_their_magic() {
        assert_eq!(detect(&[0x1f, 0x8b, 0x08, 0]), Compression::Gzip);
        assert_eq!(detect(b"BZh91AY"), Compression::Bzip2);
        assert_eq!(
            detect(&[0xfd, b'7', b'z', b'X', b'Z', 0, 0]),
            Compression::Xz
        );
        assert_eq!(detect(&[0x28, 0xb5, 0x2f, 0xfd]), Compression::Zstd);
        // A skippable frame, then the frames.
        assert_eq!(
            detect(&[0x5e, 0x2a, 0x4d, 0x18, 4, 0, 0, 0]),
            Compression::Zstd
        );
        assert_eq!(detect(&[0x5e, 0x2a, 0x4d, 0x18]), Compression::None);
        assert_eq!(detect(b"plain text"), Compression::None);
    }
}
