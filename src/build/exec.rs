//! Runs a build: the bases of the stages the target needs, each step in a root of its
//! own (an overlay of the layers so far) turned into a layer, or taken from the cache,
//! and the image the target stage ends with, installed under each tag.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufWriter, Read as _};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context as _, Result};
use tokio_util::sync::CancellationToken;

use crate::api::{line, note, Context, Report};
use crate::backend::{Backend, BackendChoice};
use crate::build::cache::{self, Cache};
use crate::build::image::{self, Config};
use crate::build::plan::{self, Action, BaseRef, From, Platform, RunMount};
use crate::build::untar::{self, Compression};
use crate::build::walk::bytes_path;
use crate::build::{copy, layer, root, sandbox};
use crate::oci::Mode;
use crate::reference::ImageRef;
use crate::store::{now_unix, ImageRecord, Ownership};

/// The names of the network namespaces of builds: a machine's name never starts with a
/// dot, so none is taken for one.
const NETNS_PREFIX: &str = ".build-";

/// The network namespace of the build `id`.
fn netns_of(id: &str) -> String {
    format!("{NETNS_PREFIX}{}", short(&cache::key(&[id.as_bytes()])))
}

/// The media type of the layers a build makes.
pub const LAYER_MEDIA_TYPE: &str = "application/vnd.oci.image.layer.v1.tar+gzip";

/// A build as the service takes it: the context is unpacked already.
#[derive(Debug, Clone)]
pub struct Request {
    pub dockerfile: Vec<u8>,
    pub context: PathBuf,
    pub tags: Vec<String>,
    pub options: plan::Options,
    pub no_cache: bool,
    /// Stages built again, by name or index.
    pub no_cache_filter: Vec<String>,
    /// Ask the registry for base images even when a local one has the reference.
    pub pull: bool,
    /// The network of RUN steps that ask for the default one.
    pub network: plan::Network,
    pub allow_host_network: bool,
    pub add_hosts: Vec<(String, String)>,
    pub secrets: BTreeMap<String, Vec<u8>>,
    pub shm_size: Option<u64>,
    pub ulimits: BTreeMap<String, (u64, u64)>,
    pub verify: bool,
    pub backend: BackendChoice,
    pub mode: Option<Mode>,
    pub force: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Built {
    /// The digest of the image's config, as image IDs go.
    pub id: String,
    pub manifest_digest: String,
    /// The local images made, one per tag.
    pub names: Vec<String>,
}

/// A layer of an image being built.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LayerRef {
    digest: String,
    media_type: String,
    size: u64,
}

/// Where a stage got to.
#[derive(Debug, Clone)]
struct State {
    config: Config,
    layers: Vec<LayerRef>,
    /// The cache key of everything so far: what the steps are, not what they made.
    key: String,
}

impl State {
    /// What the stage holds, for the keys of steps that take files from it: its layers'
    /// contents: a copy from a stage is keyed by the files it takes, not by the steps
    /// that made them.
    fn made(&self) -> String {
        let parts: Vec<&[u8]> = self
            .config
            .rootfs
            .diff_ids
            .iter()
            .map(|d| d.as_bytes())
            .collect();
        cache::key(&parts)
    }

    /// The stage of a cached step: the layers it ran on and made, with their entries of
    /// the history, and this build's entries of the steps that made no layer, which may
    /// differ from those of the build that made it as they are not in its key.
    fn take(&mut self, stage: &cache::Stage) {
        self.layers = stage
            .layers
            .iter()
            .map(|l| LayerRef {
                digest: l.digest.clone(),
                media_type: l.media_type.clone(),
                size: l.size,
            })
            .collect();
        self.config.rootfs.diff_ids = stage.layers.iter().map(|l| l.diff_id.clone()).collect();
        let mut layers = stage.history.iter().filter(|h| !h.empty_layer);
        let mut history: Vec<_> = self
            .config
            .history
            .iter()
            .map(|h| match h.empty_layer {
                true => h.clone(),
                false => layers.next().cloned().unwrap_or_else(|| h.clone()),
            })
            .collect();
        history.extend(layers.cloned());
        self.config.history = history;
    }

    /// The stage once a step that ran added its layer, for the step's cache entry.
    fn add(&mut self, entry: &cache::Entry, created_by: String) -> cache::Stage {
        self.layers.push(LayerRef {
            digest: entry.layer.clone(),
            media_type: LAYER_MEDIA_TYPE.to_string(),
            size: entry.size,
        });
        self.config
            .add_layer(entry.diff_id.clone(), created_by, &entry.created);
        cache::Stage {
            layers: self
                .layers
                .iter()
                .zip(&self.config.rootfs.diff_ids)
                .map(|(l, diff_id)| cache::StageLayer {
                    digest: l.digest.clone(),
                    media_type: l.media_type.clone(),
                    size: l.size,
                    diff_id: diff_id.clone(),
                })
                .collect(),
            history: self.config.history.clone(),
        }
    }
}

/// The action as its step's key takes it: without the proxy variables, and without a
/// cache mount's id and sharing, which ExecOp.CacheMap leaves out (checkShouldClearCacheOpts).
fn keyed(action: &Action) -> Action {
    let mut action = action.clone();
    if let Action::Run(run) = &mut action {
        run.proxy_env.clear();
        for mount in &mut run.mounts {
            if let RunMount::Cache { id, sharing, .. } = mount {
                id.clear();
                sharing.clear();
            }
        }
    }
    action
}

/// A read-only overlay of an image's layers, for a stage or image a step takes files
/// from.
struct View {
    root: root::StepRoot,
}

pub struct Builder<'a> {
    ctx: &'a Context,
    req: &'a Request,
    report: Report<'a>,
    /// builds/ID: what the build makes as it goes, gone at the end.
    work: PathBuf,
    id: String,
    cache: Cache,
    platform: Platform,
    /// Step numbers of the progress output.
    step: usize,
    states: HashMap<usize, State>,
    views: HashMap<String, View>,
    netns: Option<String>,
    /// Set when the client that asked for the build went away.
    cancel: CancellationToken,
    /// What the build uses of the store (its bases' and steps' layers, its config),
    /// kept from a gc or a prune until it ends.
    held: std::collections::BTreeSet<String>,
    hold: Option<crate::store::BlobHold>,
    /// Images taken from, each resolved once a build.
    images: HashMap<String, State>,
    /// URLs ADD fetched, each once a build: where to, its file name and its digest.
    downloads: HashMap<String, Downloaded>,
    /// SOURCE_DATE_EPOCH, once the Dockerfile is read.
    epoch: Option<i64>,
}

const CANCELLED: &str = "the build was cancelled: its client went away";

const HOST_NETWORK_REFUSED: &str =
    "network.host is not allowed: --allow network.host lets a step use it";

/// In the work directory of a Dockerfile build, for the sweep at the service's start.
pub(crate) const MARKER: &str = ".nspawn-dockerfile-build";

/// In the output directory of an mkosi build until it ends, for the same sweep.
pub(crate) const MKOSI_MARKER: &str = ".nspawn-mkosi-build";

/// SOURCE_DATE_EPOCH as the history writes it: encoding/json of a time.Time in UTC.
fn epoch_time(epoch: i64) -> String {
    let usec = u64::try_from(epoch).unwrap_or(0).saturating_mul(1_000_000);
    crate::dockerapi::util::rfc3339_nano(usec, false)
}

fn timestamp_now() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    image::timestamp(nanos)
}

/// The layer directory of a blob in the store, as overlay machines have it.
fn layer_dir(ctx: &Context, digest: &str) -> PathBuf {
    ctx.store.layer_dir(digest, Ownership::Root)
}

/// A layer of a base image under the OCI media type of the same bytes.
fn oci_media_type(media_type: &str) -> String {
    match media_type {
        "application/vnd.docker.image.rootfs.diff.tar.gzip" => LAYER_MEDIA_TYPE.to_string(),
        "application/vnd.docker.image.rootfs.diff.tar" => {
            "application/vnd.oci.image.layer.v1.tar".to_string()
        }
        other => other.to_string(),
    }
}

impl<'a> Builder<'a> {
    fn with_work(
        ctx: &'a Context,
        req: &'a Request,
        report: Report<'a>,
        id: String,
        work: PathBuf,
        cancel: CancellationToken,
    ) -> Builder<'a> {
        Builder {
            ctx,
            req,
            report,
            work,
            id,
            cache: Cache::new(&ctx.store.build_cache_dir()),
            platform: Platform::host(),
            step: 0,
            states: HashMap::new(),
            views: HashMap::new(),
            netns: None,
            cancel,
            held: Default::default(),
            hold: None,
            images: HashMap::new(),
            downloads: HashMap::new(),
            epoch: None,
        }
    }

    /// An ADD of a URL: fetched once a build into the work directory, checked against
    /// `checksum`; its directory, file name and digest. A step taking it is keyed by its
    /// contents, as the reference keys it.
    async fn download(&mut self, url: &str, checksum: Option<&str>) -> Result<Downloaded> {
        // Each ADD checks its own --checksum, the URL fetched before included.
        let check = |found: &Downloaded| -> Result<()> {
            match checksum {
                Some(checksum) if found.digest != checksum => bail!(
                    "digest mismatch for {url}: expected {checksum}, got {}",
                    found.digest
                ),
                _ => Ok(()),
            }
        };
        if let Some(found) = self.downloads.get(url) {
            check(found)?;
            return Ok(found.clone());
        }
        let dir = self.work.join(format!("url-{}", self.downloads.len()));
        let found = tokio::select! {
            fetched = fetch_url(url, &dir) => fetched?,
            () = self.cancel.cancelled() => bail!(CANCELLED),
        };
        self.downloads.insert(url.to_string(), found.clone());
        check(&found)?;
        Ok(found)
    }

    /// Keeps `digests` for the rest of the build. A gc between their arrival and this
    /// would take them: the caller holds the store lock, or another hold on them.
    fn keep(&mut self, digests: impl IntoIterator<Item = String>) -> Result<()> {
        let before = self.held.len();
        self.held.extend(digests);
        if self.held.len() == before {
            return Ok(());
        }
        let list: Vec<String> = self.held.iter().cloned().collect();
        match &self.hold {
            Some(hold) => hold.set(&list),
            None => {
                self.hold = Some(self.ctx.store.hold_blobs(&list)?);
                Ok(())
            }
        }
    }

    /// Stops a build whose client went away, between two of its steps.
    fn check(&self) -> Result<()> {
        if self.cancel.is_cancelled() {
            bail!(CANCELLED);
        }
        Ok(())
    }

    fn say(&self, text: impl Into<String>) {
        line(self.report, text.into());
    }

    /// The build, its work directory, mounts and network namespace gone whatever the
    /// outcome.
    pub async fn run(mut self) -> Result<Built> {
        let result = self.build().await;
        self.cleanup().await;
        result
    }

    async fn cleanup(&mut self) {
        if let Ok(sd) = self.ctx.sd().await {
            for (_, view) in self.views.drain() {
                let _ = root::unmount(sd, &view.root).await;
            }
            // A step root a failure left mounted, so that nothing below is removed
            // through it.
            for unit in mounted_below(&self.work) {
                let _ = sd.stop_unit(&unit).await;
            }
        }
        if let Some(name) = self.netns.take() {
            crate::bridge::delete_netns(&name);
            let _ = std::process::Command::new("ip")
                .args(["link", "del", &crate::bridge::host_end_name_at(&name, 0)])
                .output();
        }
        let _ = crate::store::remove_tree(&self.work);
    }

    async fn build(&mut self) -> Result<Built> {
        let req = self.req;
        // The parse and the expansions go a level down the stack per level of nesting,
        // and take as long as a Dockerfile makes them: the runtime's other tasks go on
        // meanwhile.
        let platform = self.platform.clone();
        let prepared = tokio::task::block_in_place(|| {
            crate::build::deep(|| {
                crate::build::dispatch::prepare(&req.dockerfile, &req.options, &platform)
            })
        })?;
        self.epoch = prepared.epoch;
        for &index in &prepared.order {
            let info = prepared.stages[index].clone();
            let state = match &info.base {
                BaseRef::Scratch => State {
                    config: Config::scratch(),
                    layers: Vec::new(),
                    key: cache::key(&[
                        b"scratch",
                        self.platform.architecture.as_bytes(),
                        env!("CARGO_PKG_VERSION").as_bytes(),
                    ]),
                },
                BaseRef::Stage(i) => self
                    .states
                    .get(i)
                    .cloned()
                    .with_context(|| format!("stage {i} was not built before stage {index}"))?,
                BaseRef::Image(reference) => self.base(reference).await?,
            };
            let mut settings = state.config.clone();
            settings.history.clear();
            let dispatched = tokio::task::block_in_place(|| {
                crate::build::deep(|| {
                    crate::build::dispatch::dispatch(
                        &prepared,
                        index,
                        &settings,
                        &req.options,
                        &platform,
                    )
                })
            })?;
            let label = if info.name.is_empty() {
                format!("stage-{index}")
            } else {
                info.name.clone()
            };
            let no_cache = req.no_cache
                || req
                    .no_cache_filter
                    .iter()
                    .any(|f| f.eq_ignore_ascii_case(&info.name) || *f == index.to_string());
            let state = self.stage(&label, state, &dispatched, no_cache).await?;
            self.states.insert(index, state);
        }
        let mut final_state = self
            .states
            .get(&prepared.target)
            .cloned()
            .context("the target stage was not built")?;
        final_state.config.stamp();
        self.finish(final_state).await
    }

    /// The steps of a stage, from `state` on.
    async fn stage(
        &mut self,
        label: &str,
        mut state: State,
        dispatched: &plan::Dispatched,
        no_cache: bool,
    ) -> Result<State> {
        let total = dispatched.steps.len();
        let base_len = state.config.history.len();
        // The steps that only set settings before the stage's first layer: they get its
        // time, as normalizeLayersAndHistory gives them, once it is known.
        let mut leading: Option<Vec<usize>> = Some(Vec::new());
        // A step's time: the last layer's, so that a build taken from the cache gives
        // the same image.
        let mut created = state
            .config
            .history
            .iter()
            .rev()
            .find_map(|h| h.created.clone())
            .or_else(|| state.config.created.clone())
            .unwrap_or_else(|| image::timestamp(0));
        for (i, step) in dispatched.steps.iter().enumerate() {
            self.check()?;
            self.step += 1;
            let n = self.step;
            let start = Instant::now();
            self.say(format!(
                "#{n} [{label} {}/{total}] {}",
                i + 1,
                step.description
            ));
            // A step that only sets the image's settings changes no layer: the steps after
            // it are keyed by what they run with, as an op is, not by it.
            let Some(action) = &step.action else {
                if let Some(leading) = &mut leading {
                    leading.push(state.config.history.len());
                }
                state.config.note(step.created_by.clone(), &created);
                self.say(format!("#{n} DONE {:.1}s", start.elapsed().as_secs_f64()));
                continue;
            };
            // An entitlement is checked before the cache is: a step taken from it is
            // one the build asks for all the same.
            if let Action::Run(run) = action {
                if self.network_of(run.network) == plan::Network::Host
                    && !self.req.allow_host_network
                {
                    let e = anyhow!(HOST_NETWORK_REFUSED);
                    self.say(format!("#{n} ERROR: {e:#}"));
                    return Err(e);
                }
            }
            let key = cache::key(&[
                state.key.as_bytes(),
                format!("{:?}", keyed(action)).as_bytes(),
                self.inputs(action).await?.as_bytes(),
            ]);
            let cached = if no_cache {
                None
            } else {
                // Under the lock, so that a prune does not take it before it is kept.
                let _lock = self.ctx.store.lock().await?;
                match self.cache.get(&key) {
                    Some(cache::Entry {
                        stage: Some(stage),
                        created,
                        ..
                    }) if stage
                        .layers
                        .iter()
                        .all(|l| self.ctx.store.has_blob(&l.digest)) =>
                    {
                        self.keep(stage.layers.iter().map(|l| l.digest.clone()))?;
                        Some((stage, created))
                    }
                    _ => None,
                }
            };
            match cached {
                Some((stage, at)) => {
                    self.say(format!("#{n} CACHED"));
                    state.take(&stage);
                    created = at;
                }
                None => {
                    let mut entry = self
                        .execute(n, &state, action, &step.description)
                        .await
                        .map_err(|e| {
                            self.say(format!("#{n} ERROR: {e:#}"));
                            e
                        })?;
                    entry.stage = Some(state.add(&entry, step.created_by.clone()));
                    created.clone_from(&entry.created);
                    self.cache.put(&key, &entry)?;
                    self.say(format!("#{n} DONE {:.1}s", start.elapsed().as_secs_f64()));
                }
            }
            state.key = key;
            if let Some(leading) = leading.take() {
                for i in leading {
                    state.config.history[i].created = Some(created.clone());
                }
            }
        }
        // With SOURCE_DATE_EPOCH every entry the stage adds has that time, as
        // commitToHistory gives it.
        if let Some(epoch) = self.epoch {
            let at = epoch_time(epoch);
            for h in &mut state.config.history[base_len..] {
                h.created = Some(at.clone());
            }
        }
        let history = std::mem::take(&mut state.config.history);
        let rootfs = std::mem::take(&mut state.config.rootfs);
        // The base image's author stays unless MAINTAINER names another.
        let base_author = state.config.author.take();
        state.config = dispatched.config.clone();
        state.config.history = history;
        state.config.rootfs = rootfs;
        state.config.author = dispatched.author.clone().or(base_author);
        Ok(state)
    }

    /// What a step's key takes from outside the image: the files it copies or mounts
    /// from the context, the results of the stages it takes from.
    async fn inputs(&mut self, action: &Action) -> Result<String> {
        let mut parts: Vec<String> = Vec::new();
        // An image is taken by what its reference names now, not by the reference.
        let images: Vec<String> = match action {
            Action::Copy(copy) => match &copy.from {
                From::Image(reference) => vec![reference.clone()],
                _ => Vec::new(),
            },
            Action::Run(run) => run
                .mounts
                .iter()
                .filter_map(|m| match m {
                    RunMount::Bind {
                        from: From::Image(reference),
                        ..
                    }
                    | RunMount::Cache {
                        from: Some((From::Image(reference), _)),
                        ..
                    } => Some(reference.clone()),
                    _ => None,
                })
                .collect(),
            Action::Workdir { .. } => Vec::new(),
        };
        let mut image_keys = HashMap::new();
        for reference in images {
            let key = self.base(&reference).await?.key;
            image_keys.insert(reference, key);
        }
        if let Action::Copy(copy) = action {
            if copy.add {
                for source in copy.sources.iter().filter(|s| is_url(s)) {
                    let found = self.download(source, copy.checksum.as_deref()).await?;
                    // The time is the file's, so its key, as Last-Modified is the key's of
                    // the reference.
                    let modified = found.modified.map(|t| t.to_string()).unwrap_or_default();
                    parts.push(format!("url {source} {} {modified}", found.digest));
                }
            }
        }
        let from = |from: &From,
                    paths: &[String],
                    exclude: &[String],
                    parts: &mut Vec<String>|
         -> Result<()> {
            match from {
                From::Context => {
                    // A large context takes a while to hash: the runtime's other tasks go
                    // on meanwhile.
                    for path in paths {
                        parts.push(tokio::task::block_in_place(|| {
                            content_hash(&self.req.context, path, exclude)
                        })?);
                    }
                }
                From::Stage(i) => parts.push(
                    self.states
                        .get(i)
                        .map(State::made)
                        .with_context(|| format!("stage {i} is not built"))?,
                ),
                From::Image(reference) => parts.push(
                    image_keys
                        .get(reference)
                        .cloned()
                        .with_context(|| format!("image {reference} is not resolved"))?,
                ),
                From::Scratch => parts.push("scratch".to_string()),
            }
            Ok(())
        };
        match action {
            Action::Copy(copy) => from(&copy.from, &copy.sources, &copy.exclude, &mut parts)?,
            Action::Run(run) => {
                for mount in &run.mounts {
                    match mount {
                        RunMount::Bind {
                            from: f, source, ..
                        } => from(f, std::slice::from_ref(source), &[], &mut parts)?,
                        RunMount::Cache {
                            from: Some((f, source)),
                            ..
                        } => from(f, std::slice::from_ref(source), &[], &mut parts)?,
                        _ => {}
                    }
                }
                parts.push(format!("{:?}", self.network_of(run.network)));
                // What the step's sandbox is given, as the reference keys it: the names of
                // the extra hosts, not their addresses.
                let hosts: Vec<&str> = self.req.add_hosts.iter().map(|(h, _)| h.as_str()).collect();
                parts.push(format!(
                    "{:?} {:?} {:?}",
                    hosts, self.req.ulimits, self.req.shm_size
                ));
            }
            Action::Workdir { .. } => {}
        }
        Ok(parts.join("\n"))
    }

    fn network_of(&self, asked: plan::Network) -> plan::Network {
        match asked {
            plan::Network::Default => self.req.network,
            other => other,
        }
    }

    /// A base image: a local one with that reference, or the registry's, its layers in
    /// the store and kept by the cache.
    async fn base(&mut self, reference: &str) -> Result<State> {
        if let Some(state) = self.images.get(reference) {
            return Ok(state.clone());
        }
        let state = self.resolve_base(reference).await?;
        self.images.insert(reference.to_string(), state.clone());
        Ok(state)
    }

    async fn resolve_base(&mut self, reference: &str) -> Result<State> {
        let ctx = self.ctx;
        let store = &ctx.store;
        let image = ImageRef::normalized(reference)?;
        let wanted = image.to_string();
        // `build -t NAME` records NAME under the configured registry: an image built
        // here is taken by that name too, but nothing pulled from that registry stands
        // in for a Docker Hub image.
        let built = ImageRef::parse(reference, &ctx.config.registry)
            .map(|r| r.to_string())
            .ok()
            .filter(|b| *b != wanted);
        let local = if self.req.pull {
            None
        } else {
            local_base(&store.list_images()?, &wanted, built.as_deref()).cloned()
        };
        let (manifest_bytes, manifest_digest) = match local {
            Some(record) => {
                // Kept before an images rm can take it.
                let _lock = store.lock().await?;
                let record = store
                    .load_image(&record.name)?
                    .with_context(|| format!("{wanted} went while the build took it"))?;
                let bytes = store.load_manifest(&record.name)?;
                let manifest: oci_client::manifest::OciImageManifest =
                    serde_json::from_slice(&bytes)
                        .with_context(|| format!("the manifest of {wanted}"))?;
                self.keep(
                    manifest
                        .layers
                        .iter()
                        .map(|l| l.digest.clone())
                        .chain([manifest.config.digest.clone()]),
                )?;
                (bytes, record.manifest_digest.clone())
            }
            None => {
                let fetched = tokio::select! {
                    fetched = crate::api::pull::fetch(ctx, &image, self.req.verify, None, self.report) => fetched?,
                    () = self.cancel.cancelled() => bail!(CANCELLED),
                };
                // The pull's own hold is let go once the build's has them.
                self.keep(
                    fetched
                        .manifest
                        .layers
                        .iter()
                        .map(|l| l.digest.clone())
                        .chain([fetched.manifest.config.digest.clone()]),
                )?;
                (
                    fetched.manifest_bytes.clone(),
                    fetched.manifest_digest.clone(),
                )
            }
        };
        let manifest: oci_client::manifest::OciImageManifest =
            serde_json::from_slice(&manifest_bytes)
                .with_context(|| format!("the manifest of {wanted}"))?;
        let config_blob = std::fs::read(store.blob_path(&manifest.config.digest))
            .with_context(|| format!("reading the config of {wanted}"))?;
        let config: Config = serde_json::from_slice(&config_blob)
            .with_context(|| format!("the config of {wanted}"))?;
        if config.architecture != self.platform.architecture {
            bail!(
                "{wanted} is an image for {}, and builds run for this host's {}",
                config.architecture,
                self.platform.architecture
            );
        }
        // Kept by the cache before its layers are made, so that a gc meanwhile leaves
        // them.
        let _lock = store.lock().await?;
        self.cache.put_base(&cache::Base {
            reference: wanted.clone(),
            manifest_digest: manifest_digest.clone(),
            config: manifest.config.digest.clone(),
            layers: manifest.layers.iter().map(|l| l.digest.clone()).collect(),
            used: now_unix(),
        })?;
        for layer in &manifest.layers {
            let (store, digest, media_type) = (
                store.clone(),
                layer.digest.clone(),
                layer.media_type.clone(),
            );
            tokio::task::spawn_blocking(move || {
                let blob = store.blob_path(&digest);
                store.import_layer(&digest, &media_type, &blob, Ownership::Root)
            })
            .await
            .context("extracting a layer")??;
        }
        Ok(State {
            config,
            layers: manifest
                .layers
                .iter()
                .map(|l| LayerRef {
                    digest: l.digest.clone(),
                    media_type: oci_media_type(&l.media_type),
                    size: l.size as u64,
                })
                .collect(),
            // Steps of another release of the builder are made again: its sandbox may
            // differ.
            key: cache::key(&[
                b"image",
                manifest_digest.as_bytes(),
                env!("CARGO_PKG_VERSION").as_bytes(),
            ]),
        })
    }

    /// The layer directories of `layers`, bottom first.
    fn lowers(&self, layers: &[LayerRef]) -> Vec<PathBuf> {
        layers
            .iter()
            .map(|l| layer_dir(self.ctx, &l.digest))
            .collect()
    }

    /// A read-only root of a stage or image, for files a step takes from it.
    async fn source_root(&mut self, from: &From) -> Result<PathBuf> {
        let (name, layers) = match from {
            From::Context => return Ok(self.req.context.clone()),
            From::Scratch => {
                let empty = self.work.join("scratch");
                std::fs::create_dir_all(&empty)
                    .with_context(|| format!("creating {}", empty.display()))?;
                return Ok(empty);
            }
            From::Stage(i) => (
                format!("stage-{i}"),
                self.states
                    .get(i)
                    .map(|s| s.layers.clone())
                    .with_context(|| format!("stage {i} is not built"))?,
            ),
            From::Image(reference) => {
                let state = self.base(reference).await?;
                (format!("image-{}", short(&state.key)), state.layers)
            }
        };
        if let Some(view) = self.views.get(&name) {
            return Ok(view.root.merged.clone());
        }
        let sd = self.ctx.sd().await?;
        let dir = self.work.join("views").join(&name);
        let root = root::mount(sd, &dir, &self.lowers(&layers)).await?;
        let merged = root.merged.clone();
        self.views.insert(name, View { root });
        Ok(merged)
    }

    /// Runs a step that changes the filesystem, its layer committed.
    async fn execute(
        &mut self,
        n: usize,
        state: &State,
        action: &Action,
        description: &str,
    ) -> Result<cache::Entry> {
        let sd = self.ctx.sd().await?;
        let dir = self.work.join(format!("step-{n}"));
        let lowers = self.lowers(&state.layers);
        let step_root = root::mount(sd, &dir, &lowers).await?;
        let done = match action {
            Action::Run(run) => self.run_step(n, &step_root.merged, run).await,
            Action::Copy(c) => self.copy_step(n, &step_root.merged, c).await,
            Action::Workdir { path, user } => {
                tokio::task::block_in_place(|| workdir(&step_root.merged, path, user, self.epoch))
            }
        };
        let unmounted = root::unmount(sd, &step_root).await;
        done?;
        unmounted?;
        let created = timestamp_now();
        let committed = self.commit(n, &step_root.upper, &lowers).await?;
        let _ = crate::store::remove_tree(&dir);
        Ok(cache::Entry {
            layer: committed.digest,
            diff_id: committed.diff_id,
            size: committed.size,
            created,
            used: now_unix(),
            description: description.to_string(),
            stage: None,
        })
    }

    /// The layer of what a step left in `upper`, its blob and directory in the store.
    async fn commit(
        &mut self,
        n: usize,
        upper: &Path,
        lowers: &[PathBuf],
    ) -> Result<layer::Committed> {
        let store = self.ctx.store.clone();
        let (upper, lowers) = (upper.to_path_buf(), lowers.to_vec());
        let part = store
            .blobs_dir()
            .join(format!(".part-build-{}-{n}", self.id));
        let written = part.clone();
        let committed = tokio::task::spawn_blocking(move || -> Result<layer::Committed> {
            let upper_tree = layer::DirTree::new(&upper);
            let trees: Vec<layer::DirTree> = lowers.iter().rev().map(layer::DirTree::new).collect();
            let refs: Vec<&dyn layer::Tree> = trees.iter().map(|t| t as &dyn layer::Tree).collect();
            let file = std::fs::File::create(&written)
                .with_context(|| format!("creating {}", written.display()))?;
            layer::commit(&upper_tree, &refs, BufWriter::new(file), std::io::sink())
                .context("making the layer")
        })
        .await
        .context("making the layer")
        .and_then(|r| r);
        let committed = match committed {
            Ok(committed) => committed,
            Err(e) => {
                let _ = std::fs::remove_file(&part);
                return Err(e);
            }
        };
        // The blob and its directory come in under the lock, and are kept by the build
        // before it is released: no cache entry refers to them yet.
        let _lock = store.lock().await?;
        let blob = store.blob_path(&committed.digest);
        if blob.exists() {
            let _ = std::fs::remove_file(&part);
        } else {
            std::fs::rename(&part, &blob).with_context(|| format!("keeping {}", blob.display()))?;
        }
        let digest = committed.digest.clone();
        tokio::task::spawn_blocking(move || {
            store.import_layer(&digest, LAYER_MEDIA_TYPE, &blob, Ownership::Root)
        })
        .await
        .context("extracting the layer")??;
        self.keep([committed.digest.clone()])?;
        Ok(committed)
    }

    /// The network namespace of RUN steps on the default bridge, made at the first one.
    async fn namespace(&mut self) -> Result<String> {
        if let Some(name) = &self.netns {
            return Ok(name.clone());
        }
        let ctx = self.ctx;
        let store = &ctx.store;
        let sd = ctx.sd().await?;
        let name = netns_of(&self.id);
        // Under the lock, as every start brings its networks up: the bridge is made
        // once, and the firewall table written from the networks as they are.
        let (net, addr) = {
            let _lock = store.lock().await?;
            let all = crate::api::network::all(store, &ctx.config)?;
            let net = all
                .iter()
                .find(|n| n.name == crate::bridge::DEFAULT_NETWORK)
                .cloned()
                .context("no default network")?;
            crate::bridge::up(&net, &all, sd, self.report).await?;
            let mut used: Vec<std::net::Ipv4Addr> = store
                .list_images_strict()?
                .iter()
                .filter_map(|r| crate::bridge::address_on(r, &net.name))
                .collect();
            used.extend(crate::bridge::build_addresses(store, &net.name));
            let addr = net.subnet.allocate(&used)?;
            std::fs::write(self.work.join("address"), format!("{} {addr}\n", net.name))
                .context("reserving the build's address")?;
            (net, addr)
        };
        crate::bridge::create_netns(
            &[(&net, addr)],
            &name,
            net.subnet.gateway(),
            &BTreeMap::new(),
        )?;
        self.netns = Some(name.clone());
        Ok(name)
    }

    /// The hosts and resolv.conf files of RUN steps.
    fn network_files(&self) -> Result<(PathBuf, PathBuf)> {
        let hosts = self.work.join("hosts");
        let mut text = String::from(
            "127.0.0.1\tlocalhost buildkitsandbox\n::1\tlocalhost ip6-localhost ip6-loopback\n",
        );
        // host-gateway is the host as the build's network reaches it: the gateway of the
        // default network, which the host has on its bridge whatever the step's network.
        let gateway = || -> Result<String> {
            let all = crate::api::network::all(&self.ctx.store, &self.ctx.config)?;
            let net = all
                .iter()
                .find(|n| n.name == crate::bridge::DEFAULT_NETWORK)
                .context("no default network")?;
            Ok(net.subnet.gateway().to_string())
        };
        for (host, ip) in &self.req.add_hosts {
            let ip = if ip == "host-gateway" {
                gateway()?
            } else {
                ip.clone()
            };
            text.push_str(&format!("{ip}\t{host}\n"));
        }
        std::fs::write(&hosts, text).context("writing the build's hosts file")?;
        let resolv = self.work.join("resolv.conf");
        std::fs::write(
            &resolv,
            crate::bridge::resolv_conf(&crate::bridge::upstream_dns(&self.ctx.config.dns), &[]),
        )
        .context("writing the build's resolv.conf")?;
        Ok((hosts, resolv))
    }

    async fn run_step(&mut self, n: usize, merged: &Path, run: &plan::Run) -> Result<()> {
        let req = self.req;
        let dir = self.work.join(format!("run-{n}"));
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let mut mounts = Vec::new();
        let mut env = run.env.clone();
        env.extend(run.proxy_env.iter().cloned());
        let mut secret_env: Vec<(String, Vec<u8>)> = Vec::new();
        let mut targets: Vec<String> = Vec::new();
        let mut locks = Vec::new();
        // The directory each cache of the step is in: two mounts of one cache are one
        // directory, as the reference gives a step one ref per cache, and neither waits
        // for the other's lock.
        let mut caches: HashMap<String, PathBuf> = HashMap::new();
        for (i, mount) in run.mounts.iter().enumerate() {
            match mount {
                RunMount::Bind {
                    target,
                    from,
                    source,
                    read_write,
                } => {
                    let src_root = self.source_root(from).await?;
                    let mut path = copy::root_path(&src_root, source)?;
                    if *read_write {
                        // A writable bind keeps nothing: the step writes into a copy.
                        let scratch = dir.join(format!("rw-{i}"));
                        std::fs::create_dir_all(&scratch)?;
                        // Off the runtime's workers, as long as the files take.
                        tokio::task::block_in_place(|| {
                            copy::docopy(
                                &src_root,
                                &scratch,
                                &copy::CopyAction {
                                    src: source.clone(),
                                    dest: "/data".to_string(),
                                    follow_symlink: true,
                                    create_dest_path: true,
                                    ..Default::default()
                                },
                            )
                        })?;
                        path = scratch.join("data");
                    }
                    targets.push(target.clone());
                    mounts.push(sandbox::Mount::Bind {
                        source: path,
                        target: target.clone(),
                        read_only: !read_write,
                    });
                }
                RunMount::Cache {
                    target,
                    id,
                    from,
                    sharing,
                    read_only,
                    mode,
                    uid,
                    gid,
                } => {
                    // Named as the reference keys a cache: its id and the base it starts
                    // from, the owner and mode it is made with or what from= seeds it
                    // with, so that another of these is another cache.
                    let base = match from {
                        Some((f, source)) => {
                            format!("{} {source}", self.source_identity(f, source).await?)
                        }
                        None => String::new(),
                    };
                    let key = cache_mount_name(id, *uid, *gid, *mode, &base);
                    if let Some(cache_dir) = caches.get(&key) {
                        targets.push(target.clone());
                        mounts.push(sandbox::Mount::Bind {
                            source: cache_dir.clone(),
                            target: target.clone(),
                            read_only: *read_only,
                        });
                        continue;
                    }
                    let mounts_dir = self.ctx.store.build_cache_dir().join("mounts");
                    // A private cache another step holds is taken in an instance of its
                    // own, as the reference makes one.
                    let mut instance = 0usize;
                    let cache_dir = loop {
                        let name = if instance == 0 {
                            key.clone()
                        } else {
                            format!("{key}-{instance}")
                        };
                        let cache_dir = mounts_dir.join(name);
                        if !cache_dir.exists() {
                            self.seed_cache(&cache_dir, from.as_ref(), *mode, *uid, *gid)
                                .await?;
                        }
                        if sharing != "private" {
                            break cache_dir;
                        }
                        match try_lock_dir(&cache_dir)? {
                            Some(lock) => {
                                locks.push(lock);
                                break cache_dir;
                            }
                            None => instance += 1,
                        }
                    };
                    // Its last use, which builder prune goes by: nothing inside need
                    // change for it.
                    let now = nix::sys::time::TimeSpec::UTIME_NOW;
                    let _ = nix::sys::stat::utimensat(
                        nix::fcntl::AT_FDCWD,
                        &cache_dir,
                        &now,
                        &now,
                        nix::sys::stat::UtimensatFlags::NoFollowSymlink,
                    );
                    // Waited for off the runtime's threads.
                    if sharing == "locked" {
                        let dir = cache_dir.clone();
                        let lock = tokio::select! {
                            lock = tokio::task::spawn_blocking(move || lock_dir(&dir)) => {
                                lock.context("locking a cache mount")??
                            }
                            () = self.cancel.cancelled() => bail!(CANCELLED),
                        };
                        locks.push(lock);
                    }
                    caches.insert(key, cache_dir.clone());
                    targets.push(target.clone());
                    mounts.push(sandbox::Mount::Bind {
                        source: cache_dir,
                        target: target.clone(),
                        read_only: *read_only,
                    });
                }
                RunMount::Tmpfs { target, size } => {
                    targets.push(target.clone());
                    // The kernel's tmpfs, 1777, as the executor mounts one: systemd-nspawn's
                    // --tmpfs alone would be 0755.
                    let mut options = "mode=1777,nosuid".to_string();
                    if let Some(size) = size {
                        options.push_str(&format!(",size={size}"));
                    }
                    mounts.push(sandbox::Mount::Tmpfs {
                        target: target.clone(),
                        options,
                    });
                }
                RunMount::Secret {
                    id,
                    target,
                    env: var,
                    required,
                    mode,
                    uid,
                    gid,
                } => {
                    let Some(content) = req.secrets.get(id) else {
                        if *required {
                            bail!("secret {id}: not found");
                        }
                        continue;
                    };
                    if let Some(var) = var {
                        if content.contains(&0) {
                            bail!("secret {id} holds a NUL byte, which a variable cannot");
                        }
                        secret_env.push((var.clone(), content.clone()));
                    }
                    if let Some(target) = target {
                        let file = dir.join(format!("secret-{i}"));
                        std::fs::write(&file, content)?;
                        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(*mode))?;
                        std::os::unix::fs::chown(&file, Some(*uid), Some(*gid))?;
                        targets.push(target.clone());
                        mounts.push(sandbox::Mount::Bind {
                            source: file,
                            target: target.clone(),
                            read_only: true,
                        });
                    }
                }
            }
        }
        if let Some(script) = &run.script {
            let scripts = dir.join("pipes");
            std::fs::create_dir_all(&scripts)?;
            let name = heredoc_file(&script.name)?;
            let file = scripts.join(&name);
            std::fs::write(&file, &script.data)?;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755))?;
            mounts.push(sandbox::Mount::Bind {
                source: file,
                target: format!("/dev/pipes/{name}"),
                read_only: true,
            });
        }
        let user = (!run.user.is_empty()).then(|| run.user.clone());
        if let Some(user) = &user {
            for bind in crate::getent::shim_in_dir(merged, user, &dir.join("getent"))? {
                mounts.push(sandbox::Mount::Bind {
                    source: bind.source,
                    target: bind.target,
                    read_only: true,
                });
            }
            targets.extend(crate::getent::TARGETS.iter().map(|t| t.to_string()));
        }
        let network = match self.network_of(run.network) {
            plan::Network::Default => sandbox::Network::Namespace(self.namespace().await?),
            plan::Network::None => sandbox::Network::None,
            plan::Network::Host => {
                if !req.allow_host_network {
                    bail!(HOST_NETWORK_REFUSED);
                }
                sandbox::Network::Host
            }
        };
        if run.insecure {
            bail!("RUN --security=insecure is not supported: steps run with the confinement of an app machine");
        }
        let (hosts, resolv) = self.network_files()?;
        let step = sandbox::Step {
            root: merged.to_path_buf(),
            machine: format!(
                "nspawn-build-{}",
                short(&cache::key(&[self.id.as_bytes(), &n.to_le_bytes()]))
            ),
            argv: run.argv.clone(),
            env,
            secret_env,
            workdir: if run.workdir.is_empty() {
                "/".to_string()
            } else {
                run.workdir.clone()
            },
            user: user.map(|u| {
                u.split_once(':')
                    .map_or_else(|| u.clone(), |(u, _)| u.to_string())
            }),
            network,
            hosts: Some(hosts),
            resolv: Some(resolv),
            mounts,
            shm_size: req.shm_size,
            ulimits: req.ulimits.clone(),
        };
        // A working directory that is not there is made, 0755 and the step's user's, as
        // the executor's MkdirAllAndChown makes it: the step's --chdir needs it.
        if !copy::root_path(merged, &step.workdir)?.exists() {
            workdir(merged, &step.workdir, &run.user, None)?;
        }
        let stubs = sandbox::Stubs::before(merged, &targets);
        let result = self.spawn(n, &step).await;
        stubs.clean();
        drop(locks);
        result
    }

    /// Runs systemd-nspawn for a step, its output going to the progress lines.
    async fn spawn(&self, n: usize, step: &sandbox::Step) -> Result<()> {
        let argv = sandbox::argv(step);
        let mut cmd = tokio::process::Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .envs(step.secret_env.iter().map(|(var, value)| {
                use std::os::unix::ffi::OsStrExt;
                (var.as_str(), std::ffi::OsStr::from_bytes(value))
            }))
            .env("SYSTEMD_NSPAWN_TMPFS_TMP", "0")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        if let Some(context) = crate::nsenter::unit_context() {
            crate::nsenter::in_context(&mut cmd, context);
        }
        let started = Instant::now();
        let mut child = cmd.spawn().context("running systemd-nspawn")?;
        let stdout = child.stdout.take().context("no output of the step")?;
        let stderr = child.stderr.take().context("no output of the step")?;
        let report = self.report;
        // Each stream read to its end by a task of its own: a stream nobody reads would hold
        // the step forever once its pipe filled.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(STEP_LINES_IN_TRANSIT);
        let readers: [Box<dyn tokio::io::AsyncRead + Send + Unpin>; 2] =
            [Box::new(stdout), Box::new(stderr)];
        let pumps = readers.map(|stream| tokio::spawn(pump(stream, tx.clone(), started)));
        drop(tx);
        let show = |text: &str| {
            line(
                report,
                format!("#{n} {:.3} {text}", started.elapsed().as_secs_f64()),
            );
        };
        // systemd-nspawn says when the step's process died of a signal, and exits 1 then,
        // or 0 for SIGINT: the executor reports 128 plus the signal.
        let mut signalled: Option<i32> = None;
        loop {
            tokio::select! {
                () = self.cancel.cancelled() => {
                    self.end_step(&mut child, step).await;
                    bail!(CANCELLED);
                }
                text = rx.recv() => match text {
                    Some(text) => match terminated_by(&text, &step.machine) {
                        Some(signal) => signalled = Some(signal),
                        None => show(&text),
                    },
                    None => break,
                },
            }
        }
        let status = tokio::select! {
            status = child.wait() => status.context("waiting for systemd-nspawn")?,
            () = self.cancel.cancelled() => {
                self.end_step(&mut child, step).await;
                bail!(CANCELLED);
            }
        };
        if !status.success() || signalled.is_some() {
            for pump in pumps {
                if let Ok(clip) = pump.await {
                    for kept in &clip.tail {
                        show(kept);
                    }
                }
            }
            let code = signalled.map_or_else(|| status.code().unwrap_or(1), |s| 128 + s);
            bail!(
                "process {:?} did not complete successfully: exit code: {code}",
                step.argv.join(" "),
            );
        }
        Ok(())
    }

    /// What a cache mount's from= takes, as the step's key names it.
    async fn source_identity(&mut self, from: &From, source: &str) -> Result<String> {
        Ok(match from {
            From::Context => {
                tokio::task::block_in_place(|| content_hash(&self.req.context, source, &[]))?
            }
            From::Stage(i) => self
                .states
                .get(i)
                .map(State::made)
                .with_context(|| format!("stage {i} is not built"))?,
            From::Image(reference) => self.base(reference).await?.key,
            From::Scratch => "scratch".to_string(),
        })
    }

    /// Makes a cache mount's directory: beside its name, seeded, then put in place unless
    /// another build put one there meanwhile, whose stays.
    async fn seed_cache(
        &mut self,
        cache_dir: &Path,
        from: Option<&(From, String)>,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<()> {
        let staged = cache_dir.with_file_name(format!(".new-{}", crate::store::unique_suffix()));
        std::fs::create_dir_all(&staged)
            .with_context(|| format!("creating {}", staged.display()))?;
        let seeded: Result<()> = async {
            if let Some((f, source)) = from {
                let src_root = self.source_root(f).await?;
                tokio::task::block_in_place(|| {
                    copy::docopy(
                        &src_root,
                        &staged,
                        &copy::CopyAction {
                            src: source.clone(),
                            dest: "/".to_string(),
                            follow_symlink: true,
                            dir_copy_contents: true,
                            create_dest_path: true,
                            ..Default::default()
                        },
                    )
                })?;
            }
            std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(mode))?;
            std::os::unix::fs::chown(&staged, Some(uid), Some(gid))?;
            Ok(())
        }
        .await;
        if let Err(e) = seeded {
            let _ = crate::store::remove_tree(&staged);
            return Err(e);
        }
        // Without replacing: a rename over another build's directory, still empty, would
        // give the two of them different directories under one name.
        match nix::fcntl::renameat2(
            nix::fcntl::AT_FDCWD,
            &staged,
            nix::fcntl::AT_FDCWD,
            cache_dir,
            nix::fcntl::RenameFlags::RENAME_NOREPLACE,
        ) {
            Ok(()) => Ok(()),
            Err(nix::errno::Errno::EEXIST) => {
                let _ = crate::store::remove_tree(&staged);
                Ok(())
            }
            Err(e) => {
                let _ = crate::store::remove_tree(&staged);
                Err(anyhow!("putting {} in place: {e}", cache_dir.display()))
            }
        }
    }

    /// Ends a step cut short: systemd-nspawn, and the payload, which goes on without it
    /// (the payload is the init of its PID namespace, systemd-nspawn only its parent) in
    /// the scope systemd-nspawn moved it to.
    async fn end_step(&self, child: &mut tokio::process::Child, step: &sandbox::Step) {
        let _ = child.kill().await;
        if let Ok(sd) = self.ctx.sd().await {
            end_scope(sd, &format!("{}.scope", step.machine)).await;
        }
    }

    async fn copy_step(&mut self, n: usize, merged: &Path, c: &plan::Copy) -> Result<()> {
        let dir = self.work.join(format!("copy-{n}"));
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let owner = tokio::task::block_in_place(|| {
            c.chown.as_ref().map(|chown| resolve_chown(merged, chown))
        });
        let chmod = c.chmod.as_deref().map(copy::parse_chmod).transpose()?;
        // Each source with how its name is taken: a heredoc's and a download's as they are,
        // never as a pattern, as the reference copies them; a heredoc never as an archive.
        #[derive(Clone, Copy, PartialEq)]
        enum Named {
            Pattern,
            Inline,
            Url,
        }
        let mut sources: Vec<(PathBuf, String, Named)> = Vec::new();
        for source in &c.sources {
            if c.add && is_url(source) {
                let found = self.download(source, c.checksum.as_deref()).await?;
                sources.push((found.dir, found.name, Named::Url));
                continue;
            }
            if c.add && source.contains("://") {
                bail!("ADD of {source}: only local files and http or https URLs are taken");
            }
            let root = self.source_root(&c.from).await?;
            sources.push((root, source.clone(), Named::Pattern));
        }
        // The heredocs after the paths, as dispatchCopy copies them: one wins a name
        // they share.
        for (i, file) in c.inline.iter().enumerate() {
            let inline = dir.join(format!("inline-{i}"));
            std::fs::create_dir_all(&inline)?;
            let name = heredoc_file(&file.name)?;
            std::fs::write(inline.join(&name), &file.data)?;
            std::fs::set_permissions(inline.join(&name), std::fs::Permissions::from_mode(0o644))?;
            sources.push((inline, name, Named::Inline));
        }
        // A URL is copied as it came unless --unpack says otherwise, a local archive
        // unpacked unless --unpack=false says so.
        let unpack = |r: &copy::UnpackRequest<'_>| -> Result<bool> {
            let url = is_url_download(r.src_root, &self.work);
            if !c.add || c.unpack == Some(false) || (url && c.unpack != Some(true)) {
                return Ok(false);
            }
            unpack_archive(r, &dir)
        };
        for (src_root, source, named) in &sources {
            let literal = *named != Named::Pattern;
            let mut action = source_action(source, &c.dest, c.parents, literal);
            action.owner = owner;
            if let Some(chmod) = chmod.clone() {
                action.set_chmod(chmod);
            }
            action.exclude_patterns.clone_from(&c.exclude);
            if c.add && *named != Named::Inline {
                action.unpack = Some(&unpack);
            }
            // Off the runtime's worker while it copies, which takes as long as the files.
            tokio::task::block_in_place(|| copy::docopy(src_root, merged, &action))?;
        }
        Ok(())
    }

    /// The image of the target: its blobs in the store, a local image per tag.
    async fn finish(&mut self, state: State) -> Result<Built> {
        let ctx = self.ctx;
        let store = &ctx.store;
        let config_bytes = state.config.to_bytes();
        let config_digest = crate::layout::sha256_digest(&config_bytes);
        let layers: Vec<(String, String, u64)> = state
            .layers
            .iter()
            .map(|l| (l.digest.clone(), l.media_type.clone(), l.size))
            .collect();
        let manifest_bytes = image::manifest(&config_bytes, &layers, &BTreeMap::new());
        let manifest_digest = crate::layout::sha256_digest(&manifest_bytes);
        {
            let _lock = store.lock().await?;
            let path = store.blob_path(&config_digest);
            if !path.exists() {
                std::fs::write(&path, &config_bytes)
                    .with_context(|| format!("writing {}", path.display()))?;
            }
            self.keep([config_digest.clone()])?;
        }
        let manifest: oci_client::manifest::OciImageManifest =
            serde_json::from_slice(&manifest_bytes).context("the image's manifest")?;
        let mut names = Vec::new();
        let sd = ctx.sd().await?;
        let tags = tag_names(&self.req.tags, &ctx.config.registry)?;
        // Every name is looked at before one is replaced: a refused tag leaves the others
        // as they were. What changes between the look and the install is said by name.
        for (name, reference) in &tags {
            let rebuilt = store
                .load_image(name)?
                .is_some_and(|r| replaced_by_build(store, &r, reference));
            crate::install::ensure_no_pending_rollback(store, name, self.req.force)?;
            crate::install::ensure_replaceable(store, sd, name, self.req.force || rebuilt).await?;
        }
        for (name, reference) in tags {
            let installed = self
                .install_tag(
                    sd,
                    &manifest,
                    &manifest_bytes,
                    &manifest_digest,
                    &name,
                    &reference,
                )
                .await;
            if let Err(e) = installed {
                if names.is_empty() {
                    return Err(e);
                }
                return Err(e.context(format!("{} installed already", names.join(", "))));
            }
            names.push(name);
        }
        if names.is_empty() {
            note(
                self.report,
                "note: no -t: the image stays in the build cache only".to_string(),
            );
        }
        Ok(Built {
            id: config_digest,
            manifest_digest,
            names,
        })
    }

    /// One tag's image installed under the store lock, replacing what had the name.
    async fn install_tag(
        &self,
        sd: &crate::systemd::Systemd,
        manifest: &oci_client::manifest::OciImageManifest,
        manifest_bytes: &[u8],
        manifest_digest: &str,
        name: &str,
        reference: &str,
    ) -> Result<()> {
        let ctx = self.ctx;
        let store = &ctx.store;
        let choice = if self.req.backend == BackendChoice::Auto {
            ctx.config.backend
        } else {
            self.req.backend
        };
        let backend = Backend::choose(choice, sd).await?;
        let _lock = store.lock().await?;
        // A tag built again replaces the image of that reference it built or pulled
        // before, as long as nothing ran from it: a machine goes only with --force.
        let rebuilt = store
            .load_image(name)?
            .is_some_and(|r| replaced_by_build(store, &r, reference));
        crate::install::ensure_no_pending_rollback(store, name, self.req.force)?;
        crate::install::ensure_replaceable(store, sd, name, self.req.force || rebuilt).await?;
        crate::install::stage_layers(store, backend, manifest, self.req.mode, name).await?;
        crate::install::remove_existing(store, sd, name, self.report).await?;
        store.forget_rollback(name)?;
        crate::install::install(
            store,
            sd,
            &ctx.config,
            backend,
            crate::install::Install {
                name,
                reference,
                manifest_bytes,
                manifest,
                manifest_digest,
                index_digest: None,
                origin: "build",
                mode: self.req.mode,
                signed_by: None,
                signed_at: None,
            },
            self.report,
        )
        .await?;
        crate::api::events::emit_machine(
            store,
            "build",
            name,
            &[("image", reference), ("reference", reference)],
        );
        Ok(())
    }
}

/// The local name and the reference of each tag, refused before anything is built when
/// one is no valid tag, carries a digest, or names an image another tag names.
pub fn tag_names(tags: &[String], registry: &str) -> Result<Vec<(String, String)>> {
    let mut out: Vec<(String, String)> = Vec::new();
    for tag in tags {
        let image = ImageRef::parse(tag, registry)?;
        if image.digest.is_some() {
            bail!("a build tag cannot carry a digest: {tag}");
        }
        let name = image.local_name();
        crate::reference::validate_machine_name(&name)?;
        let reference = image.to_string();
        if let Some((_, other)) = out.iter().find(|(n, _)| *n == name) {
            if *other == reference {
                continue;
            }
            bail!("the tags {other} and {reference} would both be the local image {name}");
        }
        out.push((name, reference));
    }
    Ok(out)
}

/// Whether a build tagged `reference` replaces `record` without --force: an image of
/// that reference built or pulled before, that nothing ran from.
fn replaced_by_build(store: &crate::store::Store, record: &ImageRecord, reference: &str) -> bool {
    matches!(record.origin.as_str(), "build" | "pull")
        && record.reference == reference
        && crate::api::disk::image_only(store, record)
}

fn short(digest: &str) -> String {
    digest
        .strip_prefix("sha256:")
        .unwrap_or(digest)
        .chars()
        .take(12)
        .collect()
}

/// The limits on what a stream of a step prints: 200 KiB a second since it started,
/// 2 MiB in all. Past them the output is dropped, said once each time, until it is within
/// them again; from the first line dropped on, the last 256 KiB are kept, shown if the step
/// fails.
#[derive(Default)]
struct Clip {
    size: usize,
    clipping: bool,
    tail: std::collections::VecDeque<String>,
    tail_size: usize,
}

impl Clip {
    const MAX: usize = 2 << 20;
    const SPEED: usize = 200 << 10;
    const TAIL: usize = 256 << 10;

    /// What of `text`, a line, is shown.
    fn take(&mut self, text: &str, elapsed: Duration) -> Vec<String> {
        let old = self.size;
        let wanted = text.len() + 1;
        self.size = self.size.saturating_add(wanted);
        let by_speed = (elapsed.as_secs_f64().ceil() as usize).saturating_mul(Self::SPEED);
        let (max, limit) = if by_speed > Self::MAX {
            (Self::MAX, "2MiB")
        } else {
            (by_speed, "200KiB/s")
        };
        let room = max.saturating_sub(old).min(wanted);
        if room < wanted || !self.tail.is_empty() {
            self.keep(text);
        }
        if room == wanted {
            self.clipping = false;
            return vec![text.to_string()];
        }
        if self.clipping {
            return Vec::new();
        }
        self.clipping = true;
        let mut cut = room.min(text.len());
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        let mut shown: Vec<String> = Vec::new();
        if cut > 0 {
            shown.push(text[..cut].to_string());
        }
        shown.push(format!("[output clipped, log limit {limit} reached]"));
        shown
    }

    fn keep(&mut self, text: &str) {
        self.tail_size += text.len() + 1;
        self.tail.push_back(text.to_string());
        while self.tail_size > Self::TAIL {
            let Some(old) = self.tail.pop_front() else {
                break;
            };
            self.tail_size -= old.len() + 1;
        }
    }
}

/// The directory name of a cache mount: its id and the base it starts from.
fn cache_mount_name(id: &str, uid: u32, gid: u32, mode: u32, base: &str) -> String {
    short(&cache::key(&[
        id.as_bytes(),
        uid.to_string().as_bytes(),
        gid.to_string().as_bytes(),
        format!("{mode:o}").as_bytes(),
        base.as_bytes(),
    ]))
}

/// The signal systemd-nspawn's `Container NAME terminated by signal KILL.` names, for the
/// step's own machine.
fn terminated_by(line: &str, machine: &str) -> Option<i32> {
    let name = line
        .strip_prefix(&format!("Container {machine} terminated by signal "))?
        .strip_suffix('.')?;
    let signal: nix::sys::signal::Signal = format!("SIG{name}").parse().ok()?;
    Some(signal as i32)
}

/// The lock of a directory if nobody holds it, None if somebody does.
fn try_lock_dir(dir: &Path) -> Result<Option<nix::fcntl::Flock<std::fs::File>>> {
    let file = std::fs::File::open(dir).with_context(|| format!("opening {}", dir.display()))?;
    match nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusiveNonblock) {
        Ok(lock) => Ok(Some(lock)),
        Err((_, nix::errno::Errno::EWOULDBLOCK)) => Ok(None),
        Err((_, e)) => Err(anyhow!("locking {}: {e}", dir.display())),
    }
}

/// The lock of a directory, waited for: a cache mount with sharing=locked is held by one
/// step at a time.
fn lock_dir(dir: &Path) -> Result<nix::fcntl::Flock<std::fs::File>> {
    let file = std::fs::File::open(dir).with_context(|| format!("opening {}", dir.display()))?;
    nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusive)
        .map_err(|(_, e)| anyhow!("locking {}: {e}", dir.display()))
}

/// WORKDIR: the directory made, with its missing parents, owned by `user`.
/// The directories made get `epoch` as their times when SOURCE_DATE_EPOCH gives one.
fn workdir(merged: &Path, path: &str, user: &str, epoch: Option<i64>) -> Result<()> {
    let target = copy::root_path(merged, path)?;
    // One there already is kept; a file or anything else there is refused by mkdir_all
    // (ENOTDIR), as fsutil's MkdirAll refuses it.
    if target.is_dir() {
        return Ok(());
    }
    let owner = (!user.is_empty()).then(|| resolve_chown(merged, user));
    let chown = |u: Option<copy::User>| -> Result<Option<copy::User>> { Ok(owner.or(u)) };
    let time = epoch.map(|e| nix::sys::time::TimeSpec::new(e, 0));
    copy::mkdir_all(&target, 0o755, Some(&chown), time)?;
    Ok(())
}

/// The copy of one source of COPY or ADD: a name taken as it is (a heredoc's, a
/// download's) is no pattern and has no parents.
fn source_action(
    source: &str,
    dest: &str,
    parents: bool,
    literal: bool,
) -> copy::CopyAction<'static> {
    let src = copy::copy_source(source, parents && !literal);
    let mut action = copy::CopyAction::for_instruction(&src, dest);
    action.allow_wildcard &= !literal;
    action
}

/// USER[:GROUP] by name or number against the root's passwd and group files, as the
/// reference resolves --chown: a name the files lack is 0.
fn resolve_chown(root: &Path, spec: &str) -> copy::User {
    // A regular file only: a FIFO an image has there would hold the open for ever, and a
    // device node would be opened on the host.
    let read = |file: &str| -> String {
        copy::root_path(root, file)
            .ok()
            .and_then(|p| {
                let file = crate::volume::open_regular(&p).ok()?;
                let mut bytes = Vec::new();
                file.take(10 << 20).read_to_end(&mut bytes).ok()?;
                Some(String::from_utf8_lossy(&bytes).into_owned())
            })
            .unwrap_or_default()
    };
    let (user, group) = match spec.split_once(':') {
        Some((u, g)) => (u, Some(g)),
        None => (spec, None),
    };
    let mut out = copy::User { uid: 0, gid: 0 };
    if !user.is_empty() {
        match user.parse::<u32>() {
            Ok(id) => out = copy::User { uid: id, gid: id },
            Err(_) => {
                if let Some(fields) = read("/etc/passwd")
                    .lines()
                    .map(|l| l.split(':').collect::<Vec<_>>())
                    .find(|f| f.first() == Some(&user))
                {
                    out.uid = fields.get(2).and_then(|v| v.parse().ok()).unwrap_or(0);
                    out.gid = fields.get(3).and_then(|v| v.parse().ok()).unwrap_or(0);
                }
            }
        }
    }
    if let Some(group) = group {
        match group.parse::<u32>() {
            Ok(id) => out.gid = id,
            Err(_) => {
                if let Some(gid) = read("/etc/group").lines().find_map(|l| {
                    let f: Vec<&str> = l.split(':').collect();
                    (f.first() == Some(&group)).then(|| f.get(2)?.parse().ok())?
                }) {
                    out.gid = gid;
                }
            }
        }
    }
    out
}

/// A digest of what a source of the context names: every path it matches as the copy
/// finds them (wildcards in any component, the source's own symlinks followed), and
/// below each the names, kinds, modes, owners, file capabilities, link targets and
/// contents.
fn content_hash(context: &Path, path: &str, exclude: &[String]) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(path.as_bytes());
    // What --exclude leaves out of the copy is left out of its key, as the selectors of
    // the copy's content checksum leave it.
    let excluded = if exclude.is_empty() {
        None
    } else {
        Some(crate::build::ignore::PatternMatcher::new(exclude).map_err(|e| anyhow!("{e}"))?)
    };
    // A source that matches nothing fails in the copy itself.
    for found in copy::resolve_wildcards(context, path, true).unwrap_or_default() {
        let found = found.to_string_lossy().into_owned();
        hash.update(b"\0match\0");
        hash.update(found.as_bytes());
        let Ok(real) = copy::root_path(context, &found) else {
            continue;
        };
        hash_tree(&mut hash, &real, excluded.as_ref())?;
    }
    Ok(format!("sha256:{}", hex::encode(hash.finalize())))
}

/// Everything below `top` into `hash`, by paths relative to it, without following
/// symlinks, but what `exclude` matches.
fn hash_tree(
    hash: &mut sha2::Sha256,
    top: &Path,
    exclude: Option<&crate::build::ignore::PatternMatcher>,
) -> Result<()> {
    use sha2::Digest;
    use std::os::unix::fs::MetadataExt;
    let mut stack = vec![top.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(meta) = p.symlink_metadata() else {
            continue;
        };
        let relative = p
            .strip_prefix(top)
            .unwrap_or(&p)
            .to_string_lossy()
            .into_owned();
        if let Some(m) = exclude.filter(|_| !relative.is_empty()) {
            if m.matches(&relative).map_err(|e| anyhow!("{e}"))? {
                // A directory whose patterns could take something below back is gone
                // through all the same.
                if meta.is_dir() && m.exclusions() {
                    let mut children: Vec<PathBuf> = std::fs::read_dir(&p)
                        .with_context(|| format!("reading {}", p.display()))?
                        .flatten()
                        .map(|e| e.path())
                        .collect();
                    children.sort();
                    children.reverse();
                    stack.extend(children);
                }
                continue;
            }
        }
        hash.update(b"\0entry\0");
        hash.update(
            p.strip_prefix(top)
                .unwrap_or(&p)
                .as_os_str()
                .as_encoded_bytes(),
        );
        hash.update(meta.mode().to_le_bytes());
        hash.update(meta.uid().to_le_bytes());
        hash.update(meta.gid().to_le_bytes());
        if meta.file_type().is_symlink() {
            hash.update(std::fs::read_link(&p)?.as_os_str().as_encoded_bytes());
        } else if meta.is_file() {
            hash.update(meta.len().to_le_bytes());
            // The file capabilities, the one extended attribute a layer keeps.
            if let Ok(Some(capability)) = xattr::get(&p, "security.capability") {
                hash.update(b"\0capability\0");
                hash.update(&capability);
            }
            let mut file = crate::volume::open_regular(&p)
                .with_context(|| format!("reading {}", p.display()))?;
            let mut buf = vec![0u8; 1 << 16];
            loop {
                let n = file.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hash.update(&buf[..n]);
            }
        } else if meta.is_dir() {
            let mut children: Vec<PathBuf> = std::fs::read_dir(&p)
                .with_context(|| format!("reading {}", p.display()))?
                .flatten()
                .map(|e| e.path())
                .collect();
            children.sort();
            children.reverse();
            stack.extend(children);
        } else {
            hash.update(meta.rdev().to_le_bytes());
        }
    }
    Ok(())
}

/// Whether a source root is a directory an ADD of a URL downloaded into.
fn is_url_download(src_root: &Path, dir: &Path) -> bool {
    src_root
        .strip_prefix(dir)
        .ok()
        .and_then(|r| r.to_str())
        .is_some_and(|r| r.starts_with("url-"))
}

/// The local record a FROM takes: the image of its reference, or one built here under
/// the name `build -t` gave it.
fn local_base<'a>(
    records: &'a [ImageRecord],
    wanted: &str,
    built: Option<&str>,
) -> Option<&'a ImageRecord> {
    records
        .iter()
        .find(|r| r.reference == wanted && r.origin != "create")
        .or_else(|| {
            records
                .iter()
                .find(|r| Some(r.reference.as_str()) == built && r.origin == "build")
        })
}

/// The longest line of a step's output taken as one; a longer one comes in pieces.
const STEP_LINE: u64 = 64 * 1024;

/// Lines of a step read and not yet shown.
const STEP_LINES_IN_TRANSIT: usize = 1024;

/// One output stream of a step, read to its end a line at a time, as bytes (a line that is
/// no UTF-8 is still a line), and what the clip lets through sent on; the clip comes back,
/// with the tail a failure shows. A step printing faster than its lines are taken waits on
/// its pipe, and that wait is no time of the step's: the clip goes by the time the step
/// took to write, so counting it would let a step print more the slower its lines are
/// shown.
async fn pump(
    stream: impl tokio::io::AsyncRead + Unpin,
    tx: tokio::sync::mpsc::Sender<String>,
    started: Instant,
) -> Clip {
    let mut clip = Clip::default();
    let mut waited = Duration::ZERO;
    let mut reader = tokio::io::BufReader::new(stream);
    while let Ok(Some(text)) = step_line(&mut reader).await {
        for shown in clip.take(&text, started.elapsed().saturating_sub(waited)) {
            let sending = Instant::now();
            if tx.send(shown).await.is_err() {
                return clip;
            }
            waited += sending.elapsed();
        }
    }
    clip
}

/// The next line of a step's output, whatever its bytes, without its newline.
async fn step_line<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<String>> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};
    let mut bytes = Vec::new();
    if (&mut *reader)
        .take(STEP_LINE)
        .read_until(b'\n', &mut bytes)
        .await?
        == 0
    {
        return Ok(None);
    }
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

/// The file a heredoc becomes: its name cleaned against `/`, as the reference makes it in an
/// empty root, where anything but one plain name has no directory to land in.
fn heredoc_file(name: &str) -> Result<String> {
    let mut parts: Vec<&str> = Vec::new();
    for part in name.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    match parts.as_slice() {
        [one] => Ok(one.to_string()),
        _ => bail!("the heredoc {name:?} names no file in an empty root"),
    }
}

fn is_url(source: &str) -> bool {
    source.starts_with("http://") || source.starts_with("https://")
}

/// How long a download may go without a byte.
const DOWNLOAD_STALL: std::time::Duration = std::time::Duration::from_secs(120);

/// ADD of a URL: the file written into `dir` as it comes, hashed on the way; its name
/// and digest.
async fn fetch_url(url: &str, dir: &Path) -> Result<Downloaded> {
    use sha2::{Digest, Sha256};
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::create_dir_all(dir)?;
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()
        .context("making the HTTP client")?;
    let mut response = tokio::time::timeout(DOWNLOAD_STALL, client.get(url).send())
        .await
        .with_context(|| format!("fetching {url}: no answer"))?
        .with_context(|| format!("fetching {url}"))?
        .error_for_status()
        .with_context(|| format!("fetching {url}"))?;
    let last_modified = response
        .headers()
        .get(reqwest::header::LAST_MODIFIED)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    // Named after the URL asked for, decoded, as the reference names it: not after
    // where a redirect led.
    let name = crate::build::dispatch::url_file_name(url);
    if name == ".." {
        bail!("ADD of {url}: the URL names no file");
    }
    let path = dir.join(&name);
    let mut file = std::fs::File::options()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("creating {}", path.display()))?;
    let mut hash = Sha256::new();
    loop {
        let chunk = tokio::time::timeout(DOWNLOAD_STALL, response.chunk())
            .await
            .with_context(|| format!("fetching {url}: stalled"))?
            .with_context(|| format!("fetching {url}"))?;
        let Some(chunk) = chunk else { break };
        hash.update(&chunk);
        file.write_all(&chunk)
            .with_context(|| format!("writing {}", path.display()))?;
    }
    // Last-Modified is the file's time, the epoch without one that parses.
    let modified = last_modified.as_deref().and_then(http_time);
    let at = |nanos: i64| {
        let since = Duration::from_nanos(nanos.unsigned_abs());
        if nanos < 0 {
            std::time::UNIX_EPOCH - since
        } else {
            std::time::UNIX_EPOCH + since
        }
    };
    let time = at(modified.unwrap_or(0));
    file.set_times(
        std::fs::FileTimes::new()
            .set_accessed(time)
            .set_modified(time),
    )
    .with_context(|| format!("setting the times of {}", path.display()))?;
    Ok(Downloaded {
        dir: dir.to_path_buf(),
        name,
        digest: format!("sha256:{}", hex::encode(hash.finalize())),
        modified,
    })
}

/// A file an ADD of a URL fetched: where it is, its digest, and the time its
/// Last-Modified gave, in nanoseconds since the epoch.
#[derive(Debug, Clone)]
struct Downloaded {
    dir: PathBuf,
    name: String,
    digest: String,
    modified: Option<i64>,
}

/// A Last-Modified value as net/http's ParseTime reads it: the form of http.TimeFormat,
/// RFC 850's or asctime's, as time.Parse takes each; in nanoseconds since the epoch.
fn http_time(text: &str) -> Option<i64> {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const LONG_DAYS: [&str; 7] = [
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
    ];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    struct Cursor<'a>(&'a str);
    impl Cursor<'_> {
        fn lit(&mut self, s: &str) -> Option<()> {
            self.0 = self.0.strip_prefix(s)?;
            Some(())
        }
        /// The first of `names` the text starts with, letters in either case.
        fn name(&mut self, names: &[&str]) -> Option<i64> {
            let (i, n) = names.iter().enumerate().find(|(_, n)| {
                self.0
                    .get(..n.len())
                    .is_some_and(|head| head.eq_ignore_ascii_case(n))
            })?;
            self.0 = &self.0[n.len()..];
            Some(i as i64)
        }
        /// getnum: two digits, or one when `fixed` is not asked for.
        fn num(&mut self, fixed: bool) -> Option<i64> {
            let b = self.0.as_bytes();
            if !b.first()?.is_ascii_digit() {
                return None;
            }
            let n = if b.get(1).is_some_and(u8::is_ascii_digit) {
                2
            } else if fixed {
                return None;
            } else {
                1
            };
            let v = self.0[..n].parse().ok()?;
            self.0 = &self.0[n..];
            Some(v)
        }
        fn year(&mut self) -> Option<i64> {
            let head = self.0.get(..4)?;
            if !head.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            self.0 = &self.0[4..];
            head.parse().ok()
        }
        /// Seconds, with the fraction time.Parse takes after them though the layout has
        /// none; in nanoseconds.
        fn seconds(&mut self) -> Option<i64> {
            let sec = self.num(true)?;
            let b = self.0.as_bytes();
            let mut nanos = 0;
            if b.len() >= 2 && matches!(b[0], b'.' | b',') && b[1].is_ascii_digit() {
                let n = 1 + b[1..].iter().take_while(|c| c.is_ascii_digit()).count();
                let digits: String = self.0[1..n]
                    .chars()
                    .chain(std::iter::repeat('0'))
                    .take(9)
                    .collect();
                if n - 1 > 9 {
                    return None;
                }
                nanos = digits.parse().ok()?;
                self.0 = &self.0[n..];
            }
            (sec < 60).then_some(sec * 1_000_000_000 + nanos)
        }
        /// A zone as time.Parse takes one for MST: UTC, GMT with an hour offset or not,
        /// a signed hour offset, or three to five capitals. The instant stays the one of
        /// the clock read as UTC, as it does where the local zone does not name the zone.
        fn zone(&mut self) -> Option<()> {
            if self.lit("UTC").is_some() {
                return Some(());
            }
            let signed = |s: &str| -> usize {
                let Some(rest) = s.strip_prefix(['+', '-']) else {
                    return 0;
                };
                let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
                match rest[..digits].parse::<u64>() {
                    Ok(x) if x <= 23 => 1 + digits,
                    _ => 0,
                }
            };
            let n = if self.0.starts_with("ChST") || self.0.starts_with("MeST") {
                4
            } else if let Some(rest) = self.0.strip_prefix("GMT") {
                3 + signed(rest)
            } else if self.0.starts_with(['+', '-']) {
                signed(self.0)
            } else {
                let b = self.0.as_bytes();
                match b
                    .iter()
                    .take(6)
                    .take_while(|c| c.is_ascii_uppercase())
                    .count()
                {
                    3 => 3,
                    4 if b[3] == b'T' || self.0.starts_with("WITA") => 4,
                    5 if b[4] == b'T' => 5,
                    _ => 0,
                }
            };
            if n == 0 {
                return None;
            }
            self.0 = &self.0[n..];
            Some(())
        }
    }
    let leap = |y: i64| (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let at = |year: i64, month: i64, day: i64, hour: i64, min: i64, nanos: i64| {
        let mdays = [
            31,
            if leap(year) { 29 } else { 28 },
            31,
            30,
            31,
            30,
            31,
            31,
            30,
            31,
            30,
            31,
        ];
        if day < 1 || day > mdays[month as usize] || hour > 23 || min > 59 {
            return None;
        }
        let y = year - 1;
        let days = 365 * (year - 1970) + (y.div_euclid(4) - y.div_euclid(100) + y.div_euclid(400))
            - (1969 / 4 - 1969 / 100 + 1969 / 400)
            + mdays[..month as usize].iter().sum::<i64>()
            + day
            - 1;
        let secs = days * 86_400 + hour * 3600 + min * 60;
        secs.checked_mul(1_000_000_000)?.checked_add(nanos)
    };
    // Mon, 02 Jan 2006 15:04:05 GMT
    let fixdate = || {
        let mut c = Cursor(text);
        c.name(&DAYS)?;
        c.lit(", ")?;
        let day = c.num(true)?;
        c.lit(" ")?;
        let month = c.name(&MONTHS)?;
        c.lit(" ")?;
        let year = c.year()?;
        c.lit(" ")?;
        let hour = c.num(false)?;
        c.lit(":")?;
        let min = c.num(true)?;
        c.lit(":")?;
        let nanos = c.seconds()?;
        c.lit(" GMT")?;
        c.0.is_empty().then_some(())?;
        at(year, month, day, hour, min, nanos)
    };
    // Monday, 02-Jan-06 15:04:05 MST
    let rfc850 = || {
        let mut c = Cursor(text);
        c.name(&LONG_DAYS)?;
        c.lit(", ")?;
        let day = c.num(true)?;
        c.lit("-")?;
        let month = c.name(&MONTHS)?;
        c.lit("-")?;
        let yy = c.num(true)?;
        let year = if yy >= 69 { 1900 + yy } else { 2000 + yy };
        c.lit(" ")?;
        let hour = c.num(false)?;
        c.lit(":")?;
        let min = c.num(true)?;
        c.lit(":")?;
        let nanos = c.seconds()?;
        c.lit(" ")?;
        c.zone()?;
        c.0.is_empty().then_some(())?;
        at(year, month, day, hour, min, nanos)
    };
    // Mon Jan _2 15:04:05 2006
    let asctime = || {
        let mut c = Cursor(text);
        c.name(&DAYS)?;
        c.lit(" ")?;
        let month = c.name(&MONTHS)?;
        c.lit(" ")?;
        let _ = c.lit(" ");
        let day = c.num(false)?;
        c.lit(" ")?;
        let hour = c.num(false)?;
        c.lit(":")?;
        let min = c.num(true)?;
        c.lit(":")?;
        let nanos = c.seconds()?;
        c.lit(" ")?;
        let year = c.year()?;
        c.0.is_empty().then_some(())?;
        at(year, month, day, hour, min, nanos)
    };
    fixdate().or_else(rfc850).or_else(asctime)
}

/// The contents of an archive compressed as `kind`; xz, whose decoder writes, through a
/// file in `dir`.
fn decompressed(
    kind: Compression,
    file: std::fs::File,
    dir: &Path,
) -> Result<Box<dyn std::io::Read>> {
    Ok(match kind {
        Compression::Gzip => Box::new(flate2::read::MultiGzDecoder::new(file)),
        Compression::Bzip2 => Box::new(bzip2::read::MultiBzDecoder::new(file)),
        Compression::Zstd => Box::new(zstd::stream::read::Decoder::new(file)?),
        Compression::Xz => {
            let path = dir.join(format!("xz-{}", crate::store::unique_suffix()));
            let mut out = std::fs::File::options()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
                .with_context(|| format!("creating {}", path.display()))?;
            let _ = std::fs::remove_file(&path);
            lzma_rs::xz_decompress(&mut std::io::BufReader::new(file), &mut out)
                .map_err(|e| anyhow!("xz: {e:?}"))?;
            std::io::Seek::rewind(&mut out)?;
            Box::new(out)
        }
        Compression::None => Box::new(file),
    })
}

/// ADD of a local archive, unpacked into the destination as the reference's unpack does
/// it: a regular file that decompresses, if it is compressed, to something a tar reader
/// takes a first header from. False when the source is no archive, which is copied as it
/// is then.
fn unpack_archive(r: &copy::UnpackRequest<'_>, dir: &Path) -> Result<bool> {
    use crate::build::walk::path_bytes;
    let src = bytes_path(&copy::root_path_bytes(
        path_bytes(r.src_root),
        path_bytes(r.src),
    )?);
    if !std::fs::metadata(&src).is_ok_and(|m| m.is_file()) {
        return Ok(false);
    }
    let mut head = Vec::with_capacity(10);
    std::fs::File::open(&src)?
        .take(10)
        .read_to_end(&mut head)
        .with_context(|| format!("reading {}", src.display()))?;
    let Ok(reader) = decompressed(untar::detect(&head), std::fs::File::open(&src)?, dir) else {
        return Ok(false);
    };
    let mut archive = tar::Archive::new(reader);
    let Ok(entries) = archive.entries() else {
        return Ok(false);
    };
    let mut entries = entries.peekable();
    if !matches!(entries.peek(), Some(Ok(_))) {
        return Ok(false);
    }
    let dest = bytes_path(&copy::root_path_bytes(
        path_bytes(r.dest_root),
        path_bytes(r.dest),
    )?);
    let owner = r.owner;
    let chowner = move |_: Option<copy::User>| -> Result<Option<copy::User>> { Ok(owner) };
    copy::mkdir_all(
        &dest,
        0o755,
        Some(&chowner),
        r.timestamp.map(copy::timespec_from_nanos),
    )?;
    untar::unpack(entries, &dest, r.owner)
        .with_context(|| format!("unpacking {}", r.src.display()))?;
    Ok(true)
}

/// A build whose context comes as a tar stream on `context`: unpacked into the build's
/// work directory as an archive is, owned as the tar says (root, for a directory the
/// command line packed, as the reference's context transfer leaves it).
pub async fn build(
    ctx: &Context,
    mut request: Request,
    context: std::os::fd::OwnedFd,
    report: Report<'_>,
    cancel: CancellationToken,
) -> Result<Built> {
    crate::api::require_root("build")?;
    // A tag that cannot be installed is said before the steps run, not after them.
    tag_names(&request.tags, &ctx.config.registry)?;
    ctx.store.init()?;
    let id = format!(
        "{:x}-{}",
        now_unix(),
        crate::store::unique_suffix().replace('.', "-")
    );
    let work = ctx.store.builds_dir().join(format!("df-{id}"));
    let dir = work.join("context");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::write(work.join(MARKER), b"")
        .with_context(|| format!("writing {}", work.join(MARKER).display()))?;
    let target = dir.clone();
    let unpacked = tokio::task::spawn_blocking(move || -> Result<()> {
        let mut archive = tar::Archive::new(std::fs::File::from(context));
        let entries = archive.entries().context("reading the build context")?;
        untar::unpack(entries, &target, None).context("unpacking the build context")
    })
    .await
    .context("unpacking the build context")?;
    if let Err(e) = unpacked {
        let _ = crate::store::remove_tree(&work);
        return Err(e);
    }
    request.context = dir;
    let builder = Builder::with_work(ctx, &request, report, id, work, cancel);
    builder.run().await
}

/// Kills what runs in the scope of a step and waits for the scope to go.
async fn end_scope(sd: &crate::systemd::Systemd, scope: &str) {
    let _ = sd.kill_unit(scope, nix::libc::SIGKILL).await;
    let _ = sd.stop_unit(scope).await;
}

/// Removes what builds of a service that went away left: their work directories, the
/// mounts in them, the layers they were writing and their network namespaces, and the
/// output of mkosi builds.
pub async fn sweep(ctx: &Context) {
    let entries = std::fs::read_dir(ctx.store.builds_dir())
        .map(|d| d.flatten().collect::<Vec<_>>())
        .unwrap_or_default();
    let sd = ctx.sd().await.ok();
    // The payloads of steps whose service went: they outlive systemd-nspawn in their
    // scopes, and hold the step roots below.
    if let Some(sd) = sd {
        for scope in sd
            .units_matching("nspawn-build-*.scope")
            .await
            .unwrap_or_default()
        {
            end_scope(sd, &scope).await;
        }
    }
    // The workspace of an mkosi build the service's end cut short: no build runs yet.
    if let Ok(spaces) =
        std::fs::read_dir(ctx.config.state_dir.join("cache").join("mkosi-workspace"))
    {
        for space in spaces.flatten() {
            let _ = crate::store::remove_tree(space.path());
        }
    }
    // A cache mount a build was seeding when it went.
    if let Ok(mounts) = std::fs::read_dir(ctx.store.build_cache_dir().join("mounts")) {
        for mount in mounts.flatten() {
            if mount.file_name().to_string_lossy().starts_with(".new-") {
                let _ = crate::store::remove_tree(mount.path());
            }
        }
    }
    // Cache mounts a prune took and had not deleted yet.
    if let Ok(entries) = std::fs::read_dir(ctx.store.build_cache_dir()) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(".trash-") {
                let _ = crate::store::remove_tree(entry.path());
            }
        }
    }
    if let Ok(blobs) = std::fs::read_dir(ctx.store.blobs_dir()) {
        for blob in blobs.flatten() {
            if blob
                .file_name()
                .to_string_lossy()
                .starts_with(".part-build-")
            {
                let _ = std::fs::remove_file(blob.path());
            }
        }
    }
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        // Nothing of an mkosi build's output is mounted or runs.
        if mkosi_left(&name, &entry.path()) {
            let _ = crate::store::remove_tree(entry.path());
            continue;
        }
        // Only a Dockerfile build's directory has its marker.
        if !name.starts_with("df-") || !entry.path().join(MARKER).exists() {
            continue;
        }
        let dir = entry.path();
        if let Some(sd) = sd {
            for unit in mounted_below(&dir) {
                let _ = sd.stop_unit(&unit).await;
            }
        }
        // Only the namespace of a build being swept: the build's own cleanup removes
        // its namespace before its directory.
        let ns = netns_of(&name["df-".len()..]);
        if Path::new("/run/netns")
            .join(format!("nspawn-{ns}"))
            .exists()
        {
            crate::bridge::delete_netns(&ns);
            let _ = std::process::Command::new("ip")
                .args(["link", "del", &crate::bridge::host_end_name_at(&ns, 0)])
                .output();
        }
        let _ = crate::store::remove_tree(&dir);
    }
}

/// Whether `dir`, an entry `name` of the builds directory, is the output of an mkosi build
/// that did not end: one whose output was kept has no marker.
fn mkosi_left(name: &str, dir: &Path) -> bool {
    name.starts_with("mkosi-") && dir.join(MKOSI_MARKER).exists()
}

/// The mount units of the step roots below `dir`.
fn mounted_below(dir: &Path) -> Vec<String> {
    let Ok(mounts) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return Vec::new();
    };
    let prefix = dir.to_string_lossy();
    let mut out: Vec<String> = mounts
        .lines()
        .filter_map(|l| l.split(' ').nth(4))
        // df-1-2-1 is no directory of df-1-2-12.
        .filter(|p| *p == prefix || p.starts_with(&format!("{prefix}/")))
        .map(crate::unitname::mount_unit_for)
        .collect();
    out.sort();
    out.dedup();
    out.reverse();
    out
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_step_killed_by_a_signal_says_which() {
        assert_eq!(
            terminated_by(
                "Container nspawn-build-ab terminated by signal KILL.",
                "nspawn-build-ab"
            ),
            Some(9)
        );
        assert_eq!(
            terminated_by(
                "Container nspawn-build-ab terminated by signal INT.",
                "nspawn-build-ab"
            ),
            Some(2)
        );
        // What the step prints itself is its own.
        assert_eq!(
            terminated_by(
                "Container other terminated by signal KILL.",
                "nspawn-build-ab"
            ),
            None
        );
        assert_eq!(terminated_by("hello", "nspawn-build-ab"), None);
    }

    #[test]
    fn a_working_directory_is_made_as_the_user_and_never_over_a_file() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("etc")).unwrap();
        std::fs::write(
            root.path().join("etc/passwd"),
            "app:x:1000:1000::/home/app:/bin/sh\n",
        )
        .unwrap();
        std::fs::write(root.path().join("app"), "").unwrap();
        // A file where the directory goes fails the step, as fsutil's MkdirAll does.
        let err = workdir(root.path(), "/app", "", None).unwrap_err();
        assert!(format!("{err:#}").contains("not a directory"), "{err:#}");
        assert!(workdir(root.path(), "/etc", "", None).is_ok());
        // Missing, it is made 0755 and the user's, its parents with it.
        if nix::unistd::geteuid().is_root() {
            workdir(root.path(), "/srv/w", "app", None).unwrap();
            let meta = std::fs::metadata(root.path().join("srv/w")).unwrap();
            assert_eq!((meta.uid(), meta.gid()), (1000, 1000));
            assert_eq!(meta.permissions().mode() & 0o777, 0o755);
        }
    }

    use super::*;

    #[test]
    fn the_sweep_takes_only_unfinished_mkosi_outputs() {
        let builds = tempfile::tempdir().unwrap();
        let dir = |name: &str, marker: Option<&str>| {
            let dir = builds.path().join(name);
            std::fs::create_dir(&dir).unwrap();
            if let Some(marker) = marker {
                std::fs::write(dir.join(marker), b"").unwrap();
            }
            (name.to_string(), dir)
        };
        let (name, path) = dir("mkosi-app-1-1792344103-4242.1", Some(MKOSI_MARKER));
        assert!(mkosi_left(&name, &path));
        // Kept with --keep-output.
        let (name, path) = dir("mkosi-app-1-1792344103-4242.2", None);
        assert!(!mkosi_left(&name, &path));
        // A Dockerfile build, and a directory of an older release.
        let (name, path) = dir("df-1792344103-4242.3", Some(MARKER));
        assert!(!mkosi_left(&name, &path));
        let (name, path) = dir("app-1-1792344103-4242.4", Some(MKOSI_MARKER));
        assert!(!mkosi_left(&name, &path));
    }

    #[test]
    fn a_cached_step_brings_the_stage_it_was_made_on() {
        let entry = |layer: &str, created: &str| cache::Entry {
            layer: layer.into(),
            diff_id: format!("diff-{layer}"),
            size: 1,
            created: created.into(),
            used: 0,
            description: String::new(),
            stage: None,
        };
        let base = State {
            config: Config::scratch(),
            layers: Vec::new(),
            key: "base".into(),
        };
        // A build: COPY made l1, RUN made l2 on it.
        let mut first = base.clone();
        first.add(&entry("l1", "t1"), "COPY".into());
        let copied = first.made();
        let run = first.add(&entry("l2", "t2"), "RUN".into());
        // Another build ran the COPY again and made l1b: a third takes the RUN from the
        // cache with the l1 it ran on, its history as it was.
        let mut again = base.clone();
        again.add(&entry("l1b", "t3"), "COPY".into());
        assert_ne!(again.made(), copied);
        again.take(&run);
        assert_eq!(
            again
                .layers
                .iter()
                .map(|l| l.digest.as_str())
                .collect::<Vec<_>>(),
            ["l1", "l2"]
        );
        assert_eq!(again.config.rootfs.diff_ids, ["diff-l1", "diff-l2"]);
        assert_eq!(
            again
                .config
                .history
                .iter()
                .map(|h| h.created.as_deref().unwrap())
                .collect::<Vec<_>>(),
            ["t1", "t2"]
        );
        assert_eq!(again.config, first.config);
    }

    #[test]
    fn a_downloaded_file_is_copied_by_its_name() {
        let src = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("pkg[1].tar"), b"x").unwrap();
        let literal = source_action("pkg[1].tar", "/opt/", false, true);
        copy::docopy(src.path(), dest.path(), &literal).unwrap();
        assert!(dest.path().join("opt/pkg[1].tar").is_file());
        // As a pattern the name matches pkg1.tar only, and nothing is copied.
        let other = tempfile::tempdir().unwrap();
        let pattern = source_action("pkg[1].tar", "/opt/", false, false);
        copy::docopy(src.path(), other.path(), &pattern).unwrap();
        assert!(!other.path().join("opt/pkg[1].tar").exists());
    }

    #[test]
    fn a_fifo_for_passwd_holds_nothing() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("etc")).unwrap();
        nix::unistd::mkfifo(
            &root.path().join("etc/passwd"),
            nix::sys::stat::Mode::from_bits_truncate(0o644),
        )
        .unwrap();
        std::fs::write(root.path().join("etc/group"), b"staff:x:50:\xff\n").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let path = root.path().to_path_buf();
        std::thread::spawn(move || {
            let _ = tx.send(resolve_chown(&path, "app:staff"));
        });
        let user = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("resolving the owner waited on the FIFO");
        assert_eq!(
            (user.uid, user.gid),
            (0, 50),
            "a group file that is no UTF-8 still read"
        );
    }

    #[test]
    fn a_step_s_output_is_clipped_as_buildkit_clips_it() {
        let mut clip = Clip::default();
        let line = "x".repeat(1023);
        let second = Duration::from_millis(500);
        // 200 KiB in the first second, and a word of it once.
        for _ in 0..200 {
            assert_eq!(clip.take(&line, second).len(), 1);
        }
        let past = clip.take(&line, second);
        assert_eq!(
            past.last().unwrap(),
            "[output clipped, log limit 200KiB/s reached]"
        );
        assert!(clip.take(&line, second).is_empty(), "said once");
        // The next second makes room again.
        assert_eq!(
            clip.take(&line, Duration::from_millis(1500)),
            std::slice::from_ref(&line)
        );
        // Past 2 MiB nothing more, whatever the time.
        let mut clip = Clip::default();
        let mut said = Vec::new();
        for _ in 0..3000 {
            said.extend(clip.take(&line, Duration::from_secs(60)));
        }
        assert_eq!(
            said.last().unwrap(),
            "[output clipped, log limit 2MiB reached]"
        );
        assert_eq!(said.len(), 2048 + 1);
        // What a failure shows: the last 256 KiB from where the clipping began.
        assert_eq!(clip.tail.len(), 256);
        assert!(clip.tail.iter().all(|kept| *kept == line));
        assert_eq!(Clip::default().tail.len(), 0);
    }

    #[tokio::test]
    async fn the_clip_leaves_out_the_time_lines_wait_to_be_shown() {
        // 300 KiB in lines of 100 bytes, all there at once: past 200 KiB in its first
        // second, however slowly its lines are taken.
        let line = format!("{}\n", "x".repeat(99));
        let data = std::io::Cursor::new(line.repeat(3072).into_bytes());
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(16);
        let pumped = tokio::spawn(pump(data, tx, Instant::now()));
        let mut shown = Vec::new();
        while let Some(text) = rx.recv().await {
            shown.push(text);
            // 2048 lines taken in more than a second.
            if shown.len() % 256 == 0 {
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        }
        let clip = pumped.await.unwrap();
        assert_eq!(
            shown.last().unwrap(),
            "[output clipped, log limit 200KiB/s reached]"
        );
        assert_eq!(shown.len(), 2048 + 1);
        assert_eq!(clip.tail.len(), 3072 - 2048);
    }

    #[test]
    fn a_cache_mount_is_named_by_its_id_and_its_base() {
        let plain = cache_mount_name("/root/.npm", 0, 0, 0o755, "");
        assert_eq!(plain, cache_mount_name("/root/.npm", 0, 0, 0o755, ""));
        // Another owner, mode or seed is another cache, as with the reference.
        for other in [
            cache_mount_name("/root/.npm", 1000, 0, 0o755, ""),
            cache_mount_name("/root/.npm", 0, 1000, 0o755, ""),
            cache_mount_name("/root/.npm", 0, 0, 0o700, ""),
            cache_mount_name("/root/.npm", 0, 0, 0o755, "stage-key /src"),
            cache_mount_name("/root/.cache", 0, 0, 0o755, ""),
        ] {
            assert_ne!(plain, other);
        }
    }

    #[test]
    fn a_held_cache_is_seen_as_held() {
        let tmp = tempfile::tempdir().unwrap();
        let first = try_lock_dir(tmp.path()).unwrap();
        assert!(first.is_some());
        assert!(
            try_lock_dir(tmp.path()).unwrap().is_none(),
            "held by the first"
        );
        drop(first);
        assert!(try_lock_dir(tmp.path()).unwrap().is_some());
    }

    #[test]
    fn every_tag_is_checked_before_the_build() {
        let tags = |list: &[&str]| {
            tag_names(
                &list.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                "hub.example",
            )
        };
        assert_eq!(
            tags(&["app:1", "app:1", "team/app:2"]).unwrap(),
            [
                ("app-1".to_string(), "hub.example/app:1".to_string()),
                (
                    "team-app-2".to_string(),
                    "hub.example/team/app:2".to_string()
                ),
            ]
        );
        assert!(tags(&["app:1", "App:2"])
            .unwrap_err()
            .to_string()
            .contains("lowercase"));
        let digest = format!("app@sha256:{}", "a".repeat(64));
        assert!(tags(&[&digest]).unwrap_err().to_string().contains("digest"));
        assert!(tags(&["app:1", "other.example/app:1"])
            .unwrap_err()
            .to_string()
            .contains("both be the local image app-1"));
    }
    use std::io::Write as _;

    #[test]
    fn from_takes_a_tag_built_here() {
        let record = |name: &str, reference: &str, origin: &str| -> ImageRecord {
            serde_json::from_value(serde_json::json!({
                "name": name, "reference": reference, "manifest_digest": "sha256:a",
                "layers": [], "backend": "overlay", "created": 1, "origin": origin,
            }))
            .unwrap()
        };
        let records = [
            record("base", "hub.nspawn.org/mybase:1", "build"),
            record("pulled", "hub.nspawn.org/alpine:latest", "pull"),
        ];
        let hub = |r: &str| ImageRef::parse(r, "hub.nspawn.org").unwrap().to_string();
        let found = local_base(
            &records,
            "docker.io/library/mybase:1",
            Some(&hub("mybase:1")),
        );
        assert_eq!(found.unwrap().name, "base");
        // A pull from the configured registry is no stand-in for Docker Hub's image.
        assert!(local_base(
            &records,
            "docker.io/library/alpine:latest",
            Some(&hub("alpine"))
        )
        .is_none());
    }

    #[test]
    fn no_machine_has_the_namespace_of_a_build() {
        let ns = netns_of("1-2");
        assert_eq!(ns, netns_of("1-2"));
        assert_ne!(ns, netns_of("1-3"));
        assert!(crate::reference::validate_machine_name(&ns).is_err());
        assert!(crate::bridge::host_end_name_at(&ns, 0).len() <= 15);
    }

    #[tokio::test]
    async fn step_output_is_read_whatever_its_bytes() {
        let input: &[u8] = b"ok\n\xff\xfe\nlast";
        let mut reader = tokio::io::BufReader::new(input);
        assert_eq!(step_line(&mut reader).await.unwrap().unwrap(), "ok");
        assert_eq!(
            step_line(&mut reader).await.unwrap().unwrap(),
            "\u{FFFD}\u{FFFD}"
        );
        assert_eq!(step_line(&mut reader).await.unwrap().unwrap(), "last");
        assert!(step_line(&mut reader).await.unwrap().is_none());
        let long = vec![b'x'; STEP_LINE as usize + 10];
        let mut reader = tokio::io::BufReader::new(&long[..]);
        assert_eq!(
            step_line(&mut reader).await.unwrap().unwrap().len(),
            STEP_LINE as usize
        );
        assert_eq!(step_line(&mut reader).await.unwrap().unwrap().len(), 10);
    }

    #[test]
    fn a_heredoc_is_one_file_name() {
        assert_eq!(heredoc_file("EOF").unwrap(), "EOF");
        assert_eq!(heredoc_file("./run.sh").unwrap(), "run.sh");
        assert_eq!(heredoc_file("/run.sh").unwrap(), "run.sh");
        assert_eq!(heredoc_file("../../run.sh").unwrap(), "run.sh");
        assert_eq!(heredoc_file("a/../b").unwrap(), "b");
        for name in [
            "/etc/cron.d/pwn",
            "../../etc/crontab",
            "a/b",
            "..",
            ".",
            "/",
        ] {
            assert!(heredoc_file(name).is_err(), "{name}");
        }
    }

    #[test]
    fn a_tag_built_again_replaces_only_an_image_nothing_ran_from() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::new(&dir.path().join("m"), &dir.path().join("s"));
        let record = |json: serde_json::Value| -> ImageRecord {
            let mut base = serde_json::json!({
                "name": "app-1", "reference": "hub.nspawn.org/app:1", "manifest_digest": "sha256:a",
                "layers": [], "backend": "overlay", "created": 1, "origin": "build", "mode": "app",
            });
            for (k, v) in json.as_object().unwrap() {
                base[k] = v.clone();
            }
            serde_json::from_value(base).unwrap()
        };
        let reference = "hub.nspawn.org/app:1";
        assert!(replaced_by_build(
            &store,
            &record(serde_json::json!({})),
            reference
        ));
        assert!(replaced_by_build(
            &store,
            &record(serde_json::json!({ "origin": "pull" })),
            reference
        ));
        assert!(!replaced_by_build(
            &store,
            &record(serde_json::json!({})),
            "hub.nspawn.org/app:2"
        ));
        assert!(!replaced_by_build(
            &store,
            &record(serde_json::json!({ "started": 5 })),
            reference
        ));
        assert!(!replaced_by_build(
            &store,
            &record(serde_json::json!({ "origin": "create" })),
            reference
        ));
    }

    #[test]
    fn the_key_of_a_copy_follows_what_it_copies() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = dir.path();
        std::fs::create_dir_all(ctx.join("pkgs/a")).unwrap();
        std::fs::create_dir_all(ctx.join("pkgs/b")).unwrap();
        std::fs::write(ctx.join("pkgs/a/package.json"), "a1").unwrap();
        std::fs::write(ctx.join("pkgs/b/package.json"), "b1").unwrap();
        std::fs::write(ctx.join("top.txt"), "t1").unwrap();
        std::fs::write(ctx.join("target.txt"), "x1").unwrap();
        std::os::unix::fs::symlink("target.txt", ctx.join("link.txt")).unwrap();
        let keys = |sources: &[&str]| -> Vec<String> {
            sources
                .iter()
                .map(|s| content_hash(ctx, s, &[]).unwrap())
                .collect()
        };
        let sources = [".", "pkgs/.", "pkgs/*/package.json", "link.txt", "top.txt"];
        let before = keys(&sources);
        std::fs::write(ctx.join("pkgs/b/package.json"), "b2").unwrap();
        let after = keys(&sources);
        assert_ne!(before[0], after[0], "COPY . must see a changed file");
        assert_ne!(before[1], after[1], "COPY pkgs/. must see a changed file");
        assert_ne!(before[2], after[2], "a wildcard in a directory part");
        assert_eq!(before[3], after[3]);
        assert_eq!(before[4], after[4]);
        std::fs::write(ctx.join("target.txt"), "x2").unwrap();
        assert_ne!(after[3], keys(&sources)[3], "a symlink source is followed");
        // The same tree elsewhere gives the same key.
        let other = tempfile::tempdir().unwrap();
        std::fs::write(other.path().join("top.txt"), "t1").unwrap();
        assert_eq!(
            content_hash(other.path(), "top.txt", &[]).unwrap(),
            after[4]
        );
        // What --exclude leaves out of the copy does not count.
        let excluding = |e: &[&str]| {
            let e: Vec<String> = e.iter().map(|s| s.to_string()).collect();
            content_hash(ctx, "pkgs", &e).unwrap()
        };
        let kept = excluding(&["b"]);
        std::fs::write(ctx.join("pkgs/b/package.json"), "b3").unwrap();
        assert_eq!(kept, excluding(&["b"]));
        std::fs::write(ctx.join("pkgs/a/package.json"), "a2").unwrap();
        assert_ne!(kept, excluding(&["b"]));
        // A pattern that takes a file back below an excluded directory still counts it.
        let back = excluding(&["b", "!b/package.json"]);
        std::fs::write(ctx.join("pkgs/b/package.json"), "b4").unwrap();
        assert_ne!(back, excluding(&["b", "!b/package.json"]));
    }

    #[test]
    fn a_step_s_key_leaves_out_what_the_reference_leaves_out() {
        let run = |proxy: &str, id: &str| {
            Action::Run(plan::Run {
                argv: vec!["true".to_string()],
                script: None,
                env: vec!["A=1".to_string()],
                proxy_env: vec![format!("HTTP_PROXY={proxy}")],
                user: String::new(),
                workdir: "/".to_string(),
                mounts: vec![RunMount::Cache {
                    target: "/cache".to_string(),
                    id: id.to_string(),
                    from: None,
                    sharing: "locked".to_string(),
                    read_only: false,
                    mode: 0o755,
                    uid: 0,
                    gid: 0,
                }],
                network: plan::Network::Default,
                insecure: false,
            })
        };
        let key = |a: &Action| format!("{:?}", keyed(a));
        assert_eq!(key(&run("http://a", "one")), key(&run("http://b", "two")));
    }

    #[test]
    fn add_unpacks_the_archives_of_the_reference() {
        let dir = tempfile::tempdir().unwrap();
        let mut tar = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_ustar();
        header.set_size(5);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "hello.txt", &b"hello"[..])
            .unwrap();
        let tar = tar.into_inner().unwrap();
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gzip.write_all(&tar).unwrap();
        let mut bzip2 = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
        bzip2.write_all(&tar).unwrap();
        let mut xz = Vec::new();
        lzma_rs::xz_compress(&mut &tar[..], &mut xz).unwrap();
        let zstd = zstd::encode_all(&tar[..], 0).unwrap();
        for (kind, bytes) in [
            (Compression::None, tar.clone()),
            (Compression::Gzip, gzip.finish().unwrap()),
            (Compression::Bzip2, bzip2.finish().unwrap()),
            (Compression::Xz, xz),
            (Compression::Zstd, zstd),
        ] {
            assert_eq!(untar::detect(&bytes[..bytes.len().min(10)]), kind);
            let path = dir.path().join(format!("{kind:?}"));
            std::fs::write(&path, &bytes).unwrap();
            let reader =
                decompressed(kind, std::fs::File::open(&path).unwrap(), dir.path()).unwrap();
            let mut archive = tar::Archive::new(reader);
            let names: Vec<String> = archive
                .entries()
                .unwrap()
                .map(|e| e.unwrap().path().unwrap().display().to_string())
                .collect();
            assert_eq!(names, ["hello.txt"], "{kind:?}");
        }
        // Only xz's scratch file was made, and it is gone.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 5);
    }

    #[test]
    fn what_a_tar_reader_takes_is_an_archive_and_the_rest_is_copied() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let dest = dir.path().join("dest");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dest).unwrap();
        let uid = u64::from(nix::unistd::getuid().as_raw());
        let gid = u64::from(nix::unistd::getgid().as_raw());
        // A V7 tar: no magic, only the checksum says it is a header.
        let mut v7 = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_old();
        header.set_uid(uid);
        header.set_gid(gid);
        header.set_size(2);
        header.set_mode(0o644);
        header.set_mtime(0);
        v7.append_data(&mut header, "v7.txt", &b"v7"[..]).unwrap();
        let v7 = v7.into_inner().unwrap();
        // zstd behind a skippable frame.
        let mut skippable = vec![0x50, 0x2a, 0x4d, 0x18, 4, 0, 0, 0, 1, 2, 3, 4];
        let mut tar = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_uid(uid);
        header.set_gid(gid);
        header.set_size(1);
        header.set_mode(0o644);
        tar.append_data(&mut header, "z.txt", &b"z"[..]).unwrap();
        skippable.extend(zstd::encode_all(&tar.into_inner().unwrap()[..], 0).unwrap());
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gzip.write_all(b"text, not a tar").unwrap();
        for (name, bytes) in [
            ("v7.tar", v7),
            ("skippable.tar.zst", skippable),
            ("text.gz", gzip.finish().unwrap()),
            ("BZh.txt", b"BZh, not bzip2".to_vec()),
            ("plain", b"plain text".to_vec()),
        ] {
            std::fs::write(src.join(name), bytes).unwrap();
        }
        let unpack = |name: &str| {
            unpack_archive(
                &copy::UnpackRequest {
                    src_root: &src,
                    src: Path::new(name),
                    dest_root: &dest,
                    dest: Path::new("/out/"),
                    owner: None,
                    timestamp: None,
                },
                dir.path(),
            )
            .unwrap()
        };
        assert!(unpack("v7.tar"));
        assert_eq!(std::fs::read(dest.join("out/v7.txt")).unwrap(), b"v7");
        assert!(unpack("skippable.tar.zst"));
        assert_eq!(std::fs::read(dest.join("out/z.txt")).unwrap(), b"z");
        for name in ["text.gz", "BZh.txt", "plain"] {
            assert!(!unpack(name), "{name}");
        }
    }

    #[test]
    fn last_modified_is_read_as_net_http_reads_it() {
        // What Go 1.26's http.ParseTime gives for each, under TZ=UTC.
        for (text, want) in [
            (
                "Mon, 02 Jan 2006 15:04:05 GMT",
                Some(1_136_214_245_000_000_000),
            ),
            (
                "Sun, 06 Nov 1994 08:49:37 GMT",
                Some(784_111_777_000_000_000),
            ),
            (
                "Sunday, 06-Nov-94 08:49:37 GMT",
                Some(784_111_777_000_000_000),
            ),
            ("Sun Nov  6 08:49:37 1994", Some(784_111_777_000_000_000)),
            ("Sun Nov 6 08:49:37 1994", Some(784_111_777_000_000_000)),
            (
                "mon, 02 jan 2006 15:04:05 GMT",
                Some(1_136_214_245_000_000_000),
            ),
            ("Mon, 02 Jan 2006 15:04:05 UTC", None),
            ("Mon, 30 Feb 2006 15:04:05 GMT", None),
            (
                "Mon, 29 Feb 2000 15:04:05 GMT",
                Some(951_836_645_000_000_000),
            ),
            (
                "Mon, 02 Jan 2006 15:04:05.5 GMT",
                Some(1_136_214_245_500_000_000),
            ),
            ("Mon, 2 Jan 2006 15:04:05 GMT", None),
            ("Thursday, 01-Jan-70 00:00:00 EST", Some(0)),
            (
                "Monday, 02-Jan-06 15:04:05 GMT+2",
                Some(1_136_214_245_000_000_000),
            ),
            (
                "Monday, 02-Jan-06 15:04:05 UTC",
                Some(1_136_214_245_000_000_000),
            ),
            ("Mon, 02 Jan 2006 24:00:00 GMT", None),
            (
                "Mon, 02 Jan 2006 1:04:05 GMT",
                Some(1_136_163_845_000_000_000),
            ),
            ("Wed, 31 Dec 1969 23:59:59 GMT", Some(-1_000_000_000)),
            ("Mon, 02 Jan 2006 15:04:05 GMT ", None),
            (
                "Friday, 31-Dec-99 23:59:59 CEST",
                Some(946_684_799_000_000_000),
            ),
        ] {
            assert_eq!(http_time(text), want, "{text}");
        }
    }
}
