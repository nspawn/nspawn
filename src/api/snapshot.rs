//! A machine's files as an image has them, for commit and export. They are read on the
//! host through the machine's own tree: the overlay mount its unit keeps, or its flat
//! directory. Its init's root (/proc/PID/root) would bring its volumes, tmpfs and the
//! files nspawn binds in along. Every lookup is resolved beneath that tree, so that a
//! link a running machine makes leads nowhere outside it, and owners are shifted back
//! from the machine's user namespace to the image's.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context as _, Result};
use nix::errno::Errno;
use nix::fcntl::{readlinkat, AtFlags, OFlag};
use nix::sys::stat::{fstat, fstatat, FileStat, Mode};

use crate::api::copy::{scoped_raw, Ids};
use crate::api::{Context, FailureKind};
use crate::backend::{is_mountpoint, BackendChoice};
use crate::build::layer::{Change, ChangeKind, Stat, Tree};
use crate::store::ImageRecord;

/// A directory tree read through a descriptor of its root, every path resolved beneath
/// it (RESOLVE_IN_ROOT) and no link followed at the end of one.
pub struct RootTree {
    root: OwnedFd,
}

impl RootTree {
    pub fn new(root: OwnedFd) -> Self {
        RootTree { root }
    }

    fn open_at(&self, path: &Path, flags: OFlag) -> io::Result<OwnedFd> {
        scoped_raw(self.root.as_fd(), path, flags).map_err(io::Error::from)
    }

    /// The directory `path` is in, and its last component.
    fn parent<'p>(&self, path: &'p Path) -> io::Result<(OwnedFd, &'p OsStr)> {
        let leaf = path
            .file_name()
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        let parent = path.parent().unwrap_or_else(|| Path::new(""));
        Ok((
            self.open_at(parent, OFlag::O_PATH | OFlag::O_DIRECTORY)?,
            leaf,
        ))
    }
}

fn stat_of(st: &FileStat) -> Stat {
    Stat {
        dev: st.st_dev,
        ino: st.st_ino,
        mode: st.st_mode,
        nlink: st.st_nlink,
        uid: st.st_uid,
        gid: st.st_gid,
        rdev: st.st_rdev,
        size: u64::try_from(st.st_size).unwrap_or(0),
        mtime: st.st_mtime,
        mtime_nsec: st.st_mtime_nsec,
    }
}

impl Tree for RootTree {
    fn lstat(&self, path: &Path) -> io::Result<Stat> {
        if path.as_os_str().is_empty() {
            return fstat(&self.root)
                .map(|st| stat_of(&st))
                .map_err(io::Error::from);
        }
        let (dir, leaf) = self.parent(path)?;
        fstatat(&dir, leaf, AtFlags::AT_SYMLINK_NOFOLLOW)
            .map(|st| stat_of(&st))
            .map_err(io::Error::from)
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<OsString>> {
        let fd = self.open_at(
            path,
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW,
        )?;
        let mut names = Vec::new();
        for entry in nix::dir::Dir::from_fd(fd).map_err(io::Error::from)?.iter() {
            let entry = entry.map_err(io::Error::from)?;
            let name = entry.file_name().to_bytes();
            if name != b"." && name != b".." {
                names.push(OsStr::from_bytes(name).to_os_string());
            }
        }
        Ok(names)
    }

    fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        let (dir, leaf) = self.parent(path)?;
        readlinkat(&dir, leaf)
            .map(PathBuf::from)
            .map_err(|e| match e {
                // The link a running machine had there went, another kind of entry
                // in its place.
                Errno::EINVAL => io::Error::new(io::ErrorKind::NotFound, "no longer a link"),
                e => io::Error::from(e),
            })
    }

    fn xattr(&self, path: &Path, name: &str) -> io::Result<Option<Vec<u8>>> {
        // Read through the descriptor's /proc link, which leads to the entry it holds:
        // nothing is opened for it, and only files and directories are asked.
        let fd = self.open_at(path, OFlag::O_PATH | OFlag::O_NOFOLLOW)?;
        let st = fstat(&fd).map_err(io::Error::from)?;
        let kind = st.st_mode & nix::libc::S_IFMT;
        if kind != nix::libc::S_IFREG && kind != nix::libc::S_IFDIR {
            return Ok(None);
        }
        match xattr::get_deref(fd_path(&fd), name) {
            Err(e) if e.raw_os_error() == Some(nix::libc::EOPNOTSUPP) => Ok(None),
            other => other,
        }
    }

    fn open(&self, path: &Path) -> io::Result<Box<dyn Read + '_>> {
        // A running machine may have put another kind of entry at the path since it was
        // looked at: only a regular file is opened, a device or a fifo never is.
        let fd = self.open_at(path, OFlag::O_PATH | OFlag::O_NOFOLLOW)?;
        let st = fstat(&fd).map_err(io::Error::from)?;
        if st.st_mode & nix::libc::S_IFMT != nix::libc::S_IFREG {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no longer a regular file",
            ));
        }
        let file = nix::fcntl::open(
            fd_path(&fd).as_path(),
            OFlag::O_RDONLY | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(io::Error::from)?;
        Ok(Box::new(File::from(file)))
    }
}

/// The /proc link of a descriptor of this process, which leads to what it holds.
fn fd_path(fd: &OwnedFd) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", fd.as_raw_fd()))
}

const CAPABILITY: &str = "security.capability";
const VFS_CAP_REVISION_MASK: u32 = 0xff00_0000;
const VFS_CAP_REVISION_2: u32 = 0x0200_0000;
const VFS_CAP_REVISION_3: u32 = 0x0300_0000;
const XATTR_CAPS_SZ_2: usize = 20;
const XATTR_CAPS_SZ_3: usize = 24;

/// A file capability as an image carries it: one set inside a user namespace names the
/// host UID of that namespace's root (revision 3), which means nothing in another one,
/// so it goes back to revision 2, as the reference's tar writer does.
fn image_capability(value: Vec<u8>) -> Vec<u8> {
    if value.len() != XATTR_CAPS_SZ_3 {
        return value;
    }
    let magic = u32::from_le_bytes([value[0], value[1], value[2], value[3]]);
    if magic & VFS_CAP_REVISION_MASK != VFS_CAP_REVISION_3 {
        return value;
    }
    let magic = (magic & !VFS_CAP_REVISION_MASK) | VFS_CAP_REVISION_2;
    let mut out = magic.to_le_bytes().to_vec();
    out.extend_from_slice(&value[4..XATTR_CAPS_SZ_2]);
    out
}

/// A tree whose owners are shifted back from a machine's user namespace: the host UID
/// that is root inside it becomes 0.
pub struct Shifted<T: Tree> {
    pub tree: T,
    pub ids: Ids,
    /// An overlay tree whose root is not shifted, which its first start may be shifting
    /// or a stop left half shifted: an ID in a range is the low 16 bits the shift kept.
    pub unfinished: bool,
}

impl<T: Tree> Shifted<T> {
    /// The ID in the image of one the host sees.
    fn image_id(&self, id: u32) -> u32 {
        if self.unfinished && id >= crate::uidshift::BASE_MIN {
            id & 0xFFFF
        } else {
            self.ids.outward(id)
        }
    }
}

impl<T: Tree> Tree for Shifted<T> {
    fn lstat(&self, path: &Path) -> io::Result<Stat> {
        let mut stat = self.tree.lstat(path)?;
        stat.uid = self.image_id(stat.uid);
        stat.gid = self.image_id(stat.gid);
        Ok(stat)
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<OsString>> {
        self.tree.read_dir(path)
    }

    fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        self.tree.read_link(path)
    }

    fn xattr(&self, path: &Path, name: &str) -> io::Result<Option<Vec<u8>>> {
        let value = self.tree.xattr(path, name)?;
        Ok(if name == CAPABILITY {
            value.map(image_capability)
        } else {
            value
        })
    }

    fn open(&self, path: &Path) -> io::Result<Box<dyn Read + '_>> {
        self.tree.open(path)
    }
}

/// A machine's files, opened for commit or export.
pub struct Snapshot {
    pub record: ImageRecord,
    /// Its tree as it sees it, owned as in the image.
    pub tree: Shifted<RootTree>,
    /// The upper directory of an overlay machine's mount.
    pub upper: Option<PathBuf>,
    pub running: bool,
}

/// Opens the files of machine `name` to `what` them (commit, export). An mstack machine
/// is refused: its root exists only inside it while it runs.
pub async fn open(ctx: &Context, name: &str, what: &str) -> Result<Snapshot> {
    crate::reference::validate_machine_name(name)?;
    let sd = ctx.sd().await?;
    crate::api::machines::refuse_foreign(sd, name).await?;
    let store = &ctx.store;
    if store.is_starting(name) {
        bail!("machine {name} is starting; wait for it");
    }
    let record = store.load_image(name)?.ok_or_else(|| {
        FailureKind::NoSuchMachine.error(format!("no machine or image named {name}"))
    })?;
    let running = sd.machine_exists(name).await?;
    if !running && crate::api::machines::starting_unit(sd, name).await? {
        bail!("machine {name} is starting; wait for it");
    }
    let tree = store.machines_dir.join(name);
    let upper = match record.backend {
        BackendChoice::Mstack => bail!(
            "{name} is an mstack machine, whose root exists only inside it while it runs; nspawn cannot {what} it"
        ),
        BackendChoice::Flat => None,
        BackendChoice::Overlay | BackendChoice::Auto => {
            // Mounted by its unit, which a stop leaves alone; mounted again if it went.
            if !is_mountpoint(&tree)? {
                sd.start_unit(&crate::unitname::mount_unit_for(&tree.to_string_lossy()))
                    .await
                    .with_context(|| format!("mounting the tree of {name}"))?;
            }
            Some(store.machines_private_dir().join(name).join("upper"))
        }
    };
    let fd = nix::fcntl::open(
        &tree,
        OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .with_context(|| format!("opening {}", tree.display()))?;
    // How the tree is owned on the host, which is what is read: shifted by the chown
    // into the machine's user namespace, its / then owned by the range's first UID, which
    // systemd-nspawn picks as a multiple of 65536; as the image owns it where the
    // namespace is mapped onto the tree (an idmapped mount, as a flat machine's may be)
    // or there is none. machined's shift says what the machine sees, not what the host
    // tree holds. Any other owner of / (one the image or the machine gave it) is no
    // shift.
    let owner = fstat(&fd)
        .with_context(|| format!("looking at {}", tree.display()))?
        .st_uid;
    let base = if owner.is_multiple_of(65536) {
        owner
    } else {
        0
    };
    // Only an overlay tree is shifted file by file, and its root last.
    let unfinished = base == 0 && record.backend != BackendChoice::Flat;
    Ok(Snapshot {
        record,
        tree: Shifted {
            tree: RootTree::new(fd),
            ids: Ids::Shift(base),
            unfinished,
        },
        upper,
        running,
    })
}

/// What nspawn and systemd-nspawn write into a machine's tree for that machine, from the
/// host's files or its own: a commit keeps the image's.
const MANAGED: [&str; 5] = [
    "etc/hosts",
    "etc/resolv.conf",
    "etc/hostname",
    "etc/machine-id",
    "etc/localtime",
];

/// A path inside a machine as its tree has it, without the leading slash; None for one
/// that climbs.
fn tree_path(target: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in Path::new(target).components() {
        match component {
            Component::Normal(c) => out.push(c),
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) => return None,
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

/// The mount points systemd-nspawn makes in every machine whose image lacks them.
const MOUNT_POINTS: [&str; 5] = ["/proc", "/sys", "/dev", "/tmp", "/root"];

/// Where something is mounted in the machine: systemd-nspawn makes a file or a
/// directory there, and the directories on its way, when the image has none.
fn mount_targets(record: &ImageRecord) -> Vec<String> {
    let mut targets: Vec<String> = crate::build::sandbox::STUBS
        .iter()
        .chain(MOUNT_POINTS.iter())
        .map(|s| s.to_string())
        .collect();
    targets.extend(record.tuning.devices.iter().map(|d| d.container.clone()));
    // Bound by the hooks of its CDI devices, as the specs on this host say now.
    if let Ok(Some(plan)) = crate::cdi::plan(&record.tuning) {
        targets.extend(plan.mounts.iter().map(|m| m.target.clone()));
    }
    targets.extend(record.volumes.iter().map(|v| v.target.clone()));
    targets.extend(
        record
            .tuning
            .tmpfs
            .iter()
            .map(|t| t.split(':').next().unwrap_or(t).to_string()),
    );
    targets.extend(record.tuning.secrets.iter().map(|s| s.target.clone()));
    targets.extend(crate::getent::TARGETS.iter().map(|t| t.to_string()));
    targets
}

/// The directory systemd-nspawn makes for the machine's journal (--link-journal), named
/// by its machine ID, which a machine of another image does not have.
fn machine_journal(path: &Path) -> bool {
    path.parent() == Some(Path::new("var/log/journal"))
        && path.file_name().is_some_and(|n| {
            let n = n.as_bytes();
            n.len() == 32 && n.iter().all(|c| c.is_ascii_hexdigit())
        })
}

/// The links to /usr systemd-nspawn makes in a tree without them, and where each leads.
fn usr_link(path: &Path, target: &Path) -> bool {
    let target = target.to_string_lossy();
    match path.to_str() {
        Some("bin") => target == "usr/bin",
        Some("sbin") => target == "usr/sbin",
        Some("lib") => target == "usr/lib",
        Some("lib64") => {
            target == "usr/lib64"
                || (target.starts_with("usr/lib/") && target.ends_with("-linux-gnu"))
        }
        _ => false,
    }
}

/// `path` and the directories on its way.
fn with_parents(path: &Path, into: &mut HashSet<PathBuf>) {
    let mut path = path.to_path_buf();
    loop {
        into.insert(path.clone());
        match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => path = parent.to_path_buf(),
            _ => break,
        }
    }
}

/// The changes the machine made, without the ones its runtime made: what nspawn and
/// systemd-nspawn write for the machine (MANAGED), the journal directory of its machine
/// ID, the links to /usr systemd-nspawn makes where the image has none, and the empty
/// files and directories made to mount on, with the directories made on their way.
pub fn own_changes(changes: Vec<Change>, tree: &dyn Tree, record: &ImageRecord) -> Vec<Change> {
    let managed: HashSet<PathBuf> = MANAGED.iter().map(PathBuf::from).collect();
    let mut stubs: HashSet<PathBuf> = HashSet::new();
    for target in mount_targets(record) {
        if let Some(path) = tree_path(&target) {
            with_parents(&path, &mut stubs);
        }
    }
    let journals: Vec<PathBuf> = changes
        .iter()
        .filter(|c| c.kind == ChangeKind::Add && machine_journal(&c.path))
        .map(|c| c.path.clone())
        .collect();
    // What nspawn writes for the machine is no part of what a directory made for it holds.
    let mut dropped: HashSet<PathBuf> = changes
        .iter()
        .filter(|c| managed.contains(&c.path))
        .map(|c| c.path.clone())
        .collect();
    for path in &managed {
        with_parents(path, &mut stubs);
    }
    for journal in &journals {
        if let Some(parent) = journal.parent() {
            with_parents(parent, &mut stubs);
        }
        dropped.extend(
            changes
                .iter()
                .filter(|c| c.path.starts_with(journal))
                .map(|c| c.path.clone()),
        );
    }
    let mut made: Vec<&Change> = changes
        .iter()
        .filter(|c| c.kind == ChangeKind::Add && stubs.contains(&c.path))
        .collect();
    // The deepest first: a directory made on the way is empty once what was made in it
    // is left out.
    made.sort_by_key(|c| std::cmp::Reverse(c.path.components().count()));
    for change in made {
        let Some(stat) = change.stat else {
            continue;
        };
        let empty = if stat.is_regular() {
            stat.size == 0
        } else if stat.is_symlink() {
            tree.read_link(&change.path)
                .is_ok_and(|target| usr_link(&change.path, &target))
        } else if stat.is_dir() {
            tree.read_dir(&change.path)
                .is_ok_and(|names| names.iter().all(|n| dropped.contains(&change.path.join(n))))
        } else {
            false
        };
        if empty {
            dropped.insert(change.path.clone());
        }
    }
    changes
        .into_iter()
        .filter(|c| !dropped.contains(&c.path) && !managed.contains(&c.path))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unfinished_shift_is_read_by_the_low_bits_of_a_range_s_ids() {
        let tree = |ids, unfinished| Shifted {
            tree: crate::build::layer::DirTree::new("/nonexistent"),
            ids,
            unfinished,
        };
        let half = tree(Ids::Shift(0), true);
        assert_eq!(half.image_id(1000), 1000);
        assert_eq!(half.image_id(0x4000_0000 | 1000), 1000);
        assert_eq!(half.image_id(0x0007_ffff), 0x0007_ffff);
        let whole = tree(Ids::Shift(0x4000_0000), false);
        assert_eq!(whole.image_id(0x4000_0000 | 1000), 1000);
        assert_eq!(whole.image_id(1000), 65534);
        assert_eq!(
            tree(Ids::Shift(0), false).image_id(0x4000_0000),
            0x4000_0000
        );
    }

    #[test]
    fn a_capability_of_a_user_namespace_loses_its_root() {
        let mut v3 = (VFS_CAP_REVISION_3 | 1).to_le_bytes().to_vec();
        v3.extend_from_slice(&[0x00, 0x20, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        v3.extend_from_slice(&100_000u32.to_le_bytes());
        let v2 = image_capability(v3);
        assert_eq!(v2.len(), XATTR_CAPS_SZ_2);
        assert_eq!(
            u32::from_le_bytes([v2[0], v2[1], v2[2], v2[3]]),
            VFS_CAP_REVISION_2 | 1
        );
        assert_eq!(&v2[4..8], &[0x00, 0x20, 0, 0]);
        // Revision 2 stays as it is.
        let kept = image_capability(v2.clone());
        assert_eq!(kept, v2);
    }

    #[test]
    fn what_the_runtime_made_is_left_out() {
        use crate::build::layer::DirTree;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for dir in ["usr/bin", "data", "srv", "run"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::create_dir_all(root.join("etc")).unwrap();
        std::fs::write(root.join("usr/bin/getent"), b"").unwrap();
        std::fs::write(root.join("etc/hosts"), b"127.0.0.1 web\n").unwrap();
        std::fs::write(root.join("srv/app.conf"), b"x").unwrap();
        std::os::unix::fs::symlink("usr/sbin", root.join("sbin")).unwrap();
        let journal = "var/log/journal/0123456789abcdef0123456789abcdef";
        std::fs::create_dir_all(root.join(journal)).unwrap();
        std::fs::write(root.join(journal).join("system.journal"), b"logs").unwrap();
        let tree = DirTree::new(root);
        let record: ImageRecord = serde_json::from_value(serde_json::json!({
            "name": "web", "reference": "hub.nspawn.org/web:latest",
            "manifest_digest": "sha256:0", "layers": [], "backend": "overlay", "created": 0,
            "volumes": [{"source": "/srv/data", "target": "/data"}],
        }))
        .unwrap();
        let change = |kind, path: &str| Change {
            kind,
            path: PathBuf::from(path),
            stat: Some(tree.lstat(Path::new(path)).unwrap()),
        };
        let changes = vec![
            change(ChangeKind::Add, "data"),
            change(ChangeKind::Modify, "etc/hosts"),
            change(ChangeKind::Add, "run"),
            change(ChangeKind::Add, "sbin"),
            change(ChangeKind::Add, "srv"),
            change(ChangeKind::Add, "srv/app.conf"),
            change(ChangeKind::Add, "usr"),
            change(ChangeKind::Add, "usr/bin"),
            change(ChangeKind::Add, "usr/bin/getent"),
            change(ChangeKind::Add, "var"),
            change(ChangeKind::Add, "var/log"),
            change(ChangeKind::Add, "var/log/journal"),
            change(ChangeKind::Add, journal),
            change(ChangeKind::Add, &format!("{journal}/system.journal")),
        ];
        let kept: Vec<String> = own_changes(changes, &tree, &record)
            .iter()
            .map(|c| c.path.display().to_string())
            .collect();
        assert_eq!(kept, ["srv", "srv/app.conf"]);
    }

    #[test]
    fn only_systemd_nspawn_s_links_to_usr_are_its_own() {
        assert!(usr_link(Path::new("sbin"), Path::new("usr/sbin")));
        assert!(usr_link(
            Path::new("lib64"),
            Path::new("usr/lib/x86_64-linux-gnu")
        ));
        assert!(usr_link(Path::new("lib64"), Path::new("usr/lib64")));
        // The machine's own.
        assert!(!usr_link(Path::new("lib64"), Path::new("/lib")));
        assert!(!usr_link(Path::new("bin"), Path::new("busybox")));
    }

    #[test]
    fn mount_points_and_an_etc_of_nspawn_s_files_alone_are_left_out() {
        use crate::build::layer::DirTree;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for dir in ["proc", "etc", "opt"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join("etc/hosts"), b"127.0.0.1 x\n").unwrap();
        std::fs::write(root.join("etc/resolv.conf"), b"nameserver 10.0.0.1\n").unwrap();
        std::fs::write(root.join("opt/app"), b"x").unwrap();
        std::os::unix::fs::symlink("/lib", root.join("lib64")).unwrap();
        let tree = DirTree::new(root);
        let record: ImageRecord = serde_json::from_value(serde_json::json!({
            "name": "s", "reference": "hub.nspawn.org/s:latest",
            "manifest_digest": "sha256:0", "layers": [], "backend": "overlay", "created": 0,
        }))
        .unwrap();
        let changes: Vec<Change> = [
            "etc",
            "etc/hosts",
            "etc/resolv.conf",
            "lib64",
            "opt",
            "opt/app",
            "proc",
        ]
        .iter()
        .map(|p| Change {
            kind: ChangeKind::Add,
            path: PathBuf::from(p),
            stat: Some(tree.lstat(Path::new(p)).unwrap()),
        })
        .collect();
        let kept: Vec<String> = own_changes(changes, &tree, &record)
            .iter()
            .map(|c| c.path.display().to_string())
            .collect();
        assert_eq!(kept, ["lib64", "opt", "opt/app"]);
    }

    #[test]
    fn paths_inside_a_machine_never_climb() {
        assert_eq!(tree_path("/etc/hosts"), Some(PathBuf::from("etc/hosts")));
        assert_eq!(
            tree_path("run/secrets/x"),
            Some(PathBuf::from("run/secrets/x"))
        );
        assert_eq!(tree_path("/"), None);
        assert_eq!(tree_path("/data/../etc"), None);
    }
}
