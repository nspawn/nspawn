//! Registry client: catalog, tags, manifest resolution and verified layer downloads.

use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use futures_util::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use oci_client::client::{Certificate, CertificateEncoding, ClientConfig, ClientProtocol};
use oci_client::manifest::{OciDescriptor, OciImageManifest};
use oci_client::secrets::RegistryAuth;
use oci_client::{Client, Reference};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;

use crate::config::Config;

pub struct Hub {
    client: Client,
    auth: RegistryAuth,
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
            auth: RegistryAuth::Anonymous,
        })
    }

    /// All repositories of a registry (follows pagination).
    pub async fn catalog(&self, registry: &str) -> Result<Vec<String>> {
        let probe = Reference::try_from(format!("{registry}/catalog"))?;
        let mut all = Vec::new();
        let mut last: Option<String> = None;
        loop {
            let page = self
                .client
                .catalog(&probe, &self.auth, Some(100), last.as_deref())
                .await
                .with_context(|| format!("listing the catalog of {registry}"))?;
            let n = page.repositories.len();
            all.extend(page.repositories);
            if n < 100 {
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
                .list_tags(image, &self.auth, Some(100), last.as_deref())
                .await
                .with_context(|| format!("listing tags of {}", image.repository()))?;
            let n = page.tags.len();
            all.extend(page.tags);
            if n < 100 {
                break;
            }
            last = all.last().cloned();
        }
        all.sort();
        all.dedup();
        Ok(all)
    }

    /// Resolves a reference to the image manifest for this platform (indexes are followed)
    /// and returns it with its digest.
    pub async fn resolve(&self, image: &Reference) -> Result<(OciImageManifest, String)> {
        self.client
            .pull_image_manifest(image, &self.auth)
            .await
            .with_context(|| format!("fetching the manifest of {image}"))
    }

    /// Downloads one blob (layer or config) to `dest`, verifying its sha256 digest while streaming.
    pub async fn download_blob(
        &self,
        image: &Reference,
        layer: &OciDescriptor,
        dest: &Path,
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
        let total = sized.content_length.unwrap_or(layer.size.max(0) as u64);
        let bar = ProgressBar::new(total);
        bar.set_style(
            ProgressStyle::with_template(
                "{msg} {bar:30} {bytes}/{total_bytes} ({bytes_per_sec}, {eta})",
            )
            .expect("valid template"),
        );
        bar.set_message(short_digest(&layer.digest));

        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut file = tokio::fs::File::create(dest)
            .await
            .with_context(|| format!("creating {}", dest.display()))?;
        let mut hasher = Sha256::new();
        let mut stream = sized.stream;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.with_context(|| format!("downloading layer {}", layer.digest))?;
            hasher.update(&chunk);
            file.write_all(&chunk)
                .await
                .with_context(|| format!("writing {}", dest.display()))?;
            bar.inc(chunk.len() as u64);
        }
        file.flush().await?;
        bar.finish_and_clear();

        let actual = hex::encode(hasher.finalize());
        if actual != expected {
            let _ = fs::remove_file(dest);
            bail!(
                "layer {} failed verification: downloaded sha256:{actual}",
                layer.digest
            );
        }
        Ok(())
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
            .pull_manifest_raw(&by_digest, &self.auth, &accepted)
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

    /// Streams a blob file to the registry.
    pub async fn upload_blob(&self, image: &Reference, digest: &str, path: &Path) -> Result<()> {
        let file = tokio::fs::File::open(path)
            .await
            .with_context(|| format!("opening {}", path.display()))?;
        let size = file.metadata().await?.len();
        let bar = ProgressBar::new(size);
        bar.set_style(
            ProgressStyle::with_template(
                "{msg} {bar:30} {bytes}/{total_bytes} ({bytes_per_sec}, {eta})",
            )
            .expect("valid template"),
        );
        bar.set_message(short_digest(digest));
        let progress = bar.clone();
        let stream = futures_util::TryStreamExt::map_err(
            futures_util::StreamExt::inspect(
                ReaderStream::with_capacity(file, 1 << 20),
                move |chunk| {
                    if let Ok(c) = chunk {
                        progress.inc(c.len() as u64);
                    }
                },
            ),
            |e| oci_client::errors::OciDistributionError::GenericError(Some(e.to_string())),
        );
        self.client
            .push_blob_stream(image, stream, digest)
            .await
            .with_context(|| format!("uploading blob {digest}"))?;
        bar.finish_and_clear();
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

    #[test]
    fn short_digests() {
        assert_eq!(short_digest("sha256:0123456789abcdef0123"), "0123456789ab");
        assert_eq!(short_digest("abc"), "abc");
    }
}
