//! Local state under the state directory (/var/lib/nspawn by default): extracted layers,
//! image records, downloads in flight and per-machine writable directories.

use std::fs::{self, File};
use std::io::{self, BufReader, Read};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use nix::sys::stat::{mknod, Mode, SFlag};
use serde::{Deserialize, Serialize};

use crate::cli::BackendChoice;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageRecord {
    pub name: String,
    pub reference: String,
    pub manifest_digest: String,
    pub layers: Vec<String>,
    pub backend: BackendChoice,
    pub created: u64,
    /// "pull" or "build".
    #[serde(default = "default_origin")]
    pub origin: String,
}

fn default_origin() -> String {
    "pull".to_string()
}

/// First UID/GID of systemd's "foreign UID range" (see UIDS-GIDS.md): directory images
/// that belong to this range are mapped by systemd-mountfsd into managed user namespaces.
pub const FOREIGN_UID_BASE: u32 = 2_147_352_576;

/// Who owns the files of an extracted layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Ownership {
    /// UIDs and GIDs as recorded in the image (root is 0); what overlay and flat images use.
    Root,
    /// Shifted into the foreign UID range; required by mstack images booted with managed
    /// user namespaces.
    Foreign,
}

impl Ownership {
    pub fn layers_subdir(self) -> &'static str {
        match self {
            Ownership::Root => "layers",
            Ownership::Foreign => "layers-foreign",
        }
    }
}

/// Maps an image UID/GID into the foreign range; ids beyond the 64K range become nobody.
pub fn foreign_id(id: u32) -> u32 {
    if id < 0x10000 {
        FOREIGN_UID_BASE + id
    } else {
        FOREIGN_UID_BASE + 0xFFFE
    }
}

#[derive(Debug, Clone)]
pub struct Store {
    pub machines_dir: PathBuf,
    root: PathBuf,
}

impl Store {
    pub fn new(machines_dir: &Path, state_dir: &Path) -> Self {
        Store {
            machines_dir: machines_dir.to_path_buf(),
            root: state_dir.to_path_buf(),
        }
    }

    pub fn init(&self) -> Result<()> {
        for d in [
            self.layers_dir(Ownership::Root),
            self.layers_dir(Ownership::Foreign),
            self.blobs_dir(),
            self.images_dir(),
            self.machines_private_dir(),
        ] {
            fs::create_dir_all(&d).with_context(|| format!("creating {}", d.display()))?;
        }
        fs::create_dir_all(&self.machines_dir)
            .with_context(|| format!("creating {}", self.machines_dir.display()))?;
        check_writable(&self.machines_dir)
    }

    pub fn layers_dir(&self, ownership: Ownership) -> PathBuf {
        self.root.join(ownership.layers_subdir())
    }
    pub fn blobs_dir(&self) -> PathBuf {
        self.root.join("blobs")
    }
    pub fn images_dir(&self) -> PathBuf {
        self.root.join("images")
    }
    /// Writable upper/work directories of overlay machines.
    pub fn machines_private_dir(&self) -> PathBuf {
        self.root.join("machines")
    }

    /// Directory of an extracted layer. Colons are avoided on purpose: overlayfs uses them
    /// as separators in lowerdir=.
    pub fn layer_dir(&self, digest: &str, ownership: Ownership) -> PathBuf {
        self.layers_dir(ownership).join(layer_dir_name(digest))
    }

    /// Compressed blobs (layers and configs) as served by registries, kept for pushing.
    pub fn blob_path(&self, digest: &str) -> PathBuf {
        self.blobs_dir().join(layer_dir_name(digest))
    }

    pub fn has_blob(&self, digest: &str) -> bool {
        self.blob_path(digest).is_file()
    }

    pub fn manifests_dir(&self) -> PathBuf {
        self.root.join("manifests")
    }

    /// The manifest exactly as fetched or built, so its digest stays valid when pushing.
    pub fn save_manifest(&self, name: &str, bytes: &[u8]) -> Result<()> {
        fs::create_dir_all(self.manifests_dir())
            .with_context(|| format!("creating {}", self.manifests_dir().display()))?;
        let path = self.manifests_dir().join(format!("{name}.json"));
        fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))
    }

    pub fn load_manifest(&self, name: &str) -> Result<Vec<u8>> {
        let path = self.manifests_dir().join(format!("{name}.json"));
        fs::read(&path).with_context(|| format!("reading {}", path.display()))
    }

    /// Digests of every blob some image still needs: its layers and its config.
    fn referenced_blobs(&self) -> Result<std::collections::HashSet<String>> {
        let mut set = std::collections::HashSet::new();
        for record in self.list_images()? {
            for layer in &record.layers {
                set.insert(layer_dir_name(layer));
            }
            if let Ok(bytes) = self.load_manifest(&record.name) {
                if let Ok(manifest) =
                    serde_json::from_slice::<oci_client::manifest::OciImageManifest>(&bytes)
                {
                    set.insert(layer_dir_name(&manifest.config.digest));
                }
            }
        }
        Ok(set)
    }

    /// Deletes compressed blobs no image references. Returns the digests removed.
    pub fn gc_blobs(&self) -> Result<Vec<String>> {
        let referenced = self.referenced_blobs()?;
        let mut removed = Vec::new();
        if !self.blobs_dir().is_dir() {
            return Ok(removed);
        }
        for entry in fs::read_dir(self.blobs_dir())? {
            let entry = entry?;
            let file_name = entry.file_name().to_string_lossy().to_string();
            if file_name.starts_with('.') || referenced.contains(&file_name) {
                continue;
            }
            fs::remove_file(entry.path()).with_context(|| format!("removing blob {file_name}"))?;
            removed.push(file_name.replacen("sha256-", "sha256:", 1));
        }
        Ok(removed)
    }

    /// Extracts a downloaded blob into the layer store, ready to be used as an overlayfs
    /// lower directory. The extraction happens in a temporary directory that is renamed at
    /// the end, so a crash never leaves a half layer behind.
    pub fn import_layer(
        &self,
        digest: &str,
        media_type: &str,
        blob: &Path,
        ownership: Ownership,
    ) -> Result<PathBuf> {
        let dest = self.layer_dir(digest, ownership);
        if dest.is_dir() {
            return Ok(dest);
        }
        let tmp = self
            .layers_dir(ownership)
            .join(format!(".tmp-{}", layer_dir_name(digest)));
        if tmp.exists() {
            fs::remove_dir_all(&tmp).with_context(|| format!("removing {}", tmp.display()))?;
        }
        fs::create_dir_all(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        extract_layer(
            blob,
            media_type,
            &tmp,
            WhiteoutMode::OverlayLower,
            ownership,
        )
        .with_context(|| format!("extracting layer {digest}"))?;
        fs::rename(&tmp, &dest)
            .with_context(|| format!("moving layer into place at {}", dest.display()))?;
        Ok(dest)
    }

    pub fn record_image(&self, record: &ImageRecord) -> Result<()> {
        let path = self.images_dir().join(format!("{}.json", record.name));
        let text = serde_json::to_string_pretty(record)?;
        fs::write(&path, text).with_context(|| format!("writing {}", path.display()))
    }

    pub fn load_image(&self, name: &str) -> Result<Option<ImageRecord>> {
        let path = self.images_dir().join(format!("{name}.json"));
        if !path.exists() {
            return Ok(None);
        }
        let text = fs::read_to_string(&path)?;
        Ok(Some(
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?,
        ))
    }

    pub fn list_images(&self) -> Result<Vec<ImageRecord>> {
        let mut out = Vec::new();
        let entries = match fs::read_dir(self.images_dir()) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                eprintln!("note: run as root to see the backend and source of each image");
                return Ok(out);
            }
            Err(e) => {
                return Err(e).with_context(|| format!("reading {}", self.images_dir().display()))
            }
        };
        for entry in entries {
            let entry = entry?;
            if entry.path().extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let text = fs::read_to_string(entry.path())?;
            match serde_json::from_str::<ImageRecord>(&text) {
                Ok(r) => out.push(r),
                Err(e) => eprintln!("warning: ignoring {}: {e}", entry.path().display()),
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    pub fn remove_record(&self, name: &str) -> Result<()> {
        for path in [
            self.images_dir().join(format!("{name}.json")),
            self.manifests_dir().join(format!("{name}.json")),
        ] {
            match fs::remove_file(&path) {
                Ok(()) | Err(_) if !path.exists() => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).with_context(|| format!("removing {}", path.display())),
                Ok(()) => {}
            }
        }
        Ok(())
    }

    /// Finds a record by local name or by the reference it was pulled from or built as.
    pub fn find_image(
        &self,
        name_or_reference: &str,
        default_registry: &str,
    ) -> Result<Option<ImageRecord>> {
        if let Some(record) = self.load_image(name_or_reference)? {
            return Ok(Some(record));
        }
        let wanted = match crate::reference::ImageRef::parse(name_or_reference, default_registry) {
            Ok(r) => r.to_string(),
            Err(_) => return Ok(None),
        };
        Ok(self
            .list_images()?
            .into_iter()
            .find(|r| r.reference == wanted))
    }

    /// Deletes layers that no image record references. Returns the digests removed.
    pub fn gc_layers(&self) -> Result<Vec<String>> {
        let referenced: std::collections::HashSet<String> = self
            .list_images()?
            .into_iter()
            .flat_map(|r| r.layers)
            .map(|d| layer_dir_name(&d))
            .collect();
        let mut removed = Vec::new();
        for ownership in [Ownership::Root, Ownership::Foreign] {
            let dir = self.layers_dir(ownership);
            if !dir.is_dir() {
                continue;
            }
            for entry in fs::read_dir(&dir)? {
                let entry = entry?;
                let file_name = entry.file_name().to_string_lossy().to_string();
                if file_name.starts_with('.') || referenced.contains(&file_name) {
                    continue;
                }
                fs::remove_dir_all(entry.path())
                    .with_context(|| format!("removing layer {file_name}"))?;
                removed.push(file_name.replacen("sha256-", "sha256:", 1));
            }
        }
        Ok(removed)
    }
}

/// Fails early, with an explanation, when nothing can be created below `dir`. The classic
/// case is a btrfs "empty subvolume" placeholder: after booting from a snapshot of the root
/// subvolume, nested subvolumes such as /var/lib/machines turn into empty, immutable
/// directories with inode 2.
pub fn check_writable(dir: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let probe = dir.join(".nspawn-write-test");
    let _ = fs::remove_dir(&probe);
    match fs::create_dir(&probe) {
        Ok(()) => {
            let _ = fs::remove_dir(&probe);
            Ok(())
        }
        Err(e) => {
            let inode = fs::metadata(dir).map(|m| m.ino()).unwrap_or(0);
            Err(anyhow::anyhow!(e)).context(explain_unwritable(dir, inode))
        }
    }
}

pub fn explain_unwritable(dir: &Path, inode: u64) -> String {
    if inode == 2 {
        format!(
            "{d} is an empty btrfs subvolume placeholder (inode 2), typically left behind when the root \
             subvolume was restored from a snapshot; recreate it with: rmdir {d} && btrfs subvolume create {d}",
            d = dir.display()
        )
    } else {
        format!("cannot create directories below {}", dir.display())
    }
}

pub fn layer_dir_name(digest: &str) -> String {
    digest.replacen(':', "-", 1)
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// How OCI whiteout entries are handled while extracting a layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhiteoutMode {
    /// The layer is applied on top of a directory that already holds the lower layers:
    /// whiteouts delete files.
    Apply,
    /// The layer becomes an overlayfs lower directory: whiteouts are converted to overlayfs
    /// whiteout device nodes and opaque directory attributes.
    OverlayLower,
}

#[derive(Debug, PartialEq, Eq)]
enum Whiteout {
    Opaque(PathBuf),
    File(PathBuf),
}

fn classify(path: &Path) -> Option<Whiteout> {
    let name = path.file_name()?.to_str()?;
    let parent = path.parent().map(Path::to_path_buf).unwrap_or_default();
    if name == ".wh..wh..opq" {
        return Some(Whiteout::Opaque(parent));
    }
    name.strip_prefix(".wh.")
        .map(|target| Whiteout::File(parent.join(target)))
}

fn is_safe_relative(path: &Path) -> bool {
    path.components()
        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

fn open_decompressed(blob: &Path, media_type: &str) -> Result<Box<dyn Read>> {
    let file = File::open(blob).with_context(|| format!("opening {}", blob.display()))?;
    let reader = BufReader::with_capacity(1 << 20, file);
    let mt = media_type.to_ascii_lowercase();
    if mt.ends_with("+gzip") || mt.ends_with(".gzip") || mt.ends_with("+gz") {
        Ok(Box::new(flate2::read::MultiGzDecoder::new(reader)))
    } else if mt.ends_with("+zstd") || mt.ends_with(".zstd") {
        Ok(Box::new(zstd::stream::read::Decoder::new(reader)?))
    } else if mt.ends_with("tar") {
        Ok(Box::new(reader))
    } else {
        bail!("unsupported layer media type {media_type}")
    }
}

/// Extracts a layer blob into `target`. Returns the number of entries written.
pub fn extract_layer(
    blob: &Path,
    media_type: &str,
    target: &Path,
    mode: WhiteoutMode,
    ownership: Ownership,
) -> Result<u64> {
    let reader = open_decompressed(blob, media_type)?;
    let root = nix::unistd::geteuid().is_root();
    if ownership == Ownership::Foreign && !root {
        bail!("shifting a layer into the foreign UID range needs root");
    }
    let mut archive = tar::Archive::new(reader);
    archive.set_preserve_permissions(true);
    archive.set_unpack_xattrs(true);
    archive.set_preserve_ownerships(root);
    archive.set_overwrite(true);
    let mut count = 0u64;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        if !is_safe_relative(&path) {
            eprintln!("warning: skipping unsafe path {} in layer", path.display());
            continue;
        }
        if let Some(whiteout) = classify(&path) {
            handle_whiteout(target, whiteout, mode, ownership)?;
            continue;
        }
        // Malformed numeric fields are treated as root, like GNU tar does.
        let uid = entry.header().uid().unwrap_or(0) as u32;
        let gid = entry.header().gid().unwrap_or(0) as u32;
        if !entry
            .unpack_in(target)
            .with_context(|| format!("unpacking {}", path.display()))?
        {
            continue;
        }
        if ownership == Ownership::Foreign {
            let unpacked = target.join(&path);
            std::os::unix::fs::lchown(&unpacked, Some(foreign_id(uid)), Some(foreign_id(gid)))
                .with_context(|| format!("shifting ownership of {}", unpacked.display()))?;
        }
        count += 1;
    }
    if ownership == Ownership::Foreign {
        std::os::unix::fs::lchown(target, Some(FOREIGN_UID_BASE), Some(FOREIGN_UID_BASE))
            .with_context(|| format!("shifting ownership of {}", target.display()))?;
    }
    Ok(count)
}

fn handle_whiteout(
    target: &Path,
    whiteout: Whiteout,
    mode: WhiteoutMode,
    ownership: Ownership,
) -> Result<()> {
    let owner = match ownership {
        Ownership::Root => None,
        Ownership::Foreign => Some(FOREIGN_UID_BASE),
    };
    match (mode, whiteout) {
        (WhiteoutMode::Apply, Whiteout::Opaque(dir)) => {
            let dir = target.join(dir);
            if dir.is_dir() {
                for child in fs::read_dir(&dir)? {
                    remove_any(&child?.path())?;
                }
            }
        }
        (WhiteoutMode::Apply, Whiteout::File(path)) => remove_any(&target.join(path))?,
        (WhiteoutMode::OverlayLower, Whiteout::Opaque(dir)) => {
            let dir = target.join(dir);
            fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
            xattr::set(&dir, "trusted.overlay.opaque", b"y")
                .with_context(|| format!("marking {} as opaque", dir.display()))?;
        }
        (WhiteoutMode::OverlayLower, Whiteout::File(path)) => {
            let node = target.join(path);
            if let Some(parent) = node.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            remove_any(&node)?;
            mknod(&node, SFlag::S_IFCHR, Mode::empty(), 0)
                .with_context(|| format!("creating whiteout {}", node.display()))?;
            if owner.is_some() {
                std::os::unix::fs::lchown(&node, owner, owner)
                    .with_context(|| format!("shifting ownership of {}", node.display()))?;
            }
        }
    }
    Ok(())
}

fn remove_any(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(path)?,
        Ok(_) => fs::remove_file(path)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn tar_with(entries: &[(&str, Option<&str>)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, content) in entries {
            let mut header = tar::Header::new_gnu();
            match content {
                Some(data) => {
                    header.set_size(data.len() as u64);
                    header.set_mode(0o644);
                    header.set_entry_type(tar::EntryType::Regular);
                    header.set_cksum();
                    builder
                        .append_data(&mut header, path, Cursor::new(data.as_bytes()))
                        .unwrap();
                }
                None => {
                    header.set_size(0);
                    header.set_mode(0o755);
                    header.set_entry_type(tar::EntryType::Directory);
                    header.set_cksum();
                    builder
                        .append_data(&mut header, path, Cursor::new(&[][..]))
                        .unwrap();
                }
            }
        }
        builder.into_inner().unwrap()
    }

    #[test]
    fn writable_check_and_btrfs_placeholder_explanation() {
        let tmp = tempfile::tempdir().unwrap();
        check_writable(tmp.path()).unwrap();
        assert!(!tmp.path().join(".nspawn-write-test").exists());
        let msg = explain_unwritable(Path::new("/var/lib/machines"), 2);
        assert!(msg.contains("btrfs subvolume placeholder"));
        assert!(msg.contains("rmdir /var/lib/machines && btrfs subvolume create /var/lib/machines"));
        assert!(explain_unwritable(Path::new("/x"), 300)
            .starts_with("cannot create directories below /x"));
        assert!(check_writable(Path::new("/proc")).is_err());
    }

    #[test]
    fn classifies_whiteouts() {
        assert_eq!(
            classify(Path::new("a/.wh.b")),
            Some(Whiteout::File(PathBuf::from("a/b")))
        );
        assert_eq!(
            classify(Path::new("a/.wh..wh..opq")),
            Some(Whiteout::Opaque(PathBuf::from("a")))
        );
        assert_eq!(
            classify(Path::new(".wh.top")),
            Some(Whiteout::File(PathBuf::from("top")))
        );
        assert_eq!(classify(Path::new("a/normal")), None);
    }

    #[test]
    fn layer_dir_names_have_no_colons() {
        assert_eq!(layer_dir_name("sha256:abc"), "sha256-abc");
    }

    #[test]
    fn foreign_ids_stay_inside_the_range() {
        assert_eq!(foreign_id(0), FOREIGN_UID_BASE);
        assert_eq!(foreign_id(1000), FOREIGN_UID_BASE + 1000);
        assert_eq!(foreign_id(65535), FOREIGN_UID_BASE + 65535);
        assert_eq!(foreign_id(70000), FOREIGN_UID_BASE + 0xFFFE);
        assert_eq!(FOREIGN_UID_BASE, 0x7FFE0000);
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(&tmp.path().join("m"), &tmp.path().join("s"));
        assert!(store
            .layer_dir("sha256:x", Ownership::Foreign)
            .ends_with("layers-foreign/sha256-x"));
        assert!(store
            .layer_dir("sha256:x", Ownership::Root)
            .ends_with("layers/sha256-x"));
    }

    #[test]
    fn applies_whiteouts_on_top_of_previous_layers() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("root");
        fs::create_dir_all(target.join("a")).unwrap();
        fs::write(target.join("a/gone"), "old").unwrap();
        fs::write(target.join("a/kept"), "old").unwrap();
        fs::create_dir_all(target.join("b")).unwrap();
        fs::write(target.join("b/wiped"), "old").unwrap();

        let blob = tmp.path().join("layer.tar");
        fs::write(
            &blob,
            tar_with(&[
                ("a/", None),
                ("a/.wh.gone", Some("")),
                ("a/new", Some("new")),
                ("b/", None),
                ("b/.wh..wh..opq", Some("")),
                ("b/fresh", Some("fresh")),
            ]),
        )
        .unwrap();
        let n = extract_layer(
            &blob,
            "application/vnd.oci.image.layer.v1.tar",
            &target,
            WhiteoutMode::Apply,
            Ownership::Root,
        )
        .unwrap();
        assert_eq!(n, 4);
        assert!(!target.join("a/gone").exists());
        assert_eq!(fs::read_to_string(target.join("a/kept")).unwrap(), "old");
        assert_eq!(fs::read_to_string(target.join("a/new")).unwrap(), "new");
        assert!(!target.join("b/wiped").exists());
        assert_eq!(fs::read_to_string(target.join("b/fresh")).unwrap(), "fresh");
    }

    #[test]
    fn rejects_paths_escaping_the_target() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("root");
        fs::create_dir_all(&target).unwrap();
        // tar::Builder refuses to write ".." paths, so forge the header by hand like an
        // attacker would.
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        let evil = b"../escape";
        header.as_gnu_mut().unwrap().name[..evil.len()].copy_from_slice(evil);
        header.set_size(1);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_cksum();
        builder.append(&header, Cursor::new(b"x")).unwrap();
        let mut ok = tar::Header::new_gnu();
        ok.set_size(1);
        ok.set_mode(0o644);
        ok.set_entry_type(tar::EntryType::Regular);
        ok.set_cksum();
        builder
            .append_data(&mut ok, "ok", Cursor::new(b"y"))
            .unwrap();
        let blob = tmp.path().join("layer.tar");
        fs::write(&blob, builder.into_inner().unwrap()).unwrap();
        let n = extract_layer(
            &blob,
            "application/vnd.oci.image.layer.v1.tar",
            &target,
            WhiteoutMode::Apply,
            Ownership::Root,
        )
        .unwrap();
        assert_eq!(n, 1);
        assert!(!tmp.path().join("escape").exists());
        assert!(target.join("ok").exists());
    }

    #[test]
    fn gzip_and_zstd_layers_are_decompressed() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let raw = tar_with(&[("f", Some("data"))]);

        let gz = tmp.path().join("l.tar.gz");
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(&raw).unwrap();
        fs::write(&gz, enc.finish().unwrap()).unwrap();
        let t1 = tmp.path().join("t1");
        fs::create_dir_all(&t1).unwrap();
        extract_layer(
            &gz,
            "application/vnd.oci.image.layer.v1.tar+gzip",
            &t1,
            WhiteoutMode::Apply,
            Ownership::Root,
        )
        .unwrap();
        assert_eq!(fs::read_to_string(t1.join("f")).unwrap(), "data");

        let zs = tmp.path().join("l.tar.zst");
        fs::write(&zs, zstd::encode_all(Cursor::new(&raw), 3).unwrap()).unwrap();
        let t2 = tmp.path().join("t2");
        fs::create_dir_all(&t2).unwrap();
        extract_layer(
            &zs,
            "application/vnd.oci.image.layer.v1.tar+zstd",
            &t2,
            WhiteoutMode::Apply,
            Ownership::Root,
        )
        .unwrap();
        assert_eq!(fs::read_to_string(t2.join("f")).unwrap(), "data");

        assert!(extract_layer(
            &zs,
            "application/x-unknown",
            &t2,
            WhiteoutMode::Apply,
            Ownership::Root
        )
        .is_err());
    }

    #[test]
    fn records_round_trip_and_gc() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(&tmp.path().join("machines"), &tmp.path().join("state"));
        store.init().unwrap();
        let rec = ImageRecord {
            name: "fedora-44".into(),
            reference: "hub/fedora:44".into(),
            manifest_digest: "sha256:m".into(),
            layers: vec!["sha256:aaa".into()],
            backend: BackendChoice::Overlay,
            created: 1,
            origin: "pull".into(),
        };
        store.record_image(&rec).unwrap();
        assert_eq!(
            store.load_image("fedora-44").unwrap().unwrap().reference,
            "hub/fedora:44"
        );
        assert_eq!(store.list_images().unwrap().len(), 1);
        fs::create_dir_all(store.layer_dir("sha256:aaa", Ownership::Root)).unwrap();
        fs::create_dir_all(store.layer_dir("sha256:bbb", Ownership::Foreign)).unwrap();
        assert_eq!(store.gc_layers().unwrap(), vec!["sha256:bbb".to_string()]);
        assert!(store.layer_dir("sha256:aaa", Ownership::Root).is_dir());
        store.remove_record("fedora-44").unwrap();
        assert!(store.load_image("fedora-44").unwrap().is_none());
        assert_eq!(store.gc_layers().unwrap(), vec!["sha256:aaa".to_string()]);
    }

    #[test]
    fn blob_gc_keeps_layers_and_config_of_recorded_images() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(&tmp.path().join("machines"), &tmp.path().join("state"));
        store.init().unwrap();
        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": "sha256:cfg", "size": 2},
            "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar+zstd", "digest": "sha256:lay", "size": 3}]
        });
        let rec = ImageRecord {
            name: "img".into(),
            reference: "hub.example/img:1".into(),
            manifest_digest: "sha256:m".into(),
            layers: vec!["sha256:lay".into()],
            backend: BackendChoice::Flat,
            created: 1,
            origin: "build".into(),
        };
        store.record_image(&rec).unwrap();
        store
            .save_manifest("img", manifest.to_string().as_bytes())
            .unwrap();
        for d in ["sha256:cfg", "sha256:lay", "sha256:orphan"] {
            fs::write(store.blob_path(d), b"x").unwrap();
        }
        assert_eq!(store.gc_blobs().unwrap(), vec!["sha256:orphan".to_string()]);
        assert!(store.has_blob("sha256:cfg") && store.has_blob("sha256:lay"));
        assert_eq!(
            store
                .find_image("img", "hub.example")
                .unwrap()
                .unwrap()
                .origin,
            "build"
        );
        assert_eq!(
            store
                .find_image("img:1", "hub.example")
                .unwrap()
                .unwrap()
                .name,
            "img"
        );
        assert!(store
            .find_image("other:1", "hub.example")
            .unwrap()
            .is_none());
        store.remove_record("img").unwrap();
        assert!(store.load_manifest("img").is_err());
    }
}
