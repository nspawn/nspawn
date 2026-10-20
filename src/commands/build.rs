//! build: a Dockerfile build or an mkosi one, told apart by the flags and the context.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context as _, Result};
use nix::fcntl::OFlag;
use zbus::zvariant::Value;

use crate::cli::BuildArgs;
use crate::client::{self, Client};
use crate::config::Config;

/// The names a Dockerfile goes by in a context, in the order they are looked for.
pub const DOCKERFILE_NAMES: [&str; 3] = ["Dockerfile", "dockerfile", "Containerfile"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Dockerfile,
    Mkosi,
}

/// Which builder `args` ask for: a flag only one of them takes, else what the context
/// holds, a Dockerfile first.
pub fn kind(args: &BuildArgs) -> Result<Kind> {
    if let Some(text) = args.dockerfile.refused() {
        bail!("{text}");
    }
    let dockerfile = args.dockerfile.given();
    let mkosi = args.mkosi.given();
    if dockerfile && mkosi {
        bail!("the flags of a Dockerfile build and of an mkosi build do not go together");
    }
    if dockerfile || args.directory == Path::new("-") {
        return Ok(Kind::Dockerfile);
    }
    if mkosi {
        return Ok(Kind::Mkosi);
    }
    let dir = &args.directory;
    if DOCKERFILE_NAMES.iter().any(|n| dir.join(n).is_file()) {
        return Ok(Kind::Dockerfile);
    }
    if dir.join("mkosi.conf").is_file() || dir.join("mkosi.conf.d").is_dir() {
        return Ok(Kind::Mkosi);
    }
    bail!(
        "{} has no Dockerfile and no mkosi.conf; -f names a Dockerfile elsewhere",
        dir.display()
    )
}

/// The Dockerfile's bytes and where they came from, and the context: a directory, or a
/// tar given on standard input.
struct Source {
    dockerfile: Vec<u8>,
    /// The Dockerfile's path, for the ignore file next to it.
    dockerfile_path: Option<PathBuf>,
    context: Context,
}

pub enum Context {
    Dir(PathBuf),
    Tar(Vec<u8>),
}

fn read_stdin() -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .read_to_end(&mut bytes)
        .context("reading standard input")?;
    Ok(bytes)
}

/// Whether bytes are a tar, compressed or not: what `build -` reads as a context.
fn is_tar(bytes: &[u8]) -> bool {
    let plain = |b: &[u8]| b.len() > 262 && &b[257..262] == b"ustar";
    if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut head = vec![0u8; 512];
        let mut reader = flate2::read::GzDecoder::new(bytes);
        let mut filled = 0;
        while filled < head.len() {
            match reader.read(&mut head[filled..]) {
                Ok(0) | Err(_) => break,
                Ok(n) => filled += n,
            }
        }
        return plain(&head[..filled]);
    }
    plain(bytes)
}

/// A file of a context tar.
fn file_in_tar(tar: &[u8], name: &str) -> Result<Option<Vec<u8>>> {
    let reader: Box<dyn Read> = if tar.starts_with(&[0x1f, 0x8b]) {
        Box::new(flate2::read::GzDecoder::new(tar))
    } else {
        Box::new(tar)
    };
    let mut archive = tar::Archive::new(reader);
    for entry in archive.entries().context("reading the context tar")? {
        let mut entry = entry.context("reading the context tar")?;
        let path = entry
            .path()?
            .to_string_lossy()
            .trim_start_matches("./")
            .to_string();
        if path == name {
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes)?;
            return Ok(Some(bytes));
        }
    }
    Ok(None)
}

fn source(args: &BuildArgs) -> Result<Source> {
    let file = args.dockerfile.file.as_deref();
    if args.directory == Path::new("-") {
        let input = read_stdin()?;
        if is_tar(&input) {
            let name = file.unwrap_or("Dockerfile");
            let dockerfile = file_in_tar(&input, name)?
                .with_context(|| format!("the context on standard input has no {name}"))?;
            return Ok(Source {
                dockerfile,
                dockerfile_path: None,
                context: Context::Tar(input),
            });
        }
        return Ok(Source {
            dockerfile: input,
            dockerfile_path: None,
            context: Context::Tar(Vec::new()),
        });
    }
    let dir = args.directory.clone();
    if !dir.is_dir() {
        bail!("{}: no such directory", dir.display());
    }
    let (dockerfile, path) = match file {
        Some("-") => (read_stdin()?, None),
        Some(path) => {
            let path = PathBuf::from(path);
            if path.is_dir() {
                bail!(
                    "-f {}: -f names the Dockerfile; --force replaces an existing image",
                    path.display()
                );
            }
            let bytes = std::fs::read(&path)
                .with_context(|| format!("reading the Dockerfile {}", path.display()))?;
            (bytes, Some(path))
        }
        None => {
            let path = DOCKERFILE_NAMES
                .iter()
                .map(|n| dir.join(n))
                .find(|p| p.is_file())
                .with_context(|| format!("{} has no Dockerfile", dir.display()))?;
            (std::fs::read(&path)?, Some(path))
        }
    };
    Ok(Source {
        dockerfile,
        dockerfile_path: path,
        context: Context::Dir(dir),
    })
}

/// The patterns of the ignore file: the one named after the Dockerfile first
/// (Dockerfile.dockerignore), then the context's .dockerignore.
fn excludes(dir: &Path, dockerfile: Option<&Path>) -> Result<Vec<String>> {
    let beside = dockerfile.map(|p| {
        let mut name = p.file_name().unwrap_or_default().to_os_string();
        name.push(".dockerignore");
        p.with_file_name(name)
    });
    for path in beside.into_iter().chain([dir.join(".dockerignore")]) {
        if let Ok(file) = File::open(&path) {
            return crate::build::ignore::read_all(file)
                .with_context(|| format!("reading {}", path.display()));
        }
    }
    Ok(Vec::new())
}

/// Packs the context into `out` as a tar, everything owned by root, modes and times
/// kept, what the ignore patterns exclude left out.
fn pack(dir: &Path, excludes: Vec<String>, out: File) -> Result<()> {
    use crate::build::walk::{walk, FilterOpt, Step};
    let mut builder = tar::Builder::new(BufWriter::new(out));
    builder.follow_symlinks(false);
    walk(
        dir,
        FilterOpt {
            exclude_patterns: excludes,
            ..FilterOpt::default()
        },
        &mut |st| {
            let kind = st.mode & nix::libc::S_IFMT;
            let mut header = tar::Header::new_gnu();
            header.set_mode(st.mode & 0o7777);
            header.set_uid(0);
            header.set_gid(0);
            header.set_mtime((st.mod_time / 1_000_000_000).max(0) as u64);
            match kind {
                nix::libc::S_IFDIR => {
                    header.set_entry_type(tar::EntryType::Directory);
                    header.set_size(0);
                    builder.append_data(&mut header, &st.path, std::io::empty())?;
                }
                nix::libc::S_IFLNK => {
                    header.set_entry_type(tar::EntryType::Symlink);
                    header.set_size(0);
                    builder.append_link(&mut header, &st.path, &st.linkname)?;
                }
                nix::libc::S_IFREG if !st.linkname.as_os_str().is_empty() => {
                    header.set_entry_type(tar::EntryType::Link);
                    header.set_size(0);
                    builder.append_link(&mut header, &st.path, &st.linkname)?;
                }
                nix::libc::S_IFREG => {
                    let file = File::open(dir.join(&st.path))
                        .with_context(|| format!("reading {}", st.path.display()))?;
                    header.set_entry_type(tar::EntryType::Regular);
                    header.set_size(file.metadata()?.len());
                    builder.append_data(&mut header, &st.path, file)?;
                }
                _ => {}
            }
            Ok(Step::Continue)
        },
    )?;
    builder.into_inner()?.flush()?;
    Ok(())
}

/// --secret id=ID[,src=PATH][,env=VAR][,type=file|env]: the secret's bytes, read here as
/// the user; without src or env, the variable named ID.
fn secret(spec: &str) -> Result<(String, Vec<u8>)> {
    let (mut id, mut src, mut env, mut kind) = (None, None, None, None);
    for field in spec.split(',') {
        match field.split_once('=') {
            Some(("id", v)) => id = Some(v.to_string()),
            Some(("src" | "source", v)) => src = Some(v.to_string()),
            Some(("env", v)) => env = Some(v.to_string()),
            Some(("type", v)) => kind = Some(v.to_string()),
            _ => bail!("--secret {spec}: id=ID[,src=PATH][,env=VAR]"),
        }
    }
    let id = id.with_context(|| format!("--secret {spec}: id= is missing"))?;
    let read_env = |var: &str| {
        std::env::var_os(var)
            .map(|v| v.into_encoded_bytes())
            .with_context(|| format!("--secret {spec}: {var} is not set"))
    };
    let bytes = match (kind.as_deref(), src, env) {
        (Some("env"), src, env) => read_env(env.or(src).as_deref().unwrap_or(&id))?,
        (_, Some(path), _) => {
            std::fs::read(&path).with_context(|| format!("--secret {spec}: reading {path}"))?
        }
        (_, None, Some(var)) => read_env(&var)?,
        (_, None, None) => read_env(&id)?,
    };
    Ok((id, bytes))
}

/// Runs a Dockerfile build through the service: `options` as BuildDockerfile takes
/// them, the context packed here as the user while the service unpacks it.
pub async fn send(
    client: &Client,
    options: client::Options<'_>,
    context: Context,
    ignore_from: Option<PathBuf>,
    quiet: bool,
) -> Result<client::Dict> {
    let (read, write) = nix::unistd::pipe2(OFlag::O_CLOEXEC).context("creating a pipe")?;
    let watch = client.watch_jobs_quiet(quiet).await?;
    let job = client
        .manager
        .build_dockerfile(zbus::zvariant::Fd::from(read.as_fd()), options)
        .await
        .map_err(client::error)?;
    drop(read);
    let packer = std::thread::spawn(move || -> Result<()> {
        crate::api::copy::block_sigpipe();
        let out = File::from(write);
        match context {
            Context::Dir(dir) => pack(&dir, excludes(&dir, ignore_from.as_deref())?, out),
            Context::Tar(bytes) => {
                let mut out = out;
                if bytes.starts_with(&[0x1f, 0x8b]) {
                    std::io::copy(
                        &mut flate2::read::GzDecoder::new(bytes.as_slice()),
                        &mut out,
                    )?;
                } else if bytes.is_empty() {
                    tar::Builder::new(&mut out).finish()?;
                } else {
                    out.write_all(&bytes)?;
                }
                Ok(())
            }
        }
    });
    let remote = watch.finish(job).await;
    let local = packer
        .join()
        .map_err(|_| anyhow!("packing the context stopped"))?;
    match (local, remote) {
        (_, Ok(done)) => Ok(done),
        (Err(local), Err(remote)) => {
            let text = format!("{local:#}");
            Err(if text.contains("Broken pipe") {
                remote
            } else {
                local
            })
        }
        (Ok(()), Err(e)) => Err(e),
    }
}

/// A Dockerfile build through the service, the context packed here as the user.
pub async fn dockerfile(args: BuildArgs, client: &Client, config: &Config) -> Result<()> {
    let d = &args.dockerfile;
    if let Some(platform) = &d.platform {
        let host = format!("linux/{}", crate::oci::host_architecture());
        if platform.split(',').any(|p| p != host) {
            bail!("--platform {platform}: builds run for this host's {host}");
        }
    }
    if let Some(progress) = d.progress.as_deref() {
        if !matches!(progress, "auto" | "plain" | "tty" | "quiet" | "rawjson") {
            bail!("--progress {progress}: auto, plain, tty or quiet");
        }
    }
    let quiet = d.quiet || d.progress.as_deref() == Some("quiet");
    let source = source(&args)?;
    let mut options = client::registry_options(config);
    options.insert("dockerfile", Value::from(source.dockerfile.clone()));
    let mut put_all = |key: &'static str, values: Vec<String>| {
        if !values.is_empty() {
            options.insert(key, Value::from(values));
        }
    };
    put_all("tags", args.tag.clone());
    put_all(
        "build_args",
        d.build_arg
            .iter()
            .filter_map(|a| match a.split_once('=') {
                Some(_) => Some(a.clone()),
                None => std::env::var(a).ok().map(|v| format!("{a}={v}")),
            })
            .collect(),
    );
    put_all("no_cache_filter", d.no_cache_filter.clone());
    put_all("labels", d.label.clone());
    put_all("add_hosts", d.add_host.clone());
    put_all("allow", d.allow.clone());
    put_all("ulimits", d.ulimit.clone());
    if let Some(target) = &d.target {
        options.insert("target", Value::from(target.clone()));
    }
    if let Some(network) = &d.network {
        options.insert("network", Value::from(network.clone()));
    }
    if let Some(size) = d.shm_size {
        options.insert("shm_size", Value::from(size));
    }
    for (key, value) in [
        ("no_cache", d.no_cache),
        ("pull", d.pull),
        ("force", args.force),
    ] {
        if value {
            options.insert(key, Value::from(true));
        }
    }
    if d.no_verify {
        options.insert("verify", Value::from(false));
    }
    if args.backend != crate::backend::BackendChoice::Auto {
        options.insert(
            "backend",
            Value::from(format!("{:?}", args.backend).to_lowercase()),
        );
    }
    if args.mode != crate::oci::ModeChoice::Auto {
        options.insert(
            "mode",
            Value::from(format!("{:?}", args.mode).to_lowercase()),
        );
    }
    let secrets: HashMap<String, Vec<u8>> =
        d.secret.iter().map(|s| secret(s)).collect::<Result<_>>()?;
    if !secrets.is_empty() {
        options.insert("secrets", Value::from(secrets));
    }
    if !d.cache_from.is_empty() {
        eprintln!("note: --cache-from is ignored: the build cache is this host's");
    }
    let done = send(
        client,
        options,
        source.context,
        source.dockerfile_path,
        quiet,
    )
    .await?;
    let id = client::string(&done, "id");
    if let Some(path) = &d.iidfile {
        std::fs::write(path, &id).with_context(|| format!("writing {}", path.display()))?;
    }
    let names = client::strings(&done, "names");
    if quiet {
        println!("{id}");
    } else {
        for name in &names {
            println!("image {name} is ready: nspawn run {name}, nspawn push {name}");
        }
        if names.is_empty() {
            println!("{id}");
        }
    }
    if d.push {
        for name in &names {
            client
                .run_job_to_stderr(|| {
                    client
                        .manager
                        .push_image(name, client::registry_options(config))
                })
                .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn args(argv: &[&str]) -> BuildArgs {
        let mut full = vec!["nspawn", "build"];
        full.extend(argv);
        match crate::cli::Cli::try_parse_from(full).unwrap().command {
            crate::cli::Command::Build(a) => a,
            _ => unreachable!(),
        }
    }

    #[test]
    fn the_context_leaves_out_what_the_ignore_file_says() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = dir.path().join("ctx");
        std::fs::create_dir_all(ctx.join("src/skip")).unwrap();
        std::fs::write(ctx.join("Dockerfile"), "FROM scratch\n").unwrap();
        std::fs::write(ctx.join("src/a.txt"), "a").unwrap();
        std::fs::write(ctx.join("src/b.log"), "b").unwrap();
        std::fs::write(ctx.join("src/skip/c"), "c").unwrap();
        std::fs::write(ctx.join(".dockerignore"), "**/*.log\nsrc/skip\n").unwrap();
        std::os::unix::fs::symlink("a.txt", ctx.join("src/link")).unwrap();
        let excluded = excludes(&ctx, Some(&ctx.join("Dockerfile"))).unwrap();
        assert_eq!(excluded, ["**/*.log", "src/skip"]);
        // Dockerfile.dockerignore comes first.
        std::fs::write(ctx.join("Dockerfile.dockerignore"), "src\n").unwrap();
        assert_eq!(
            excludes(&ctx, Some(&ctx.join("Dockerfile"))).unwrap(),
            ["src"]
        );
        let out = dir.path().join("ctx.tar");
        pack(&ctx, excluded, File::create(&out).unwrap()).unwrap();
        let mut names = Vec::new();
        let mut archive = tar::Archive::new(File::open(&out).unwrap());
        for entry in archive.entries().unwrap() {
            let entry = entry.unwrap();
            assert_eq!(
                (entry.header().uid().unwrap(), entry.header().gid().unwrap()),
                (0, 0)
            );
            names.push(entry.path().unwrap().to_string_lossy().into_owned());
        }
        names.sort();
        assert_eq!(
            names,
            [
                ".dockerignore",
                "Dockerfile",
                "Dockerfile.dockerignore",
                "src",
                "src/a.txt",
                "src/link"
            ]
        );
        assert!(is_tar(&std::fs::read(&out).unwrap()));
        assert!(!is_tar(b"FROM scratch\n"));
        assert_eq!(
            file_in_tar(&std::fs::read(&out).unwrap(), "Dockerfile")
                .unwrap()
                .as_deref(),
            Some(&b"FROM scratch\n"[..])
        );
    }

    #[test]
    fn secrets_are_read_here() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("s"), "x").unwrap();
        let spec = format!("id=tok,src={}", dir.path().join("s").display());
        assert_eq!(secret(&spec).unwrap(), ("tok".to_string(), b"x".to_vec()));
        assert!(secret("src=/x").is_err());
    }

    #[test]
    fn the_flags_and_the_context_pick_the_builder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap();
        assert!(kind(&args(&[path])).is_err(), "an empty context");
        std::fs::write(dir.path().join("mkosi.conf"), "").unwrap();
        assert_eq!(kind(&args(&[path, "-t", "x:1"])).unwrap(), Kind::Mkosi);
        assert_eq!(
            kind(&args(&[path, "--build-arg", "A=1"])).unwrap(),
            Kind::Dockerfile
        );
        std::fs::write(dir.path().join("Containerfile"), "FROM scratch\n").unwrap();
        assert_eq!(kind(&args(&[path])).unwrap(), Kind::Dockerfile);
        assert_eq!(kind(&args(&[path, "-d", "fedora"])).unwrap(), Kind::Mkosi);
        assert!(kind(&args(&[path, "-d", "fedora", "--target", "x"])).is_err());
        assert_eq!(kind(&args(&["-"])).unwrap(), Kind::Dockerfile);
        let refused = kind(&args(&[path, "--ssh", "default"])).unwrap_err();
        assert!(refused.to_string().starts_with("--ssh:"), "{refused}");
        // -f is the Dockerfile; --force keeps its long form only.
        let a = args(&[
            path,
            "-f",
            "Other.Dockerfile",
            "--force",
            "-t",
            "a:1",
            "-t",
            "b:2",
        ]);
        assert_eq!(a.dockerfile.file.as_deref(), Some("Other.Dockerfile"));
        assert!(a.force);
        assert_eq!(a.tag, ["a:1", "b:2"]);
    }
}
