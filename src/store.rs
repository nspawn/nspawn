//! Local state under the state directory (/var/lib/nspawn by default): extracted layers,
//! image records, downloads in flight and per-machine writable directories.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufReader, Read};
use std::net::Ipv4Addr;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use nix::fcntl::{Flock, FlockArg};
use nix::sys::stat::{mknod, Mode as FileMode, SFlag};
use serde::{Deserialize, Serialize};

use crate::bridge::PortMap;
use crate::cli::BackendChoice;
use crate::oci::{Mode, RunSpec};
use crate::settings::Network;
use crate::volume::Volume;

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
    /// Booted with an init system, or a single program under nspawn's stub init.
    #[serde(default = "default_mode")]
    pub mode: Mode,
    /// Command, environment and friends from the OCI config.
    #[serde(default)]
    pub run: RunSpec,
    #[serde(default = "default_network")]
    pub network: Network,
    /// Fixed address on the nspawn bridge, once assigned.
    #[serde(default)]
    pub address: Option<Ipv4Addr>,
    /// Ports published on the host, like docker -p (bridge network only).
    #[serde(default)]
    pub ports: Vec<PortMap>,
    /// Entrypoint override (--entrypoint); None keeps the image's. An empty list runs
    /// the cmd alone.
    #[serde(default)]
    pub entrypoint: Option<Vec<String>>,
    /// Cmd override (the arguments after --); None keeps the image's. Older records kept
    /// a whole command under "command".
    #[serde(default, alias = "command")]
    pub cmd: Option<Vec<String>>,
    /// Environment on top of the image's (-e).
    #[serde(default)]
    pub env: Vec<String>,
    /// Bind mounts and named volumes (-v).
    #[serde(default)]
    pub volumes: Vec<Volume>,
}

impl ImageRecord {
    /// What the machine runs: the overrides where given, the image's otherwise.
    pub fn effective_command(&self) -> Vec<String> {
        let mut argv = self
            .entrypoint
            .as_deref()
            .unwrap_or(self.run.entrypoint())
            .to_vec();
        argv.extend_from_slice(self.cmd.as_deref().unwrap_or(self.run.cmd()));
        argv
    }

    /// Older records stored an empty "command" list meaning "no override".
    fn normalize(mut self) -> Self {
        if self.cmd.as_ref().is_some_and(|c| c.is_empty()) {
            self.cmd = None;
        }
        self
    }
}

fn default_mode() -> Mode {
    Mode::Boot
}

fn default_network() -> Network {
    Network::Bridge
}

fn default_origin() -> String {
    "pull".to_string()
}

/// First UID/GID of systemd's "foreign UID range" (see UIDS-GIDS.md): directory images
/// that belong to this range are mapped by systemd-mountfsd into managed user namespaces.
pub const FOREIGN_UID_BASE: u32 = 2_147_352_576;

/// Who owns the files of an extracted layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

/// Exclusive hold on the store; see `Store::lock`.
pub struct StoreLock(#[allow(dead_code)] Flock<File>);

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

    /// Serialises the commands that change the store (pull, create, build, rm, the
    /// preparation done by start and the unit hooks), so that two of them never hand out
    /// the same address or extract the same layer at once. Released when dropped.
    pub fn lock(&self) -> Result<StoreLock> {
        fs::create_dir_all(&self.root)
            .with_context(|| format!("creating {}", self.root.display()))?;
        let path = self.root.join(".lock");
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        match Flock::lock(file, FlockArg::LockExclusive) {
            Ok(lock) => Ok(StoreLock(lock)),
            Err((_, errno)) => bail!("locking {}: {errno}", path.display()),
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
    /// Writable upper/work directories of overlay machines and generated per-machine files.
    pub fn machines_private_dir(&self) -> PathBuf {
        self.root.join("machines")
    }
    /// Named volumes (-v name:/path), one directory each.
    pub fn volumes_dir(&self) -> PathBuf {
        self.root.join("volumes")
    }
    /// Files nspawn generates for one machine (network configuration, hosts).
    pub fn machine_files_dir(&self, name: &str) -> PathBuf {
        self.machines_private_dir().join(name)
    }
    pub fn remove_machine_files(&self, name: &str) -> Result<()> {
        let dir = self.machine_files_dir(name);
        match fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("removing {}", dir.display())),
        }
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
            if file_name.starts_with(".part-") {
                // A download or copy that never finished; the store is locked, so nobody
                // is writing it now.
                let _ = fs::remove_file(entry.path());
                continue;
            }
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
        let record: ImageRecord =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        Ok(Some(record.normalize()))
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
                Ok(r) => out.push(r.normalize()),
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

    /// Deletes layers that no image record references, in each layer store separately:
    /// overlay machines use the root-owned copies, mstack machines the foreign-owned ones
    /// and flat machines none at all. Returns the digests removed.
    pub fn gc_layers(&self) -> Result<Vec<String>> {
        let mut referenced: HashMap<Ownership, HashSet<String>> = HashMap::new();
        for record in self.list_images()? {
            let ownership = match record.backend {
                BackendChoice::Overlay => Ownership::Root,
                BackendChoice::Mstack => Ownership::Foreign,
                BackendChoice::Flat | BackendChoice::Auto => continue,
            };
            referenced
                .entry(ownership)
                .or_default()
                .extend(record.layers.iter().map(|d| layer_dir_name(d)));
        }
        let mut removed = Vec::new();
        for ownership in [Ownership::Root, Ownership::Foreign] {
            let dir = self.layers_dir(ownership);
            if !dir.is_dir() {
                continue;
            }
            let used = referenced.remove(&ownership).unwrap_or_default();
            for entry in fs::read_dir(&dir)? {
                let entry = entry?;
                let file_name = entry.file_name().to_string_lossy().to_string();
                if file_name.starts_with(".tmp-") {
                    // An extraction that never finished; the store is locked, so nobody
                    // is writing it now.
                    let _ = fs::remove_dir_all(entry.path());
                    continue;
                }
                if file_name.starts_with('.') || used.contains(&file_name) {
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
        let header = entry.header();
        // Malformed numeric fields are treated as root, like GNU tar does.
        let uid = header.uid().unwrap_or(0) as u32;
        let gid = header.gid().unwrap_or(0) as u32;
        let file_mode = header.mode().unwrap_or(0o644) & 0o7777;
        let kind = header.entry_type();
        let capabilities = file_capabilities(&mut entry)?;
        if mode == WhiteoutMode::Apply {
            // A later layer may turn a file into a directory or the other way round; tar
            // only knows how to overwrite like with like.
            match resolve_inside(target, &path, true)? {
                Some(dest) => {
                    if let Ok(meta) = fs::symlink_metadata(&dest) {
                        if kind.is_dir() != meta.is_dir() {
                            remove_any(&dest)?;
                        }
                    }
                }
                None => {
                    eprintln!(
                        "warning: skipping {} in layer: a symlink on its path leads outside",
                        path.display()
                    );
                    continue;
                }
            }
        }
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
            // chown clears setuid/setgid bits and file capabilities, for root too.
            if !kind.is_symlink() {
                fs::set_permissions(&unpacked, fs::Permissions::from_mode(file_mode))
                    .with_context(|| format!("restoring the mode of {}", unpacked.display()))?;
                if let Some(caps) = &capabilities {
                    xattr::set(&unpacked, "security.capability", caps).with_context(|| {
                        format!("restoring the capabilities of {}", unpacked.display())
                    })?;
                }
            }
        }
        count += 1;
    }
    if ownership == Ownership::Foreign {
        // Directories tar created implicitly for entries without a parent entry, and the
        // target itself, are still root's.
        shift_remaining(target)?;
    }
    Ok(count)
}

/// The file capabilities an entry carries in its PAX extended header, if any.
fn file_capabilities<R: Read>(entry: &mut tar::Entry<'_, R>) -> Result<Option<Vec<u8>>> {
    let Some(extensions) = entry.pax_extensions()? else {
        return Ok(None);
    };
    for extension in extensions.flatten() {
        if extension.key_bytes() == b"SCHILY.xattr.security.capability" {
            return Ok(Some(extension.value_bytes().to_vec()));
        }
    }
    Ok(None)
}

/// Moves whatever is still owned below the foreign range into it.
fn shift_remaining(dir: &Path) -> Result<()> {
    let shift = |path: &Path, meta: &fs::Metadata| -> Result<()> {
        if meta.uid() < FOREIGN_UID_BASE || meta.gid() < FOREIGN_UID_BASE {
            std::os::unix::fs::lchown(
                path,
                Some(foreign_id(meta.uid())),
                Some(foreign_id(meta.gid())),
            )
            .with_context(|| format!("shifting ownership of {}", path.display()))?;
            if !meta.is_dir() && !meta.file_type().is_symlink() {
                fs::set_permissions(path, fs::Permissions::from_mode(meta.mode() & 0o7777))?;
            }
        }
        Ok(())
    };
    shift(dir, &fs::symlink_metadata(dir)?)?;
    for child in fs::read_dir(dir)? {
        let child = child?;
        let meta = child.metadata()?; // does not follow symlinks
        if meta.is_dir() {
            shift_remaining(&child.path())?;
        } else {
            shift(&child.path(), &meta)?;
        }
    }
    Ok(())
}

/// `target/relative`, unless a symlink sits on the way: a symlink from this or an
/// earlier layer would take a deletion or a marker outside the layer, up to the host's
/// root. The last component may be a symlink when `symlink_leaf` allows it.
fn resolve_inside(target: &Path, relative: &Path, symlink_leaf: bool) -> Result<Option<PathBuf>> {
    let names: Vec<_> = relative
        .components()
        .filter_map(|c| match c {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect();
    let mut path = target.to_path_buf();
    for (i, name) in names.iter().enumerate() {
        path.push(name);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                if symlink_leaf && i + 1 == names.len() {
                    return Ok(Some(path));
                }
                return Ok(None);
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // Nothing below exists yet; whatever gets created will be real.
                for rest in &names[i + 1..] {
                    path.push(rest);
                }
                return Ok(Some(path));
            }
            Err(e) => return Err(e).with_context(|| format!("inspecting {}", path.display())),
        }
    }
    Ok(Some(path))
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
    let (relative, is_file) = match &whiteout {
        Whiteout::Opaque(dir) => (dir.clone(), false),
        Whiteout::File(path) => (path.clone(), true),
    };
    let Some(node) = resolve_inside(target, &relative, is_file)? else {
        eprintln!(
            "warning: skipping whiteout of {} in layer: a symlink on its path leads outside",
            relative.display()
        );
        return Ok(());
    };
    match (mode, whiteout) {
        (WhiteoutMode::Apply, Whiteout::Opaque(_)) => {
            if fs::symlink_metadata(&node)
                .map(|m| m.is_dir())
                .unwrap_or(false)
            {
                for child in fs::read_dir(&node)? {
                    remove_any(&child?.path())?;
                }
            }
        }
        (WhiteoutMode::Apply, Whiteout::File(_)) => remove_any(&node)?,
        (WhiteoutMode::OverlayLower, Whiteout::Opaque(_)) => {
            fs::create_dir_all(&node).with_context(|| format!("creating {}", node.display()))?;
            xattr::set(&node, "trusted.overlay.opaque", b"y")
                .with_context(|| format!("marking {} as opaque", node.display()))?;
        }
        (WhiteoutMode::OverlayLower, Whiteout::File(_)) => {
            if let Some(parent) = node.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            remove_any(&node)?;
            mknod(&node, SFlag::S_IFCHR, FileMode::empty(), 0)
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
            mode: Mode::Boot,
            run: RunSpec::default(),
            network: Network::Veth,
            address: None,
            ports: Vec::new(),
            entrypoint: None,
            cmd: None,
            env: Vec::new(),
            volumes: Vec::new(),
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
            mode: Mode::App,
            run: RunSpec::default(),
            network: Network::Host,
            address: None,
            ports: Vec::new(),
            entrypoint: None,
            cmd: None,
            env: Vec::new(),
            volumes: Vec::new(),
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

    fn tar_with_symlink(entries: &[(&str, &str)], files: &[(&str, Option<&str>)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, target) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(0);
            header.set_mode(0o777);
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_link_name(target).unwrap();
            header.set_cksum();
            builder
                .append_data(&mut header, path, Cursor::new(&[][..]))
                .unwrap();
        }
        let rest = tar_with(files);
        let mut inner = tar::Archive::new(Cursor::new(rest));
        for entry in inner.entries().unwrap() {
            let mut entry = entry.unwrap();
            let mut header = entry.header().clone();
            let path = entry.path().unwrap().into_owned();
            let mut data = Vec::new();
            entry.read_to_end(&mut data).unwrap();
            header.set_cksum();
            builder
                .append_data(&mut header, path, Cursor::new(data))
                .unwrap();
        }
        builder.into_inner().unwrap()
    }

    #[test]
    fn whiteouts_never_reach_outside_through_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        fs::create_dir_all(outside.join("etc")).unwrap();
        fs::write(outside.join("etc/passwd"), "root").unwrap();
        fs::write(outside.join("victim"), "keep me").unwrap();
        let blob = tmp.path().join("layer.tar");
        // usr -> /outside, then whiteouts below usr: they must not touch /outside.
        fs::write(
            &blob,
            tar_with_symlink(
                &[("usr", outside.to_str().unwrap())],
                &[
                    ("usr/.wh.victim", Some("")),
                    ("usr/etc/.wh..wh..opq", Some("")),
                    ("usr/.wh..wh..opq", Some("")),
                ],
            ),
        )
        .unwrap();
        for mode in [WhiteoutMode::Apply, WhiteoutMode::OverlayLower] {
            let target = tmp.path().join(format!("target-{mode:?}"));
            fs::create_dir_all(&target).unwrap();
            extract_layer(
                &blob,
                "application/vnd.oci.image.layer.v1.tar",
                &target,
                mode,
                Ownership::Root,
            )
            .unwrap();
            assert_eq!(
                fs::read_to_string(outside.join("victim")).unwrap(),
                "keep me"
            );
            assert_eq!(
                fs::read_to_string(outside.join("etc/passwd")).unwrap(),
                "root"
            );
            assert!(!outside.join("etc").join(".wh.victim").exists());
            assert!(fs::symlink_metadata(target.join("usr"))
                .unwrap()
                .file_type()
                .is_symlink());
        }
        // Deleting a symlink itself is legitimate.
        let blob2 = tmp.path().join("layer2.tar");
        fs::write(&blob2, tar_with(&[(".wh.usr", Some(""))])).unwrap();
        let target = tmp.path().join("target-Apply");
        extract_layer(
            &blob2,
            "application/vnd.oci.image.layer.v1.tar",
            &target,
            WhiteoutMode::Apply,
            Ownership::Root,
        )
        .unwrap();
        assert!(fs::symlink_metadata(target.join("usr")).is_err());
        assert!(outside.join("victim").exists());
    }

    #[test]
    fn flat_extraction_lets_a_later_layer_change_an_entry_type() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("root");
        fs::create_dir_all(&target).unwrap();
        let first = tmp.path().join("1.tar");
        let second = tmp.path().join("2.tar");
        let third = tmp.path().join("3.tar");
        fs::write(
            &first,
            tar_with(&[("a", Some("file")), ("d", None), ("d/x", Some("x"))]),
        )
        .unwrap();
        // a becomes a directory, d becomes a file.
        fs::write(
            &second,
            tar_with(&[("a", None), ("a/b", Some("b")), ("d", Some("now a file"))]),
        )
        .unwrap();
        for blob in [&first, &second] {
            extract_layer(
                blob,
                "application/vnd.oci.image.layer.v1.tar",
                &target,
                WhiteoutMode::Apply,
                Ownership::Root,
            )
            .unwrap();
        }
        assert_eq!(fs::read_to_string(target.join("a/b")).unwrap(), "b");
        assert_eq!(fs::read_to_string(target.join("d")).unwrap(), "now a file");
        // And a symlink over a directory.
        fs::write(&third, tar_with_symlink(&[("a", "d")], &[])).unwrap();
        extract_layer(
            &third,
            "application/vnd.oci.image.layer.v1.tar",
            &target,
            WhiteoutMode::Apply,
            Ownership::Root,
        )
        .unwrap();
        assert!(fs::symlink_metadata(target.join("a"))
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn layer_gc_keeps_each_store_apart_and_drops_leftovers() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(&tmp.path().join("machines"), &tmp.path().join("state"));
        store.init().unwrap();
        let record = |name: &str, backend: BackendChoice| ImageRecord {
            name: name.into(),
            reference: "hub/x:1".into(),
            manifest_digest: "sha256:m".into(),
            layers: vec!["sha256:shared".into()],
            backend,
            created: 1,
            origin: "pull".into(),
            mode: Mode::Boot,
            run: RunSpec::default(),
            network: Network::Bridge,
            address: None,
            ports: Vec::new(),
            entrypoint: None,
            cmd: None,
            env: Vec::new(),
            volumes: Vec::new(),
        };
        store
            .record_image(&record("ovl", BackendChoice::Overlay))
            .unwrap();
        store
            .record_image(&record("flat", BackendChoice::Flat))
            .unwrap();
        for ownership in [Ownership::Root, Ownership::Foreign] {
            fs::create_dir_all(store.layer_dir("sha256:shared", ownership)).unwrap();
            fs::create_dir_all(store.layers_dir(ownership).join(".tmp-sha256-abandoned")).unwrap();
        }
        // The overlay record pins the root copy only; the flat one pins nothing.
        assert_eq!(
            store.gc_layers().unwrap(),
            vec!["sha256:shared".to_string()]
        );
        assert!(store.layer_dir("sha256:shared", Ownership::Root).is_dir());
        assert!(!store
            .layer_dir("sha256:shared", Ownership::Foreign)
            .exists());
        assert!(!store
            .layers_dir(Ownership::Root)
            .join(".tmp-sha256-abandoned")
            .exists());
        store
            .record_image(&record("ms", BackendChoice::Mstack))
            .unwrap();
        fs::create_dir_all(store.layer_dir("sha256:shared", Ownership::Foreign)).unwrap();
        assert!(store.gc_layers().unwrap().is_empty());
        store.remove_record("ovl").unwrap();
        assert_eq!(
            store.gc_layers().unwrap(),
            vec!["sha256:shared".to_string()]
        );
        assert!(store
            .layer_dir("sha256:shared", Ownership::Foreign)
            .is_dir());
        // Abandoned partial blobs go too, finished ones stay.
        fs::write(store.blobs_dir().join(".part-sha256-x"), b"half").unwrap();
        fs::write(store.blob_path("sha256:m"), b"manifest?").unwrap();
        store.gc_blobs().unwrap();
        assert!(!store.blobs_dir().join(".part-sha256-x").exists());
    }

    #[test]
    fn store_lock_is_exclusive_between_holders() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(&tmp.path().join("machines"), &tmp.path().join("state"));
        let first = store.lock().unwrap();
        let path = tmp.path().join("state/.lock");
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        assert!(
            Flock::lock(file, FlockArg::LockExclusiveNonblock).is_err(),
            "a second holder must wait"
        );
        drop(first);
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        assert!(Flock::lock(file, FlockArg::LockExclusiveNonblock).is_ok());
    }
}
