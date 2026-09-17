//! Reading OCI image layouts (what `mkosi -t oci` produces): index.json plus blobs/.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use oci_client::manifest::{OciImageIndex, OciImageManifest};
use sha2::{Digest, Sha256};

pub struct Layout {
    pub dir: PathBuf,
    pub manifest: OciImageManifest,
}

impl Layout {
    /// Opens a layout directory and picks its (single) image manifest.
    pub fn open(dir: &Path) -> Result<Self> {
        if !dir.join("oci-layout").is_file() || !dir.join("index.json").is_file() {
            bail!(
                "{} is not an OCI image layout (no oci-layout or index.json)",
                dir.display()
            );
        }
        let index: OciImageIndex = serde_json::from_slice(&fs::read(dir.join("index.json"))?)
            .with_context(|| format!("parsing {}", dir.join("index.json").display()))?;
        let entry = match index.manifests.len() {
            0 => bail!("{} contains no image", dir.display()),
            1 => &index.manifests[0],
            _ => index
                .manifests
                .iter()
                .find(|m| m.media_type.contains("image.manifest"))
                .context("the layout has several manifests and none is an image manifest")?,
        };
        let manifest_bytes = read_blob(dir, &entry.digest)?;
        let manifest: OciImageManifest = serde_json::from_slice(&manifest_bytes)
            .with_context(|| format!("parsing manifest {}", entry.digest))?;
        Ok(Layout {
            dir: dir.to_path_buf(),
            manifest,
        })
    }

    pub fn blob_path(&self, digest: &str) -> Result<PathBuf> {
        blob_path(&self.dir, digest)
    }

    /// Finds a layout below `root` (mkosi names the output directory after the image).
    pub fn find_below(root: &Path) -> Result<Self> {
        let mut candidates = Vec::new();
        for entry in fs::read_dir(root).with_context(|| format!("reading {}", root.display()))? {
            let path = entry?.path();
            if path.is_dir() && path.join("oci-layout").is_file() {
                candidates.push(path);
            }
        }
        candidates.sort();
        candidates.dedup_by_key(|p| {
            let canonical = fs::canonicalize(&*p);
            canonical.unwrap_or_else(|_| p.clone())
        });
        match candidates.as_slice() {
            [one] => Layout::open(one),
            [] => bail!("mkosi produced no OCI layout below {}", root.display()),
            many => bail!("several OCI layouts below {}: {:?}", root.display(), many),
        }
    }
}

fn blob_path(dir: &Path, digest: &str) -> Result<PathBuf> {
    let (algo, hex) = digest
        .split_once(':')
        .with_context(|| format!("malformed digest {digest}"))?;
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("malformed digest {digest}");
    }
    Ok(dir.join("blobs").join(algo).join(hex))
}

fn read_blob(dir: &Path, digest: &str) -> Result<Vec<u8>> {
    let path = blob_path(dir, digest)?;
    fs::read(&path).with_context(|| format!("reading {}", path.display()))
}

pub fn sha256_digest(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

/// Streams a file through sha256 and returns its digest.
pub fn sha256_file(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut file = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("sha256:{}", hex::encode(hasher.finalize())))
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn write_layout(dir: &Path, layer_data: &[u8]) -> (String, String) {
        let layer_digest = sha256_digest(layer_data);
        let config =
            br#"{"architecture":"amd64","os":"linux","rootfs":{"type":"layers","diff_ids":[]}}"#;
        let config_digest = sha256_digest(config);
        let manifest = format!(
            r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{config_digest}","size":{}}},"layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar+zstd","digest":"{layer_digest}","size":{}}}]}}"#,
            config.len(),
            layer_data.len()
        );
        let manifest_digest = sha256_digest(manifest.as_bytes());
        let blobs = dir.join("blobs/sha256");
        fs::create_dir_all(&blobs).unwrap();
        fs::write(blobs.join(&layer_digest[7..]), layer_data).unwrap();
        fs::write(blobs.join(&config_digest[7..]), config).unwrap();
        fs::write(blobs.join(&manifest_digest[7..]), &manifest).unwrap();
        fs::write(dir.join("oci-layout"), r#"{"imageLayoutVersion":"1.0.0"}"#).unwrap();
        fs::write(
            dir.join("index.json"),
            format!(
                r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[{{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"{manifest_digest}","size":{}}}]}}"#,
                manifest.len()
            ),
        )
        .unwrap();
        (manifest_digest, layer_digest)
    }

    #[test]
    fn opens_a_layout_and_finds_it_below_a_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("image_0_x86-64");
        fs::create_dir_all(&dir).unwrap();
        let (manifest_digest, layer_digest) = write_layout(&dir, b"layer bytes");
        let layout = Layout::open(&dir).unwrap();
        assert!(manifest_digest.starts_with("sha256:"));
        assert_eq!(
            layout.manifest.config.media_type,
            "application/vnd.oci.image.config.v1+json"
        );
        assert_eq!(layout.manifest.layers[0].digest, layer_digest);
        assert_eq!(
            fs::read(layout.blob_path(&layer_digest).unwrap()).unwrap(),
            b"layer bytes"
        );
        assert_eq!(
            sha256_file(&layout.blob_path(&layer_digest).unwrap()).unwrap(),
            layer_digest
        );
        let found = Layout::find_below(tmp.path()).unwrap();
        assert_eq!(found.dir, dir);
        assert!(Layout::open(tmp.path()).is_err());
        assert!(blob_path(&dir, "sha256:not-hex").is_err());
    }
}
