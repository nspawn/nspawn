//! docker build, with mkosi: an OCI layout built into a private directory, its blobs
//! copied into the store, the image assembled and recorded, ready to push.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};

use anyhow::{bail, Context as _, Result};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::api::{line, note, require_root, Context, Report};
use crate::backend::{Backend, BackendChoice};
use crate::hub::short_digest;
use crate::install::{ensure_replaceable, install, remove_existing, Install};
use crate::layout::{sha256_digest, sha256_file, Layout};
use crate::oci::Mode;
use crate::reference::{validate_machine_name, ImageRef};
use crate::store::now_unix;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildRequest {
    /// Directory with the mkosi configuration (mkosi.conf, mkosi.conf.d, ...).
    pub directory: PathBuf,
    /// Reference for the result, for example myapp:1 or hub.example/team/app:2.
    pub tag: String,
    /// Local name; derived from the tag when missing.
    pub name: Option<String>,
    pub distribution: Option<String>,
    pub release: Option<String>,
    /// mkosi profiles to enable.
    pub profile: Vec<String>,
    pub backend: BackendChoice,
    pub mode: Option<Mode>,
    pub force: bool,
    /// Keep the mkosi output directory instead of deleting it after the import.
    pub keep_output: bool,
    /// Extra arguments passed to mkosi verbatim.
    pub mkosi_args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Built {
    pub name: String,
    pub reference: String,
    pub mode: Mode,
    /// The mkosi output directory, when it was kept.
    pub output: Option<PathBuf>,
}

pub async fn build(ctx: &Context, request: &BuildRequest, report: Report<'_>) -> Result<Built> {
    require_root("build")?;
    let config = &ctx.config;
    let image = ImageRef::parse(&request.tag, &config.registry)?;
    if image.digest.is_some() {
        bail!("a build tag cannot carry a digest");
    }
    let name = request.name.clone().unwrap_or_else(|| image.local_name());
    validate_machine_name(&name)?;
    let directory = fs::canonicalize(&request.directory)
        .with_context(|| format!("build directory {}", request.directory.display()))?;
    if !directory.join("mkosi.conf").is_file() && !directory.join("mkosi.conf.d").is_dir() {
        bail!("{} has no mkosi.conf or mkosi.conf.d", directory.display());
    }
    let mkosi = find_in_path("mkosi").context("mkosi is not installed or not in PATH")?;

    let sd = ctx.sd().await?;
    let store = &ctx.store;
    store.init()?;
    ensure_replaceable(store, sd, &name, request.force).await?;
    let choice = if request.backend == BackendChoice::Auto {
        config.backend
    } else {
        request.backend
    };
    let backend = Backend::choose(choice, sd).await?;
    if backend == Backend::Mstack {
        note(report, crate::backend::MSTACK_EXPERIMENTAL);
    }

    let build_id = format!("{name}-{}-{}", now_unix(), crate::store::unique_suffix());
    let output_dir = config.state_dir.join("builds").join(&build_id);
    let cache_dir = config.state_dir.join("cache").join("mkosi");
    // mkosi builds in its workspace and renames the result into place; kept next to
    // the output so that what lands there carries the store's labels, not /var/tmp's,
    // and one per build, since the service may run two at once.
    let workspace_dir = config
        .state_dir
        .join("cache")
        .join("mkosi-workspace")
        .join(&build_id);
    for dir in [&output_dir, &cache_dir, &workspace_dir] {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }

    let argv = mkosi_arguments(
        request,
        &image,
        &directory,
        &output_dir,
        &cache_dir,
        &workspace_dir,
    );
    line(report, format!("running: mkosi {}", argv.join(" ")));
    let status = run_mkosi(&mkosi, &argv, report).await;
    let _ = fs::remove_dir_all(&workspace_dir);
    let status = status?;
    if !status.success() {
        if !request.keep_output {
            let _ = fs::remove_dir_all(&output_dir);
        }
        bail!("mkosi failed with {status}");
    }
    let _lock = store.lock().await?;
    // The name may have been taken while mkosi ran.
    ensure_replaceable(store, sd, &name, request.force).await?;
    let outcome: Result<Mode> = async {
        let layout = Layout::find_below(&output_dir)?;
        let mut manifest = layout.manifest.clone();
        let annotations = manifest.annotations.get_or_insert_with(Default::default);
        if let Some(tag) = &image.tag {
            annotations.insert("org.opencontainers.image.version".to_string(), tag.clone());
        }
        annotations.insert(
            "org.opencontainers.image.ref.name".to_string(),
            image.to_string(),
        );
        annotations.insert(
            "org.nspawn.builder".to_string(),
            format!("nspawn {} / mkosi", env!("CARGO_PKG_VERSION")),
        );
        let manifest_bytes = serde_json::to_vec(&manifest)?;
        let manifest_digest = sha256_digest(&manifest_bytes);

        for descriptor in manifest
            .layers
            .iter()
            .chain(std::iter::once(&manifest.config))
        {
            let source = layout.blob_path(&descriptor.digest)?;
            let actual = sha256_file(&source)?;
            if actual != descriptor.digest {
                bail!(
                    "blob {} in the mkosi output has digest {actual}",
                    descriptor.digest
                );
            }
            let dest = store.blob_path(&descriptor.digest);
            if !dest.exists() {
                let part = crate::hub::part_path(&dest);
                fs::copy(&source, &part)
                    .with_context(|| format!("copying {}", source.display()))?;
                fs::rename(&part, &dest)
                    .with_context(|| format!("moving {} into place", dest.display()))?;
            }
        }
        line(
            report,
            format!(
                "built {image}: manifest {} with {} layer(s), assembling as {}",
                short_digest(&manifest_digest),
                manifest.layers.len(),
                backend.name()
            ),
        );
        remove_existing(store, sd, &name, report).await?;
        install(
            store,
            sd,
            config,
            backend,
            Install {
                name: &name,
                reference: &image.to_string(),
                manifest_bytes: &manifest_bytes,
                manifest: &manifest,
                manifest_digest: &manifest_digest,
                origin: "build",
                mode: request.mode,
                signed_by: None,
                signed_at: None,
            },
            report,
        )
        .await
    }
    .await;
    let mode = match outcome {
        Ok(mode) => mode,
        Err(e) => {
            if !request.keep_output {
                let _ = fs::remove_dir_all(&output_dir);
            }
            return Err(e);
        }
    };
    let output = if request.keep_output {
        Some(output_dir)
    } else {
        let _ = fs::remove_dir_all(&output_dir);
        None
    };
    crate::api::events::emit(
        "machine",
        "build",
        &name,
        &[
            ("image", &image.to_string()),
            ("reference", &image.to_string()),
        ],
    );
    Ok(Built {
        name,
        reference: image.to_string(),
        mode,
        output,
    })
}

/// Runs mkosi with both its streams reported line by line as they come, so that a
/// caller sees the build wherever it sits.
async fn run_mkosi(mkosi: &Path, argv: &[String], report: Report<'_>) -> Result<ExitStatus> {
    let (read, write) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)
        .context("creating a pipe for mkosi's output")?;
    let mut child = {
        let write2 = write.try_clone()?;
        let mut command = tokio::process::Command::new(mkosi);
        command
            .args(argv)
            .stdin(Stdio::null())
            .stdout(Stdio::from(write))
            .stderr(Stdio::from(write2));
        command
            .spawn()
            .with_context(|| format!("running {}", mkosi.display()))?
    };
    let mut lines = BufReader::new(tokio::fs::File::from_std(fs::File::from(read))).lines();
    while let Ok(Some(text)) = lines.next_line().await {
        line(report, text);
    }
    child.wait().await.context("waiting for mkosi")
}

/// The mkosi command line: OCI output with zstd layers into a private directory.
pub fn mkosi_arguments(
    request: &BuildRequest,
    image: &ImageRef,
    directory: &Path,
    output_dir: &Path,
    cache_dir: &Path,
    workspace_dir: &Path,
) -> Vec<String> {
    let mut argv = vec![
        format!("--directory={}", directory.display()),
        "--format=oci".to_string(),
        "--compress-output=zstd".to_string(),
        format!("--output-directory={}", output_dir.display()),
        format!("--cache-directory={}", cache_dir.display()),
        format!("--workspace-directory={}", workspace_dir.display()),
        format!("--image-id={}", image.repository.replace('/', "-")),
    ];
    if let Some(d) = &request.distribution {
        argv.push(format!("--distribution={d}"));
    }
    if let Some(r) = &request.release {
        argv.push(format!("--release={r}"));
    }
    for p in &request.profile {
        argv.push(format!("--profile={p}"));
    }
    argv.extend(request.mkosi_args.iter().cloned());
    argv.push("--force".to_string());
    argv.push("build".to_string());
    argv
}

fn find_in_path(program: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(program))
            .find(|candidate| candidate.is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mkosi_command_line() {
        let request = BuildRequest {
            directory: PathBuf::from("."),
            tag: "team/app:2".into(),
            name: None,
            distribution: Some("fedora".into()),
            release: Some("44".into()),
            profile: vec!["web".into()],
            backend: BackendChoice::Auto,
            mode: None,
            force: false,
            keep_output: false,
            mkosi_args: vec!["--debug".into()],
        };
        let image = ImageRef::parse("team/app:2", "hub.example").unwrap();
        let argv = mkosi_arguments(
            &request,
            &image,
            Path::new("/src"),
            Path::new("/out"),
            Path::new("/cache"),
            Path::new("/work"),
        );
        assert_eq!(
            argv,
            vec![
                "--directory=/src",
                "--format=oci",
                "--compress-output=zstd",
                "--output-directory=/out",
                "--cache-directory=/cache",
                "--workspace-directory=/work",
                "--image-id=team-app",
                "--distribution=fedora",
                "--release=44",
                "--profile=web",
                "--debug",
                "--force",
                "build",
            ]
        );
    }
}
