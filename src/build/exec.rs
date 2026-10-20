//! Runs a build: the bases of the stages the target needs, each step in a root of its
//! own (an overlay of the layers so far) turned into a layer, or taken from the cache,
//! and the image the target stage ends with, installed under each tag.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufWriter, Read as _};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{anyhow, bail, Context as _, Result};
use tokio_util::sync::CancellationToken;

use crate::api::{line, note, Context, Report};
use crate::backend::{Backend, BackendChoice};
use crate::build::cache::{self, Cache};
use crate::build::image::{self, Config};
use crate::build::plan::{self, Action, BaseRef, From, Platform, RunMount};
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
    /// The cache key of everything so far.
    key: String,
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
    downloads: HashMap<String, (PathBuf, String, String)>,
}

const CANCELLED: &str = "the build was cancelled: its client went away";

const HOST_NETWORK_REFUSED: &str =
    "network.host is not allowed: --allow network.host lets a step use it";

/// In the work directory of a Dockerfile build, for the sweep at the service's start.
pub(crate) const MARKER: &str = ".nspawn-dockerfile-build";

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
        }
    }

    /// An ADD of a URL: fetched once a build into the work directory, checked against
    /// `checksum`; its directory, file name and digest. A step taking it is keyed by its
    /// contents, as the reference keys it.
    async fn download(
        &mut self,
        url: &str,
        checksum: Option<&str>,
    ) -> Result<(PathBuf, String, String)> {
        // Each ADD checks its own --checksum, the URL fetched before included.
        let check = |found: &(PathBuf, String, String)| -> Result<()> {
            match checksum {
                Some(checksum) if found.2 != checksum => bail!(
                    "digest mismatch for {url}: expected {checksum}, got {}",
                    found.2
                ),
                _ => Ok(()),
            }
        };
        if let Some(found) = self.downloads.get(url) {
            check(found)?;
            return Ok(found.clone());
        }
        let dir = self.work.join(format!("url-{}", self.downloads.len()));
        let fetched = tokio::select! {
            fetched = fetch_url(url, &dir) => fetched?,
            () = self.cancel.cancelled() => bail!(CANCELLED),
        };
        let found = (dir, fetched.0, fetched.1);
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
        let _ = std::fs::remove_dir_all(&self.work);
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
            let Some(action) = &step.action else {
                state.config.note(step.created_by.clone(), &created);
                state.key = cache::key(&[state.key.as_bytes(), step.created_by.as_bytes()]);
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
                format!("{action:?}").as_bytes(),
                self.inputs(action).await?.as_bytes(),
            ]);
            let cached = if no_cache {
                None
            } else {
                // Under the lock, so that a prune does not take it before it is kept.
                let _lock = self.ctx.store.lock().await?;
                match self.cache.get(&key) {
                    Some(entry) if self.ctx.store.has_blob(&entry.layer) => {
                        self.keep([entry.layer.clone()])?;
                        Some(entry)
                    }
                    _ => None,
                }
            };
            let entry = match cached {
                Some(entry) => {
                    self.say(format!("#{n} CACHED"));
                    entry
                }
                _ => {
                    let entry = self
                        .execute(n, &state, action, &step.description)
                        .await
                        .map_err(|e| {
                            self.say(format!("#{n} ERROR: {e:#}"));
                            e
                        })?;
                    self.cache.put(&key, &entry)?;
                    self.say(format!("#{n} DONE {:.1}s", start.elapsed().as_secs_f64()));
                    entry
                }
            };
            created.clone_from(&entry.created);
            state.layers.push(LayerRef {
                digest: entry.layer.clone(),
                media_type: LAYER_MEDIA_TYPE.to_string(),
                size: entry.size,
            });
            state
                .config
                .add_layer(entry.diff_id.clone(), step.created_by.clone(), &created);
            // What follows chains on what the step made, not on what it was asked: a step
            // that ran again and made something else is no cache hit for the ones after it.
            state.key = cache::key(&[key.as_bytes(), entry.diff_id.as_bytes()]);
        }
        let history = std::mem::take(&mut state.config.history);
        let rootfs = std::mem::take(&mut state.config.rootfs);
        // The base image's author stays unless MAINTAINER names another, as BuildKit
        // keeps it.
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
                    let (_, _, digest) = self.download(source, copy.checksum.as_deref()).await?;
                    parts.push(format!("url {source} {digest}"));
                }
            }
        }
        let from = |from: &From, paths: &[String], parts: &mut Vec<String>| -> Result<()> {
            match from {
                From::Context => {
                    // A large context takes a while to hash: the runtime's other tasks go
                    // on meanwhile.
                    for path in paths {
                        parts.push(tokio::task::block_in_place(|| {
                            content_hash(&self.req.context, path)
                        })?);
                    }
                }
                From::Stage(i) => parts.push(
                    self.states
                        .get(i)
                        .map(|s| s.key.clone())
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
            Action::Copy(copy) => from(&copy.from, &copy.sources, &mut parts)?,
            Action::Run(run) => {
                for mount in &run.mounts {
                    match mount {
                        RunMount::Bind {
                            from: f, source, ..
                        } => from(f, std::slice::from_ref(source), &mut parts)?,
                        RunMount::Cache {
                            from: Some((f, source)),
                            ..
                        } => from(f, std::slice::from_ref(source), &mut parts)?,
                        _ => {}
                    }
                }
                parts.push(format!("{:?}", self.network_of(run.network)));
                // What the step's sandbox is given, as the reference keys it.
                parts.push(format!(
                    "{:?} {:?} {:?}",
                    self.req.add_hosts, self.req.ulimits, self.req.shm_size
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
        let image = ImageRef::docker(reference)?;
        let wanted = image.to_string();
        // `build -t NAME` records NAME under the configured registry: an image built
        // here is taken by that name too, as docker takes its own tags, but nothing
        // pulled from that registry stands in for a Docker Hub image.
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
            Action::Workdir { path, user } => workdir(&step_root.merged, path, user),
        };
        let unmounted = root::unmount(sd, &step_root).await;
        done?;
        unmounted?;
        let created = timestamp_now();
        let committed = self.commit(n, &step_root.upper, &lowers).await?;
        let _ = std::fs::remove_dir_all(&dir);
        Ok(cache::Entry {
            layer: committed.digest,
            diff_id: committed.diff_id,
            size: committed.size,
            created,
            used: now_unix(),
            description: description.to_string(),
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
        let env = run.env.clone();
        let mut secret_env: Vec<(String, Vec<u8>)> = Vec::new();
        let mut targets: Vec<String> = Vec::new();
        let mut locks = Vec::new();
        let mut locked = std::collections::HashSet::new();
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
                        )?;
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
                    let cache_dir = self
                        .ctx
                        .store
                        .build_cache_dir()
                        .join("mounts")
                        .join(short(&cache::key(&[id.as_bytes()])));
                    if !cache_dir.exists() {
                        // Made beside its name and renamed into place once seeded: a seed
                        // cut short is made again by the next build.
                        let staged = cache_dir
                            .with_file_name(format!(".new-{}", crate::store::unique_suffix()));
                        std::fs::create_dir_all(&staged)?;
                        let seeded: Result<()> = async {
                            if let Some((f, source)) = from {
                                let src_root = self.source_root(f).await?;
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
                                )?;
                            }
                            std::fs::set_permissions(
                                &staged,
                                std::fs::Permissions::from_mode(*mode),
                            )?;
                            std::os::unix::fs::chown(&staged, Some(*uid), Some(*gid))?;
                            Ok(())
                        }
                        .await;
                        // Another build may have made it meanwhile: its copy stays.
                        if seeded.is_err() || std::fs::rename(&staged, &cache_dir).is_err() {
                            let _ = std::fs::remove_dir_all(&staged);
                        }
                        seeded?;
                    }
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
                    // Waited for off the runtime's threads, and once per directory: a
                    // second lock of the same one would wait for itself.
                    if sharing == "locked" && locked.insert(cache_dir.clone()) {
                        let dir = cache_dir.clone();
                        let lock = tokio::select! {
                            lock = tokio::task::spawn_blocking(move || lock_dir(&dir)) => {
                                lock.context("locking a cache mount")??
                            }
                            () = self.cancel.cancelled() => bail!(CANCELLED),
                        };
                        locks.push(lock);
                    }
                    targets.push(target.clone());
                    mounts.push(sandbox::Mount::Bind {
                        source: cache_dir,
                        target: target.clone(),
                        read_only: *read_only,
                    });
                }
                RunMount::Tmpfs { target, size } => {
                    targets.push(target.clone());
                    mounts.push(sandbox::Mount::Tmpfs {
                        target: target.clone(),
                        options: size.map(|s| format!("size={s}")).unwrap_or_default(),
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
                            bail!("secret {id} not found");
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
        // The working directory is the reference's to make, as runc makes it.
        let workdir = copy::root_path(merged, &step.workdir)?;
        if !workdir.exists() {
            std::fs::create_dir_all(&workdir)?;
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
        // Each stream read to its end by a task of its own, as bytes: a line that is no
        // UTF-8 is still a line, and a stream nobody reads would hold the step forever
        // once its pipe filled.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let readers: [Box<dyn tokio::io::AsyncRead + Send + Unpin>; 2] =
            [Box::new(stdout), Box::new(stderr)];
        for stream in readers {
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut reader = tokio::io::BufReader::new(stream);
                while let Ok(Some(text)) = step_line(&mut reader).await {
                    if tx.send(text).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
        loop {
            tokio::select! {
                () = self.cancel.cancelled() => {
                    // The step's processes end with systemd-nspawn, the init of their
                    // PID namespace going with it.
                    let _ = child.kill().await;
                    bail!(CANCELLED);
                }
                text = rx.recv() => match text {
                    Some(text) => line(report, format!("#{n} {:.3} {text}", started.elapsed().as_secs_f64())),
                    None => break,
                },
            }
        }
        let status = tokio::select! {
            status = child.wait() => status.context("waiting for systemd-nspawn")?,
            () = self.cancel.cancelled() => {
                let _ = child.kill().await;
                bail!(CANCELLED);
            }
        };
        if !status.success() {
            bail!(
                "process {:?} did not complete successfully: exit code: {}",
                step.argv.join(" "),
                status.code().unwrap_or(1)
            );
        }
        Ok(())
    }

    async fn copy_step(&mut self, n: usize, merged: &Path, c: &plan::Copy) -> Result<()> {
        let dir = self.work.join(format!("copy-{n}"));
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let owner = c.chown.as_ref().map(|chown| resolve_chown(merged, chown));
        let chmod = c.chmod.as_deref().map(copy::parse_chmod).transpose()?;
        // Each source with whether it is a heredoc: those are taken by their name as it
        // is, never as a pattern or an archive.
        let mut sources: Vec<(PathBuf, String, bool)> = Vec::new();
        for (i, file) in c.inline.iter().enumerate() {
            let inline = dir.join(format!("inline-{i}"));
            std::fs::create_dir_all(&inline)?;
            let name = heredoc_file(&file.name)?;
            std::fs::write(inline.join(&name), &file.data)?;
            std::fs::set_permissions(inline.join(&name), std::fs::Permissions::from_mode(0o644))?;
            sources.push((inline, name, true));
        }
        for source in &c.sources {
            if c.add && is_url(source) {
                let (fetched, name, _) = self.download(source, c.checksum.as_deref()).await?;
                sources.push((fetched, name, false));
                continue;
            }
            if c.add && source.contains("://") {
                bail!("ADD of {source}: only local files and http or https URLs are taken");
            }
            sources.push((self.source_root(&c.from).await?, source.clone(), false));
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
        for (src_root, source, inline) in &sources {
            let src = copy::copy_source(source, c.parents && !inline);
            let mut action = copy::CopyAction::for_instruction(&src, &c.dest);
            action.allow_wildcard &= !inline;
            action.owner = owner;
            if let Some(chmod) = chmod.clone() {
                action.set_chmod(chmod);
            }
            action.exclude_patterns.clone_from(&c.exclude);
            if c.add && !inline {
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
        crate::api::events::emit("machine", "build", name, &[("image", reference)]);
        Ok(())
    }
}

/// The local name and the reference of each tag, refused before anything is built when
/// one is no tag docker takes, carries a digest, or names an image another tag names.
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

/// Holds an exclusive lock of a cache mount while a step uses it (sharing=locked).
fn lock_dir(dir: &Path) -> Result<nix::fcntl::Flock<std::fs::File>> {
    let file = std::fs::File::open(dir).with_context(|| format!("opening {}", dir.display()))?;
    nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusive)
        .map_err(|(_, e)| anyhow!("locking {}: {e}", dir.display()))
}

/// WORKDIR: the directory made, with its missing parents, owned by `user`.
fn workdir(merged: &Path, path: &str, user: &str) -> Result<()> {
    let target = copy::root_path(merged, path)?;
    if target.exists() {
        return Ok(());
    }
    let owner = (!user.is_empty()).then(|| resolve_chown(merged, user));
    let chown = |u: Option<copy::User>| -> Result<Option<copy::User>> { Ok(owner.or(u)) };
    copy::mkdir_all(&target, 0o755, Some(&chown), None)?;
    Ok(())
}

/// USER[:GROUP] by name or number against the root's passwd and group files, as the
/// reference resolves --chown: a name the files lack is 0.
fn resolve_chown(root: &Path, spec: &str) -> copy::User {
    let read = |file: &str| -> String {
        copy::root_path(root, file)
            .ok()
            .and_then(|p| {
                let mut text = String::new();
                std::fs::File::open(p)
                    .ok()?
                    .take(10 << 20)
                    .read_to_string(&mut text)
                    .ok()?;
                Some(text)
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
/// below each the names, kinds, modes, owners, link targets and contents.
fn content_hash(context: &Path, path: &str) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(path.as_bytes());
    // A source that matches nothing fails in the copy itself.
    for found in copy::resolve_wildcards(context, path, true).unwrap_or_default() {
        let found = found.to_string_lossy().into_owned();
        hash.update(b"\0match\0");
        hash.update(found.as_bytes());
        let Ok(real) = copy::root_path(context, &found) else {
            continue;
        };
        hash_tree(&mut hash, &real)?;
    }
    Ok(format!("sha256:{}", hex::encode(hash.finalize())))
}

/// Everything below `top` into `hash`, by paths relative to it, without following
/// symlinks.
fn hash_tree(hash: &mut sha2::Sha256, top: &Path) -> Result<()> {
    use sha2::Digest;
    use std::os::unix::fs::MetadataExt;
    let mut stack = vec![top.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(meta) = p.symlink_metadata() else {
            continue;
        };
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
            let mut file =
                std::fs::File::open(&p).with_context(|| format!("reading {}", p.display()))?;
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
async fn fetch_url(url: &str, dir: &Path) -> Result<(String, String)> {
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
    Ok((name, format!("sha256:{}", hex::encode(hash.finalize()))))
}

/// The compression of an archive, by its first bytes; None when it is no tar.
fn archive_kind(head: &[u8]) -> Option<&'static str> {
    if head.starts_with(&[0x1f, 0x8b, 0x08]) {
        Some("gzip")
    } else if head.starts_with(b"BZh") {
        Some("bzip2")
    } else if head.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0x00]) {
        Some("xz")
    } else if head.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        Some("zstd")
    } else if head.len() > 262 && &head[257..262] == b"ustar" {
        Some("tar")
    } else {
        None
    }
}

/// The contents of an archive of `kind`; xz, whose decoder writes, through a file in
/// `dir`.
fn decompressed(kind: &str, file: std::fs::File, dir: &Path) -> Result<Box<dyn std::io::Read>> {
    Ok(match kind {
        "gzip" => Box::new(flate2::read::MultiGzDecoder::new(file)),
        "bzip2" => Box::new(bzip2::read::MultiBzDecoder::new(file)),
        "zstd" => Box::new(zstd::stream::read::Decoder::new(file)?),
        "xz" => {
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
        _ => Box::new(file),
    })
}

/// ADD of a local archive: unpacked into a scratch directory, whose contents then go to
/// the destination as a directory copy would. False when the source is no archive.
fn unpack_archive(r: &copy::UnpackRequest<'_>, dir: &Path) -> Result<bool> {
    let src = copy::root_path(r.src_root, &r.src.to_string_lossy())?;
    if !src.is_file() {
        return Ok(false);
    }
    let mut head = vec![0u8; 512];
    let n = {
        let mut file = std::fs::File::open(&src)?;
        file.read(&mut head)?
    };
    head.truncate(n);
    let Some(kind) = archive_kind(&head) else {
        return Ok(false);
    };
    // A compressed file that does not decompress, or holds no tar, is copied as it is.
    let Ok(mut reader) = decompressed(kind, std::fs::File::open(&src)?, dir) else {
        return Ok(false);
    };
    let mut inner = vec![0u8; 512];
    let mut filled = 0;
    while filled < inner.len() {
        match reader.read(&mut inner[filled..]) {
            Ok(0) | Err(_) => break,
            Ok(m) => filled += m,
        }
    }
    if filled < 262 || &inner[257..262] != b"ustar" {
        return Ok(false);
    }
    inner.truncate(filled);
    let scratch = dir.join(format!("unpack-{}", crate::store::unique_suffix()));
    std::fs::create_dir_all(&scratch)?;
    let mut archive = tar::Archive::new(std::io::Cursor::new(inner).chain(reader));
    archive.set_preserve_permissions(true);
    archive.set_preserve_ownerships(true);
    archive.set_unpack_xattrs(false);
    archive
        .unpack(&scratch)
        .with_context(|| format!("unpacking {}", r.src.display()))?;
    let dest = r.dest.to_string_lossy().into_owned();
    copy::docopy(
        &scratch,
        r.dest_root,
        &copy::CopyAction {
            src: "/".to_string(),
            dest,
            owner: r.owner,
            dir_copy_contents: true,
            create_dest_path: true,
            ..Default::default()
        },
    )?;
    let _ = std::fs::remove_dir_all(&scratch);
    Ok(true)
}

/// A build whose context comes as a tar stream on `context`: unpacked into the build's
/// work directory, everything owned by root as the reference's context transfer leaves
/// it.
pub async fn build(
    ctx: &Context,
    mut request: Request,
    context: std::os::fd::OwnedFd,
    report: Report<'_>,
    cancel: CancellationToken,
) -> Result<Built> {
    crate::api::require_root("build")?;
    // A tag that cannot be installed is said before the steps run, as docker says it.
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
        archive.set_preserve_permissions(true);
        archive.set_preserve_mtime(true);
        archive.set_preserve_ownerships(false);
        archive.set_unpack_xattrs(false);
        for entry in archive.entries().context("reading the build context")? {
            let mut entry = entry.context("reading the build context")?;
            entry
                .unpack_in(&target)
                .context("unpacking the build context")?;
        }
        Ok(())
    })
    .await
    .context("unpacking the build context")?;
    if let Err(e) = unpacked {
        let _ = std::fs::remove_dir_all(&work);
        return Err(e);
    }
    request.context = dir;
    let builder = Builder::with_work(ctx, &request, report, id, work, cancel);
    builder.run().await
}

/// Removes what builds of a service that went away left: their work directories, the
/// mounts in them, the layers they were writing and their network namespaces.
pub async fn sweep(ctx: &Context) {
    let entries = std::fs::read_dir(ctx.store.builds_dir())
        .map(|d| d.flatten().collect::<Vec<_>>())
        .unwrap_or_default();
    let sd = ctx.sd().await.ok();
    // The workspace of an mkosi build the service's end cut short: no build runs yet.
    if let Ok(spaces) =
        std::fs::read_dir(ctx.config.state_dir.join("cache").join("mkosi-workspace"))
    {
        for space in spaces.flatten() {
            let _ = std::fs::remove_dir_all(space.path());
        }
    }
    // A cache mount a build was seeding when it went.
    if let Ok(mounts) = std::fs::read_dir(ctx.store.build_cache_dir().join("mounts")) {
        for mount in mounts.flatten() {
            if mount.file_name().to_string_lossy().starts_with(".new-") {
                let _ = std::fs::remove_dir_all(mount.path());
            }
        }
    }
    // Cache mounts a prune took and had not deleted yet.
    if let Ok(entries) = std::fs::read_dir(ctx.store.build_cache_dir()) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(".trash-") {
                let _ = std::fs::remove_dir_all(entry.path());
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
        // An mkosi build's directory may be named alike; only a Dockerfile build's has
        // the marker.
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
        let _ = std::fs::remove_dir_all(&dir);
    }
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
    use super::*;

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
                .map(|s| content_hash(ctx, s).unwrap())
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
        assert_eq!(content_hash(other.path(), "top.txt").unwrap(), after[4]);
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
            ("tar", tar.clone()),
            ("gzip", gzip.finish().unwrap()),
            ("bzip2", bzip2.finish().unwrap()),
            ("xz", xz),
            ("zstd", zstd),
        ] {
            assert_eq!(archive_kind(&bytes[..bytes.len().min(512)]), Some(kind));
            let path = dir.path().join(kind);
            std::fs::write(&path, &bytes).unwrap();
            let reader =
                decompressed(kind, std::fs::File::open(&path).unwrap(), dir.path()).unwrap();
            let mut archive = tar::Archive::new(reader);
            let names: Vec<String> = archive
                .entries()
                .unwrap()
                .map(|e| e.unwrap().path().unwrap().display().to_string())
                .collect();
            assert_eq!(names, ["hello.txt"], "{kind}");
        }
        assert_eq!(archive_kind(b"plain text"), None);
        // Only xz's scratch file was made, and it is gone.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 5);
    }
}
