//! Registry client: catalog, tags, manifest resolution and verified layer downloads.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use futures_util::StreamExt;
use oci_client::client::{Certificate, CertificateEncoding, ClientConfig, ClientProtocol};
use oci_client::manifest::{ImageIndexEntry, OciDescriptor, OciImageManifest, OciManifest};
use oci_client::secrets::RegistryAuth;
use oci_client::{Client, Reference, RegistryOperation};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;

use crate::api::{Event, Report};
use crate::auth;
use crate::config::Config;
use crate::store::validate_digest;
use crate::verify::{Candidate, BUNDLE_ARTIFACT_TYPE, BUNDLE_MAX_SIZE};

/// How often a transfer says how far it got: often enough for a progress bar, seldom
/// enough that the bus signals cost nothing next to the transfer.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);

/// The progress of one transfer, reported when it starts, every `interval` while it
/// runs and when it ends.
struct Progress<'a> {
    report: Report<'a>,
    item: String,
    total: u64,
    done: u64,
    interval: Duration,
    last: Option<(Instant, u64)>,
}

impl<'a> Progress<'a> {
    fn start(report: Report<'a>, item: String, total: u64, interval: Duration) -> Self {
        let mut progress = Progress {
            report,
            item,
            total,
            done: 0,
            interval,
            last: None,
        };
        progress.emit();
        progress
    }

    fn advance(&mut self, bytes: u64) {
        self.done += bytes;
        if self
            .last
            .is_none_or(|(at, _)| at.elapsed() >= self.interval)
        {
            self.emit();
        }
    }

    /// The last word, unless it was said already.
    fn finish(&mut self) {
        if self.last.is_none_or(|(_, done)| done != self.done) {
            self.emit();
        }
    }

    fn emit(&mut self) {
        (self.report)(Event::Progress {
            item: self.item.clone(),
            done: self.done,
            total: self.total,
        });
        self.last = Some((Instant::now(), self.done));
    }
}

pub struct Hub {
    /// For the configured registry, which the configured CA certificate is for.
    hub: Client,
    hub_name: String,
    /// For every other registry: the system's roots alone.
    public: Client,
    /// The same for uploads, whose requests no read timeout bounds: it would count the
    /// time a body takes to send and a registry takes to commit a large blob.
    hub_upload: Client,
    public_upload: Client,
}

/// How long an upload may go without the registry taking the next part of a blob before
/// it is given up; the commit after the last part is the registry's to take.
const UPLOAD_STALL_SECS: u64 = 600;

impl Hub {
    pub fn new(config: &Config) -> Result<Self> {
        let mut extra_root_certificates = Vec::new();
        if let Some(path) = &config.ca_cert {
            let data = fs::read(path)
                .with_context(|| format!("reading CA certificate {}", path.display()))?;
            // The registry client drops a file it cannot read without a word, and the
            // pull then fails on an unknown issuer that names no file.
            if !reqwest::Certificate::from_pem_bundle(&data).is_ok_and(|c| !c.is_empty()) {
                bail!(
                    "CA certificate {}: no PEM certificate in it",
                    path.display()
                );
            }
            extra_root_certificates.push(Certificate {
                encoding: CertificateEncoding::Pem,
                data,
            });
        }
        // A registry that stops sending in the middle of a blob, the connection left
        // open, would hold the job, its blobs and the service for ever: a read that
        // waits this long ends the transfer.
        let client_config = |extra_root_certificates, read_timeout| ClientConfig {
            protocol: ClientProtocol::Https,
            extra_root_certificates,
            // Hub::resolve picks an index's entry itself.
            platform_resolver: None,
            user_agent: concat!("nspawn/", env!("CARGO_PKG_VERSION")),
            read_timeout,
            connect_timeout: Some(std::time::Duration::from_secs(30)),
            ..Default::default()
        };
        let reads = Some(std::time::Duration::from_secs(120));
        Ok(Hub {
            hub: Client::new(client_config(extra_root_certificates.clone(), reads)),
            hub_name: config.registry.clone(),
            public: Client::new(client_config(Vec::new(), reads)),
            hub_upload: Client::new(client_config(extra_root_certificates, None)),
            public_upload: Client::new(client_config(Vec::new(), None)),
        })
    }

    /// The upload client for a registry, as `client` picks the other.
    fn upload_client(&self, registry: &str) -> &Client {
        if registry == self.hub_name {
            &self.hub_upload
        } else {
            &self.public_upload
        }
    }

    /// The client for a registry: the configured CA certificate is trusted for the
    /// configured registry only, as nspawn login trusts it.
    fn client(&self, registry: &str) -> &Client {
        if registry == self.hub_name {
            &self.hub
        } else {
            &self.public
        }
    }

    /// The credentials nspawn login left for a registry, else anonymous. Chosen per
    /// registry, so that the hub's never travel to Docker Hub.
    fn auth(&self, registry: &str) -> RegistryAuth {
        match auth::lookup(registry) {
            Some(c) => RegistryAuth::Basic(c.username, c.password),
            None => RegistryAuth::Anonymous,
        }
    }

    /// Obtains the tokens a push needs (and the pull ones the blob checks use) up front;
    /// a registry that wants credentials rejects the push here, with a clear message.
    pub async fn authenticate_push(&self, image: &Reference) -> Result<()> {
        let auth = self.auth(image.registry());
        let clients = [
            self.client(image.registry()),
            self.upload_client(image.registry()),
        ];
        for (client, operation) in clients.into_iter().flat_map(|client| {
            [RegistryOperation::Pull, RegistryOperation::Push].map(|operation| (client, operation))
        }) {
            client
                .auth(image, &auth, operation)
                .await
                .with_context(|| {
                    format!(
                        "authenticating to {} (nspawn login {} if it needs credentials)",
                        image.registry(),
                        image.registry()
                    )
                })?;
        }
        Ok(())
    }

    /// All repositories of a registry (follows pagination).
    pub async fn catalog(&self, registry: &str) -> Result<Vec<String>> {
        // From its parts: parsed from text, a registry without a dot or a port would be
        // a namespace of Docker Hub, and its credentials would go there.
        let probe = Reference::with_tag(
            registry.to_string(),
            "catalog".to_string(),
            "latest".to_string(),
        );
        let mut all = Vec::new();
        let mut last: Option<String> = None;
        loop {
            let page = self
                .client(registry)
                .catalog(&probe, &self.auth(registry), Some(100), last.as_deref())
                .await
                .with_context(|| format!("listing the catalog of {registry}"))?;
            let n = page.repositories.len();
            // What no reference can name is no repository: a registry's text, which ends
            // up on the bus and in tables.
            all.extend(
                page.repositories
                    .into_iter()
                    .filter(|r| crate::reference::valid_repository(r)),
            );
            if !more_pages(n, all.last().map(String::as_str), last.as_deref()) {
                break;
            }
            last = all.last().cloned();
        }
        all.sort();
        all.dedup();
        Ok(all)
    }

    /// All tags of one repository (follows pagination).
    pub async fn tags(&self, image: &Reference) -> Result<Vec<String>> {
        let mut all = Vec::new();
        let mut last: Option<String> = None;
        loop {
            let page = self
                .client(image.registry())
                .list_tags(
                    image,
                    &self.auth(image.registry()),
                    Some(100),
                    last.as_deref(),
                )
                .await
                .with_context(|| format!("listing tags of {}", image.repository()))?;
            let n = page.tags.len();
            all.extend(
                page.tags
                    .into_iter()
                    .filter(|t| crate::reference::valid_tag(t)),
            );
            if !more_pages(n, all.last().map(String::as_str), last.as_deref()) {
                break;
            }
            last = all.last().cloned();
        }
        all.sort();
        all.dedup();
        Ok(all)
    }

    /// The digest a reference names on its registry now, as the registry gives it without
    /// a download: an index's for a tag of several platforms.
    pub async fn digest(&self, image: &Reference) -> Result<String> {
        self.client(image.registry())
            .fetch_manifest_digest(image, &self.auth(image.registry()))
            .await
            .with_context(|| format!("fetching the digest of {image}"))
    }

    /// Resolves a reference to the image manifest for this host and returns it with its
    /// digest, and the index's when the reference named one. Of an index, the entry of the
    /// best platform the host runs or, without one, the first entry naming no platform
    /// whose config says the host runs it.
    pub async fn resolve(&self, image: &Reference) -> Result<Resolved> {
        let (manifest, digest) = self
            .manifest(image)
            .await
            .with_context(|| format!("fetching the manifest of {image}"))?;
        let index = match manifest {
            OciManifest::Image(manifest) => {
                return Ok(Resolved {
                    manifest,
                    digest,
                    index_digest: None,
                })
            }
            OciManifest::ImageIndex(index) => index,
        };
        let host = HostPlatform::current();
        let unlisted = match choose_entries(&index.manifests, &host) {
            IndexChoice::Listed(i) => {
                let entry = &index.manifests[i].digest;
                let (manifest, entry_digest) = self.index_entry(image, entry).await?;
                let OciManifest::Image(manifest) = manifest else {
                    bail!("manifest {entry} of {image} is an index, not an image's manifest");
                };
                return Ok(Resolved {
                    manifest,
                    digest: entry_digest,
                    index_digest: Some(digest),
                });
            }
            IndexChoice::Unlisted(unlisted) => unlisted,
        };
        let vector = platform_vector(&host);
        let mut seen = Vec::new();
        for i in unlisted {
            let entry = &index.manifests[i].digest;
            // An index inside the index is not followed.
            let (OciManifest::Image(manifest), entry_digest) =
                self.index_entry(image, entry).await?
            else {
                continue;
            };
            validate_digest(&manifest.config.digest)?;
            let config = self
                .small_blob(image, &manifest.config, CONFIG_MAX_SIZE)
                .await
                .with_context(|| format!("reading the config of manifest {entry} of {image}"))?;
            let Some((os, architecture, variant)) = config_platform(&config) else {
                continue;
            };
            if platform_rank(&vector, &host, &os, &architecture, &variant).is_some() {
                return Ok(Resolved {
                    manifest,
                    digest: entry_digest,
                    index_digest: Some(digest),
                });
            }
            seen.push(platform_name(&os, &architecture, &variant));
        }
        bail!(no_image_for(image, &host, &index.manifests, &seen))
    }

    /// What a reference names, an image's manifest or an index, with its digest.
    async fn manifest(&self, image: &Reference) -> Result<(OciManifest, String)> {
        Ok(self
            .client(image.registry())
            .pull_manifest(image, &self.auth(image.registry()))
            .await?)
    }

    /// The manifest an entry of the image's index names, by its digest.
    async fn index_entry(&self, image: &Reference, digest: &str) -> Result<(OciManifest, String)> {
        validate_digest(digest)?;
        self.manifest(&image.clone_with_digest(digest.to_string()))
            .await
            .with_context(|| format!("fetching manifest {digest} of {image}"))
    }

    /// The Sigstore bundles kept as referrers of `subject`, a digest in the image's
    /// repository: the referrers index filtered by artifact type, each entry's manifest
    /// checked to be a signature of `subject`, its one layer read. Called after `resolve`
    /// on the same Hub, which cached the token the registry wants for the repository.
    pub async fn signature_bundles(
        &self,
        image: &Reference,
        subject: &str,
    ) -> Result<Vec<Candidate>> {
        let by_digest = Reference::with_digest(
            image.registry().to_string(),
            image.repository().to_string(),
            subject.to_string(),
        );
        let index = self
            .client(by_digest.registry())
            .pull_referrers(&by_digest, Some(BUNDLE_ARTIFACT_TYPE))
            .await
            .with_context(|| format!("fetching the signatures of {image}"))?;
        let mut bundles = Vec::new();
        for entry in index
            .manifests
            .iter()
            .filter(|e| e.artifact_type.as_deref() == Some(BUNDLE_ARTIFACT_TYPE))
            .filter(|e| !attestation(e.annotations.as_ref()))
        {
            // One that cannot be read is one that does not verify, not the end of the
            // lookup: a valid signature beside it still counts.
            match self.bundle(image, subject, &entry.digest).await {
                Ok(Some(json)) => bundles.push(Candidate {
                    manifest_digest: entry.digest.clone(),
                    json,
                    unreadable: None,
                }),
                Ok(None) => {}
                Err(e) => bundles.push(Candidate {
                    manifest_digest: entry.digest.clone(),
                    json: String::new(),
                    unreadable: Some(format!("{e:#}")),
                }),
            }
        }
        Ok(bundles)
    }

    /// The bundle of a referrer: none when it is no signature of this image (somebody
    /// else's business, an attestation among them).
    async fn bundle(
        &self,
        image: &Reference,
        subject: &str,
        digest: &str,
    ) -> Result<Option<String>> {
        validate_digest(digest)?;
        let bytes = self.manifest_bytes(image, digest).await?;
        let Ok(manifest) = serde_json::from_slice::<OciImageManifest>(&bytes) else {
            return Ok(None);
        };
        if attestation(manifest.annotations.as_ref()) {
            return Ok(None);
        }
        let Ok(layer) = signature_layer(&manifest, subject) else {
            return Ok(None);
        };
        let json = self.small_blob(image, layer, BUNDLE_MAX_SIZE).await?;
        Ok(Some(String::from_utf8_lossy(&json).into_owned()))
    }

    /// A blob small enough to hold in memory, checked against its digest; refused past `cap`.
    async fn small_blob(
        &self,
        image: &Reference,
        layer: &OciDescriptor,
        cap: u64,
    ) -> Result<Vec<u8>> {
        let expected = layer
            .digest
            .strip_prefix("sha256:")
            .with_context(|| format!("blob digest {} is not sha256", layer.digest))?;
        let sized = self
            .client(image.registry())
            .pull_blob_stream(image, layer)
            .await
            .with_context(|| format!("fetching blob {}", layer.digest))?;
        let mut stream = sized.stream;
        let mut bytes = Vec::new();
        let mut hasher = Sha256::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.with_context(|| format!("downloading blob {}", layer.digest))?;
            if bytes.len() as u64 + chunk.len() as u64 > cap {
                bail!("blob {} is bigger than {cap} bytes", layer.digest);
            }
            hasher.update(&chunk);
            bytes.extend_from_slice(&chunk);
        }
        let actual = hex::encode(hasher.finalize());
        if actual != expected {
            bail!(
                "blob {} failed verification: downloaded sha256:{actual}",
                layer.digest
            );
        }
        Ok(bytes)
    }

    /// Downloads one blob (layer or config) to `dest`, verifying its sha256 digest while
    /// streaming, and reports how far it got.
    pub async fn download_blob(
        &self,
        image: &Reference,
        layer: &OciDescriptor,
        dest: &Path,
        report: Report<'_>,
    ) -> Result<()> {
        let expected = layer
            .digest
            .strip_prefix("sha256:")
            .with_context(|| format!("layer digest {} is not sha256", layer.digest))?
            .to_string();
        let sized = self
            .client(image.registry())
            .pull_blob_stream(image, layer)
            .await
            .with_context(|| format!("fetching layer {}", layer.digest))?;
        let total = sized
            .content_length
            .unwrap_or_else(|| layer.size.max(0) as u64);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        // Written next to its final name and renamed once verified, so that an interrupted
        // download never passes for a complete blob; a download that ends any other way,
        // dropped when another one failed included, takes its part file with it.
        let part = part_path(dest);
        let mut file = tokio::fs::File::create(&part)
            .await
            .with_context(|| format!("creating {}", part.display()))?;
        let unfinished = PartFile(Some(part.clone()));
        let mut hasher = Sha256::new();
        let mut stream = sized.stream;
        let mut progress = Progress::start(
            report,
            short_digest(&layer.digest),
            total,
            PROGRESS_INTERVAL,
        );
        // The manifest, checked against its digest, says how large the layer is: a
        // registry that keeps sending past that would fill the disk before the digest
        // check ever ran.
        let declared = u64::try_from(layer.size).unwrap_or(0);
        let mut written = 0u64;
        let transfer = async {
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.with_context(|| format!("downloading layer {}", layer.digest))?;
                written += chunk.len() as u64;
                if declared > 0 && written > declared {
                    bail!(
                        "layer {} is larger than the {declared} bytes its manifest declares",
                        layer.digest
                    );
                }
                hasher.update(&chunk);
                file.write_all(&chunk)
                    .await
                    .with_context(|| format!("writing {}", part.display()))?;
                progress.advance(chunk.len() as u64);
            }
            file.flush().await?;
            // On disk before it takes its name: a blob there is never downloaded again.
            file.sync_all()
                .await
                .with_context(|| format!("writing {}", part.display()))?;
            progress.finish();
            Ok::<String, anyhow::Error>(hex::encode(hasher.finalize()))
        };
        let actual = transfer.await?;
        if actual != expected {
            bail!(
                "layer {} failed verification: downloaded sha256:{actual}",
                layer.digest
            );
        }
        fs::rename(&part, dest).with_context(|| format!("moving {} into place", dest.display()))?;
        if let Some(dir) = dest.parent() {
            fs::File::open(dir)
                .and_then(|d| d.sync_all())
                .with_context(|| format!("writing {}", dir.display()))?;
        }
        unfinished.keep();
        Ok(())
    }
}

/// An image's config, read whole to see the platform of an index entry that names none.
const CONFIG_MAX_SIZE: u64 = 8 << 20;

/// The platform images run on here: the host's OS and architecture, and on arm the
/// CPU's variant.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HostPlatform {
    os: String,
    architecture: String,
    variant: String,
}

impl HostPlatform {
    fn current() -> Self {
        let architecture = crate::oci::host_architecture().to_string();
        let variant = match architecture.as_str() {
            "arm64" => "v8".to_string(),
            "arm" => fs::read_to_string("/proc/cpuinfo")
                .ok()
                .and_then(|cpuinfo| arm_cpu_variant(&cpuinfo))
                .unwrap_or_else(|| "v7".to_string()),
            _ => String::new(),
        };
        HostPlatform {
            os: std::env::consts::OS.to_string(),
            architecture,
            variant,
        }
    }

    fn name(&self) -> String {
        platform_name(&self.os, &self.architecture, &self.variant)
    }
}

/// OS/ARCHITECTURE[/VARIANT], as indexes write a platform.
fn platform_name(os: &str, architecture: &str, variant: &str) -> String {
    if variant.is_empty() {
        format!("{os}/{architecture}")
    } else {
        format!("{os}/{architecture}/{variant}")
    }
}

/// The variant of an arm CPU from the "CPU architecture" line of /proc/cpuinfo: v7 for 7.
fn arm_cpu_variant(cpuinfo: &str) -> Option<String> {
    cpuinfo.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        if !key.trim().eq_ignore_ascii_case("CPU architecture") {
            return None;
        }
        let version: String = value
            .trim()
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        (!version.is_empty()).then(|| format!("v{version}"))
    })
}

/// An architecture and its variant in the one spelling each has: x86_64 is amd64, aarch64
/// is arm64 with v8 left out, armhf is arm/v7 and an arm without a variant v7.
fn normalize_arch(architecture: &str, variant: &str) -> (String, String) {
    let architecture = architecture.to_lowercase();
    let mut variant = variant.to_lowercase();
    let architecture = match architecture.as_str() {
        "i386" => {
            variant.clear();
            "386".to_string()
        }
        "x86_64" | "x86-64" | "amd64" => {
            if variant == "v1" {
                variant.clear();
            }
            "amd64".to_string()
        }
        "aarch64" | "arm64" => {
            match variant.as_str() {
                "8" | "v8" | "v8.0" => variant.clear(),
                "9" | "9.0" | "v9.0" => variant = "v9".to_string(),
                _ => {}
            }
            "arm64".to_string()
        }
        "armhf" => {
            variant = "v7".to_string();
            "arm".to_string()
        }
        "armel" => {
            variant = "v6".to_string();
            "arm".to_string()
        }
        "arm" => {
            match variant.as_str() {
                "" | "7" => variant = "v7".to_string(),
                "5" | "6" | "8" => variant = format!("v{variant}"),
                _ => {}
            }
            "arm".to_string()
        }
        _ => architecture,
    };
    (architecture, variant)
}

/// A variant's major and minor version: v8.2 is (8, 2), v7 is (7, 0).
fn variant_version(variant: &str) -> Option<(u32, u32)> {
    let version = variant.strip_prefix('v')?;
    let (major, minor) = version.split_once('.').unwrap_or((version, "0"));
    if minor.contains('.') {
        return None;
    }
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// The arm64 variants from vMAJOR.MINOR down to vMAJOR, v8 written as none.
fn push_arm64(vector: &mut Vec<(String, String)>, major: u32, minor: u32) {
    for minor in (0..=minor).rev() {
        let variant = match (major, minor) {
            (8, 0) => String::new(),
            (_, 0) => format!("v{major}"),
            _ => format!("v{major}.{minor}"),
        };
        vector.push(("arm64".to_string(), variant));
    }
}

/// An arm variant and every older one down to v5.
fn arm_vector(variant: &str) -> Vec<(String, String)> {
    let mut vector = vec![("arm".to_string(), variant.to_string())];
    if let Some((major, 0)) = variant_version(variant) {
        for version in (5..major).rev() {
            vector.push(("arm".to_string(), format!("v{version}")));
        }
    }
    vector
}

/// The architectures and variants the host runs, best first: amd64 then 386; arm64 from
/// the CPU's variant down to v8, then 32-bit arm from v8; arm from the CPU's variant down
/// to v5. An arm64 v9.N CPU runs what v8.(N+5) does.
fn platform_vector(host: &HostPlatform) -> Vec<(String, String)> {
    let (architecture, variant) = normalize_arch(&host.architecture, &host.variant);
    match architecture.as_str() {
        "amd64" => {
            let mut vector = vec![(architecture, variant.clone())];
            if let Some((major, 0)) = variant_version(&variant) {
                for version in (1..major).rev() {
                    vector.push(normalize_arch("amd64", &format!("v{version}")));
                }
            }
            vector.push(("386".to_string(), String::new()));
            vector
        }
        "arm" => arm_vector(&variant),
        "arm64" => {
            let effective = if variant.is_empty() {
                "v8".to_string()
            } else {
                variant
            };
            let Some((major, minor)) = variant_version(&effective) else {
                return Vec::new();
            };
            let mut vector = Vec::new();
            match (major, minor) {
                (8, 0..=9) => push_arm64(&mut vector, 8, minor),
                (9, 0..=7) => {
                    push_arm64(&mut vector, 9, minor);
                    push_arm64(&mut vector, 8, (minor + 5).min(9));
                }
                _ => return vector,
            }
            vector.extend(arm_vector("v8"));
            vector
        }
        _ => vec![(architecture, variant)],
    }
}

/// Where a platform comes in `vector`, the host's order; None for one the host does not
/// run.
fn platform_rank(
    vector: &[(String, String)],
    host: &HostPlatform,
    os: &str,
    architecture: &str,
    variant: &str,
) -> Option<usize> {
    if !os.eq_ignore_ascii_case(&host.os) {
        return None;
    }
    let candidate = normalize_arch(architecture, variant);
    vector.iter().position(|wanted| *wanted == candidate)
}

/// The entries of an index to try on this host.
#[derive(Debug, PartialEq, Eq)]
enum IndexChoice {
    /// The entry of the best platform the host runs, the first of equals.
    Listed(usize),
    /// Without one, the entries that name no platform, in the index's order: each one's
    /// config says what it is for.
    Unlisted(Vec<usize>),
}

fn choose_entries(manifests: &[ImageIndexEntry], host: &HostPlatform) -> IndexChoice {
    let vector = platform_vector(host);
    let mut best: Option<(usize, usize)> = None;
    let mut unlisted = Vec::new();
    for (index, entry) in manifests.iter().enumerate() {
        let Some(platform) = &entry.platform else {
            unlisted.push(index);
            continue;
        };
        let rank = platform_rank(
            &vector,
            host,
            &platform.os.to_string(),
            &platform.architecture.to_string(),
            platform.variant.as_deref().unwrap_or(""),
        );
        if let Some(rank) = rank {
            if best.is_none_or(|(best_rank, _)| rank < best_rank) {
                best = Some((rank, index));
            }
        }
    }
    match best {
        Some((_, index)) => IndexChoice::Listed(index),
        None => IndexChoice::Unlisted(unlisted),
    }
}

/// The OS, architecture and variant an image's config gives; None for a config that is
/// no image's, an artifact's.
fn config_platform(config: &[u8]) -> Option<(String, String, String)> {
    #[derive(serde::Deserialize)]
    struct ConfigPlatform {
        os: String,
        architecture: String,
        #[serde(default)]
        variant: String,
    }
    let config: ConfigPlatform = serde_json::from_slice(config).ok()?;
    Some((config.os, config.architecture, config.variant))
}

/// Why an index has nothing for this host: the platforms it names, and those the configs
/// of its entries without one give (`unlisted`). The unknown/unknown entries that carry
/// attestations are left out.
fn no_image_for(
    image: &Reference,
    host: &HostPlatform,
    manifests: &[ImageIndexEntry],
    unlisted: &[String],
) -> String {
    let mut has: Vec<String> = Vec::new();
    for platform in manifests.iter().filter_map(|e| e.platform.as_ref()) {
        let name = platform_name(
            &platform.os.to_string(),
            &platform.architecture.to_string(),
            platform.variant.as_deref().unwrap_or(""),
        );
        if name != "unknown/unknown" && !has.contains(&name) {
            has.push(name);
        }
    }
    if !unlisted.is_empty() {
        has.push(format!(
            "entries without a platform for {}",
            unlisted.join(", ")
        ));
    }
    let has = if has.is_empty() {
        "no images".to_string()
    } else {
        has.join(", ")
    };
    format!(
        "{image} has no image for {}: its index has {has}",
        host.name()
    )
}

#[cfg(test)]
mod platform_tests {
    use super::*;
    use oci_client::manifest::Platform;

    fn host(architecture: &str, variant: &str) -> HostPlatform {
        HostPlatform {
            os: "linux".to_string(),
            architecture: architecture.to_string(),
            variant: variant.to_string(),
        }
    }

    fn entry(os: &str, architecture: &str, variant: &str) -> ImageIndexEntry {
        ImageIndexEntry {
            media_type: oci_client::manifest::OCI_IMAGE_MEDIA_TYPE.to_string(),
            digest: "sha256:0".to_string(),
            size: 1,
            platform: Some(Platform {
                architecture: architecture.into(),
                os: os.into(),
                os_version: None,
                os_features: None,
                variant: (!variant.is_empty()).then(|| variant.to_string()),
                features: None,
            }),
            annotations: None,
            artifact_type: None,
        }
    }

    fn without_platform() -> ImageIndexEntry {
        ImageIndexEntry {
            platform: None,
            ..entry("linux", "amd64", "")
        }
    }

    fn runs(host: &HostPlatform, config: &str) -> bool {
        let (os, architecture, variant) = config_platform(config.as_bytes()).unwrap();
        platform_rank(&platform_vector(host), host, &os, &architecture, &variant).is_some()
    }

    #[test]
    fn amd64_prefers_its_default_variant_then_386() {
        let entries = [
            entry("linux", "386", ""),
            entry("linux", "amd64", "v2"),
            entry("linux", "amd64", "v1"),
            entry("linux", "amd64", ""),
        ];
        // v1 and no variant are the same: the first of them.
        assert_eq!(
            choose_entries(&entries, &host("amd64", "")),
            IndexChoice::Listed(2)
        );
        assert_eq!(
            choose_entries(&entries[..2], &host("amd64", "")),
            IndexChoice::Listed(0)
        );
    }

    #[test]
    fn arm64_and_arm_prefer_the_newest_variant_they_run() {
        let entries = [
            entry("linux", "arm", "v7"),
            entry("linux", "arm64", "v9"),
            entry("linux", "arm64", "v8.1"),
            entry("linux", "arm64", "v8"),
            entry("linux", "aarch64", ""),
        ];
        assert_eq!(
            choose_entries(&entries, &host("arm64", "v8")),
            IndexChoice::Listed(3)
        );
        assert_eq!(
            choose_entries(&entries[..1], &host("arm64", "v8")),
            IndexChoice::Listed(0)
        );
        let entries = [
            entry("linux", "arm", "v5"),
            entry("linux", "arm", "v6"),
            entry("linux", "arm", ""),
        ];
        assert_eq!(
            choose_entries(&entries, &host("arm", "v7")),
            IndexChoice::Listed(2)
        );
        assert_eq!(
            choose_entries(&entries[..2], &host("arm", "v7")),
            IndexChoice::Listed(1)
        );
        assert_eq!(
            arm_cpu_variant("processor\t: 0\nCPU architecture: 7\n").as_deref(),
            Some("v7")
        );
    }

    #[test]
    fn entries_without_a_platform_are_the_last_resort_in_order() {
        let entries = [
            without_platform(),
            entry("linux", "arm64", "v8"),
            without_platform(),
            entry("linux", "amd64", ""),
            entry("windows", "amd64", ""),
        ];
        assert_eq!(
            choose_entries(&entries, &host("amd64", "")),
            IndexChoice::Listed(3)
        );
        assert_eq!(
            choose_entries(&entries[..3], &host("amd64", "")),
            IndexChoice::Unlisted(vec![0, 2])
        );
        assert_eq!(
            choose_entries(&entries[4..], &host("amd64", "")),
            IndexChoice::Unlisted(Vec::new())
        );
    }

    #[test]
    fn a_config_says_whether_the_host_runs_it() {
        let amd64 = host("amd64", "");
        assert!(runs(&amd64, r#"{"os":"linux","architecture":"amd64"}"#));
        assert!(runs(&amd64, r#"{"os":"linux","architecture":"386"}"#));
        assert!(!runs(&amd64, r#"{"os":"linux","architecture":"arm64"}"#));
        assert!(!runs(&amd64, r#"{"os":"windows","architecture":"amd64"}"#));
        // The config's variant counts: an arm config without one is v7.
        let armv6 = host("arm", "v6");
        assert!(runs(
            &armv6,
            r#"{"os":"linux","architecture":"arm","variant":"v6"}"#
        ));
        assert!(!runs(&armv6, r#"{"os":"linux","architecture":"arm"}"#));
        // An artifact's empty config is no image's.
        assert!(config_platform(b"{}").is_none());
    }

    #[test]
    fn no_image_names_what_the_index_has() {
        let image: Reference = "registry.test/app:1".parse().unwrap();
        let entries = [
            entry("linux", "arm64", "v8"),
            entry("unknown", "unknown", ""),
            entry("linux", "arm64", "v8"),
            without_platform(),
        ];
        assert_eq!(
            no_image_for(&image, &host("amd64", ""), &entries, &["linux/arm64".to_string()]),
            "registry.test/app:1 has no image for linux/amd64: its index has linux/arm64/v8, entries without a platform for linux/arm64"
        );
        assert_eq!(
            no_image_for(&image, &host("amd64", ""), &[], &[]),
            "registry.test/app:1 has no image for linux/amd64: its index has no images"
        );
    }
}

/// A reference resolved on the registry.
#[derive(Debug, Clone)]
pub struct Resolved {
    /// The manifest for this platform.
    pub manifest: OciImageManifest,
    pub digest: String,
    /// The index the reference named, when it named one rather than a manifest: what
    /// cosign signs unless told to sign each platform's manifest too.
    pub index_digest: Option<String>,
}

/// `stream`, setting `ended` once it is asked for an item past its last.
fn marking_end<S: futures_util::Stream + Unpin>(
    mut stream: S,
    ended: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> impl futures_util::Stream<Item = S::Item> {
    futures_util::stream::poll_fn(move |cx| {
        let polled = std::pin::Pin::new(&mut stream).poll_next(cx);
        if matches!(polled, std::task::Poll::Ready(None)) {
            ended.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        polled
    })
}

/// Whether an upload is given up: parts are left to send and the registry took none for
/// `since` seconds.
fn stalled_upload(all_sent: bool, since: u64) -> bool {
    !all_sent && since >= UPLOAD_STALL_SECS
}

/// Whether a Sigstore bundle's annotations say it holds an attestation, which shares the
/// signatures' artifact type and is no signature.
fn attestation(annotations: Option<&std::collections::BTreeMap<String, String>>) -> bool {
    annotations
        .and_then(|a| a.get("dev.sigstore.bundle.predicateType"))
        .is_some_and(|p| p != crate::verify::COSIGN_SIGN_PREDICATE)
}

/// The bundle layer of a manifest that is a signature of `subject`, or why it is not one:
/// the artifact type, the subject, one layer of the bundle media type, a size a bundle has.
pub fn signature_layer<'a>(
    manifest: &'a OciImageManifest,
    subject: &str,
) -> Result<&'a OciDescriptor> {
    if manifest.artifact_type.as_deref() != Some(BUNDLE_ARTIFACT_TYPE) {
        bail!(
            "artifact type {} is not a Sigstore bundle",
            manifest.artifact_type.as_deref().unwrap_or("none")
        );
    }
    match &manifest.subject {
        Some(s) if s.digest == subject => {}
        Some(s) => bail!("a signature of {} rather than {subject}", s.digest),
        None => bail!("no subject"),
    }
    let [layer] = manifest.layers.as_slice() else {
        bail!("{} layers instead of one", manifest.layers.len());
    };
    if layer.media_type != BUNDLE_ARTIFACT_TYPE {
        bail!(
            "layer media type {} is not a Sigstore bundle",
            layer.media_type
        );
    }
    if layer.size <= 0 || layer.size as u64 > BUNDLE_MAX_SIZE {
        bail!("a bundle of {} bytes", layer.size);
    }
    validate_digest(&layer.digest)?;
    Ok(layer)
}

/// Where a blob is written while it is incomplete (".part-<name>.<pid>.<n>", skipped by
/// the store): a name of its own per download, since downloads run without the store
/// lock and the service runs several at once, the same blob among them.
/// The part file of a download under way, removed unless the download kept it.
struct PartFile(Option<PathBuf>);

impl PartFile {
    fn keep(mut self) {
        self.0.take();
    }
}

impl Drop for PartFile {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = fs::remove_file(path);
        }
    }
}

pub fn part_path(dest: &Path) -> PathBuf {
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "blob".to_string());
    dest.with_file_name(format!(".part-{name}.{}", crate::store::unique_suffix()))
}

/// Whether another page must be asked for: a page with something on it that moved the
/// cursor. A registry that ignores `last` would otherwise be asked forever; one that
/// caps its pages below what `n` asks is followed to its end.
pub fn more_pages(page_len: usize, page_last: Option<&str>, previous: Option<&str>) -> bool {
    page_len > 0 && page_last.is_some() && page_last != previous
}

#[cfg(test)]
mod pagination_tests {
    use super::*;

    #[test]
    fn an_upload_waits_for_a_slow_commit_and_not_for_a_stall() {
        assert!(
            !stalled_upload(true, 10 * UPLOAD_STALL_SECS),
            "the commit is the registry's"
        );
        assert!(!stalled_upload(false, UPLOAD_STALL_SECS - 1));
        assert!(stalled_upload(false, UPLOAD_STALL_SECS));
    }

    #[test]
    fn an_attestation_is_no_signature() {
        let with = |p: &str| {
            std::collections::BTreeMap::from([(
                "dev.sigstore.bundle.predicateType".to_string(),
                p.to_string(),
            )])
        };
        assert!(attestation(Some(&with("https://slsa.dev/provenance/v1"))));
        assert!(!attestation(Some(&with(
            crate::verify::COSIGN_SIGN_PREDICATE
        ))));
        assert!(!attestation(None));
    }

    #[test]
    fn pagination_stops_on_short_or_repeated_pages() {
        assert!(more_pages(100, Some("z"), None));
        assert!(more_pages(100, Some("z"), Some("m")));
        assert!(
            more_pages(50, Some("z"), Some("m")),
            "a registry with smaller pages than asked is followed"
        );
        assert!(!more_pages(0, None, Some("z")), "an empty page is the end");
        assert!(
            !more_pages(100, Some("z"), Some("z")),
            "the cursor did not move"
        );
        assert!(!more_pages(100, None, None));
    }

    #[test]
    fn a_transfer_reports_its_start_a_few_times_and_its_end() {
        let events = std::sync::Mutex::new(Vec::new());
        let report = |event: Event| events.lock().unwrap().push(event);
        let at = |done: u64| Event::Progress {
            item: "aaef90e06523".to_string(),
            done,
            total: 300,
        };
        let mut quiet = Progress::start(
            &report,
            "aaef90e06523".to_string(),
            300,
            Duration::from_secs(3600),
        );
        for _ in 0..3 {
            quiet.advance(100);
        }
        quiet.finish();
        quiet.finish();
        assert_eq!(
            *events.lock().unwrap(),
            [at(0), at(300)],
            "between its start and its end nothing is due within the interval"
        );
        events.lock().unwrap().clear();
        let mut chatty = Progress::start(&report, "aaef90e06523".to_string(), 300, Duration::ZERO);
        for _ in 0..3 {
            chatty.advance(100);
        }
        chatty.finish();
        assert_eq!(
            *events.lock().unwrap(),
            [at(0), at(100), at(200), at(300)],
            "the end is not said twice"
        );
    }

    #[test]
    fn part_names_never_repeat() {
        let dest = Path::new("/var/lib/nspawn/blobs/sha256-abc");
        let first = part_path(dest);
        let second = part_path(dest);
        assert_ne!(first, second, "two downloads of the same blob at once");
        for part in [&first, &second] {
            let name = part.file_name().unwrap().to_string_lossy().into_owned();
            assert!(
                name.starts_with(&format!(".part-sha256-abc.{}.", std::process::id())),
                "{name}"
            );
            assert_eq!(part.parent(), dest.parent());
        }
    }
}

impl Hub {
    /// Exact bytes of a manifest, fetched by digest so that they can be stored and pushed
    /// again without changing the digest.
    pub async fn manifest_bytes(&self, image: &Reference, digest: &str) -> Result<Vec<u8>> {
        let by_digest = Reference::with_digest(
            image.registry().to_string(),
            image.repository().to_string(),
            digest.to_string(),
        );
        let accepted = [
            oci_client::manifest::IMAGE_MANIFEST_MEDIA_TYPE,
            oci_client::manifest::IMAGE_MANIFEST_LIST_MEDIA_TYPE,
            oci_client::manifest::OCI_IMAGE_MEDIA_TYPE,
            oci_client::manifest::OCI_IMAGE_INDEX_MEDIA_TYPE,
        ];
        let (bytes, fetched_digest) = self
            .client(by_digest.registry())
            .pull_manifest_raw(&by_digest, &self.auth(by_digest.registry()), &accepted)
            .await
            .with_context(|| format!("fetching manifest {digest}"))?;
        if fetched_digest != digest {
            bail!("registry returned manifest {fetched_digest} instead of {digest}");
        }
        Ok(bytes.to_vec())
    }

    pub async fn blob_exists(&self, image: &Reference, digest: &str) -> Result<bool> {
        self.client(image.registry())
            .blob_exists(image, digest)
            .await
            .with_context(|| format!("checking blob {digest} on {}", image.registry()))
    }

    /// Streams a blob file to the registry, and reports how far it got.
    pub async fn upload_blob(
        &self,
        image: &Reference,
        digest: &str,
        path: &Path,
        report: Report<'_>,
    ) -> Result<()> {
        let file = tokio::fs::File::open(path)
            .await
            .with_context(|| format!("opening {}", path.display()))?;
        let size = file.metadata().await?.len();
        let mut progress = Progress::start(report, short_digest(digest), size, PROGRESS_INTERVAL);
        // The client keeps the stream for itself, so it cannot borrow this call's
        // progress: each chunk sends its size over a channel, which closes with the
        // stream, and the counting side advances the progress from here.
        let (sent, mut counts) = tokio::sync::mpsc::unbounded_channel();
        // When the registry last took a part, and whether it took them all.
        let last = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(crate::store::now_unix()));
        let all_sent = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(size == 0));
        let taken = last.clone();
        let stream = futures_util::TryStreamExt::map_err(
            futures_util::StreamExt::inspect(
                ReaderStream::with_capacity(file, 1 << 20),
                move |chunk| {
                    if let Ok(c) = chunk {
                        taken.store(
                            crate::store::now_unix(),
                            std::sync::atomic::Ordering::Relaxed,
                        );
                        let _ = sent.send(c.len() as u64);
                    }
                },
            ),
            |e| oci_client::errors::OciDistributionError::GenericError(Some(e.to_string())),
        );
        // The client asks for a part once the registry answered the one before: asked
        // past the last, every part was taken, and only the commit is left.
        let stream = marking_end(stream, all_sent.clone());
        let upload = self.upload_client(image.registry()).push_blob_stream(
            image,
            stream,
            digest,
            Some(size),
        );
        let count = async {
            while let Some(bytes) = counts.recv().await {
                progress.advance(bytes);
            }
        };
        let work = async {
            let (uploaded, ()) = tokio::join!(upload, count);
            uploaded
        };
        let stalled = async {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                let since = crate::store::now_unix()
                    .saturating_sub(last.load(std::sync::atomic::Ordering::Relaxed));
                if stalled_upload(all_sent.load(std::sync::atomic::Ordering::Relaxed), since) {
                    return;
                }
            }
        };
        tokio::select! {
            uploaded = work => {
                uploaded.with_context(|| format!("uploading blob {digest}"))?;
            }
            () = stalled => bail!(
                "uploading blob {digest}: the registry took nothing of it for {UPLOAD_STALL_SECS} seconds"
            ),
        }
        progress.finish();
        Ok(())
    }

    /// Pushes manifest bytes verbatim under the reference's tag. Returns the registry URL.
    pub async fn push_manifest(
        &self,
        image: &Reference,
        bytes: &[u8],
        media_type: &str,
    ) -> Result<String> {
        let content_type = media_type.parse().context("invalid manifest media type")?;
        self.client(image.registry())
            .push_manifest_raw(image, Bytes::copy_from_slice(bytes), content_type)
            .await
            .with_context(|| format!("pushing the manifest of {image}"))
    }
}

pub fn short_digest(digest: &str) -> String {
    let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
    hex.chars().take(12).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_upload_ends_once_asked_past_its_last_part() {
        use futures_util::{FutureExt, StreamExt};
        use std::sync::atomic::Ordering::Relaxed;
        let ended = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut parts = marking_end(futures_util::stream::iter([1, 2]), ended.clone());
        assert_eq!(parts.next().now_or_never(), Some(Some(1)));
        assert_eq!(parts.next().now_or_never(), Some(Some(2)));
        assert!(
            !ended.load(Relaxed),
            "the last part is out, not answered yet"
        );
        assert_eq!(parts.next().now_or_never(), Some(None));
        assert!(ended.load(Relaxed));
    }

    #[test]
    fn a_ca_file_without_a_certificate_is_said() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("ca.pem");
        fs::write(&bad, "not a certificate\n").unwrap();
        let config =
            crate::config::Config::merge(Default::default(), None, Some(bad.clone())).unwrap();
        let err = Hub::new(&config).err().unwrap().to_string();
        assert!(err.contains("no PEM certificate"), "{err}");
    }

    const KALI_DIGEST: &str =
        "sha256:24a0bba642e3ed90b94428a0bf8e9cfaac49d46f4844fc1801a0267a9b9d693a";

    fn signature_manifest() -> OciImageManifest {
        serde_json::from_str(include_str!(
            "../tests/fixtures/signatures/key.manifest.json"
        ))
        .unwrap()
    }

    #[test]
    fn a_signature_manifest_is_one_of_the_subject_with_one_bundle_layer() {
        let manifest = signature_manifest();
        assert_eq!(
            signature_layer(&manifest, KALI_DIGEST).unwrap().digest,
            "sha256:ecfda5c374357d5d7ec835bfc61fc2e259ba339bd0412bfed2f8c3c6e86ac166"
        );
        let keyless: OciImageManifest = serde_json::from_str(include_str!(
            "../tests/fixtures/signatures/keyless.manifest.json"
        ))
        .unwrap();
        assert!(signature_layer(&keyless, KALI_DIGEST).is_ok());
        let other = "sha256:0000000000000000000000000000000000000000000000000000000000000000";
        let e = signature_layer(&manifest, other).unwrap_err().to_string();
        assert!(e.contains("a signature of sha256:24a0bba6"), "{e}");
        let mut m = signature_manifest();
        m.artifact_type = Some("application/vnd.example".to_string());
        assert!(signature_layer(&m, KALI_DIGEST).is_err());
        let mut m = signature_manifest();
        m.subject = None;
        assert!(signature_layer(&m, KALI_DIGEST).is_err());
        let mut m = signature_manifest();
        m.layers.push(m.layers[0].clone());
        assert!(signature_layer(&m, KALI_DIGEST).is_err());
        let mut m = signature_manifest();
        m.layers[0].media_type = "application/octet-stream".to_string();
        assert!(signature_layer(&m, KALI_DIGEST).is_err());
        let mut m = signature_manifest();
        m.layers[0].size = BUNDLE_MAX_SIZE as i64 + 1;
        assert!(signature_layer(&m, KALI_DIGEST).is_err());
        let mut m = signature_manifest();
        m.layers[0].digest = "sha256:short".to_string();
        assert!(signature_layer(&m, KALI_DIGEST).is_err());
    }

    #[test]
    fn short_digests() {
        assert_eq!(short_digest("sha256:0123456789abcdef0123"), "0123456789ab");
        assert_eq!(short_digest("abc"), "abc");
    }
}
