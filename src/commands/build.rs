//! build: a Dockerfile build or an mkosi one, told apart by the flags and the context.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context as _, Result};
use nix::fcntl::OFlag;
use zbus::zvariant::Value;

use crate::build::untar::Compression;
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
    if args.directory == Path::new("-") {
        if mkosi {
            bail!("an mkosi build takes a directory, not standard input");
        }
        return Ok(Kind::Dockerfile);
    }
    if dockerfile {
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
    /// A tar as standard input gave it, compressed or not.
    Tar(Vec<u8>),
}

fn read_stdin() -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .read_to_end(&mut bytes)
        .context("reading standard input")?;
    Ok(bytes)
}

/// How bytes are compressed, by their magic as isArchive and DetectCompression tell it.
fn compression(bytes: &[u8]) -> Compression {
    crate::build::untar::detect(bytes)
}

/// The tar compressed `bytes` hold, undone as it is read: only the compressed bytes
/// stay in memory, xz's excepted, which its decoder gives whole.
fn tar_reader(bytes: &[u8]) -> Result<Box<dyn Read + '_>> {
    Ok(match compression(bytes) {
        Compression::None => Box::new(bytes),
        Compression::Gzip => Box::new(flate2::read::MultiGzDecoder::new(bytes)),
        Compression::Bzip2 => Box::new(bzip2::read::MultiBzDecoder::new(bytes)),
        Compression::Xz => {
            let mut out = Vec::new();
            lzma_rs::xz_decompress(&mut &bytes[..], &mut out)
                .map_err(|e| anyhow!("reading the context on standard input: {e}"))?;
            Box::new(std::io::Cursor::new(out))
        }
        Compression::Zstd => Box::new(
            zstd::stream::read::Decoder::new(bytes)
                .context("reading the context on standard input")?,
        ),
    })
}

/// Whether bytes are a tar: what `build -` reads as a context. As buildx's isArchive
/// with Go's archive/tar tells it: a first block that is a header, its checksum right,
/// with or without a magic (a V7 tar has none).
fn is_tar(bytes: &[u8]) -> bool {
    let Some(block) = bytes.get(..512) else {
        return false;
    };
    if block.iter().all(|&b| b == 0) {
        return false;
    }
    let field = &block[148..156];
    let trimmed = {
        let start = field.iter().position(|&b| b != b' ' && b != 0);
        let end = field.iter().rposition(|&b| b != b' ' && b != 0);
        match (start, end) {
            (Some(start), Some(end)) => &field[start..=end],
            _ => &field[..0],
        }
    };
    let digits = trimmed.split(|&b| b == 0).next().unwrap_or_default();
    let value = if digits.is_empty() {
        Some(0)
    } else {
        std::str::from_utf8(digits)
            .ok()
            .and_then(|d| u64::from_str_radix(d, 8).ok())
    };
    let Some(value) = value else {
        return false;
    };
    let byte = |(i, &b): (usize, &u8)| if (148..156).contains(&i) { b' ' } else { b };
    let unsigned: u64 = block.iter().enumerate().map(|e| u64::from(byte(e))).sum();
    let signed: i64 = block
        .iter()
        .enumerate()
        .map(|e| i64::from(byte(e) as i8))
        .sum();
    value == unsigned || i64::try_from(value).is_ok_and(|v| v == signed)
}

/// A path of the context as the reference looks it up there: cleaned, a `..` going no
/// higher than the root.
fn in_root(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    parts.join("/")
}

/// A file of a context tar, compressed or not.
fn file_in_tar(tar: &[u8], name: &str) -> Result<Option<Vec<u8>>> {
    let name = in_root(name);
    let mut archive = tar::Archive::new(tar_reader(tar)?);
    for entry in archive.entries().context("reading the context tar")? {
        let mut entry = entry.context("reading the context tar")?;
        let path = in_root(&entry.path()?.to_string_lossy());
        if path == name {
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes)?;
            return Ok(Some(bytes));
        }
    }
    Ok(None)
}

/// What standard input holds, as buildx's loadInputs tells it: a context tar with the
/// Dockerfile in it, compressed (a stream that does not undo is an error then) or not,
/// or a Dockerfile with no context.
fn stdin_source(input: Vec<u8>, file: Option<&str>) -> Result<Source> {
    if compression(&input) != Compression::None || is_tar(&input) {
        let name = file.unwrap_or("Dockerfile");
        let dockerfile = file_in_tar(&input, name)?
            .with_context(|| format!("the context on standard input has no {name}"))?;
        return Ok(Source {
            dockerfile,
            dockerfile_path: None,
            context: Context::Tar(input),
        });
    }
    if file.is_some() {
        bail!("ambiguous Dockerfile source: both stdin and flag correspond to Dockerfiles");
    }
    Ok(Source {
        dockerfile: input,
        dockerfile_path: None,
        context: Context::Tar(Vec::new()),
    })
}

fn source(args: &BuildArgs) -> Result<Source> {
    let file = args.dockerfile.file.as_deref();
    if args.directory == Path::new("-") {
        // As buildx's loadInputs: standard input is the context or the Dockerfile, and a
        // Dockerfile there leaves -f nothing to name.
        if file == Some("-") {
            bail!("invalid argument: can't use stdin for both build context and dockerfile");
        }
        return stdin_source(read_stdin()?, file);
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
        // One that is not there, or no file, is none; one that cannot be read fails the
        // build.
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        if file.metadata().is_ok_and(|m| m.is_dir()) {
            continue;
        }
        return crate::build::ignore::read_all(file)
            .with_context(|| format!("reading {}", path.display()));
    }
    Ok(Vec::new())
}

/// A file's bytes, zeros once a read fails, the error kept: the entry keeps the size its
/// header gives, so what marks a failed packing lands where the service reads a header.
struct Reading<'a, R> {
    file: R,
    failed: &'a mut Option<std::io::Error>,
}

impl<R: Read> Read for Reading<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.failed.is_none() {
            match self.file.read(buf) {
                Err(e) if e.kind() != std::io::ErrorKind::Interrupted => *self.failed = Some(e),
                read => return read,
            }
        }
        buf.fill(0);
        Ok(buf.len())
    }
}

/// Packs the context into `out` as a tar, everything owned by root, modes, times and
/// file capabilities kept, what the ignore patterns exclude left out.
fn pack(dir: &Path, excludes: Vec<String>, out: impl Write) -> Result<()> {
    use crate::build::walk::{walk, FilterOpt, Step};
    let mut builder = tar::Builder::new(BufWriter::new(out));
    builder.follow_symlinks(false);
    let walked = walk(
        dir,
        FilterOpt {
            exclude_patterns: excludes,
            ..FilterOpt::default()
        },
        &mut |st| {
            let kind = st.mode & nix::libc::S_IFMT;
            if matches!(
                kind,
                nix::libc::S_IFDIR | nix::libc::S_IFLNK | nix::libc::S_IFREG
            ) {
                append_records(&mut builder, st)?;
            }
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
                    // What cannot be opened (a socket, a file the user may not read) goes
                    // as an empty file, as the reference sends it.
                    header.set_entry_type(tar::EntryType::Regular);
                    match File::open(dir.join(&st.path)) {
                        Ok(file) => {
                            let size = file.metadata()?.len();
                            header.set_size(size);
                            append_file(&mut builder, &mut header, &st.path, file, size)?;
                        }
                        Err(_) => {
                            header.set_size(0);
                            builder.append_data(&mut header, &st.path, std::io::empty())?;
                        }
                    }
                }
                _ => {}
            }
            Ok(Step::Continue)
        },
    );
    if let Err(e) = walked {
        broken(&mut builder);
        return Err(e);
    }
    builder.into_inner()?.flush()?;
    Ok(())
}

/// The PAX records an entry needs ahead of its header: its time to the nanosecond when
/// it has a fraction of a second, and the file capabilities of a regular file, the one
/// extended attribute a layer keeps.
fn append_records<W: Write>(
    builder: &mut tar::Builder<W>,
    st: &crate::build::walk::Stat,
) -> Result<()> {
    use crate::build::layer::format_pax_record;
    let mut body = Vec::new();
    let (secs, nanos) = (
        st.mod_time.div_euclid(1_000_000_000),
        st.mod_time.rem_euclid(1_000_000_000),
    );
    if nanos != 0 && secs >= 0 {
        let time = format!("{secs}.{nanos:09}");
        body.extend(format_pax_record(
            b"mtime",
            time.trim_end_matches('0').as_bytes(),
        ));
    }
    let capability = st
        .xattrs
        .get(std::ffi::OsStr::new("security.capability"))
        .filter(|c| !c.is_empty());
    if let Some(capability) = capability.filter(|_| st.linkname.as_os_str().is_empty()) {
        if st.mode & nix::libc::S_IFMT == nix::libc::S_IFREG {
            body.extend(format_pax_record(
                b"SCHILY.xattr.security.capability",
                capability,
            ));
        }
    }
    if body.is_empty() {
        return Ok(());
    }
    let mut header = tar::Header::new_ustar();
    header.set_entry_type(tar::EntryType::XHeader);
    header.set_mode(0o644);
    header.set_size(body.len() as u64);
    builder.append_data(&mut header, "PaxHeader", &body[..])?;
    Ok(())
}

/// Appends a regular file of `size` bytes, the size it had when looked at: one that grows
/// or shrinks meanwhile is cut or padded to it, or the stream would go out of step. A
/// read that fails fails the packing after the entry is written whole, zeros in place
/// of what could not be read.
fn append_file<W: Write>(
    builder: &mut tar::Builder<W>,
    header: &mut tar::Header,
    path: &Path,
    file: impl Read,
    size: u64,
) -> Result<()> {
    let mut failed = None;
    let data = Reading {
        file: file.take(size),
        failed: &mut failed,
    }
    .chain(std::io::repeat(0))
    .take(size);
    builder.append_data(header, path, data)?;
    match failed {
        Some(e) => Err(anyhow::Error::new(e).context(format!("reading {}", path.display()))),
        None => Ok(()),
    }
}

/// Marks a packing that failed: the builder ends the archive when dropped, and a clean
/// end would pass for the whole context, so a block that is no header makes the service
/// fail instead.
fn broken<W: Write>(builder: &mut tar::Builder<W>) {
    let _ = builder.get_mut().write_all(&[0xff; 512]);
}

/// --secret id=ID[,src=PATH][,env=VAR][,type=file|env]: the secret's bytes, read here as
/// the user: an env wins over a file, and without either the variable named ID when it
/// is set, else the file named ID.
fn secret(spec: &str) -> Result<(String, Vec<u8>)> {
    secret_with(spec, &|var| std::env::var_os(var))
}

/// The largest secret --secret takes.
const MAX_SECRET: u64 = 500 * 1024;

fn secret_with(
    spec: &str,
    var: &dyn Fn(&str) -> Option<std::ffi::OsString>,
) -> Result<(String, Vec<u8>)> {
    let (mut id, mut src, mut env, mut kind) = (String::new(), String::new(), String::new(), None);
    for field in crate::mountopt::csv_fields(spec)? {
        let Some((key, value)) = field.split_once('=') else {
            bail!("invalid field '{field}' must be a key=value pair");
        };
        match key.to_lowercase().as_str() {
            "type" => {
                if value != "file" && value != "env" {
                    bail!("unsupported secret type {value:?}");
                }
                kind = Some(value.to_string());
            }
            "id" => id = value.to_string(),
            "source" | "src" => src = value.to_string(),
            "env" => env = value.to_string(),
            key => bail!("unexpected key '{key}' in '{field}'"),
        }
    }
    if kind.as_deref() == Some("env") && env.is_empty() {
        env = std::mem::take(&mut src);
    }
    if id.is_empty() {
        bail!("secret missing ID");
    }
    if env.is_empty() && src.is_empty() {
        if var(&id).is_some() {
            env.clone_from(&id);
        } else {
            src.clone_from(&id);
        }
    }
    // The file is looked at even when the variable is the one taken.
    if !src.is_empty() {
        let size = std::fs::metadata(&src)
            .with_context(|| format!("failed to stat {src}"))?
            .len();
        if size > MAX_SECRET {
            bail!("secret {id} too big. max size 500KiB");
        }
    }
    if !env.is_empty() {
        // An unset one is empty, as Go's os.Getenv gives it.
        return Ok((
            id,
            var(&env)
                .map(|v| v.into_encoded_bytes())
                .unwrap_or_default(),
        ));
    }
    let bytes = std::fs::read(&src).with_context(|| format!("reading {src}"))?;
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
    // An ignore file that cannot be read fails the build before the service has the
    // context.
    let excludes = match &context {
        Context::Dir(dir) => excludes(dir, ignore_from.as_deref())?,
        Context::Tar(_) => Vec::new(),
    };
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
            Context::Dir(dir) => pack(&dir, excludes, out),
            Context::Tar(bytes) => {
                let mut out = out;
                if bytes.is_empty() {
                    tar::Builder::new(&mut out).finish()?;
                } else {
                    std::io::copy(&mut tar_reader(&bytes)?, &mut out)
                        .context("reading the context on standard input")?;
                }
                Ok(())
            }
        }
    });
    let remote = watch.finish(job).await;
    let local = packer
        .join()
        .map_err(|_| anyhow!("packing the context stopped"))?;
    // The service stopping its reads ends the packing with a broken pipe: its own
    // outcome is the one to tell then.
    let broken_pipe = |e: &anyhow::Error| {
        e.chain().any(|cause| {
            cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::BrokenPipe)
        })
    };
    match local {
        Ok(()) => remote,
        Err(local) if broken_pipe(&local) => remote,
        Err(local) => Err(local),
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
    for warning in d.warnings() {
        eprintln!("warning: {warning}");
    }
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
    // As ValidateLabel reads them: a KEY alone is KEY with an empty value.
    put_all(
        "labels",
        d.label
            .iter()
            .map(|l| {
                if l.contains('=') {
                    l.clone()
                } else {
                    format!("{l}=")
                }
            })
            .collect(),
    );
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
    // The ID file of an earlier build goes first: a build that fails leaves none.
    if let Some(path) = &d.iidfile {
        match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(e).with_context(|| format!("removing {}", path.display()));
            }
            _ => {}
        }
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

    #[test]
    fn secrets_are_read_as_buildkit_reads_them() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("npmrc");
        std::fs::write(&file, "from-file").unwrap();
        let vars = |var: &str| match var {
            "TOKEN" => Some(std::ffi::OsString::from("from-env")),
            _ => None,
        };
        let read = |spec: &str| secret_with(spec, &vars);
        let src = file.display().to_string();
        // A file named as the ID when no variable is.
        assert_eq!(read(&format!("id={src}")).unwrap().1, b"from-file");
        // The variable named as the ID when it is set.
        assert_eq!(read("id=TOKEN").unwrap().1, b"from-env");
        // env wins over src.
        assert_eq!(
            read(&format!("id=t,src={src},env=TOKEN")).unwrap().1,
            b"from-env"
        );
        assert_eq!(read("ID=t,TYPE=env,SRC=TOKEN").unwrap().1, b"from-env");
        // An unset variable is empty.
        assert!(read("id=t,env=NOPE").unwrap().1.is_empty());
        assert!(read("id=t,type=other")
            .unwrap_err()
            .to_string()
            .contains("unsupported"));
        assert!(read("src=x")
            .unwrap_err()
            .to_string()
            .contains("missing ID"));
        assert!(read("id=t,bogus=1")
            .unwrap_err()
            .to_string()
            .contains("unexpected key"));
        // The file is looked at even when the variable wins.
        let missing = dir.path().join("missing").display().to_string();
        assert!(read(&format!("id=t,src={missing},env=TOKEN"))
            .unwrap_err()
            .to_string()
            .contains("failed to stat"));
        let big = dir.path().join("big");
        std::fs::write(&big, vec![b'x'; MAX_SECRET as usize + 1]).unwrap();
        assert_eq!(
            read(&format!("id=t,src={},env=TOKEN", big.display()))
                .unwrap_err()
                .to_string(),
            "secret t too big. max size 500KiB"
        );
    }

    #[test]
    fn a_file_that_fails_to_read_breaks_the_context() {
        struct Failing(usize);
        impl Read for Failing {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.0 == 0 {
                    return Err(std::io::Error::from_raw_os_error(nix::libc::EIO));
                }
                let n = buf.len().min(self.0);
                buf[..n].fill(b'a');
                self.0 -= n;
                Ok(n)
            }
        }
        // Sizes where the mark would have been read as the file's own bytes.
        for (size, good) in [(1000, 0), (1536, 0), (300, 0), (9000, 8192), (2000, 0)] {
            let mut out = Vec::new();
            {
                let mut builder = tar::Builder::new(&mut out);
                let mut header = tar::Header::new_gnu();
                header.set_mode(0o644);
                header.set_size(size);
                let read = Failing(good);
                let packed = append_file(&mut builder, &mut header, Path::new("f"), read, size);
                assert!(format!("{:#}", packed.unwrap_err()).contains("reading f"));
                broken(&mut builder);
            }
            let mut archive = tar::Archive::new(out.as_slice());
            let failed = archive.entries().unwrap().any(|e| match e {
                Err(_) => true,
                Ok(mut entry) => std::io::copy(&mut entry, &mut std::io::sink()).is_err(),
            });
            assert!(
                failed,
                "{size} bytes, {good} read: the context passed for whole"
            );
        }
    }

    #[test]
    fn standard_input_is_the_context_or_the_dockerfile() {
        let mut tar = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar);
            // A V7 header, with no magic, is a tar too.
            let mut header = tar::Header::new_old();
            header.set_path("Dockerfile.prod").unwrap();
            header.set_size(13);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append(&header, &b"FROM scratch\n"[..]).unwrap();
            builder.finish().unwrap();
        }
        assert!(is_tar(&tar));
        assert!(!is_tar(&[0; 1024]));
        let mut wrong = tar.clone();
        wrong[0] ^= 1;
        assert!(!is_tar(&wrong), "its checksum is no longer right");
        for name in [
            "Dockerfile.prod",
            "./Dockerfile.prod",
            "sub/../Dockerfile.prod",
            "/Dockerfile.prod",
        ] {
            let source = stdin_source(tar.clone(), Some(name)).unwrap();
            assert_eq!(source.dockerfile, b"FROM scratch\n", "{name}");
        }
        assert!(
            format!("{:#}", stdin_source(tar.clone(), None).err().unwrap())
                .contains("has no Dockerfile")
        );
        let dockerfile = b"FROM scratch\n".to_vec();
        let source = stdin_source(dockerfile.clone(), None).unwrap();
        assert_eq!(source.dockerfile, dockerfile);
        assert!(matches!(source.context, Context::Tar(ref t) if t.is_empty()));
        assert_eq!(
            stdin_source(dockerfile, Some("Dockerfile.prod"))
                .err()
                .unwrap()
                .to_string(),
            "ambiguous Dockerfile source: both stdin and flag correspond to Dockerfiles"
        );
    }
    use clap::Parser;
    use std::os::unix::fs::PermissionsExt;

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
        // A directory is no ignore file; one that cannot be read fails.
        let other = dir.path().join("other");
        std::fs::create_dir_all(other.join(".dockerignore")).unwrap();
        assert!(excludes(&other, None).unwrap().is_empty());
        if !nix::unistd::geteuid().is_root() {
            let closed = dir.path().join("closed");
            std::fs::create_dir(&closed).unwrap();
            std::fs::write(closed.join(".dockerignore"), "secret\n").unwrap();
            std::fs::set_permissions(
                closed.join(".dockerignore"),
                std::fs::Permissions::from_mode(0o000),
            )
            .unwrap();
            assert!(excludes(&closed, None).is_err());
        }
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
        let tar = std::fs::read(&out).unwrap();
        assert!(is_tar(&tar));
        assert!(!is_tar(b"FROM scratch\n"));
        // A gzip, bzip2, xz or zstd context is undone, a Dockerfile left as it is.
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut gz, &tar).unwrap();
        let mut bz = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::fast());
        std::io::Write::write_all(&mut bz, &tar).unwrap();
        let mut xz = Vec::new();
        lzma_rs::xz_compress(&mut tar.as_slice(), &mut xz).unwrap();
        let zst = zstd::encode_all(tar.as_slice(), 1).unwrap();
        for packed in [gz.finish().unwrap(), bz.finish().unwrap(), xz, zst] {
            assert_ne!(compression(&packed), Compression::None);
            let mut undone = Vec::new();
            tar_reader(&packed)
                .unwrap()
                .read_to_end(&mut undone)
                .unwrap();
            assert_eq!(undone, tar);
            assert_eq!(
                file_in_tar(&packed, "Dockerfile").unwrap().as_deref(),
                Some(&b"FROM scratch\n"[..])
            );
            // Cut short, a compressed context is still one, and broken.
            let cut = packed[..packed.len() / 2].to_vec();
            let e = stdin_source(cut, Some("Dockerfile.prod")).err().unwrap();
            assert!(!format!("{e:#}").contains("ambiguous"), "{e:#}");
        }
        assert_eq!(compression(b"FROM scratch\n"), Compression::None);
        assert_eq!(
            file_in_tar(&std::fs::read(&out).unwrap(), "Dockerfile")
                .unwrap()
                .as_deref(),
            Some(&b"FROM scratch\n"[..])
        );
    }

    #[test]
    fn a_context_keeps_its_times_when_unpacked() {
        use std::os::unix::fs::MetadataExt;
        use std::time::{Duration, UNIX_EPOCH};
        let dir = tempfile::tempdir().unwrap();
        let ctx = dir.path().join("ctx");
        std::fs::create_dir_all(ctx.join("d")).unwrap();
        let at = |path: &Path, nanos: u64| {
            let times = std::fs::FileTimes::new()
                .set_accessed(UNIX_EPOCH + Duration::from_nanos(nanos))
                .set_modified(UNIX_EPOCH + Duration::from_nanos(nanos));
            File::open(path).unwrap().set_times(times).unwrap();
        };
        std::fs::write(ctx.join("d/zero"), "0").unwrap();
        std::fs::write(ctx.join("d/nanos"), "n").unwrap();
        at(&ctx.join("d/zero"), 0);
        at(&ctx.join("d/nanos"), 5_250_000_000);
        at(&ctx.join("d"), 1_000_000_000_000);
        let mut out = Vec::new();
        pack(&ctx, Vec::new(), &mut out).unwrap();
        let into = dir.path().join("into");
        std::fs::create_dir(&into).unwrap();
        let me = crate::build::copy::User {
            uid: nix::unistd::getuid().as_raw(),
            gid: nix::unistd::getgid().as_raw(),
        };
        let mut archive = tar::Archive::new(out.as_slice());
        crate::build::untar::unpack(archive.entries().unwrap(), &into, Some(me)).unwrap();
        let meta = |p: &str| std::fs::symlink_metadata(into.join(p)).unwrap();
        assert_eq!(meta("d").mtime(), 1000);
        assert_eq!(meta("d/zero").mtime(), 0);
        assert_eq!(
            (meta("d/nanos").mtime(), meta("d/nanos").mtime_nsec()),
            (5, 250_000_000)
        );
    }

    #[test]
    fn a_context_is_whole_or_broken() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = dir.path().join("ctx");
        std::fs::create_dir_all(ctx.join("tmp")).unwrap();
        let _socket = std::os::unix::net::UnixListener::bind(ctx.join("tmp/app.sock")).unwrap();
        std::fs::write(ctx.join("z"), "after").unwrap();
        let mut out = Vec::new();
        pack(&ctx, Vec::new(), &mut out).unwrap();
        let mut seen = Vec::new();
        for entry in tar::Archive::new(out.as_slice()).entries().unwrap() {
            let entry = entry.unwrap();
            seen.push((
                entry.path().unwrap().to_string_lossy().into_owned(),
                entry.header().size().unwrap(),
            ));
        }
        // The socket goes as an empty file and what comes after it still goes.
        assert!(seen.contains(&("tmp/app.sock".to_string(), 0)));
        assert!(seen.contains(&("z".to_string(), 5)));
        // A walk that fails leaves a stream a reader refuses, not a shorter context.
        if nix::unistd::geteuid().is_root() {
            return;
        }
        let closed = ctx.join("closed");
        std::fs::create_dir(&closed).unwrap();
        std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o000)).unwrap();
        let mut out = Vec::new();
        assert!(pack(&ctx, Vec::new(), &mut out).is_err());
        std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut archive = tar::Archive::new(out.as_slice());
        assert!(archive.entries().unwrap().any(|e| e.is_err()));
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
        assert!(kind(&args(&["-", "-d", "fedora"])).is_err());
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
