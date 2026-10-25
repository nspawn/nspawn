//! Local state under the state directory (/var/lib/nspawn by default): extracted layers,
//! image records, downloads in flight and per-machine writable directories.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufReader, Read};
use std::net::Ipv4Addr;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use nix::fcntl::{Flock, FlockArg};
use nix::sys::stat::{mknod, Mode as FileMode, SFlag};
use serde::{Deserialize, Serialize};

use crate::backend::BackendChoice;
use crate::bridge::{Attachment, NetSpec, PortMap};
use crate::build::copy::root_path_bytes;
use crate::build::ignore::lexical;
use crate::health::Healthcheck;
use crate::oci::{Mode, RunSpec};
use crate::settings::Network;
use crate::tuning::Tuning;
use crate::volume::Volume;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageRecord {
    pub name: String,
    pub reference: String,
    pub manifest_digest: String,
    /// The index the reference named when it was pulled, if it named one: what the
    /// registry answers for the tag, which auto-update compares.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index_digest: Option<String>,
    pub layers: Vec<String>,
    pub backend: BackendChoice,
    pub created: u64,
    /// "pull" or "build".
    #[serde(default = "default_origin")]
    pub origin: String,
    /// Booted with an init system, or a single program.
    #[serde(default = "default_mode")]
    pub mode: Mode,
    /// An app whose image wants its program to be PID 1 (s6-overlay), found at install.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pid1: bool,
    /// Made from an image built on this host, which no registry has a newer one of.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub built: bool,
    /// When it first started, in unix seconds: a record that never did is an image only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<u64>,
    /// The OCI config's command, environment and the rest.
    #[serde(default)]
    pub run: RunSpec,
    #[serde(default = "default_network")]
    pub network: Network,
    /// With the bridge kind: a network made with `network create` rather than the
    /// default one. Kept apart from `network` so that an older nspawn still reads the
    /// record (as a machine of the default network).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_name: Option<String>,
    /// Fixed address on the primary network, once assigned.
    #[serde(default)]
    pub address: Option<Ipv4Addr>,
    /// The networks whose address was asked for (--ip, network connect --ip), not given
    /// out at a start.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub fixed_addresses: BTreeSet<String>,
    /// The networks joined besides the primary one, with the address on each.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_networks: Vec<Attachment>,
    /// Extra names on each network (--network-alias), by network; the machine's own name
    /// is always there.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub aliases: BTreeMap<String, Vec<String>>,
    /// --network none: nothing but lo. Kept apart from `network` so that an older nspawn
    /// still reads the record (as a machine of the default network).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_network: bool,
    /// --network container:NAME: the network namespace of that machine, with no address
    /// or interface of its own. Kept apart from `network` so that an older nspawn still
    /// reads the record (as a machine of the default network).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_container: Option<String>,
    /// The --health-* flags, on top of the image's healthcheck (run.healthcheck).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub healthcheck: Option<Healthcheck>,
    /// The other per-machine settings (hostname, user, capabilities, ...).
    #[serde(default, skip_serializing_if = "Tuning::is_default")]
    pub tuning: Tuning,
    /// Ports published on the host with -p (bridge network only).
    #[serde(default)]
    pub ports: Vec<PortMap>,
    /// Entrypoint override (--entrypoint); None keeps the image's, an empty list none.
    #[serde(default)]
    pub entrypoint: Option<Vec<String>>,
    /// Cmd override (the arguments after --); None keeps the image's. An empty list
    /// after an entrypoint given is none: an entrypoint given without arguments runs
    /// alone. Also read from a "command" key.
    #[serde(default, alias = "command")]
    pub cmd: Option<Vec<String>>,
    /// Environment on top of the image's (-e).
    #[serde(default)]
    pub env: Vec<String>,
    /// Bind mounts and named volumes (-v).
    #[serde(default)]
    pub volumes: Vec<Volume>,
    /// Labels given with --label, on top of the image's own (run.labels).
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// The restart policy (--restart).
    #[serde(default)]
    pub restart: crate::policy::Restart,
    /// --memory, --cpus and --pids-limit.
    #[serde(default)]
    pub limits: crate::policy::Limits,
    /// run --rm: removed once its current run ends.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub remove_on_exit: bool,
    /// Who signed the image, as the pull verified it: "key <hint>, keyless <identity>".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signed_by: Option<String>,
    /// When the signature was made, in unix seconds, as its log entry or timestamp vouched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signed_at: Option<u64>,
}

impl ImageRecord {
    /// The machine's healthcheck, or the image's.
    pub fn effective_healthcheck(&self) -> Option<&Healthcheck> {
        self.healthcheck.as_ref().or(self.run.healthcheck.as_ref())
    }

    /// --user, or the image's.
    pub fn effective_user(&self) -> Option<&str> {
        self.tuning.user.as_deref().or(self.run.user.as_deref())
    }

    /// --workdir, or the image's.
    pub fn effective_working_dir(&self) -> Option<&str> {
        self.tuning
            .working_dir
            .as_deref()
            .or(self.run.working_dir.as_deref())
    }

    /// --stop-signal, or the image's.
    pub fn effective_stop_signal(&self) -> Option<&str> {
        self.tuning
            .stop_signal
            .as_deref()
            .or(self.run.stop_signal.as_deref())
    }

    /// Whether an app's program runs as PID 1 of the machine rather than as the child of
    /// systemd-nspawn's stub init: --init=false and --init say which, and without either
    /// the image does.
    pub fn program_is_pid1(&self) -> bool {
        self.mode == Mode::App && self.tuning.init.map_or(self.pid1, |init| !init)
    }

    /// Whether the program runs under the stub init, which reaps, forwards and reports
    /// signals its own way.
    pub fn under_stub_init(&self) -> bool {
        self.mode == Mode::App && !self.program_is_pid1()
    }

    /// Whether it mounts docker's socket, and so gets nspawn's Docker API.
    pub fn takes_api_socket(&self) -> bool {
        self.volumes.iter().any(Volume::is_api_socket)
    }

    /// The image's labels with the ones given to this machine on top.
    pub fn effective_labels(&self) -> BTreeMap<String, String> {
        let mut labels = self.run.labels.clone();
        labels.extend(self.labels.clone());
        labels
    }
}

impl ImageRecord {
    /// What the machine runs: the overrides where given, the image's otherwise.
    pub fn effective_command(&self) -> Vec<String> {
        let mut argv = self.effective_entrypoint().to_vec();
        argv.extend_from_slice(self.effective_cmd());
        argv
    }

    /// The entrypoint given, none for an empty one, or the image's.
    pub fn effective_entrypoint(&self) -> &[String] {
        self.entrypoint
            .as_deref()
            .unwrap_or_else(|| self.run.entrypoint())
    }

    /// The arguments after the entrypoint: the ones given, none after an entrypoint
    /// given alone, the image's otherwise. An entrypoint without a cmd at all is how
    /// records made before an entrypoint dropped the image's command keep an entrypoint
    /// given alone: the image's command still follows it there.
    pub fn effective_cmd(&self) -> &[String] {
        match &self.cmd {
            Some(cmd) => cmd,
            None => self.run.cmd(),
        }
    }

    /// An empty cmd override means none, and a run config with only `command` has it as
    /// its cmd.
    fn normalize(mut self) -> Self {
        // Without an entrypoint of its own an empty cmd overrides nothing.
        if self.entrypoint.is_none() && self.cmd.as_ref().is_some_and(|c| c.is_empty()) {
            self.cmd = None;
        }
        if self.run.entrypoint.is_empty() && self.run.cmd.is_empty() && !self.run.command.is_empty()
        {
            self.run.cmd = std::mem::take(&mut self.run.command);
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

/// Blobs a pull is bringing in, kept from the garbage collector until a record refers to
/// them, since downloads run without the store lock: a file under the blobs directory
/// listing the digests, gone with the guard.
pub struct BlobHold(PathBuf);

/// Shared by a pull from writing its hold until it has seen which blobs are there,
/// exclusive for the collector from reading the holds until it has deleted: a hold made
/// in between would keep nothing from a collector that read them before it. An flock,
/// so that it holds across processes: `nspawn remove-after-exit` collects too.
pub struct HoldsLock(#[allow(dead_code)] Flock<File>);

/// Deletions of what collections moved aside, under way on threads of their own.
static TRASH: std::sync::Mutex<Vec<std::thread::JoinHandle<()>>> =
    std::sync::Mutex::new(Vec::new());

/// Deletes trees already moved out of the names, on a thread of their own: a tree of
/// many files takes long to delete, and the caller may hold the store's lock or run on
/// the runtime's workers.
fn delete_later(trees: Vec<PathBuf>) {
    if trees.is_empty() {
        return;
    }
    let deleting = std::thread::spawn(move || {
        for tree in trees {
            let _ = remove_tree(tree);
        }
    });
    let mut under_way = TRASH.lock().unwrap_or_else(|e| e.into_inner());
    under_way.retain(|handle| !handle.is_finished());
    under_way.push(deleting);
}

/// Waits for the deletions of what collections moved aside: a process other than the
/// service ends with them done, rather than leaving them to the service's next start.
pub fn finish_trash() {
    let handles = std::mem::take(&mut *TRASH.lock().unwrap_or_else(|e| e.into_inner()));
    for handle in handles {
        let _ = handle.join();
    }
}

impl Drop for BlobHold {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

impl BlobHold {
    /// What the hold keeps from now on: a build adds its layers as it makes them.
    pub fn set(&self, digests: &[String]) -> Result<()> {
        write_atomically(&self.0, digests.join("\n").as_bytes())
    }
}

/// A start in progress, from the preparation of its files until the machine registers or
/// the start fails: keeps a second start, a removal or a replacement off it while
/// machined does not list it yet.
pub struct Starting(PathBuf);

impl Drop for Starting {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Holds and start markers older than this belong to a process that died.
const STALE_MARKER: std::time::Duration = std::time::Duration::from_secs(3600);

/// This boot's ID, which a marker keeps: one of another boot was left by a crash.
fn boot_id() -> String {
    fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|id| id.trim().to_string())
        .unwrap_or_default()
}

/// Whether a marker at `path` stands: there, of this boot, and younger than a crash
/// makes one. An empty one, as releases that kept no boot ID wrote, goes by its age.
fn marked(path: &Path) -> bool {
    let Ok(text) = fs::read_to_string(path) else {
        return false;
    };
    let of = text.trim();
    (of.is_empty() || of == boot_id()) && !is_stale(path)
}

/// The file that says since when records keep their first start.
const STARTS_KEPT: &str = "starts-kept-since";

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

    /// Serialises the commands that change the store (pull, create, build, rm, a start's
    /// preparation, the unit hooks). Released when dropped. The wait runs on a blocking
    /// thread: while a caller queues, the service must keep answering the bus, the
    /// lock's holder among its callers.
    pub async fn lock(&self) -> Result<StoreLock> {
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || lock_blocking(&root, None))
            .await
            .context("waiting for the store lock")?
    }

    /// Like `lock`, giving up after `wait` with a clear message: for the unit hooks, which
    /// run inside a start job with a timeout of its own.
    pub async fn lock_for(&self, wait: std::time::Duration) -> Result<StoreLock> {
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || lock_blocking(&root, Some(wait)))
            .await
            .context("waiting for the store lock")?
    }

    pub fn init(&self) -> Result<()> {
        create_dir_with_mode(&self.root, PASSAGE)?;
        for d in self.private_dirs() {
            create_dir_with_mode(&d, PRIVATE)?;
        }
        create_dir_with_mode(&self.machines_private_dir(), PASSAGE)?;
        self.protect()?;
        // As systemd-tmpfiles makes it.
        create_dir_with_mode(&self.machines_dir, PRIVATE)?;
        check_writable(&self.machines_dir)
    }

    /// Root's alone: layers hold setuid programs and device nodes that would work for any
    /// local user reaching them, records hold the machines' environment, volumes and
    /// builds their data.
    fn private_dirs(&self) -> [PathBuf; 13] {
        [
            self.layers_dir(Ownership::Root),
            self.layers_dir(Ownership::Foreign),
            self.blobs_dir(),
            self.images_dir(),
            self.manifests_dir(),
            self.volumes_dir(),
            self.root.join("builds"),
            self.root.join("cache"),
            self.root.join("starting"),
            self.root.join("claims"),
            self.networks_dir(),
            self.secrets_dir(),
            self.root.join("rollback"),
        ]
    }

    /// Where builds work: a directory each while it runs.
    pub fn builds_dir(&self) -> PathBuf {
        self.root.join("builds")
    }

    /// Since when records keep their first start, in unix seconds: one made earlier, by a
    /// release that did not, says nothing by lacking it.
    pub fn starts_kept_since(&self) -> Option<u64> {
        std::fs::read_to_string(self.root.join(STARTS_KEPT))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// Notes that records keep their first start from now on, once.
    pub fn keep_starts(&self) -> Result<()> {
        if self.starts_kept_since().is_some() {
            return Ok(());
        }
        let path = self.root.join(STARTS_KEPT);
        write_atomically(&path, format!("{}\n", now_unix()).as_bytes())
    }

    /// The build cache: the layers of the steps builds made and the base images they
    /// pulled.
    pub fn build_cache_dir(&self) -> PathBuf {
        self.root.join("cache").join("build")
    }

    /// Where auto-update keeps a machine's previous record, manifest and writable layer
    /// until its new image has proved itself.
    pub fn rollback_dir(&self, name: &str) -> PathBuf {
        self.root.join("rollback").join(name)
    }

    /// Keeps `record` and its manifest's bytes for a rollback.
    pub fn save_rollback(&self, record: &ImageRecord, manifest: &[u8]) -> Result<()> {
        let dir = self.rollback_dir(&record.name);
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        restrict(&dir, PRIVATE)?;
        write_atomically(&dir.join("manifest.json"), manifest)?;
        let json = serde_json::to_vec_pretty(record)?;
        write_atomically(&dir.join("record.json"), &json)
    }

    /// The record and manifest kept for a rollback of `name`, if any.
    pub fn load_rollback(&self, name: &str) -> Result<Option<(ImageRecord, Vec<u8>)>> {
        let dir = self.rollback_dir(name);
        let record = match fs::read(dir.join("record.json")) {
            Ok(bytes) => serde_json::from_slice::<ImageRecord>(&bytes)
                .with_context(|| format!("parsing {}", dir.join("record.json").display()))?
                .normalize(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
        };
        let manifest = fs::read(dir.join("manifest.json"))
            .with_context(|| format!("reading {}", dir.join("manifest.json").display()))?;
        Ok(Some((record, manifest)))
    }

    /// Lets a rollback go: renamed out of the names first, at once, so that one cut
    /// short while it is deleted is no rollback half there, which recover and the start
    /// guard would read forever.
    pub fn forget_rollback(&self, name: &str) -> Result<()> {
        let dir = self.rollback_dir(name);
        let gone = dir.with_file_name(format!(".gone-{name}-{}", unique_suffix()));
        match fs::rename(&dir, &gone) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e).with_context(|| format!("removing {}", dir.display())),
        }
        // A rollback keeps a machine's writable layer: deleted off the caller's thread.
        delete_later(vec![gone]);
        Ok(())
    }

    /// The names with a rollback kept.
    pub fn rollbacks(&self) -> Vec<String> {
        let Ok(entries) = fs::read_dir(self.root.join("rollback")) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .filter(|name| !name.starts_with('.'))
            .collect()
    }

    /// The records a rollback keeps, whose layers and blobs stay until it is decided.
    /// One cut short (a crash while it was written or removed) keeps nothing: it could
    /// not be put back, and every collection would fail on it.
    fn rollback_records(&self) -> Result<Vec<(ImageRecord, Vec<u8>)>> {
        let mut out = Vec::new();
        for name in self.rollbacks() {
            match self.load_rollback(&name) {
                Ok(Some(kept)) => out.push(kept),
                Ok(None) => {}
                Err(e) => eprintln!("warning: the rollback of {name} is left out: {e:#}"),
            }
        }
        Ok(out)
    }

    /// Gives what exists below the state directory the modes of the table. The state
    /// directory and the machines' directory stay passable: systemd-nspawn binds a machine's generated files from
    /// inside the managed user namespace of an mstack machine, where it is nobody. A
    /// machine's own directory is passable too, unless it holds the writable layer of an
    /// overlay machine, which is as private as the image's.
    pub fn protect(&self) -> Result<()> {
        passable(&self.root)?;
        for d in self.private_dirs() {
            restrict(&d, PRIVATE)?;
        }
        let machines = self.machines_private_dir();
        passable(&machines)?;
        if let Ok(entries) = fs::read_dir(&machines) {
            for entry in entries.flatten() {
                let dir = entry.path();
                if dir.is_dir() {
                    restrict(&dir, machine_dir_mode(&dir))?;
                }
            }
        }
        Ok(())
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
    /// User-defined networks, one NAME.json each.
    pub fn networks_dir(&self) -> PathBuf {
        self.root.join("networks")
    }
    /// Secrets: NAME.cred (encrypted with systemd-creds) and NAME.json.
    pub fn secrets_dir(&self) -> PathBuf {
        self.root.join("secrets")
    }

    pub fn record_network(&self, net: &NetSpec) -> Result<()> {
        crate::reference::validate_entry_name(&net.name)?;
        let dir = self.networks_dir();
        create_dir_with_mode(&dir, PRIVATE)?;
        let text = serde_json::to_string_pretty(net)?;
        write_atomically(&dir.join(format!("{}.json", net.name)), text.as_bytes())
    }

    pub fn load_network(&self, name: &str) -> Result<Option<NetSpec>> {
        if crate::reference::validate_entry_name(name).is_err() {
            return Ok(None);
        }
        let path = self.networks_dir().join(format!("{name}.json"));
        match fs::read_to_string(&path) {
            Ok(text) => Ok(Some(
                serde_json::from_str(&text)
                    .with_context(|| format!("parsing {}", path.display()))?,
            )),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Every user-defined network, sorted by name; one that cannot be read is an error,
    /// since its subnet and its rules would be missed.
    pub fn list_networks(&self) -> Result<Vec<NetSpec>> {
        let dir = self.networks_dir();
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
        };
        let mut out = Vec::new();
        for entry in entries {
            let path = entry
                .with_context(|| format!("reading {}", dir.display()))?
                .path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let text =
                fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
            out.push(
                serde_json::from_str::<NetSpec>(&text)
                    .with_context(|| format!("parsing {}", path.display()))?,
            );
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    pub fn remove_network(&self, name: &str) -> Result<()> {
        crate::reference::validate_entry_name(name)?;
        let path = self.networks_dir().join(format!("{name}.json"));
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
        }
    }
    /// Files nspawn generates for one machine (network configuration, hosts).
    pub fn machine_files_dir(&self, name: &str) -> PathBuf {
        self.machines_private_dir().join(name)
    }
    fn exit_on_next_path(&self, name: &str) -> PathBuf {
        self.machine_files_dir(name).join("exit-on-next")
    }

    /// Remembers that the machine's current run was sent its stop signal: when it ends,
    /// it is not restarted, since a kill with the stop signal is a stop.
    pub fn mark_exit_on_next(&self, name: &str) -> Result<()> {
        crate::reference::validate_entry_name(name)?;
        let path = self.exit_on_next_path(name);
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        fs::write(&path, b"").with_context(|| format!("writing {}", path.display()))
    }

    /// Forgets the mark of `mark_exit_on_next`; true when there was one.
    pub fn take_exit_on_next(&self, name: &str) -> Result<bool> {
        crate::reference::validate_entry_name(name)?;
        let path = self.exit_on_next_path(name);
        match fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
        }
    }

    fn signal_path(&self, name: &str) -> PathBuf {
        self.machine_files_dir(name).join("last-signal")
    }

    /// Remembers the signal nspawn sent the machine's current run (kill, stop), and
    /// which run that is (its unit's InvocationID), which the exit code of the run is made
    /// of: systemd-nspawn only says 255 when its program died of one, 1 when the whole
    /// machine got SIGKILL.
    pub fn mark_signal(&self, name: &str, signal: i32, run: Option<&str>) -> Result<()> {
        crate::reference::validate_entry_name(name)?;
        let path = self.signal_path(name);
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let text = match run {
            Some(run) => format!("{signal} {run}"),
            None => signal.to_string(),
        };
        fs::write(&path, text).with_context(|| format!("writing {}", path.display()))
    }

    /// The signal of `mark_signal`, if any.
    pub fn last_signal(&self, name: &str) -> Option<i32> {
        self.signal_mark(name).map(|(signal, _)| signal)
    }

    /// The signal of `mark_signal` when it was sent to the run `run`.
    pub fn signal_of_run(&self, name: &str, run: &str) -> Option<i32> {
        match self.signal_mark(name)? {
            (signal, Some(marked)) if marked == run => Some(signal),
            _ => None,
        }
    }

    fn signal_mark(&self, name: &str) -> Option<(i32, Option<String>)> {
        crate::reference::validate_entry_name(name).ok()?;
        let text = fs::read_to_string(self.signal_path(name)).ok()?;
        let mut parts = text.split_whitespace();
        let signal = parts.next()?.parse().ok()?;
        Some((signal, parts.next().map(str::to_string)))
    }

    /// Forgets the signal of the last run, as a new run begins.
    pub fn forget_signal(&self, name: &str) -> Result<()> {
        crate::reference::validate_entry_name(name)?;
        let path = self.signal_path(name);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
        }
    }

    pub fn remove_machine_files(&self, name: &str) -> Result<()> {
        crate::reference::validate_entry_name(name)?;
        let dir = self.machine_files_dir(name);
        match remove_tree(&dir) {
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
    /// Where a flat machine's layers are applied before it takes its name: beside its
    /// directory, on the same file system, hidden from machined by the leading dot.
    pub fn flat_staging_dir(&self, name: &str) -> PathBuf {
        self.machines_dir.join(format!(".nspawn-staging-{name}"))
    }

    pub fn blob_path(&self, digest: &str) -> PathBuf {
        self.blobs_dir().join(layer_dir_name(digest))
    }

    pub fn has_blob(&self, digest: &str) -> bool {
        self.blob_path(digest).is_file()
    }

    /// Keeps `digests` from `gc_blobs` and their layers from `gc_layers` while the guard
    /// lives.
    pub fn hold_blobs(&self, digests: &[String]) -> Result<BlobHold> {
        let dir = self.blobs_dir();
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join(format!(".hold-{}", unique_suffix()));
        write_atomically(&path, digests.join("\n").as_bytes())?;
        Ok(BlobHold(path))
    }

    /// Takes every hold: pulls and builds run only in the service, so at its own start
    /// any left is one of a service that was killed, whose PID may run something else by
    /// now and keep the hold alive for ever.
    pub fn forget_holds(&self) {
        if let Ok(entries) = fs::read_dir(self.blobs_dir()) {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().starts_with(".hold-") {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
    }

    /// The blob file names held by pulls in flight; stale holds are removed on the way.
    fn held_blobs(&self) -> HashSet<String> {
        let mut held = HashSet::new();
        let Ok(entries) = fs::read_dir(self.blobs_dir()) else {
            return held;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with(".hold-") {
                continue;
            }
            // A pull may download for longer than an hour: its hold goes only once the
            // process that wrote it is gone.
            if is_stale(&entry.path()) && !writer_alive(&name) {
                let _ = fs::remove_file(entry.path());
                continue;
            }
            if let Ok(text) = fs::read_to_string(entry.path()) {
                held.extend(text.lines().map(layer_dir_name));
            }
        }
        held
    }

    fn starting_path(&self, name: &str) -> PathBuf {
        self.root.join("starting").join(name)
    }

    /// Takes every start marker: only the service makes them, so at its own start any
    /// left is one a service that stopped or was killed during a start could not take.
    pub fn forget_starts(&self) {
        if let Ok(entries) = fs::read_dir(self.root.join("starting")) {
            for entry in entries.flatten() {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    fn claim_path(&self, name: &str) -> PathBuf {
        self.root.join("claims").join(name)
    }

    /// Claims the host ports of a machine whose unit passed its check, until its publish
    /// or its release: a unit systemd starts has no start marker, and machined knows it
    /// only once it registers, so that another unit's check in between would not see it.
    pub fn claim_ports(&self, name: &str) -> Result<()> {
        crate::reference::validate_entry_name(name)?;
        let path = self.claim_path(name);
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        fs::write(&path, boot_id()).with_context(|| format!("writing {}", path.display()))
    }

    pub fn release_ports_claim(&self, name: &str) {
        if crate::reference::validate_entry_name(name).is_ok() {
            let _ = fs::remove_file(self.claim_path(name));
        }
    }

    /// Whether `name` claims its ports, by a claim no older than a crash makes stale.
    pub fn claims_ports(&self, name: &str) -> bool {
        if crate::reference::validate_entry_name(name).is_err() {
            return false;
        }
        marked(&self.claim_path(name))
    }

    /// Marks `name` as starting until the guard is dropped.
    pub fn mark_starting(&self, name: &str) -> Result<Starting> {
        crate::reference::validate_entry_name(name)?;
        let path = self.starting_path(name);
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        fs::write(&path, boot_id()).with_context(|| format!("writing {}", path.display()))?;
        Ok(Starting(path))
    }

    /// Whether a start of `name` is in progress (a marker left by a start that died with
    /// its process does not count).
    pub fn is_starting(&self, name: &str) -> bool {
        if crate::reference::validate_entry_name(name).is_err() {
            return false;
        }
        marked(&self.starting_path(name))
    }

    pub fn manifests_dir(&self) -> PathBuf {
        self.root.join("manifests")
    }

    /// The manifest exactly as fetched or built, so its digest stays valid when pushing.
    pub fn save_manifest(&self, name: &str, bytes: &[u8]) -> Result<()> {
        crate::reference::validate_entry_name(name)?;
        fs::create_dir_all(self.manifests_dir())
            .with_context(|| format!("creating {}", self.manifests_dir().display()))?;
        write_atomically(&self.manifests_dir().join(format!("{name}.json")), bytes)
    }

    pub fn load_manifest(&self, name: &str) -> Result<Vec<u8>> {
        crate::reference::validate_entry_name(name)?;
        let path = self.manifests_dir().join(format!("{name}.json"));
        fs::read(&path).with_context(|| format!("reading {}", path.display()))
    }

    /// Digests of every blob some image still needs: its layers and its config.
    fn referenced_blobs(&self) -> Result<std::collections::HashSet<String>> {
        let mut set = std::collections::HashSet::new();
        for record in self.list_images_strict()? {
            for layer in &record.layers {
                set.insert(layer_dir_name(layer));
            }
            // The config blob is named by the manifest alone: one that cannot be read
            // would have its blob taken for unreferenced.
            let bytes = self.load_manifest(&record.name).with_context(|| {
                format!(
                    "the manifest of {} cannot be read; fix or remove the image first",
                    record.name
                )
            })?;
            let manifest = serde_json::from_slice::<oci_client::manifest::OciImageManifest>(&bytes)
                .with_context(|| {
                    format!(
                        "the manifest of {} cannot be read; fix or remove the image first",
                        record.name
                    )
                })?;
            set.insert(layer_dir_name(&manifest.config.digest));
        }
        set.extend(
            crate::build::cache::Cache::new(&self.build_cache_dir())
                .blobs()
                .iter()
                .map(|b| layer_dir_name(b)),
        );
        for (record, bytes) in self.rollback_records()? {
            set.extend(record.layers.iter().map(|l| layer_dir_name(l)));
            let manifest = serde_json::from_slice::<oci_client::manifest::OciImageManifest>(&bytes)
                .with_context(|| {
                    format!(
                        "the manifest kept for a rollback of {} cannot be read",
                        record.name
                    )
                })?;
            set.insert(layer_dir_name(&manifest.config.digest));
        }
        Ok(set)
    }

    /// Deletes compressed blobs no image references and no pull in flight holds.
    /// Returns the digests removed.
    pub fn gc_blobs(&self) -> Result<Vec<String>> {
        let referenced = self.referenced_blobs()?;
        let mut removed = Vec::new();
        if !self.blobs_dir().is_dir() {
            return Ok(removed);
        }
        let _collecting = self.collecting()?;
        let held = self.held_blobs();
        let dir = self.blobs_dir();
        for entry in fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
            let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
            let file_name = entry.file_name().to_string_lossy().to_string();
            if file_name.starts_with(".part-") {
                // A download or copy that never finished. Downloads run without the lock,
                // so only what nobody touched for an hour goes.
                if is_stale(&entry.path()) {
                    let _ = fs::remove_file(entry.path());
                }
                continue;
            }
            if file_name.starts_with('.')
                || referenced.contains(&file_name)
                || held.contains(&file_name)
            {
                continue;
            }
            fs::remove_file(entry.path()).with_context(|| format!("removing blob {file_name}"))?;
            removed.push(file_name.replacen("sha256-", "sha256:", 1));
        }
        Ok(removed)
    }

    /// Extracts a downloaded blob into the layer store, ready to be used as an overlayfs
    /// lower directory. The extraction happens in a temporary directory that is renamed at
    /// the end, so a crash never leaves a half layer behind. `diff_id`, the digest the
    /// image's config gives the layer's archive, is checked against the blob when the
    /// layer is extracted, and by reading the blob again for a layer found without a
    /// record.
    pub fn import_layer(
        &self,
        digest: &str,
        media_type: &str,
        blob: &Path,
        ownership: Ownership,
        diff_id: Option<&str>,
    ) -> Result<PathBuf> {
        let dest = self.layer_dir(digest, ownership);
        let record = self.diff_id_record(digest, ownership);
        if dest.is_dir() {
            let Some(wanted) = diff_id else {
                return Ok(dest);
            };
            let known = match fs::read_to_string(&record) {
                Ok(text) => text.lines().next().unwrap_or_default().to_string(),
                Err(_) => {
                    let found = layer_diff_id(blob, media_type)
                        .with_context(|| format!("checking layer {digest}"))?;
                    if found == wanted {
                        write_diff_id_record(&record, &found, 1)?;
                    }
                    found
                }
            };
            if known != wanted {
                bail!("layer {digest} holds an archive of digest {known}, and the image's config names {wanted}");
            }
            return Ok(dest);
        }
        let tmp = self
            .layers_dir(ownership)
            .join(format!(".tmp-{}", layer_dir_name(digest)));
        if fs::symlink_metadata(&tmp).is_ok() {
            remove_any(&tmp).with_context(|| format!("removing {}", tmp.display()))?;
        }
        fs::create_dir_all(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        let found = extract_layer_checked(
            blob,
            media_type,
            &tmp,
            WhiteoutMode::OverlayLower,
            ownership,
            diff_id,
        )
        .with_context(|| format!("extracting layer {digest}"))?
        .1;
        write_diff_id_record(&record, &found, LAYER_FORMAT)?;
        fs::rename(&tmp, &dest)
            .with_context(|| format!("moving layer into place at {}", dest.display()))?;
        Ok(dest)
    }

    /// Where the digest of a layer's archive is kept, beside the layer: the layer itself is
    /// an overlayfs lower directory, which a file would show up in.
    fn diff_id_record(&self, digest: &str, ownership: Ownership) -> PathBuf {
        self.layers_dir(ownership)
            .join(".diffids")
            .join(layer_dir_name(digest))
    }

    pub fn record_image(&self, record: &ImageRecord) -> Result<()> {
        crate::reference::validate_entry_name(&record.name)?;
        let path = self.images_dir().join(format!("{}.json", record.name));
        let text = serde_json::to_string_pretty(record)?;
        write_atomically(&path, text.as_bytes())
    }

    pub fn load_image(&self, name: &str) -> Result<Option<ImageRecord>> {
        // A reference ("fedora:44") or a path is no record's name.
        if crate::reference::validate_entry_name(name).is_err() {
            return Ok(None);
        }
        let path = self.images_dir().join(format!("{name}.json"));
        if !path.exists() {
            return Ok(None);
        }
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let record: ImageRecord =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        Ok(Some(record.normalize()))
    }

    /// Every record, skipping (with a warning) those this version cannot read.
    pub fn list_images(&self) -> Result<Vec<ImageRecord>> {
        self.read_records(false)
    }

    /// Every record, or an error when one cannot be read: what garbage collection and
    /// the generated files must use, since a record they cannot see is an image they
    /// would treat as gone.
    pub fn list_images_strict(&self) -> Result<Vec<ImageRecord>> {
        self.read_records(true)
    }

    fn read_records(&self, strict: bool) -> Result<Vec<ImageRecord>> {
        let mut out = Vec::new();
        let entries = match fs::read_dir(self.images_dir()) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
            // A listing may go without the records; what collects garbage may not.
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied && !strict => {
                eprintln!("note: run as root to see the backend and source of each image");
                return Ok(out);
            }
            Err(e) => {
                return Err(e).with_context(|| format!("reading {}", self.images_dir().display()))
            }
        };
        for entry in entries {
            let entry =
                entry.with_context(|| format!("reading {}", self.images_dir().display()))?;
            if entry.path().extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let text = fs::read_to_string(entry.path())
                .with_context(|| format!("reading {}", entry.path().display()))?;
            match serde_json::from_str::<ImageRecord>(&text) {
                Ok(r) => out.push(r.normalize()),
                Err(e) if strict => {
                    return Err(e).with_context(|| {
                        format!(
                            "record {} cannot be read; fix or remove it first",
                            entry.path().display()
                        )
                    })
                }
                Err(e) => eprintln!("warning: ignoring {}: {e}", entry.path().display()),
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    pub fn remove_record(&self, name: &str) -> Result<()> {
        crate::reference::validate_entry_name(name)?;
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
        // By reference: the image itself before a machine that was made from it or ran,
        // whose network and settings are its own; of several, the last one pulled, as a
        // tag names the last pull. A reference recorded in another spelling of Docker
        // Hub's names is read again.
        let same = |recorded: &str| {
            recorded == wanted
                || crate::reference::ImageRef::parse(recorded, default_registry)
                    .is_ok_and(|r| r.to_string() == wanted)
        };
        let mut of: Vec<ImageRecord> = self
            .list_images()?
            .into_iter()
            .filter(|r| same(&r.reference))
            .collect();
        of.sort_by_key(|r| {
            (
                !crate::api::disk::image_only(self, r),
                std::cmp::Reverse(r.created),
            )
        });
        Ok(of.into_iter().next())
    }

    /// Deletes layers that no image record references, in each layer store separately:
    /// overlay machines use the root-owned copies, mstack machines the foreign-owned ones
    /// and flat machines none at all. Returns the digests removed.
    pub fn gc_layers(&self) -> Result<Vec<String>> {
        let mut referenced: HashMap<Ownership, HashSet<String>> = HashMap::new();
        let kept = self.rollback_records()?.into_iter().map(|(r, _)| r);
        for record in self.list_images_strict()?.into_iter().chain(kept) {
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
        // The build cache's layers are extracted as root's, as an overlay machine's.
        referenced.entry(Ownership::Root).or_default().extend(
            crate::build::cache::Cache::new(&self.build_cache_dir())
                .blobs()
                .iter()
                .map(|b| layer_dir_name(b)),
        );
        // A build or a pull in flight keeps what it holds.
        let _collecting = self.collecting()?;
        let held = self.held_blobs();
        let mut removed = Vec::new();
        // What goes is moved aside under the lock, which a rename does at once, and deleted
        // after: a layer tree of many files takes long to delete, and the lock is what
        // every start of a machine waits for.
        let mut bins = Vec::new();
        for ownership in [Ownership::Root, Ownership::Foreign] {
            let dir = self.layers_dir(ownership);
            if !dir.is_dir() {
                continue;
            }
            let used = referenced.remove(&ownership).unwrap_or_default();
            let mut bin: Option<PathBuf> = None;
            for entry in fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
                let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
                let file_name = entry.file_name().to_string_lossy().to_string();
                // An extraction that never finished; the store is locked, so nobody is
                // writing it now.
                let unfinished = file_name.starts_with(".tmp-");
                if !unfinished
                    && (file_name.starts_with('.')
                        || used.contains(&file_name)
                        || held.contains(&file_name))
                {
                    continue;
                }
                let into = match &bin {
                    Some(bin) => bin.clone(),
                    None => {
                        let made = dir.join(format!(".trash-{}", unique_suffix()));
                        fs::create_dir(&made)
                            .with_context(|| format!("creating {}", made.display()))?;
                        bins.push(made.clone());
                        bin = Some(made.clone());
                        made
                    }
                };
                fs::rename(entry.path(), into.join(&file_name))
                    .with_context(|| format!("removing layer {file_name}"))?;
                if !unfinished {
                    let _ = fs::remove_file(dir.join(".diffids").join(&file_name));
                    removed.push(file_name.replacen("sha256-", "sha256:", 1));
                }
            }
        }
        delete_later(bins);
        Ok(removed)
    }

    fn holds_lock(&self, arg: FlockArg) -> Result<HoldsLock> {
        use std::os::unix::fs::OpenOptionsExt;
        let path = self.root.join(".holds");
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        match Flock::lock(file, arg) {
            Ok(lock) => Ok(HoldsLock(lock)),
            Err((_, errno)) => bail!("locking {}: {errno}", path.display()),
        }
    }

    /// Keeps a collector waiting while a hold is made and what it holds is looked at.
    pub fn holds_still(&self) -> Result<HoldsLock> {
        self.holds_lock(FlockArg::LockShared)
    }

    fn collecting(&self) -> Result<HoldsLock> {
        self.holds_lock(FlockArg::LockExclusive)
    }

    /// Deletes what a collection moved aside, and the rollbacks let go, that a stop of
    /// the service cut short.
    pub fn sweep_trash(&self) {
        let places = [
            (self.layers_dir(Ownership::Root), ".trash-"),
            (self.layers_dir(Ownership::Foreign), ".trash-"),
            (self.root.join("rollback"), ".gone-"),
        ];
        for (dir, prefix) in places {
            let Ok(entries) = fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().starts_with(prefix) {
                    let _ = remove_tree(entry.path());
                }
            }
        }
    }
}

/// Root's alone.
pub const PRIVATE: u32 = 0o700;
/// Root's, and passable by anyone who knows a name inside.
pub const PASSAGE: u32 = 0o711;

/// Creates a directory with `mode`, its missing parents too.
/// A directory of root's alone, its parents too.
pub fn create_private_dir(dir: &str) -> Result<()> {
    create_dir_with_mode(Path::new(dir), PRIVATE)
}

fn create_dir_with_mode(dir: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(mode)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))
}

/// Takes from `path` what `mode` does not allow; one that does not exist is fine.
pub fn restrict(path: &Path, mode: u32) -> Result<()> {
    let current = match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => meta.mode() & 0o7777,
        Ok(_) => return Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    if current & !mode != 0 {
        fs::set_permissions(path, fs::Permissions::from_mode(current & mode))
            .with_context(|| format!("restricting {}", path.display()))?;
    }
    Ok(())
}

/// Gives a directory exactly PASSAGE: less would keep an mstack machine from its files.
fn passable(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() && meta.mode() & 0o7777 != PASSAGE => {
            fs::set_permissions(path, fs::Permissions::from_mode(PASSAGE))
                .with_context(|| format!("setting the mode of {}", path.display()))
        }
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// A machine's own directory below the state directory: private once it holds the
/// writable layer of an overlay machine (what the machine wrote, setuid programs among
/// it), passable otherwise.
pub fn machine_dir_mode(dir: &Path) -> u32 {
    if dir.join("upper").exists() {
        PRIVATE
    } else {
        PASSAGE
    }
}

/// Writes a file through a temporary name in the same directory and a rename, so that
/// a crash never leaves a half-written record behind.
pub fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let tmp = path.with_file_name(format!(".tmp-{name}.{}", unique_suffix()));
    // On disk before the name: a record renamed into place but still in the page cache
    // at a power loss comes back empty, and an empty record stops every start.
    let written = File::create(&tmp).and_then(|mut file| {
        io::Write::write_all(&mut file, bytes)?;
        file.sync_all()
    });
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("writing {}", tmp.display()));
    }
    fs::rename(&tmp, path).with_context(|| format!("moving {} into place", path.display()))?;
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        if let Ok(dir) = File::open(dir) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

/// A suffix for temporary names that repeats neither inside this process (the service
/// runs several downloads and writes at once) nor across processes.
pub fn unique_suffix() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!(
        "{}.{}",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    )
}

/// Whether nobody touched `path` for an hour: a marker or a part file left by a process
/// that died.
fn is_stale(path: &Path) -> bool {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|t| t.elapsed().map(|e| e > STALE_MARKER).unwrap_or(false))
        .unwrap_or(false)
}

/// Whether the process that named a file with `unique_suffix` ("<prefix><pid>.<n>")
/// still runs; a name that says no PID reads as gone.
fn writer_alive(name: &str) -> bool {
    name.rsplit_once('.')
        .and_then(|(rest, _)| rest.rsplit(|c: char| !c.is_ascii_digit()).next())
        .and_then(|pid| pid.parse::<u32>().ok())
        .is_some_and(|pid| Path::new(&format!("/proc/{pid}")).exists())
}

/// A digest as the store accepts it for a path component: sha256 and 64 hex digits.
pub fn validate_digest(digest: &str) -> Result<()> {
    match digest.strip_prefix("sha256:") {
        Some(hex)
            if hex.len() == 64
                && hex
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()) =>
        {
            Ok(())
        }
        _ => bail!("{digest:?} is not a sha256 digest"),
    }
}

/// Fails early, with an explanation, when nothing can be created below `dir`. The classic
/// case is a btrfs "empty subvolume" placeholder: after booting from a snapshot of the root
/// subvolume, nested subvolumes such as /var/lib/machines turn into empty, immutable
/// directories with inode 2.
pub fn check_writable(dir: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    // One probe per call: two pulls at once must not remove each other's. A probe under
    // the fixed name, left by a crash, goes.
    let _ = fs::remove_dir(dir.join(".nspawn-write-test"));
    let probe = dir.join(format!(".nspawn-write-test-{}", unique_suffix()));
    match fs::create_dir(&probe) {
        Ok(()) => {
            let _ = fs::remove_dir(&probe);
            Ok(())
        }
        // Left by a process of the same PID that crashed: the directory takes new ones.
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
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

/// What a whiteout entry hides: the lower layers' entries of its directory (the opaque
/// marker), or one name of it.
#[derive(Debug, PartialEq, Eq)]
enum Whiteout {
    Opaque,
    File(Vec<u8>),
}

/// The whiteout an entry's base name is, if it is one.
fn whiteout_of(base: &[u8]) -> Option<Whiteout> {
    if base == b".wh..wh..opq" {
        return Some(Whiteout::Opaque);
    }
    base.strip_prefix(b".wh.")
        .map(|hidden| Whiteout::File(hidden.to_vec()))
}

/// An entry's name inside the layer, as the reference takes it: up to its first NUL (a
/// long name padded with more than one), cleaned with ".." stopping at the root, with
/// no leading slash (ko writes /ko-app/PROGRAM). Empty for the root itself.
fn entry_name(raw: &[u8]) -> Vec<u8> {
    let raw = raw.split(|&b| b == 0).next().unwrap_or_default();
    let cleaned = lexical::clean(&lexical::join(&[b"/", raw]));
    cleaned.strip_prefix(b"/").unwrap_or(&cleaned).to_vec()
}

/// Where an entry named `name` goes: its parent resolved inside `root` as RootPath
/// resolves it (a symlink's absolute target starting again at `root`, ".."
/// never above it, a missing component a plain name), its last component not.
fn in_root(root: &[u8], name: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
    let (parent, base) = lexical::split(name);
    let parent = if parent.is_empty() {
        root.to_vec()
    } else {
        root_path_bytes(root, &lexical::join(&[b"/", parent]))?
    };
    let dst = lexical::join(&[&parent, base]);
    Ok((parent, dst))
}

/// The source of a hard link inside `root`, as hardlinkRootPath takes it: its parent
/// resolved inside the root, and the root itself for a name that climbs out of it.
fn hardlink_source(root: &[u8], linkname: &[u8]) -> Result<Vec<u8>> {
    let linkname = linkname.split(|&b| b == 0).next().unwrap_or_default();
    let (parent, base) = lexical::split(linkname);
    let parent = root_path_bytes(root, parent)?;
    let source = lexical::join(&[&parent, base]);
    let inside = source == root
        || source
            .strip_prefix(root)
            .is_some_and(|rest| rest.first() == Some(&b'/'));
    Ok(if inside { source } else { root.to_vec() })
}

fn open_decompressed(blob: &Path, media_type: &str) -> Result<Box<dyn Read>> {
    use crate::build::untar::{detect, Compression};
    let file = File::open(blob).with_context(|| format!("opening {}", blob.display()))?;
    let mut reader = BufReader::with_capacity(1 << 20, file);
    let mt = media_type.to_ascii_lowercase();
    let layer = ["tar", "+gzip", ".gzip", "+gz", "+zstd", ".zstd"]
        .iter()
        .any(|end| mt.ends_with(end));
    if !layer {
        bail!("unsupported layer media type {media_type}");
    }
    // OCI's uncompressed type is read as it is; for the others the stream's magic says
    // what it is, as the reference's DecompressStream tells it, since images label
    // layers wrongly (Docker's diff.tar type holding gzip, an empty blob under +gzip).
    if mt.ends_with(".v1.tar") {
        return Ok(Box::new(reader));
    }
    let head = io::BufRead::fill_buf(&mut reader)
        .with_context(|| format!("reading {}", blob.display()))?;
    Ok(match detect(head) {
        Compression::None => Box::new(reader),
        Compression::Gzip => Box::new(flate2::read::MultiGzDecoder::new(reader)),
        Compression::Zstd => Box::new(zstd::stream::read::Decoder::new(reader)?),
        Compression::Bzip2 => Box::new(bzip2::read::MultiBzDecoder::new(reader)),
        Compression::Xz => bail!(
            "layer {} is compressed with xz, which nspawn does not read",
            blob.display()
        ),
    })
}

/// Extracts a layer blob into `target`, as applyNaive of containerd's pkg/archive, the
/// reference here, applies a layer (`WhiteoutMode::Apply`), or with its whiteouts
/// turned into overlayfs's (`WhiteoutMode::OverlayLower`). Every entry lands where its
/// name resolves inside `target`, and whatever is done to it is done there. Returns the
/// number of entries written.
#[cfg(test)]
pub fn extract_layer(
    blob: &Path,
    media_type: &str,
    target: &Path,
    mode: WhiteoutMode,
    ownership: Ownership,
) -> Result<u64> {
    extract_layer_checked(blob, media_type, target, mode, ownership, None).map(|(n, _)| n)
}

/// The format of the layers `extract_layer_checked` makes, kept in their record; a
/// layer found without a record is given 1.
const LAYER_FORMAT: u32 = 2;

fn write_diff_id_record(path: &Path, diff_id: &str, format: u32) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    write_atomically(path, format!("{diff_id}\nformat {format}\n").as_bytes())
}

/// A reader that hashes what is read through it.
struct Hashing<R> {
    inner: R,
    hasher: sha2::Sha256,
}

impl<R: Read> Read for Hashing<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        sha2::Digest::update(&mut self.hasher, &buf[..n]);
        Ok(n)
    }
}

impl<R: Read> Hashing<R> {
    /// What is left after the archive's end, read as the reference reads it: a block after
    /// the first zero one that is not zero too is a broken archive; then the rest to the
    /// end, which makes the decompressor check its trailer. The digest of all of it.
    fn finish(mut self) -> Result<String> {
        let mut block = Vec::with_capacity(512);
        (&mut self).take(512).read_to_end(&mut block)?;
        if block.iter().any(|&b| b != 0) {
            bail!("the layer's archive goes on after its end");
        }
        io::copy(&mut self, &mut io::sink())?;
        Ok(format!(
            "sha256:{}",
            hex::encode(sha2::Digest::finalize(self.hasher))
        ))
    }
}

/// The digest of a layer blob's archive, its diff_id, read through without extracting.
pub fn layer_diff_id(blob: &Path, media_type: &str) -> Result<String> {
    let mut reader = Hashing {
        inner: open_decompressed(blob, media_type)?,
        hasher: <sha2::Sha256 as sha2::Digest>::new(),
    };
    io::copy(&mut reader, &mut io::sink())?;
    Ok(format!(
        "sha256:{}",
        hex::encode(sha2::Digest::finalize(reader.hasher))
    ))
}

/// `extract_layer`, which also gives the digest of the layer's archive and fails when
/// it is not `diff_id`.
pub fn extract_layer_checked(
    blob: &Path,
    media_type: &str,
    target: &Path,
    mode: WhiteoutMode,
    ownership: Ownership,
    diff_id: Option<&str>,
) -> Result<(u64, String)> {
    use crate::build::untar::{implied_directories, times, Fields};
    use crate::build::walk::{bytes_path, path_bytes};
    let reader = Hashing {
        inner: open_decompressed(blob, media_type)?,
        hasher: <sha2::Sha256 as sha2::Digest>::new(),
    };
    let root_user = nix::unistd::geteuid().is_root();
    if ownership == Ownership::Foreign && !root_user {
        bail!("shifting a layer into the foreign UID range needs root");
    }
    let mut archive = tar::Archive::new(reader);
    archive.set_preserve_permissions(true);
    // Extended attributes are applied by hand so that the ones overlayfs interprets never
    // come from an image; see `foreign_xattr`.
    archive.set_unpack_xattrs(false);
    archive.set_preserve_ownerships(root_user);
    archive.set_overwrite(true);
    let root = lexical::clean(path_bytes(target));
    let mut count = 0u64;
    // Set once every entry is there: an entry made inside a directory changes its time,
    // and a build's later layers carry the times of what they change.
    let mut dirs: Vec<(Vec<u8>, crate::build::untar::Times)> = Vec::new();
    // What this layer wrote, and every directory above it: a whiteout of this layer hides
    // what the layers below have, never what the layer itself holds.
    let mut written: HashSet<Vec<u8>> = HashSet::new();
    let mut above: HashSet<Vec<u8>> = HashSet::new();
    let mut opaque: HashSet<Vec<u8>> = HashSet::new();
    let mut collapsed: Option<Vec<u8>> = None;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let kind = entry.header().entry_type();
        // Headers of the next entry or of the whole archive make nothing; the tar crate
        // hands a global header back, and those of a format it does not know.
        if kind.is_pax_global_extensions()
            || kind.is_pax_local_extensions()
            || kind.is_gnu_longname()
            || kind.is_gnu_longlink()
        {
            continue;
        }
        let records = pax_records(&mut entry)?;
        let raw = entry.path_bytes().into_owned();
        let name = entry_name(&raw);
        // The layer's root is the target itself, which no entry replaces or changes.
        if name.is_empty() {
            continue;
        }
        let header = entry.header();
        let typeflag = header.as_bytes()[156];
        // The legacy regular type with a trailing slash is a directory, as archive/tar
        // reads it.
        let is_dir = kind.is_dir() || (typeflag == 0 && raw.ends_with(b"/"));
        // Malformed numeric fields are treated as root.
        let uid = header.uid().unwrap_or(0);
        let gid = header.gid().unwrap_or(0);
        let file_mode = header.mode().unwrap_or(0o644) & 0o7777;
        let times = times(header, &Fields::of(header)?, &records)?;
        let (parent, dst) = in_root(&root, &name)?;
        if parent != root {
            implied_directories(&root, &dst)?;
        }
        let shown = || String::from_utf8_lossy(&name).into_owned();
        let base = lexical::base(&name);
        if let Some(whiteout) = whiteout_of(&base) {
            match whiteout {
                Whiteout::Opaque => {
                    if opaque.insert(parent.clone()) {
                        match mode {
                            WhiteoutMode::Apply => remove_unwritten(&parent, &written, &above)?,
                            WhiteoutMode::OverlayLower => {
                                let dir = bytes_path(&parent);
                                xattr::set(&dir, "trusted.overlay.opaque", b"y").with_context(
                                    || format!("marking {} as opaque", dir.display()),
                                )?;
                            }
                        }
                    }
                }
                Whiteout::File(hidden) => {
                    if hidden.is_empty() || hidden == b"." || hidden == b".." {
                        bail!("invalid whiteout name {} in layer", shown());
                    }
                    let path = lexical::join(&[&parent, &hidden]);
                    let node = bytes_path(&path);
                    let there = fs::symlink_metadata(&node).is_ok();
                    let own = match mode {
                        WhiteoutMode::Apply => written.contains(&path) || above.contains(&path),
                        // The target holds this layer alone.
                        WhiteoutMode::OverlayLower => there,
                    };
                    if own {
                        eprintln!(
                            "warning: keeping {} in layer: its whiteout in the same layer hides only what the layers below have",
                            node.strip_prefix(target).unwrap_or(&node).display()
                        );
                    } else if mode == WhiteoutMode::Apply {
                        remove_any(&node)?;
                    } else {
                        mknod(&node, SFlag::S_IFCHR, FileMode::empty(), 0)
                            .with_context(|| format!("creating whiteout {}", node.display()))?;
                        if ownership == Ownership::Foreign {
                            let owner = Some(FOREIGN_UID_BASE);
                            std::os::unix::fs::lchown(&node, owner, owner).with_context(|| {
                                format!("shifting ownership of {}", node.display())
                            })?;
                        }
                    }
                }
            }
            continue;
        }
        let dest = bytes_path(&dst);
        // What is there goes, unless both are directories, which merge.
        if let Ok(meta) = fs::symlink_metadata(&dest) {
            if !(meta.is_dir() && is_dir) {
                remove_any(&dest)?;
            }
        }
        let made = if is_dir {
            match fs::create_dir(&dest) {
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                r => r.with_context(|| format!("creating {}", dest.display()))?,
            }
            if root_user {
                std::os::unix::fs::lchown(&dest, Some(uid as u32), Some(gid as u32))
                    .with_context(|| format!("owning {}", dest.display()))?;
            }
            fs::set_permissions(&dest, fs::Permissions::from_mode(file_mode))
                .with_context(|| format!("setting the mode of {}", dest.display()))?;
            true
        } else if kind.is_hard_link() {
            let linkname = entry
                .link_name_bytes()
                .with_context(|| format!("hard link {} names nothing", shown()))?;
            let source = bytes_path(&hardlink_source(&root, &linkname)?);
            // linkat without AT_SYMLINK_FOLLOW: a symlink source is linked, not its target.
            fs::hard_link(&source, &dest)
                .with_context(|| format!("linking {} to {}", dest.display(), source.display()))?;
            // The inode takes the link's owner and mode too, as the reference gives them.
            if root_user {
                std::os::unix::fs::lchown(&dest, Some(uid as u32), Some(gid as u32))
                    .with_context(|| format!("owning {}", dest.display()))?;
            }
            if !fs::symlink_metadata(&dest).is_ok_and(|m| m.file_type().is_symlink()) {
                fs::set_permissions(&dest, fs::Permissions::from_mode(file_mode))
                    .with_context(|| format!("setting the mode of {}", dest.display()))?;
            }
            true
        } else if kind.is_symlink() {
            let target_name = entry
                .link_name_bytes()
                .with_context(|| format!("symlink {} names nothing", shown()))?;
            let target_name = target_name.split(|&b| b == 0).next().unwrap_or_default();
            std::os::unix::fs::symlink(bytes_path(target_name), &dest)
                .with_context(|| format!("creating {}", dest.display()))?;
            if root_user {
                std::os::unix::fs::lchown(&dest, Some(uid as u32), Some(gid as u32))
                    .with_context(|| format!("owning {}", dest.display()))?;
            }
            true
        } else if kind.is_fifo() || kind.is_character_special() || kind.is_block_special() {
            make_node(&dest, kind, file_mode, entry.header(), root_user)?
        } else {
            // Regular, contiguous and sparse files, and types the tar crate takes as
            // regular files: its unpack writes them at the place resolved above.
            entry
                .unpack(&dest)
                .with_context(|| format!("unpacking {}", dest.display()))?;
            true
        };
        if !made {
            continue;
        }
        let xattrs = entry_xattrs(&records);
        apply_xattrs(&dest, &xattrs)?;
        if ownership == Ownership::Foreign {
            if (uid >= 0x10000 || gid >= 0x10000) && collapsed.is_none() {
                collapsed = Some(name.clone());
            }
            std::os::unix::fs::lchown(
                &dest,
                Some(foreign_id(uid as u32)),
                Some(foreign_id(gid as u32)),
            )
            .with_context(|| format!("shifting ownership of {}", dest.display()))?;
            // chown clears setuid/setgid bits and file capabilities, for root too.
            if !fs::symlink_metadata(&dest).is_ok_and(|m| m.file_type().is_symlink()) {
                fs::set_permissions(&dest, fs::Permissions::from_mode(file_mode))
                    .with_context(|| format!("restoring the mode of {}", dest.display()))?;
                if let Some((_, caps)) = xattrs.iter().find(|(n, _)| n == "security.capability") {
                    xattr::set(&dest, "security.capability", caps).with_context(|| {
                        format!("restoring the capabilities of {}", dest.display())
                    })?;
                }
            }
        }
        if is_dir {
            dirs.push((name, times));
        } else {
            times.utimens(&dst)?;
        }
        let mut up = lexical::dir(&dst);
        while up.len() > root.len() && above.insert(up.clone()) {
            up = lexical::dir(&up);
        }
        written.insert(dst);
        count += 1;
    }
    // In the order of the entries, a directory given twice taking its last time, each
    // resolved inside the target again since later entries may have put a symlink on
    // its way. One that a later entry took away has none to take.
    for (name, times) in dirs {
        let path = root_path_bytes(&root, &lexical::join(&[b"/", &name]))?;
        if fs::symlink_metadata(bytes_path(&path)).is_ok_and(|m| m.is_dir()) {
            times.utimens(&path)?;
        }
    }
    if let Some(name) = collapsed {
        eprintln!(
            "warning: {} and others in this layer are owned by ids above 65535, which a machine's user namespace does not map: they are nobody's there",
            String::from_utf8_lossy(&name)
        );
    }
    if ownership == Ownership::Foreign {
        // Directories made for entries without a parent entry, and the target itself,
        // are still root's.
        shift_remaining(target)?;
    }
    let found = archive.into_inner().finish()?;
    if let Some(wanted) = diff_id {
        if found != wanted {
            bail!(
                "the layer's archive has the digest {found}, and the image's config names {wanted}"
            );
        }
    }
    Ok((count, found))
}

/// The PAX records of an entry, the last of a key winning. A record the tar crate cannot
/// read is left out and the rest taken: images in wide use carry such records.
fn pax_records<R: Read>(
    entry: &mut tar::Entry<'_, R>,
) -> Result<std::collections::BTreeMap<Vec<u8>, Vec<u8>>> {
    let mut out = std::collections::BTreeMap::new();
    if let Some(extensions) = entry.pax_extensions()? {
        for extension in extensions.flatten() {
            out.insert(
                extension.key_bytes().to_vec(),
                extension.value_bytes().to_vec(),
            );
        }
    }
    Ok(out)
}

/// What an opaque marker removes in `dir` when a layer is applied on the ones below:
/// everything this layer did not write, its own directories gone through the same way.
fn remove_unwritten(
    dir: &[u8],
    written: &HashSet<Vec<u8>>,
    above: &HashSet<Vec<u8>>,
) -> Result<()> {
    use crate::build::walk::bytes_path;
    use std::os::unix::ffi::OsStrExt;
    let mut pending = vec![dir.to_vec()];
    while let Some(dir) = pending.pop() {
        let path = bytes_path(&dir);
        let children =
            fs::read_dir(&path).with_context(|| format!("reading {}", path.display()))?;
        for child in children {
            let child = child.with_context(|| format!("reading {}", path.display()))?;
            let full = lexical::join(&[&dir, child.file_name().as_bytes()]);
            if written.contains(&full) || above.contains(&full) {
                if child.file_type().is_ok_and(|t| t.is_dir()) {
                    pending.push(full);
                }
            } else {
                remove_any(&child.path())?;
            }
        }
    }
    Ok(())
}

/// A FIFO or device node of a layer, made as it is, owned as the entry says when root
/// makes it. Without root a device cannot be made, nor one the kernel refuses to make:
/// it is left out, as the reference leaves it out unprivileged or in a user namespace.
/// Whether one was made.
fn make_node(
    dest: &Path,
    kind: tar::EntryType,
    mode: u32,
    header: &tar::Header,
    root: bool,
) -> Result<bool> {
    use nix::sys::stat::{makedev, mknod, Mode, SFlag};
    let flag = if kind.is_fifo() {
        SFlag::S_IFIFO
    } else if kind.is_character_special() {
        SFlag::S_IFCHR
    } else {
        SFlag::S_IFBLK
    };
    if flag != SFlag::S_IFIFO && !root {
        return Ok(false);
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    if fs::symlink_metadata(dest).is_ok() {
        remove_any(dest)?;
    }
    let major = u64::from(header.device_major().ok().flatten().unwrap_or(0));
    let minor = u64::from(header.device_minor().ok().flatten().unwrap_or(0));
    match mknod(
        dest,
        flag,
        Mode::from_bits_truncate(mode),
        makedev(major, minor),
    ) {
        Ok(()) => {}
        // Root of a user namespace (a service in a container) may not make a device,
        // and may be kept from a fifo: the reference leaves them out there.
        Err(nix::errno::Errno::EPERM) => {
            eprintln!(
                "warning: skipping {} in layer: it may not be made here",
                dest.display()
            );
            return Ok(false);
        }
        Err(e) => return Err(e).with_context(|| format!("making {}", dest.display())),
    }
    if root {
        let uid = header.uid().unwrap_or(0) as u32;
        let gid = header.gid().unwrap_or(0) as u32;
        std::os::unix::fs::lchown(dest, Some(uid), Some(gid))
            .with_context(|| format!("owning {}", dest.display()))?;
    }
    // mknod takes the umask off the mode.
    fs::set_permissions(dest, fs::Permissions::from_mode(mode))
        .with_context(|| format!("setting the mode of {}", dest.display()))?;
    Ok(true)
}

/// Attributes an image does not get to set. overlayfs reads its own from the layers: a
/// redirect or a metacopy marker planted by an image would make a file show up under
/// another path or with another file's data once the layers are stacked (whiteouts are
/// translated by `handle_whiteout` instead). SELinux labels are the host's business,
/// as with the container engines: the policy labels what nspawn extracts, and a label
/// the host does not know would leave the file unlabeled and unusable.
fn foreign_xattr(name: &str) -> bool {
    name.starts_with("trusted.") || name.starts_with("user.overlay.") || name == "security.selinux"
}

/// The extended attributes an entry carries in its PAX records, the foreign ones left out.
fn entry_xattrs(records: &std::collections::BTreeMap<Vec<u8>, Vec<u8>>) -> Vec<(String, Vec<u8>)> {
    let mut xattrs = Vec::new();
    for (key, value) in records {
        let Some(name) = key.strip_prefix(b"SCHILY.xattr.") else {
            continue;
        };
        let Ok(name) = std::str::from_utf8(name) else {
            continue;
        };
        if foreign_xattr(name) {
            continue;
        }
        xattrs.push((name.to_string(), value.clone()));
    }
    xattrs
}

/// Sets the attributes on an unpacked entry. A file system that does not
/// take one (ENOTSUP, or EPERM for user.* on a symlink) is not an error.
fn apply_xattrs(path: &Path, xattrs: &[(String, Vec<u8>)]) -> Result<()> {
    for (name, value) in xattrs {
        match xattr::set(path, name, value) {
            Ok(()) => {}
            Err(e)
                if matches!(
                    e.raw_os_error(),
                    Some(nix::libc::ENOTSUP) | Some(nix::libc::EPERM)
                ) => {}
            Err(e) => {
                return Err(e).with_context(|| format!("setting {name} on {}", path.display()))
            }
        }
    }
    Ok(())
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

fn remove_any(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => remove_tree(path)?,
        Ok(_) => fs::remove_file(path)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

/// Removes a directory tree as `fs::remove_dir_all` does (a link itself, not what it
/// points to), without recursion and with a few descriptors open: a machine's writable
/// layer, a volume or a layer holds a chain of directories as deep as whoever wrote it
/// made it, where `remove_dir_all` recurses once per level and an overflow of the
/// thread's stack ends the whole service. Another file system mounted inside is not
/// entered: the removal stops there with an error. A btrfs subvolume inside, a device of
/// its own on the same mount, is removed as a directory, as `remove_dir_all` removes it.
pub fn remove_tree(path: impl AsRef<Path>) -> io::Result<()> {
    use nix::errno::Errno;
    use nix::fcntl::{open, openat, AtFlags, OFlag};
    use nix::sys::stat::{fstat, fstatat};
    use nix::unistd::{unlinkat, UnlinkatFlags};
    use std::ffi::CString;

    /// A directory below the top: who it is, its name in its parent, and the names its
    /// parent has still to go through.
    struct Level {
        dev: u64,
        ino: u64,
        name: CString,
        rest: Vec<CString>,
    }

    let path = path.as_ref();
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return fs::remove_file(path);
    }
    if !meta.is_dir() {
        return Err(io::Error::from_raw_os_error(nix::libc::ENOTDIR));
    }
    let changed = || io::Error::other(format!("{} changed while it was removed", path.display()));
    let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
    let top = open(path, flags, FileMode::empty())?;
    let top_st = fstat(&top)?;
    let top_mount = crate::volmount::mount_id_at(&top, c"")?;
    let mut levels: Vec<Level> = Vec::new();
    let mut cur = top.try_clone()?;
    let mut names = entry_names(&cur)?;
    loop {
        if let Some(name) = names.pop() {
            let st = match fstatat(&cur, name.as_c_str(), AtFlags::AT_SYMLINK_NOFOLLOW) {
                Ok(st) => st,
                Err(Errno::ENOENT) => continue,
                Err(e) => return Err(e.into()),
            };
            if st.st_mode & nix::libc::S_IFMT != nix::libc::S_IFDIR {
                match unlinkat(&cur, name.as_c_str(), UnlinkatFlags::NoRemoveDir) {
                    Ok(()) | Err(Errno::ENOENT) => {}
                    Err(e) => return Err(e.into()),
                }
                continue;
            }
            // Without a mount ID to tell, another device is taken for a mount point.
            if st.st_dev != top_st.st_dev
                && (top_mount.is_none()
                    || crate::volmount::mount_id_at(&cur, name.as_c_str())? != top_mount)
            {
                return Err(io::Error::other(format!(
                    "{} holds a mount point; unmount it first",
                    path.display()
                )));
            }
            let child = openat(&cur, name.as_c_str(), flags, FileMode::empty())?;
            let child_st = fstat(&child)?;
            if (child_st.st_dev, child_st.st_ino) != (st.st_dev, st.st_ino) {
                return Err(changed());
            }
            let below = entry_names(&child)?;
            levels.push(Level {
                dev: child_st.st_dev,
                ino: child_st.st_ino,
                name,
                rest: std::mem::replace(&mut names, below),
            });
            cur = child;
        } else {
            let Some(level) = levels.pop() else { break };
            // The parent is opened again through "..", since a descriptor kept for each
            // level runs out on a deep chain; its identity tells that it is the one left.
            let parent = match levels.last() {
                None => top.try_clone()?,
                Some(up) => {
                    let parent = openat(&cur, c"..", flags, FileMode::empty())?;
                    let st = fstat(&parent)?;
                    if (st.st_dev, st.st_ino) != (up.dev, up.ino) {
                        return Err(changed());
                    }
                    parent
                }
            };
            match unlinkat(&parent, level.name.as_c_str(), UnlinkatFlags::RemoveDir) {
                Ok(()) | Err(Errno::ENOENT) => {}
                Err(e) => return Err(e.into()),
            }
            cur = parent;
            names = level.rest;
        }
    }
    drop(cur);
    drop(top);
    fs::remove_dir(path)
}

/// The names in a directory, read through a description of their own.
fn entry_names(dir: &std::os::fd::OwnedFd) -> io::Result<Vec<std::ffi::CString>> {
    use nix::fcntl::OFlag;
    let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC;
    let mut listed = nix::dir::Dir::openat(dir, c".", flags, FileMode::empty())?;
    let mut names = Vec::new();
    for entry in listed.iter() {
        let name = entry?.file_name().to_owned();
        if name.as_bytes() != b"." && name.as_bytes() != b".." {
            names.push(name);
        }
    }
    Ok(names)
}

/// flock() on the store's lock file: forever, or until `wait` has passed.
fn lock_blocking(root: &Path, wait: Option<std::time::Duration>) -> Result<StoreLock> {
    fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
    let path = root.join(".lock");
    let open = || {
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))
    };
    let Some(wait) = wait else {
        return match Flock::lock(open()?, FlockArg::LockExclusive) {
            Ok(lock) => Ok(StoreLock(lock)),
            Err((_, errno)) => bail!("locking {}: {errno}", path.display()),
        };
    };
    let deadline = std::time::Instant::now() + wait;
    loop {
        match Flock::lock(open()?, FlockArg::LockExclusiveNonblock) {
            Ok(lock) => return Ok(StoreLock(lock)),
            Err((_, nix::errno::Errno::EWOULDBLOCK)) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            Err((_, nix::errno::Errno::EWOULDBLOCK)) => {
                bail!(
                    "the store is busy (another nspawn command, a pull perhaps, holds {})",
                    path.display()
                )
            }
            Err((_, errno)) => bail!("locking {}: {errno}", path.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entrypoint_given_drops_the_images_command() {
        let record = |json: serde_json::Value| -> ImageRecord {
            let mut base = serde_json::json!({
                "name": "web", "reference": "r", "manifest_digest": "sha256:a", "layers": [],
                "backend": "overlay", "created": 1,
                "run": { "entrypoint": ["/docker-entrypoint.sh"], "cmd": ["nginx", "-g", "daemon off;"] },
            });
            for (k, v) in json.as_object().unwrap() {
                base[k] = v.clone();
            }
            serde_json::from_value::<ImageRecord>(base)
                .unwrap()
                .normalize()
        };
        assert_eq!(
            record(serde_json::json!({})).effective_command(),
            ["/docker-entrypoint.sh", "nginx", "-g", "daemon off;"]
        );
        assert_eq!(
            record(serde_json::json!({ "entrypoint": ["/bin/sh"], "cmd": [] })).effective_command(),
            ["/bin/sh"]
        );
        assert_eq!(
            record(serde_json::json!({ "entrypoint": ["/bin/sh"], "cmd": ["-c", "id"] }))
                .effective_command(),
            ["/bin/sh", "-c", "id"]
        );
        assert_eq!(
            record(serde_json::json!({ "cmd": ["nginx", "-t"] })).effective_command(),
            ["/docker-entrypoint.sh", "nginx", "-t"]
        );
        // Its parts: Cmd is none after an entrypoint given alone, not the image's.
        let alone = record(serde_json::json!({ "entrypoint": ["/bin/sh"], "cmd": [] }));
        assert_eq!(alone.effective_entrypoint(), ["/bin/sh"]);
        assert!(alone.effective_cmd().is_empty());
        // --entrypoint "" alone: nothing to run, which create and start refuse.
        let nothing = record(serde_json::json!({ "entrypoint": [], "cmd": [] }));
        assert!(nothing.effective_command().is_empty());
        // A record with an entrypoint and no cmd at all runs the image's command after
        // it.
        assert_eq!(
            record(serde_json::json!({ "entrypoint": ["/bin/sh"] })).effective_command(),
            ["/bin/sh", "nginx", "-g", "daemon off;"]
        );
        assert_eq!(
            record(serde_json::json!({ "entrypoint": [] })).effective_command(),
            ["nginx", "-g", "daemon off;"]
        );
        // Without an entrypoint of its own an empty cmd is none.
        assert_eq!(
            record(serde_json::json!({ "cmd": [] })).effective_command(),
            ["/docker-entrypoint.sh", "nginx", "-g", "daemon off;"]
        );
        let image = record(serde_json::json!({}));
        assert_eq!(image.effective_entrypoint(), ["/docker-entrypoint.sh"]);
        assert_eq!(image.effective_cmd(), ["nginx", "-g", "daemon off;"]);
    }
    use std::io::Cursor;

    /// Real layers carry an owner in every header; a header built here would leave the
    /// field blank, and a reader running as root (the service does, and so does
    /// dpkg-buildpackage) asks for it while preserving ownership.
    fn header_owner(header: &mut tar::Header) {
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
    }

    fn tar_with(entries: &[(&str, Option<&str>)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, content) in entries {
            let mut header = tar::Header::new_gnu();
            header_owner(&mut header);
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
    fn overlay_attributes_of_an_image_are_not_applied() {
        assert!(foreign_xattr("trusted.overlay.redirect"));
        assert!(foreign_xattr("trusted.overlay.metacopy"));
        assert!(foreign_xattr("user.overlay.opaque"));
        assert!(foreign_xattr("security.selinux"));
        assert!(!foreign_xattr("security.capability"));
        assert!(!foreign_xattr("user.overlayish"));
        let tmp = tempfile::tempdir().unwrap();
        // user.* attributes need a file system that stores them.
        let probe = tmp.path().join("probe");
        fs::write(&probe, "").unwrap();
        if xattr::set(&probe, "user.probe", b"1").is_err() {
            return;
        }
        let mut builder = tar::Builder::new(Vec::new());
        builder
            .append_pax_extensions([
                (
                    "SCHILY.xattr.user.overlay.redirect",
                    b"/etc/shadow".as_slice(),
                ),
                ("SCHILY.xattr.user.keep", b"yes".as_slice()),
            ])
            .unwrap();
        let mut header = tar::Header::new_gnu();
        header_owner(&mut header);
        header.set_size(2);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_cksum();
        builder
            .append_data(&mut header, "file", Cursor::new(b"hi".as_slice()))
            .unwrap();
        let blob = tmp.path().join("layer.tar");
        fs::write(&blob, builder.into_inner().unwrap()).unwrap();
        let target = tmp.path().join("root");
        fs::create_dir(&target).unwrap();
        extract_layer(
            &blob,
            "application/vnd.oci.image.layer.v1.tar",
            &target,
            WhiteoutMode::Apply,
            Ownership::Root,
        )
        .unwrap();
        let file = target.join("file");
        assert_eq!(fs::read(&file).unwrap(), b"hi");
        assert_eq!(
            xattr::get(&file, "user.keep").unwrap(),
            Some(b"yes".to_vec())
        );
        assert_eq!(xattr::get(&file, "user.overlay.redirect").unwrap(), None);
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
        // Only procfs refuses a directory there; an empty /proc of a chroot would not.
        let procfs = nix::sys::statfs::statfs("/proc")
            .map(|s| s.filesystem_type() == nix::sys::statfs::PROC_SUPER_MAGIC)
            .unwrap_or(false);
        if procfs {
            assert!(check_writable(Path::new("/proc")).is_err());
        }
    }

    #[test]
    fn classifies_whiteouts() {
        assert_eq!(whiteout_of(b".wh.b"), Some(Whiteout::File(b"b".to_vec())));
        assert_eq!(whiteout_of(b".wh..wh..opq"), Some(Whiteout::Opaque));
        assert_eq!(whiteout_of(b"normal"), None);
        // On bytes: the whiteout of a name that is not UTF-8 is still one.
        assert_eq!(
            whiteout_of(b".wh.caf\xe9.txt"),
            Some(Whiteout::File(b"caf\xe9.txt".to_vec()))
        );
    }

    #[test]
    fn entry_names_are_taken_inside_the_layer() {
        for (raw, name) in [
            (&b"usr/bin/x"[..], &b"usr/bin/x"[..]),
            (b"/ko-app/crane", b"ko-app/crane"),
            (b"./etc//passwd", b"etc/passwd"),
            (b"usr/lib/../bin/tool", b"usr/bin/tool"),
            (b"../../etc/x", b"etc/x"),
            (b"dir/", b"dir"),
            (b"dir/file\0\0", b"dir/file"),
            (b"./", b""),
            (b"/", b""),
            (b"..", b""),
        ] {
            assert_eq!(entry_name(raw), name, "{}", String::from_utf8_lossy(raw));
        }
    }

    #[test]
    fn entries_keep_their_times() {
        let tmp = tempfile::tempdir().unwrap();
        let mut builder = tar::Builder::new(Vec::new());
        let mut dir = tar::Header::new_gnu();
        header_owner(&mut dir);
        dir.set_mode(0o755);
        dir.set_entry_type(tar::EntryType::Directory);
        dir.set_size(0);
        dir.set_mtime(1_000_000);
        builder
            .append_data(&mut dir, "usr/", std::io::empty())
            .unwrap();
        let mut file = tar::Header::new_gnu();
        header_owner(&mut file);
        file.set_mode(0o644);
        file.set_entry_type(tar::EntryType::Regular);
        file.set_size(2);
        file.set_mtime(0);
        builder
            .append_data(&mut file, "usr/zero", Cursor::new(b"hi".as_slice()))
            .unwrap();
        let blob = tmp.path().join("layer.tar");
        fs::write(&blob, builder.into_inner().unwrap()).unwrap();
        let target = tmp.path().join("layer");
        fs::create_dir(&target).unwrap();
        extract_layer(
            &blob,
            "application/vnd.oci.image.layer.v1.tar",
            &target,
            WhiteoutMode::Apply,
            Ownership::Root,
        )
        .unwrap();
        let time = |p: &str| fs::symlink_metadata(target.join(p)).unwrap().mtime();
        assert_eq!(time("usr"), 1_000_000, "made inside, it keeps its time");
        assert_eq!(time("usr/zero"), 0, "0 stays 0");
    }

    #[test]
    fn a_later_symlink_on_a_directory_s_path_takes_no_time_out_of_the_layer() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        fs::create_dir_all(outside.join("shadow")).unwrap();
        let old = nix::sys::time::TimeVal::new(5_000_000, 0);
        nix::sys::stat::utimes(&outside.join("shadow"), &old, &old).unwrap();
        let mut builder = tar::Builder::new(Vec::new());
        let mut add = |path: &str, kind: tar::EntryType, mtime: u64, link: Option<&Path>| {
            let mut header = tar::Header::new_gnu();
            header_owner(&mut header);
            header.set_mode(if kind == tar::EntryType::Directory {
                0o755
            } else {
                0o644
            });
            header.set_entry_type(kind);
            header.set_size(0);
            header.set_mtime(mtime);
            match link {
                Some(to) => builder.append_link(&mut header, path, to).unwrap(),
                None => builder
                    .append_data(&mut header, path, std::io::empty())
                    .unwrap(),
            }
        };
        // The directory a later symlink stands in for, and its time set at the end.
        add("x/", tar::EntryType::Directory, 1_000_000, None);
        add("x/shadow/", tar::EntryType::Directory, 0, None);
        add(".wh.x", tar::EntryType::Regular, 0, None);
        add("x", tar::EntryType::Symlink, 0, Some(&outside));
        // A directory given twice takes its last time.
        add("etc/", tar::EntryType::Directory, 1000, None);
        add("etc/", tar::EntryType::Directory, 2000, None);
        let blob = tmp.path().join("layer.tar");
        fs::write(&blob, builder.into_inner().unwrap()).unwrap();
        let target = tmp.path().join("layer");
        fs::create_dir(&target).unwrap();
        extract_layer(
            &blob,
            "application/vnd.oci.image.layer.v1.tar",
            &target,
            WhiteoutMode::Apply,
            Ownership::Root,
        )
        .unwrap();
        assert!(target
            .join("x")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            fs::metadata(outside.join("shadow")).unwrap().mtime(),
            5_000_000,
            "a time went out of the layer through the symlink"
        );
        assert_eq!(fs::metadata(target.join("etc")).unwrap().mtime(), 2000);
    }

    /// A chain of up to `depth` directories below `top`, made through descriptors since
    /// its paths outgrow PATH_MAX; how deep it got. Some file systems refuse to go that
    /// deep (ENAMETOOLONG), as the containers of the package builds do, and the chain
    /// ends there.
    fn chain(top: &Path, depth: usize) -> usize {
        use nix::fcntl::{open, openat, OFlag};
        use nix::sys::stat::mkdirat;
        let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC;
        let mut dir = open(top, flags, FileMode::empty()).unwrap();
        for made in 0..depth {
            match mkdirat(&dir, "d", FileMode::from_bits_truncate(0o755)) {
                Err(nix::errno::Errno::ENAMETOOLONG) => return made,
                r => r.unwrap(),
            }
            dir = openat(&dir, "d", flags, FileMode::empty()).unwrap();
        }
        depth
    }

    #[test]
    fn a_unit_claims_its_ports_from_its_check_to_its_publish() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(&tmp.path().join("machines"), &tmp.path().join("state"));
        assert!(!store.claims_ports("web"));
        store.claim_ports("web").unwrap();
        assert!(store.claims_ports("web"));
        assert!(!store.claims_ports("other"));
        store.release_ports_claim("web");
        assert!(!store.claims_ports("web"));
        assert!(store.claim_ports("../web").is_err());
        assert!(!store.claims_ports("../web"));
    }

    #[test]
    fn a_tree_deeper_than_a_stack_is_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let top = tmp.path().join("upper");
        fs::create_dir(&top).unwrap();
        let depth = chain(&top, 30_000);
        assert!(depth >= 1000, "only {depth} levels");
        fs::write(top.join("d/f"), "x").unwrap();
        // remove_dir_all overflows a stack of this size a few thousand levels down.
        let removed = std::thread::Builder::new()
            .stack_size(256 << 10)
            .spawn(move || remove_tree(&top).map(|()| top))
            .unwrap()
            .join()
            .unwrap()
            .unwrap();
        assert!(!removed.exists());
    }

    #[test]
    fn removing_a_tree_leaves_what_its_links_point_to() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        fs::create_dir_all(outside.join("sub")).unwrap();
        fs::write(outside.join("sub/keep"), "x").unwrap();
        let top = tmp.path().join("tree");
        fs::create_dir_all(top.join("a/b")).unwrap();
        fs::write(top.join("a/b/f"), "x").unwrap();
        fs::write(top.join("a/g"), "x").unwrap();
        std::os::unix::fs::symlink(&outside, top.join("a/link")).unwrap();
        std::os::unix::fs::symlink(outside.join("sub/keep"), top.join("a/b/file-link")).unwrap();
        remove_tree(&top).unwrap();
        assert!(!top.exists());
        assert!(outside.join("sub/keep").exists());

        // A link named as the tree goes itself, as with remove_dir_all.
        let link = tmp.path().join("named");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        remove_tree(&link).unwrap();
        assert!(link.symlink_metadata().is_err());
        assert!(outside.join("sub/keep").exists());

        let file = tmp.path().join("file");
        fs::write(&file, "x").unwrap();
        assert_eq!(
            remove_tree(&file).unwrap_err().raw_os_error(),
            Some(nix::libc::ENOTDIR)
        );
        assert_eq!(
            remove_tree(tmp.path().join("missing")).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn a_hard_link_to_a_symlink_links_the_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header_owner(&mut header);
        header.set_mode(0o777);
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        builder
            .append_link(&mut header, "usr/lib/libbar.so", "/usr/lib/libfoo.so.1")
            .unwrap();
        let mut header = tar::Header::new_gnu();
        header_owner(&mut header);
        header.set_mode(0o777);
        header.set_entry_type(tar::EntryType::Link);
        header.set_size(0);
        builder
            .append_link(&mut header, "usr/lib/libfoo.so", "usr/lib/libbar.so")
            .unwrap();
        let blob = tmp.path().join("layer.tar");
        fs::write(&blob, builder.into_inner().unwrap()).unwrap();
        let target = tmp.path().join("layer");
        fs::create_dir(&target).unwrap();
        let count = extract_layer(
            &blob,
            "application/vnd.oci.image.layer.v1.tar",
            &target,
            WhiteoutMode::Apply,
            Ownership::Root,
        )
        .unwrap();
        assert_eq!(count, 2);
        let link = target.join("usr/lib/libfoo.so");
        assert_eq!(
            fs::read_link(&link).unwrap(),
            Path::new("/usr/lib/libfoo.so.1")
        );
        let a = fs::symlink_metadata(&link).unwrap();
        let b = fs::symlink_metadata(target.join("usr/lib/libbar.so")).unwrap();
        assert_eq!((a.dev(), a.ino()), (b.dev(), b.ino()));
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
    fn fifos_and_devices_come_out_as_they_are() {
        use std::os::unix::fs::FileTypeExt;
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("root");
        fs::create_dir_all(&target).unwrap();
        let mut builder = tar::Builder::new(Vec::new());
        for (path, kind) in [
            ("run/pipe", tar::EntryType::Fifo),
            ("dev/null2", tar::EntryType::Char),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(kind);
            header.set_mode(0o620);
            header.set_size(0);
            header.set_device_major(1).unwrap();
            header.set_device_minor(3).unwrap();
            builder.append_data(&mut header, path, io::empty()).unwrap();
        }
        let blob = tmp.path().join("layer.tar");
        fs::write(&blob, builder.into_inner().unwrap()).unwrap();
        extract_layer(
            &blob,
            "application/vnd.oci.image.layer.v1.tar",
            &target,
            WhiteoutMode::OverlayLower,
            Ownership::Root,
        )
        .unwrap();
        let pipe = fs::symlink_metadata(target.join("run/pipe")).unwrap();
        assert!(pipe.file_type().is_fifo());
        assert_eq!(pipe.mode() & 0o7777, 0o620);
        // A device takes root; without it, it is left out rather than made a file.
        let device = fs::symlink_metadata(target.join("dev/null2"));
        if nix::unistd::geteuid().is_root() {
            assert!(device.unwrap().file_type().is_char_device());
        } else {
            assert!(device.is_err());
        }
    }

    #[test]
    fn names_climbing_out_stay_inside_the_target() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("root");
        fs::create_dir_all(&target).unwrap();
        // tar::Builder refuses to write ".." paths, so forge the header by hand like an
        // attacker would.
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header_owner(&mut header);
        let evil = b"../escape";
        header.as_gnu_mut().unwrap().name[..evil.len()].copy_from_slice(evil);
        header.set_size(1);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_cksum();
        builder.append(&header, Cursor::new(b"x")).unwrap();
        let mut ok = tar::Header::new_gnu();
        header_owner(&mut ok);
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
        // ".." stops at the layer's root.
        assert_eq!(n, 2);
        assert!(!tmp.path().join("escape").exists());
        assert!(target.join("escape").exists());
        assert!(target.join("ok").exists());
    }

    #[test]
    fn a_pax_record_the_tar_crate_cannot_read_is_left_out() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("root");
        fs::create_dir_all(&target).unwrap();
        let mut builder = tar::Builder::new(Vec::new());
        let records = b"garbage\n20 mtime=1700000000\n";
        let mut pax = tar::Header::new_ustar();
        pax.set_path("PaxHeaders/f").unwrap();
        pax.set_entry_type(tar::EntryType::XHeader);
        pax.set_size(records.len() as u64);
        pax.set_mode(0o644);
        pax.set_cksum();
        builder.append(&pax, Cursor::new(&records[..])).unwrap();
        let mut file = tar::Header::new_ustar();
        header_owner(&mut file);
        file.set_path("f").unwrap();
        file.set_entry_type(tar::EntryType::Regular);
        file.set_size(1);
        file.set_mode(0o644);
        file.set_cksum();
        builder.append(&file, Cursor::new(b"x")).unwrap();
        let blob = tmp.path().join("layer.tar");
        fs::write(&blob, builder.into_inner().unwrap()).unwrap();
        let n = extract_layer(
            &blob,
            "application/vnd.oci.image.layer.v1.tar",
            &target,
            WhiteoutMode::OverlayLower,
            Ownership::Root,
        )
        .unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            fs::metadata(target.join("f")).unwrap().mtime(),
            1_700_000_000
        );
    }

    #[test]
    fn a_layer_is_checked_against_its_diff_id() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let raw = tar_with(&[("f", Some("data"))]);
        let diff_id = crate::layout::sha256_digest(&raw);
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&raw).unwrap();
        let gzipped = gz.finish().unwrap();
        let blob = tmp.path().join("layer.tar.gz");
        fs::write(&blob, &gzipped).unwrap();
        let gzip = "application/vnd.oci.image.layer.v1.tar+gzip";
        assert_eq!(layer_diff_id(&blob, gzip).unwrap(), diff_id);
        let extract = |blob: &Path, wanted: Option<&str>| {
            let target = tmp.path().join(format!("t-{}", unique_suffix()));
            fs::create_dir_all(&target).unwrap();
            extract_layer_checked(
                blob,
                gzip,
                &target,
                WhiteoutMode::OverlayLower,
                Ownership::Root,
                wanted,
            )
        };
        assert_eq!(
            extract(&blob, Some(&diff_id)).unwrap(),
            (1, diff_id.clone())
        );
        let other = format!("sha256:{}", "0".repeat(64));
        assert!(extract(&blob, Some(&other))
            .unwrap_err()
            .to_string()
            .contains("config names"));
        // A gzip trailer that does not match, and data after the archive's end.
        let mut broken = gzipped.clone();
        let at = broken.len() - 8;
        broken[at] ^= 0xff;
        let bad = tmp.path().join("bad.tar.gz");
        fs::write(&bad, &broken).unwrap();
        assert!(extract(&bad, None).is_err());
        let mut after = raw[..raw.len() - 1024].to_vec();
        after.extend_from_slice(&[0u8; 512]);
        after.extend_from_slice(&tar_with(&[("g", Some("more"))]));
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&after).unwrap();
        let trailing = tmp.path().join("after.tar.gz");
        fs::write(&trailing, gz.finish().unwrap()).unwrap();
        assert!(extract(&trailing, None)
            .unwrap_err()
            .to_string()
            .contains("after its end"));

        // The store keeps what it found beside the layer, and checks a layer it reuses.
        let store = Store::new(&tmp.path().join("m"), &tmp.path().join("s"));
        let digest = crate::layout::sha256_digest(&gzipped);
        let dir = store
            .import_layer(&digest, gzip, &blob, Ownership::Root, Some(&diff_id))
            .unwrap();
        assert!(dir.join("f").is_file());
        let record = store.diff_id_record(&digest, Ownership::Root);
        assert_eq!(
            fs::read_to_string(&record).unwrap(),
            format!("{diff_id}\nformat 2\n")
        );
        let e = store
            .import_layer(&digest, gzip, &blob, Ownership::Root, Some(&other))
            .unwrap_err();
        assert!(e.to_string().contains("config names"), "{e}");
        // One a release kept no record of is read from its blob again, not extracted.
        fs::remove_file(&record).unwrap();
        fs::write(dir.join("kept"), "x").unwrap();
        store
            .import_layer(&digest, gzip, &blob, Ownership::Root, Some(&diff_id))
            .unwrap();
        assert!(dir.join("kept").exists());
        assert_eq!(
            fs::read_to_string(&record).unwrap(),
            format!("{diff_id}\nformat 1\n")
        );
    }

    #[test]
    fn names_from_the_root_down_land_inside_the_target() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("root");
        fs::create_dir_all(&target).unwrap();
        // ko's layout: the directory relative, its program absolute, and a bare "/".
        // Then a file and a FIFO for the root, which would take the tree's place.
        let mut builder = tar::Builder::new(Vec::new());
        for (name, kind, data) in [
            (&b"/"[..], tar::EntryType::Directory, &b""[..]),
            (b"ko-app", tar::EntryType::Directory, b""),
            (b"/ko-app/crane", tar::EntryType::Regular, b"#!x"),
            (b"/", tar::EntryType::Regular, b"y"),
            (b"./", tar::EntryType::Fifo, b""),
        ] {
            let mut header = tar::Header::new_gnu();
            header_owner(&mut header);
            header.as_gnu_mut().unwrap().name[..name.len()].copy_from_slice(name);
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_mtime(1_700_000_000);
            header.set_entry_type(kind);
            header.set_cksum();
            builder.append(&header, Cursor::new(data)).unwrap();
        }
        let blob = tmp.path().join("layer.tar");
        fs::write(&blob, builder.into_inner().unwrap()).unwrap();
        for mode in [WhiteoutMode::Apply, WhiteoutMode::OverlayLower] {
            let _ = fs::remove_dir_all(&target);
            fs::create_dir_all(&target).unwrap();
            let n = extract_layer(
                &blob,
                "application/vnd.oci.image.layer.v1.tar",
                &target,
                mode,
                Ownership::Root,
            )
            .unwrap();
            // The root entry is the target itself, which no entry changes.
            assert_eq!(n, 2, "{mode:?}");
            let program = target.join("ko-app/crane");
            assert_eq!(fs::read(&program).unwrap(), b"#!x", "{mode:?}");
            let meta = fs::metadata(&program).unwrap();
            assert_eq!(meta.permissions().mode() & 0o7777, 0o755);
            assert_eq!(meta.mtime(), 1_700_000_000);
            assert!(fs::metadata(&target).unwrap().is_dir());
        }
        assert!(!Path::new("/ko-app").exists());
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
            index_digest: None,
            layers: vec!["sha256:aaa".into()],
            backend: BackendChoice::Overlay,
            created: 1,
            origin: "pull".into(),
            mode: Mode::Boot,
            pid1: false,
            started: None,
            fixed_addresses: BTreeSet::new(),
            built: false,
            run: RunSpec::default(),
            network: Network::Veth,
            network_name: None,
            address: None,
            ports: Vec::new(),
            entrypoint: None,
            cmd: None,
            env: Vec::new(),
            volumes: Vec::new(),
            labels: BTreeMap::new(),
            restart: Default::default(),
            limits: Default::default(),
            remove_on_exit: false,
            extra_networks: Vec::new(),
            aliases: BTreeMap::new(),
            no_network: false,
            network_container: None,
            signed_by: Some("key 6wiWMtJZCUkV05p2cDF3DrfY9Q1YsYRMLaOYI/PY7Lc=".into()),
            signed_at: Some(1_790_419_742),
            healthcheck: None,
            tuning: Default::default(),
        };
        store.record_image(&rec).unwrap();
        let loaded = store.load_image("fedora-44").unwrap().unwrap();
        assert_eq!(loaded.reference, "hub/fedora:44");
        assert_eq!(
            (&loaded.signed_by, loaded.signed_at),
            (&rec.signed_by, rec.signed_at)
        );
        let unsigned: ImageRecord = serde_json::from_str(
            &serde_json::to_string(&rec)
                .unwrap()
                .replace(",\"signed_by\"", ",\"was_signed_by\"")
                .replace("\"signed_at\":", "\"was_signed_at\":"),
        )
        .unwrap_or_else(|_| ImageRecord {
            signed_by: None,
            signed_at: None,
            ..rec.clone()
        });
        assert!(
            unsigned.signed_by.is_none() && unsigned.signed_at.is_none(),
            "a record from before the field loads"
        );
        assert_eq!(store.list_images().unwrap().len(), 1);
        fs::create_dir_all(store.layer_dir("sha256:aaa", Ownership::Root)).unwrap();
        fs::create_dir_all(store.layer_dir("sha256:bbb", Ownership::Foreign)).unwrap();
        assert_eq!(store.gc_layers().unwrap(), vec!["sha256:bbb".to_string()]);
        assert!(store.layer_dir("sha256:aaa", Ownership::Root).is_dir());
        store.remove_record("fedora-44").unwrap();
        assert!(store.load_image("fedora-44").unwrap().is_none());
        assert_eq!(store.gc_layers().unwrap(), vec!["sha256:aaa".to_string()]);
        // What went is deleted by the time a process other than the service ends.
        finish_trash();
        for ownership in [Ownership::Root, Ownership::Foreign] {
            let left: Vec<_> = fs::read_dir(store.layers_dir(ownership))
                .unwrap()
                .flatten()
                .map(|e| e.file_name())
                .collect();
            assert!(left.is_empty(), "{left:?}");
        }
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
            index_digest: None,
            layers: vec!["sha256:lay".into()],
            backend: BackendChoice::Flat,
            created: 1,
            origin: "build".into(),
            mode: Mode::App,
            pid1: false,
            started: None,
            fixed_addresses: BTreeSet::new(),
            built: false,
            run: RunSpec::default(),
            network: Network::Host,
            network_name: None,
            address: None,
            ports: Vec::new(),
            entrypoint: None,
            cmd: None,
            env: Vec::new(),
            volumes: Vec::new(),
            labels: BTreeMap::new(),
            restart: Default::default(),
            limits: Default::default(),
            remove_on_exit: false,
            extra_networks: Vec::new(),
            aliases: BTreeMap::new(),
            no_network: false,
            network_container: None,
            signed_by: None,
            signed_at: None,
            healthcheck: None,
            tuning: Default::default(),
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
        // A rollback cut short keeps nothing and stops no collection.
        store.save_rollback(&rec, b"{}").unwrap();
        fs::remove_file(store.rollback_dir("img").join("manifest.json")).unwrap();
        fs::write(store.root.join("rollback").join("junk"), "x").unwrap();
        store.gc_blobs().unwrap();
        store.gc_layers().unwrap();
        store.forget_rollback("img").unwrap();
        fs::remove_file(store.root.join("rollback").join("junk")).unwrap();
        // A machine of the same reference that ran, first by name, is not the image, which
        // a build makes on the bridge.
        let mut image = rec.clone();
        image.backend = BackendChoice::Overlay;
        image.network = Network::Bridge;
        store.record_image(&image).unwrap();
        let mut ran = rec.clone();
        ran.name = "aaa".to_string();
        ran.started = Some(1);
        store.record_image(&ran).unwrap();
        assert_eq!(
            store
                .find_image("img:1", "hub.example")
                .unwrap()
                .unwrap()
                .name,
            "img"
        );
        store.remove_record("aaa").unwrap();
        // A reference recorded under Docker Hub's old name is found by its name under
        // docker.io.
        let mut old = rec.clone();
        old.name = "nginx".to_string();
        old.reference = "index.docker.io/library/nginx:latest".to_string();
        store.record_image(&old).unwrap();
        assert_eq!(
            store
                .find_image("docker.io/nginx", "hub.example")
                .unwrap()
                .unwrap()
                .name,
            "nginx"
        );
        store.remove_record("nginx").unwrap();
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
            header_owner(&mut header);
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
        // The opaque marker of an overlay layer is a trusted.* attribute, which only
        // root sets.
        let modes: &[WhiteoutMode] = if nix::unistd::geteuid().is_root() {
            &[WhiteoutMode::Apply, WhiteoutMode::OverlayLower]
        } else {
            &[WhiteoutMode::Apply]
        };
        for &mode in modes {
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
            // The symlink's absolute target is taken from the layer's root.
            let inside = target.join(outside.strip_prefix("/").unwrap());
            assert!(inside.is_dir(), "{mode:?}");
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
            index_digest: None,
            layers: vec!["sha256:shared".into()],
            backend,
            created: 1,
            origin: "pull".into(),
            mode: Mode::Boot,
            pid1: false,
            started: None,
            fixed_addresses: BTreeSet::new(),
            built: false,
            run: RunSpec::default(),
            network: Network::Bridge,
            network_name: None,
            address: None,
            ports: Vec::new(),
            entrypoint: None,
            cmd: None,
            env: Vec::new(),
            volumes: Vec::new(),
            labels: BTreeMap::new(),
            restart: Default::default(),
            limits: Default::default(),
            remove_on_exit: false,
            extra_networks: Vec::new(),
            aliases: BTreeMap::new(),
            no_network: false,
            network_container: None,
            signed_by: None,
            signed_at: None,
            healthcheck: None,
            tuning: Default::default(),
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
        // What went was moved aside at once and is deleted after, the lock not waiting
        // for it; what a stop of the service left aside goes at the next start.
        let aside = |ownership| {
            fs::read_dir(store.layers_dir(ownership))
                .unwrap()
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with(".trash-"))
                .count()
        };
        for _ in 0..100 {
            if aside(Ownership::Root) == 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(aside(Ownership::Root), 0, "the deletion after the lock ran");
        let left = store.layers_dir(Ownership::Root).join(".trash-left");
        fs::create_dir_all(left.join("sha256-x/usr")).unwrap();
        store.sweep_trash();
        assert!(!left.exists());
        // Abandoned partial blobs go once nobody has touched them for an hour (a download
        // in flight runs without the lock); finished ones stay.
        let fresh = store.blobs_dir().join(".part-sha256-fresh.1");
        let old = store.blobs_dir().join(".part-sha256-old.2");
        fs::write(&fresh, b"half").unwrap();
        fs::write(&old, b"half").unwrap();
        fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(7200))
            .unwrap();
        fs::write(store.blob_path("sha256:m"), b"manifest?").unwrap();
        // Every recorded image has its manifest, which names its config blob.
        let manifest = r#"{"schemaVersion": 2, "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": "sha256:m", "size": 9}, "layers": []}"#;
        for image in store.list_images_strict().unwrap() {
            store
                .save_manifest(&image.name, manifest.as_bytes())
                .unwrap();
        }
        store.gc_blobs().unwrap();
        assert!(fresh.exists(), "a download in flight is left alone");
        assert!(!old.exists(), "an abandoned one goes");
    }

    #[test]
    fn gc_refuses_to_guess_when_a_record_cannot_be_read() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(&tmp.path().join("machines"), &tmp.path().join("state"));
        store.init().unwrap();
        fs::create_dir_all(store.layer_dir("sha256:live", Ownership::Root)).unwrap();
        fs::write(store.blob_path("sha256:live"), b"blob").unwrap();
        // A record from a newer version, or a damaged one.
        fs::write(
            store.images_dir().join("future.json"),
            "{\"name\": \"future\", \"network\": \"vpn\"}",
        )
        .unwrap();
        assert_eq!(
            store.list_images().unwrap().len(),
            0,
            "lenient listing skips it"
        );
        assert!(store.list_images_strict().is_err());
        assert!(store.gc_layers().is_err());
        assert!(store.gc_blobs().is_err());
        assert!(store.layer_dir("sha256:live", Ownership::Root).is_dir());
        assert!(store.blob_path("sha256:live").is_file());
        // A record whose manifest is gone names its config blob nowhere: blobs are not
        // collected, layers (named by the record) still are.
        fs::remove_file(store.images_dir().join("future.json")).unwrap();
        let record: ImageRecord = serde_json::from_str(
            r#"{"name": "live", "reference": "r", "manifest_digest": "d", "layers": ["sha256:live"], "backend": "overlay", "created": 0}"#,
        )
        .unwrap();
        store.record_image(&record).unwrap();
        let e = store.gc_blobs().unwrap_err().to_string();
        assert!(e.contains("manifest of live cannot be read"), "{e}");
        assert!(store.blob_path("sha256:live").is_file());
        assert!(store.gc_layers().is_ok());
        // Records that cannot be listed at all are no listing either for the collector.
        if !nix::unistd::geteuid().is_root() {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(store.images_dir(), fs::Permissions::from_mode(0o000)).unwrap();
            assert!(
                store.list_images().unwrap().is_empty(),
                "the lenient listing goes on"
            );
            assert!(store.list_images_strict().is_err());
            assert!(store.gc_layers().is_err());
            fs::set_permissions(store.images_dir(), fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[test]
    fn digests_are_validated_before_becoming_paths() {
        assert!(validate_digest(&format!("sha256:{}", "a".repeat(64))).is_ok());
        assert!(validate_digest("sha256:/../../etc").is_err());
        assert!(validate_digest(&format!("sha256:{}", "A".repeat(64))).is_err());
        assert!(validate_digest(&format!("sha512:{}", "a".repeat(64))).is_err());
        assert!(validate_digest(&format!("sha256:{}", "a".repeat(63))).is_err());
    }

    #[test]
    fn the_state_and_the_machines_are_root_s_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        fs::create_dir(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
        fs::create_dir_all(state.join("layers")).unwrap();
        fs::create_dir_all(state.join("machines/web/upper")).unwrap();
        fs::create_dir_all(state.join("machines/db")).unwrap();
        fs::create_dir_all(state.join("claims")).unwrap();
        for dir in [
            "layers",
            "machines",
            "machines/web",
            "machines/db",
            "claims",
        ] {
            fs::set_permissions(state.join(dir), fs::Permissions::from_mode(0o755)).unwrap();
        }
        let store = Store::new(&tmp.path().join("new/machines"), &state);
        store.init().unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().mode() & 0o777;
        assert_eq!(
            mode(&state),
            0o711,
            "an open state directory is closed but passable"
        );
        for private in [
            "layers",
            "layers-foreign",
            "blobs",
            "images",
            "manifests",
            "volumes",
            "claims",
        ] {
            assert_eq!(mode(&state.join(private)), 0o700, "{private}");
        }
        assert_eq!(mode(&state.join("machines")), 0o711);
        assert_eq!(
            mode(&state.join("machines/web")),
            0o700,
            "an overlay machine's writable layer is private"
        );
        assert_eq!(
            mode(&state.join("machines/db")),
            0o711,
            "the generated files of an mstack machine are reachable from its namespace"
        );
        assert_eq!(mode(&tmp.path().join("new/machines")), 0o700);
        assert_eq!(mode(&tmp.path().join("new")), 0o700, "missing parents too");
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        store.protect().unwrap();
        assert_eq!(
            mode(&state),
            0o711,
            "a closed one is opened for passing, which mstack machines need"
        );
        let missing = Store::new(tmp.path(), &tmp.path().join("nowhere"));
        missing.protect().unwrap();
        assert!(
            !tmp.path().join("nowhere").exists(),
            "protect makes nothing"
        );
    }

    #[test]
    fn a_probe_left_behind_does_not_make_a_directory_unwritable() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join(".nspawn-write-test")).unwrap();
        check_writable(tmp.path()).unwrap();
        assert!(
            !tmp.path().join(".nspawn-write-test").exists(),
            "the probe of earlier versions is cleaned up"
        );
    }

    #[test]
    fn writability_checks_at_once_do_not_trip_over_each_other() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    for _ in 0..200 {
                        check_writable(&dir).unwrap();
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(
            fs::read_dir(&dir).unwrap().count(),
            0,
            "no probe is left behind"
        );
    }

    #[test]
    fn records_are_written_atomically_and_legacy_commands_become_cmd() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(&tmp.path().join("machines"), &tmp.path().join("state"));
        store.init().unwrap();
        fs::write(
            store.images_dir().join("old.json"),
            r#"{"name":"old","reference":"hub/x:1","manifest_digest":"sha256:m","layers":[],"backend":"overlay","created":1,
                "run":{"command":["/docker-entrypoint.sh","nginx"]},"command":[]}"#,
        )
        .unwrap();
        let old = store.load_image("old").unwrap().unwrap();
        assert!(old.run.command.is_empty() && old.run.entrypoint().is_empty());
        assert_eq!(old.run.cmd(), ["/docker-entrypoint.sh", "nginx"]);
        assert_eq!(old.cmd, None, "an empty legacy override means none");
        assert!(
            old.labels.is_empty(),
            "a record from before labels has none"
        );
        let mut labelled = old.clone();
        labelled.run.labels = BTreeMap::from([
            ("a".to_string(), "image".to_string()),
            ("b".to_string(), "image".to_string()),
        ]);
        labelled.labels = BTreeMap::from([("b".to_string(), "machine".to_string())]);
        assert_eq!(old.restart, crate::policy::Restart::No);
        assert_eq!(
            old.limits,
            crate::policy::Limits::default(),
            "a record from before limits has none"
        );
        labelled.restart = crate::policy::Restart::OnFailure;
        labelled.limits = crate::policy::Limits {
            memory: 64 << 20,
            milli_cpus: 500,
            pids: 100,
            memory_swap: -1,
        };
        store.record_image(&labelled).unwrap();
        let back = store.load_image("old").unwrap().unwrap();
        assert_eq!(back.labels, labelled.labels);
        assert_eq!(back.restart, crate::policy::Restart::OnFailure);
        assert_eq!(back.limits, labelled.limits);
        assert_eq!(
            back.effective_labels(),
            BTreeMap::from([
                ("a".to_string(), "image".to_string()),
                ("b".to_string(), "machine".to_string()),
            ]),
            "the machine's labels win over the image's"
        );
        store.record_image(&old).unwrap();
        assert!(!store.images_dir().join(".tmp-old.json.0").exists());
        assert!(fs::read_dir(store.images_dir())
            .unwrap()
            .flatten()
            .all(|e| !e.file_name().to_string_lossy().starts_with(".tmp-")));
    }

    #[test]
    fn symlinks_inside_the_tree_are_followed_when_applying_layers() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("root");
        fs::create_dir_all(target.join("usr/lib")).unwrap();
        std::os::unix::fs::symlink("usr/lib", target.join("lib")).unwrap();
        fs::write(target.join("usr/lib/old"), "x").unwrap();
        let blob = tmp.path().join("layer.tar");
        fs::write(
            &blob,
            tar_with(&[("lib/.wh.old", Some("")), ("lib/new", Some("n"))]),
        )
        .unwrap();
        extract_layer(
            &blob,
            "application/vnd.oci.image.layer.v1.tar",
            &target,
            WhiteoutMode::Apply,
            Ownership::Root,
        )
        .unwrap();
        assert!(
            !target.join("usr/lib/old").exists(),
            "whiteout through an inside symlink applies"
        );
        assert_eq!(fs::read_to_string(target.join("usr/lib/new")).unwrap(), "n");
    }

    #[test]
    fn temporary_names_never_repeat() {
        let a = unique_suffix();
        let b = unique_suffix();
        assert_ne!(a, b);
        assert!(a.starts_with(&format!("{}.", std::process::id())));
    }

    #[test]
    fn held_blobs_survive_the_collector() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(&tmp.path().join("machines"), &tmp.path().join("state"));
        let digest = format!("sha256:{}", "a".repeat(64));
        fs::create_dir_all(store.blobs_dir()).unwrap();
        fs::write(store.blob_path(&digest), b"x").unwrap();
        let hold = store.hold_blobs(std::slice::from_ref(&digest)).unwrap();
        assert!(
            store.gc_blobs().unwrap().is_empty(),
            "held by a pull in flight"
        );
        assert!(store.has_blob(&digest));
        drop(hold);
        assert_eq!(store.gc_blobs().unwrap(), vec![digest.clone()]);
        assert!(!store.has_blob(&digest));
    }

    #[test]
    fn the_collector_waits_for_a_hold_being_made() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(&tmp.path().join("machines"), &tmp.path().join("state"));
        let digest = format!("sha256:{}", "e".repeat(64));
        fs::create_dir_all(store.blobs_dir()).unwrap();
        fs::write(store.blob_path(&digest), b"x").unwrap();
        let still = store.holds_still().unwrap();
        let collector = {
            let store = store.clone();
            std::thread::spawn(move || store.gc_blobs().unwrap())
        };
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(!collector.is_finished(), "it waits for the pull to look");
        // The pull found the blob present and holds it.
        let hold = store.hold_blobs(std::slice::from_ref(&digest)).unwrap();
        assert!(store.has_blob(&digest));
        drop(still);
        assert!(collector.join().unwrap().is_empty());
        assert!(store.has_blob(&digest));
        drop(hold);
    }

    #[test]
    fn a_new_service_forgets_the_holds_of_the_last_one() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(&tmp.path().join("machines"), &tmp.path().join("state"));
        let digest = format!("sha256:{}", "d".repeat(64));
        fs::create_dir_all(store.blobs_dir()).unwrap();
        fs::write(store.blob_path(&digest), b"x").unwrap();
        // Left by a service that was killed, under a PID something else runs as now.
        let left = store.hold_blobs(std::slice::from_ref(&digest)).unwrap();
        std::mem::forget(left);
        assert!(store.gc_blobs().unwrap().is_empty());
        store.forget_holds();
        assert_eq!(store.gc_blobs().unwrap(), vec![digest]);
    }

    #[test]
    fn a_long_download_keeps_its_hold() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(&tmp.path().join("machines"), &tmp.path().join("state"));
        let ours = format!("sha256:{}", "b".repeat(64));
        let orphan = format!("sha256:{}", "c".repeat(64));
        fs::create_dir_all(store.blobs_dir()).unwrap();
        for digest in [&ours, &orphan] {
            fs::write(store.blob_path(digest), b"x").unwrap();
        }
        let hold = store.hold_blobs(std::slice::from_ref(&ours)).unwrap();
        // A hold whose process is gone: no PID can be this large.
        let dead = store.blobs_dir().join(".hold-4294967295.0");
        fs::write(&dead, &orphan).unwrap();
        let two_hours_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
        for path in [&hold.0, &dead] {
            fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(two_hours_ago)
                .unwrap();
        }
        assert_eq!(
            store.gc_blobs().unwrap(),
            vec![orphan],
            "the hold of a live pull stands however old, a dead one's goes"
        );
        assert!(store.has_blob(&ours));
        assert!(!dead.exists());
        drop(hold);
    }

    #[test]
    fn a_start_in_progress_is_marked_until_it_ends() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(&tmp.path().join("machines"), &tmp.path().join("state"));
        assert!(!store.is_starting("web"));
        let mark = store.mark_starting("web").unwrap();
        assert!(store.is_starting("web"));
        // A marker a service left when it went is gone at the next service's start.
        let left = store.mark_starting("left").unwrap();
        std::mem::forget(left);
        store.forget_starts();
        assert!(!store.is_starting("left") && !store.is_starting("web"));
        let mark = {
            drop(mark);
            store.mark_starting("web").unwrap()
        };
        drop(mark);
        assert!(!store.is_starting("web"));
        let stale = store.mark_starting("old").unwrap();
        let file = fs::OpenOptions::new()
            .write(true)
            .open(store.starting_path("old"))
            .unwrap();
        file.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(7200))
            .unwrap();
        assert!(
            !store.is_starting("old"),
            "a marker left by a start that died with its process"
        );
        drop(stale);
        // One of another boot, left by a crash, stands no more; an empty one of a
        // release that kept no boot ID goes by its age.
        let other = store.mark_starting("crashed").unwrap();
        fs::write(
            store.starting_path("crashed"),
            "00000000-0000-0000-0000-000000000000",
        )
        .unwrap();
        assert!(!store.is_starting("crashed"));
        fs::write(store.starting_path("crashed"), "").unwrap();
        assert!(store.is_starting("crashed"));
        drop(other);
        store.claim_ports("web").unwrap();
        assert!(store.claims_ports("web"));
        fs::write(
            store.claim_path("web"),
            "00000000-0000-0000-0000-000000000000",
        )
        .unwrap();
        assert!(!store.claims_ports("web"), "a claim of another boot");
        store.release_ports_claim("web");
    }

    #[test]
    fn the_last_signal_is_remembered_until_forgotten() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(&tmp.path().join("machines"), &tmp.path().join("state"));
        assert_eq!(store.last_signal("web"), None);
        store.mark_signal("web", 9, None).unwrap();
        assert_eq!(store.last_signal("web"), Some(9));
        assert_eq!(store.signal_of_run("web", "abc"), None, "no run said");
        store.mark_signal("web", 15, Some("abc")).unwrap();
        assert_eq!(store.last_signal("web"), Some(15));
        assert_eq!(store.signal_of_run("web", "abc"), Some(15));
        assert_eq!(store.signal_of_run("web", "def"), None);
        store.forget_signal("web").unwrap();
        store.forget_signal("web").unwrap();
        assert_eq!(store.last_signal("web"), None);
    }

    #[test]
    fn the_exit_on_next_mark_is_taken_once() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(&tmp.path().join("machines"), &tmp.path().join("state"));
        assert!(!store.take_exit_on_next("web").unwrap());
        store.mark_exit_on_next("web").unwrap();
        assert!(store.take_exit_on_next("web").unwrap());
        assert!(!store.take_exit_on_next("web").unwrap());
        assert!(store.mark_exit_on_next("../x").is_err());
    }

    #[tokio::test]
    async fn store_lock_is_exclusive_between_holders() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(&tmp.path().join("machines"), &tmp.path().join("state"));
        let first = store.lock().await.unwrap();
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
        let waited = match store.lock_for(std::time::Duration::from_millis(300)).await {
            Ok(_) => panic!("a bounded wait must give up on a held lock"),
            Err(e) => e.to_string(),
        };
        assert!(waited.contains("the store is busy"), "{waited}");
        drop(first);
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let taken = Flock::lock(file, FlockArg::LockExclusiveNonblock);
        assert!(taken.is_ok());
        drop(taken);
        store
            .lock_for(std::time::Duration::from_millis(300))
            .await
            .expect("free again");
    }
}
