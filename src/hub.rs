//! Registry client: catalog, tags, manifest resolution and verified layer downloads.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use futures_util::StreamExt;
use oci_client::client::{Certificate, CertificateEncoding, ClientConfig, ClientProtocol};
use oci_client::manifest::{OciDescriptor, OciImageManifest};
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
    client: Client,
}

impl Hub {
    pub fn new(config: &Config) -> Result<Self> {
        let mut extra_root_certificates = Vec::new();
        if let Some(path) = &config.ca_cert {
            let data = fs::read(path)
                .with_context(|| format!("reading CA certificate {}", path.display()))?;
            extra_root_certificates.push(Certificate {
                encoding: CertificateEncoding::Pem,
                data,
            });
        }
        let client_config = ClientConfig {
            protocol: ClientProtocol::Https,
            extra_root_certificates,
            platform_resolver: Some(Box::new(oci_client::client::current_platform_resolver)),
            user_agent: concat!("nspawn/", env!("CARGO_PKG_VERSION")),
            ..Default::default()
        };
        Ok(Hub {
            client: Client::new(client_config),
        })
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
        for operation in [RegistryOperation::Pull, RegistryOperation::Push] {
            self.client
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
        let probe = Reference::try_from(format!("{registry}/catalog"))?;
        let mut all = Vec::new();
        let mut last: Option<String> = None;
        loop {
            let page = self
                .client
                .catalog(&probe, &self.auth(registry), Some(100), last.as_deref())
                .await
                .with_context(|| format!("listing the catalog of {registry}"))?;
            let n = page.repositories.len();
            all.extend(page.repositories);
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
                .client
                .list_tags(
                    image,
                    &self.auth(image.registry()),
                    Some(100),
                    last.as_deref(),
                )
                .await
                .with_context(|| format!("listing tags of {}", image.repository()))?;
            let n = page.tags.len();
            all.extend(page.tags);
            if !more_pages(n, all.last().map(String::as_str), last.as_deref()) {
                break;
            }
            last = all.last().cloned();
        }
        all.sort();
        all.dedup();
        Ok(all)
    }

    /// Resolves a reference to the image manifest for this platform (indexes are followed)
    /// and returns it with its digest, and the index's when the reference named one.
    pub async fn resolve(&self, image: &Reference) -> Result<Resolved> {
        let (manifest, digest, index_digest) = self
            .client
            .pull_image_manifest_and_list_digest(image, &self.auth(image.registry()))
            .await
            .with_context(|| format!("fetching the manifest of {image}"))?;
        Ok(Resolved {
            manifest,
            digest,
            index_digest,
        })
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
            .client
            .pull_referrers(&by_digest, Some(BUNDLE_ARTIFACT_TYPE))
            .await
            .with_context(|| format!("fetching the signatures of {image}"))?;
        let mut bundles = Vec::new();
        for entry in index
            .manifests
            .iter()
            .filter(|e| e.artifact_type.as_deref() == Some(BUNDLE_ARTIFACT_TYPE))
        {
            validate_digest(&entry.digest)?;
            let bytes = self.manifest_bytes(image, &entry.digest).await?;
            // A referrer that is not a signature of this image is somebody else's business.
            let Ok(manifest) = serde_json::from_slice::<OciImageManifest>(&bytes) else {
                continue;
            };
            let Ok(layer) = signature_layer(&manifest, subject) else {
                continue;
            };
            let json = self.small_blob(image, layer, BUNDLE_MAX_SIZE).await?;
            bundles.push(Candidate {
                manifest_digest: entry.digest.clone(),
                json: String::from_utf8_lossy(&json).into_owned(),
            });
        }
        Ok(bundles)
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
            .client
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
            .client
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
        let transfer = async {
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.with_context(|| format!("downloading layer {}", layer.digest))?;
                hasher.update(&chunk);
                file.write_all(&chunk)
                    .await
                    .with_context(|| format!("writing {}", part.display()))?;
                progress.advance(chunk.len() as u64);
            }
            file.flush().await?;
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
        unfinished.keep();
        Ok(())
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

/// Whether another page must be asked for: a full page that moved the cursor. A registry
/// that ignores `n` and `last` would otherwise be asked forever.
pub fn more_pages(page_len: usize, page_last: Option<&str>, previous: Option<&str>) -> bool {
    page_len >= 100 && page_last.is_some() && page_last != previous
}

#[cfg(test)]
mod pagination_tests {
    use super::*;

    #[test]
    fn pagination_stops_on_short_or_repeated_pages() {
        assert!(more_pages(100, Some("z"), None));
        assert!(more_pages(100, Some("z"), Some("m")));
        assert!(!more_pages(99, Some("z"), None));
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
            .client
            .pull_manifest_raw(&by_digest, &self.auth(by_digest.registry()), &accepted)
            .await
            .with_context(|| format!("fetching manifest {digest}"))?;
        if fetched_digest != digest {
            bail!("registry returned manifest {fetched_digest} instead of {digest}");
        }
        Ok(bytes.to_vec())
    }

    pub async fn blob_exists(&self, image: &Reference, digest: &str) -> Result<bool> {
        self.client
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
        let stream = futures_util::TryStreamExt::map_err(
            futures_util::StreamExt::inspect(
                ReaderStream::with_capacity(file, 1 << 20),
                move |chunk| {
                    if let Ok(c) = chunk {
                        let _ = sent.send(c.len() as u64);
                    }
                },
            ),
            |e| oci_client::errors::OciDistributionError::GenericError(Some(e.to_string())),
        );
        let upload = self
            .client
            .push_blob_stream(image, stream, digest, Some(size));
        let count = async {
            while let Some(bytes) = counts.recv().await {
                progress.advance(bytes);
            }
        };
        let (uploaded, ()) = tokio::join!(upload, count);
        uploaded.with_context(|| format!("uploading blob {digest}"))?;
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
        self.client
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
