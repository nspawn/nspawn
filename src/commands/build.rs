use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::backend::Backend;
use crate::cli::BackendChoice;
use crate::cli::BuildArgs;
use crate::commands::require_root;
use crate::config::Config;
use crate::hub::short_digest;
use crate::install::{ensure_replaceable, install, remove_existing, Install};
use crate::layout::{sha256_digest, sha256_file, Layout};
use crate::oci::Mode;
use crate::reference::{validate_machine_name, ImageRef};
use crate::store::{now_unix, Store};
use crate::systemd::Systemd;

pub async fn run(args: BuildArgs, config: &Config) -> Result<()> {
    require_root("build")?;
    let image = ImageRef::parse(&args.tag, &config.registry)?;
    if image.digest.is_some() {
        bail!("a build tag cannot carry a digest");
    }
    let name = args.name.clone().unwrap_or_else(|| image.local_name());
    validate_machine_name(&name)?;
    let directory = fs::canonicalize(&args.directory)
        .with_context(|| format!("build directory {}", args.directory.display()))?;
    if !directory.join("mkosi.conf").is_file() && !directory.join("mkosi.conf.d").is_dir() {
        bail!("{} has no mkosi.conf or mkosi.conf.d", directory.display());
    }
    let mkosi = find_in_path("mkosi").context("mkosi is not installed or not in PATH")?;

    let sd = Systemd::connect().await?;
    let store = Store::new(&config.machines_dir, &config.state_dir);
    store.init()?;
    ensure_replaceable(&store, &sd, &name, args.force).await?;
    let choice = if args.backend == BackendChoice::Auto {
        config.backend
    } else {
        args.backend
    };
    let backend = Backend::choose(choice, &sd).await?;

    let output_dir = config
        .state_dir
        .join("builds")
        .join(format!("{name}-{}", now_unix()));
    let cache_dir = config.state_dir.join("cache").join("mkosi");
    fs::create_dir_all(&output_dir)?;
    fs::create_dir_all(&cache_dir)?;

    let argv = mkosi_arguments(&args, &image, &directory, &output_dir, &cache_dir);
    println!("running: mkosi {}", argv.join(" "));
    let status = Command::new(&mkosi)
        .args(&argv)
        .status()
        .with_context(|| format!("running {}", mkosi.display()))?;
    if !status.success() {
        if !args.keep_output {
            let _ = fs::remove_dir_all(&output_dir);
        }
        bail!("mkosi failed with {status}");
    }
    let _lock = store.lock()?;
    // The name may have been taken while mkosi ran.
    ensure_replaceable(&store, &sd, &name, args.force).await?;
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
        println!(
            "built {image}: manifest {} with {} layer(s), assembling as {}",
            short_digest(&manifest_digest),
            manifest.layers.len(),
            backend.name()
        );
        remove_existing(&store, &sd, &name).await?;
        let mode = install(
            &store,
            &sd,
            backend,
            Install {
                name: &name,
                reference: &image.to_string(),
                manifest_bytes: &manifest_bytes,
                manifest: &manifest,
                manifest_digest: &manifest_digest,
                origin: "build",
                mode: args.mode.to_mode(),
            },
        )
        .await?;
        Ok(mode)
    }
    .await;
    let mode = match outcome {
        Ok(mode) => mode,
        Err(e) => {
            if !args.keep_output {
                let _ = fs::remove_dir_all(&output_dir);
            }
            return Err(e);
        }
    };
    if args.keep_output {
        println!("mkosi output kept at {}", output_dir.display());
    } else {
        let _ = fs::remove_dir_all(&output_dir);
    }
    println!(
        "image {name} ({} image) is ready: nspawn start {name}, nspawn push {name}",
        mode.name()
    );
    Ok(())
}

/// The mkosi command line: OCI output with zstd layers into a private directory.
pub fn mkosi_arguments(
    args: &BuildArgs,
    image: &ImageRef,
    directory: &Path,
    output_dir: &Path,
    cache_dir: &Path,
) -> Vec<String> {
    let mut argv = vec![
        format!("--directory={}", directory.display()),
        "--format=oci".to_string(),
        "--compress-output=zstd".to_string(),
        format!("--output-directory={}", output_dir.display()),
        format!("--cache-directory={}", cache_dir.display()),
        format!("--image-id={}", image.repository.replace('/', "-")),
    ];
    if let Some(d) = &args.distribution {
        argv.push(format!("--distribution={d}"));
    }
    if let Some(r) = &args.release {
        argv.push(format!("--release={r}"));
    }
    for p in &args.profile {
        argv.push(format!("--profile={p}"));
    }
    argv.extend(args.mkosi_args.iter().cloned());
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
        let args = BuildArgs {
            directory: PathBuf::from("."),
            tag: "team/app:2".into(),
            name: None,
            distribution: Some("fedora".into()),
            release: Some("44".into()),
            profile: vec!["web".into()],
            backend: BackendChoice::Auto,
            mode: crate::cli::ModeChoice::Auto,
            force: false,
            keep_output: false,
            mkosi_args: vec!["--debug".into()],
        };
        let image = ImageRef::parse("team/app:2", "hub.example").unwrap();
        let argv = mkosi_arguments(
            &args,
            &image,
            Path::new("/src"),
            Path::new("/out"),
            Path::new("/cache"),
        );
        assert_eq!(
            argv,
            vec![
                "--directory=/src",
                "--format=oci",
                "--compress-output=zstd",
                "--output-directory=/out",
                "--cache-directory=/cache",
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
