//! save, load, commit, export and import: images and machines' files as tar archives.
//! The service writes or reads the archive on a pipe; this side moves it between the
//! pipe and standard output, standard input or a file, which belongs to whoever runs
//! the command.

use std::fs::{self, File};
use std::io::{self, IsTerminal, Read, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context as _, Result};
use nix::fcntl::OFlag;
use zbus::zvariant::Value;

use crate::api::copy::block_sigpipe;
use crate::cli::{CommitArgs, ExportArgs, ImportArgs, LoadArgs, SaveArgs};
use crate::client::{self, Client, Options};
use crate::config::Config;

/// The file `-o` names, filled under no name (or a temporary one where the file system
/// has no unnamed files) and put in place only once the archive is whole and the job is
/// done: a failure or an interruption leaves what was there as it was.
struct Destination {
    path: PathBuf,
    dir: PathBuf,
    file: File,
    /// The temporary name, where the file has one.
    named: Option<PathBuf>,
}

impl Destination {
    /// Refuses anything but a regular file or a new name in an existing directory: a
    /// device, a link, a fifo or a directory is never replaced.
    fn new(path: &Path) -> Result<Destination> {
        match fs::symlink_metadata(path) {
            Ok(meta) if !meta.file_type().is_file() => bail!(
                "{}: not a regular file; -o writes a new file or replaces one",
                path.display()
            ),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("looking at {}", path.display())),
        }
        let dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        if !dir.is_dir() {
            bail!("{}: no such directory", dir.display());
        }
        if path.file_name().is_none() {
            bail!("{}: not a file name", path.display());
        }
        let unnamed = fs::OpenOptions::new()
            .write(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_TMPFILE)
            .open(&dir);
        let (file, named) = match unnamed {
            Ok(file) => (file, None),
            Err(_) => {
                let part = Self::part_name(path, &dir);
                let file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&part)
                    .with_context(|| format!("creating {}", part.display()))?;
                (file, Some(part))
            }
        };
        Ok(Destination {
            path: path.to_path_buf(),
            dir,
            file,
            named,
        })
    }

    fn part_name(path: &Path, dir: &Path) -> PathBuf {
        let leaf = path.file_name().unwrap_or_default().to_string_lossy();
        dir.join(format!(".{leaf}.part-{}", std::process::id()))
    }

    /// Puts the whole archive in place of the destination.
    fn keep(mut self) -> Result<()> {
        self.file
            .sync_all()
            .with_context(|| format!("writing {}", self.path.display()))?;
        let part = match self.named.take() {
            Some(part) => part,
            None => {
                let part = Self::part_name(&self.path, &self.dir);
                let fd = format!("/proc/self/fd/{}", self.file.as_raw_fd());
                nix::unistd::linkat(
                    nix::fcntl::AT_FDCWD,
                    fd.as_str(),
                    nix::fcntl::AT_FDCWD,
                    &part,
                    nix::fcntl::AtFlags::AT_SYMLINK_FOLLOW,
                )
                .with_context(|| format!("creating {}", part.display()))?;
                part
            }
        };
        fs::rename(&part, &self.path).map_err(|e| {
            let _ = fs::remove_file(&part);
            anyhow::Error::new(e).context(format!("writing {}", self.path.display()))
        })
    }
}

impl Drop for Destination {
    fn drop(&mut self) {
        if let Some(part) = &self.named {
            let _ = fs::remove_file(part);
        }
    }
}

/// Where an archive goes: a file (see `Destination`), or standard output, which must not
/// be a terminal. Looked at before anything is asked of the service.
fn destination(output: Option<&Path>) -> Result<Option<Destination>> {
    match output {
        Some(path) => Destination::new(path).map(Some),
        None if io::stdout().is_terminal() => {
            bail!("refusing to write an archive to a terminal; use -o FILE or redirect the output")
        }
        None => Ok(None),
    }
}

/// Copies `stream` to standard output or into `out`, which comes back to be kept.
fn write_out(stream: OwnedFd, out: Option<Destination>) -> Result<Option<Destination>> {
    let mut from = File::from(stream);
    match out {
        None => {
            let stdout = io::stdout();
            let mut lock = stdout.lock();
            io::copy(&mut from, &mut lock).context("writing the archive")?;
            lock.flush().context("writing the archive")?;
            Ok(None)
        }
        Some(mut dest) => {
            io::copy(&mut from, &mut dest.file)
                .with_context(|| format!("writing {}", dest.path.display()))?;
            Ok(Some(dest))
        }
    }
}

/// Waits for both ends of a stream out of the service, the job and the copy of what it
/// wrote, and keeps the file only when both are done.
async fn stream_out(
    started: impl std::future::Future<
        Output = zbus::Result<(zbus::zvariant::OwnedFd, zbus::zvariant::OwnedObjectPath)>,
    >,
    watch: client::JobWatch,
    out: Option<Destination>,
) -> Result<client::Dict> {
    let (stream, job) = started.await.map_err(client::error)?;
    let stream = OwnedFd::from(stream);
    let local = tokio::task::spawn_blocking(move || write_out(stream, out));
    let remote = watch.finish(job).await;
    let local = local.await.context("the copy stopped")?;
    match (local, remote) {
        (Ok(dest), Ok(done)) => {
            if let Some(dest) = dest {
                dest.keep()?;
            }
            Ok(done)
        }
        (Err(local), Err(remote)) => Err(crate::commands::copy::first_cause(local, remote)),
        (Err(e), Ok(_)) | (Ok(_), Err(e)) => Err(e),
    }
}

pub async fn save(args: SaveArgs, client: &Client, config: &Config) -> Result<()> {
    let out = destination(args.output.as_deref())?;
    let watch = client.watch_jobs_quiet(false).await?;
    let started = client
        .manager
        .save_images(&args.images, client::registry_options(config));
    stream_out(started, watch, out).await?;
    Ok(())
}

pub async fn export(args: ExportArgs, client: &Client) -> Result<()> {
    let out = destination(args.output.as_deref())?;
    let watch = client.watch_jobs_quiet(false).await?;
    let started = client.manager.export_machine(&args.machine, Options::new());
    stream_out(started, watch, out).await?;
    Ok(())
}

/// What an archive is read from: a file, or standard input, which must not be a
/// terminal.
fn open_input(path: Option<&Path>, flag: &str) -> Result<File> {
    match path {
        Some(path) => File::open(path).with_context(|| format!("opening {}", path.display())),
        None => {
            if io::stdin().is_terminal() {
                bail!("standard input is a terminal; use {flag} or redirect a file into it");
            }
            let fd = io::stdin()
                .as_fd()
                .try_clone_to_owned()
                .context("reading standard input")?;
            Ok(File::from(fd))
        }
    }
}

/// Sends `input` on `write`, the pipe whose other end `job` reads, and follows the job
/// to its end.
async fn send(
    watch: client::JobWatch,
    job: zbus::zvariant::OwnedObjectPath,
    input: File,
    write: OwnedFd,
) -> Result<client::Dict> {
    let sender = std::thread::spawn(move || -> Result<()> {
        block_sigpipe();
        let mut input = input;
        let mut out = File::from(write);
        let mut buf = vec![0u8; 1 << 16];
        loop {
            let n = match input.read(&mut buf) {
                Ok(0) => return Ok(()),
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e).context("reading the archive"),
            };
            out.write_all(&buf[..n]).context("sending the archive")?;
        }
    });
    let remote = watch.finish(job).await;
    let local = sender.join().map_err(|_| anyhow!("the copy stopped"))?;
    let broken = |e: &anyhow::Error| {
        e.chain().any(|c| {
            c.downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe)
        })
    };
    match (local, remote) {
        (Ok(()), Ok(done)) => Ok(done),
        // The service stopped reading when it had what it needed.
        (Err(local), Ok(done)) if broken(&local) => Ok(done),
        // What the service read was cut short here, whatever it made of it.
        (Err(local), Ok(_)) => Err(local),
        (Ok(()), Err(remote)) => Err(remote),
        (Err(local), Err(remote)) => Err(crate::commands::copy::first_cause(local, remote)),
    }
}

pub async fn load(args: LoadArgs, client: &Client, config: &Config) -> Result<()> {
    let input = open_input(args.input.as_deref(), "-i FILE")?;
    let (read, write) = nix::unistd::pipe2(OFlag::O_CLOEXEC).context("creating a pipe")?;
    let watch = if args.quiet {
        client.watch_jobs_quiet(true).await?
    } else {
        client.watch_jobs().await?
    };
    let job = client
        .manager
        .load_images(
            zbus::zvariant::Fd::from(read.as_fd()),
            client::registry_options(config),
        )
        .await
        .map_err(client::error)?;
    drop(read);
    let done = send(watch, job, input, write).await?;
    if args.quiet {
        for reference in client::strings(&done, "references") {
            println!("Loaded image: {reference}");
        }
    }
    Ok(())
}

pub async fn import(args: ImportArgs, client: &Client, config: &Config) -> Result<()> {
    if args.source.starts_with("http://") || args.source.starts_with("https://") {
        bail!(
            "import reads a file or standard input; download {} first",
            args.source
        );
    }
    let path = (args.source != "-").then(|| PathBuf::from(&args.source));
    let input = open_input(path.as_deref(), "a FILE")?;
    let mut options = client::registry_options(config);
    if let Some(message) = args.message {
        options.insert("message", Value::from(message));
    }
    if !args.change.is_empty() {
        options.insert("changes", Value::from(args.change));
    }
    let (read, write) = nix::unistd::pipe2(OFlag::O_CLOEXEC).context("creating a pipe")?;
    let watch = client.watch_jobs_quiet(false).await?;
    let job = client
        .manager
        .import_image(
            zbus::zvariant::Fd::from(read.as_fd()),
            &args.reference,
            options,
        )
        .await
        .map_err(client::error)?;
    drop(read);
    let done = send(watch, job, input, write).await?;
    println!("{}", client::string(&done, "id"));
    Ok(())
}

pub async fn commit(args: CommitArgs, client: &Client, config: &Config) -> Result<()> {
    let mut options = client::registry_options(config);
    if let Some(author) = args.author {
        options.insert("author", Value::from(author));
    }
    if let Some(message) = args.message {
        options.insert("message", Value::from(message));
    }
    if !args.change.is_empty() {
        options.insert("changes", Value::from(args.change));
    }
    options.insert("pause", Value::from(args.pause));
    let done = client
        .run_job_to_stderr(|| {
            client
                .manager
                .commit_machine(&args.machine, &args.reference, options)
        })
        .await?;
    println!("{}", client::string(&done, "id"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_archive_replaces_only_a_regular_file_and_only_once_whole() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.tar");
        fs::write(&file, b"old").unwrap();
        // Given up: what was there stays, and nothing is left beside it.
        let mut dest = Destination::new(&file).unwrap();
        dest.file.write_all(b"half").unwrap();
        drop(dest);
        assert_eq!(fs::read(&file).unwrap(), b"old");
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 1);
        // Kept: the new archive in its place.
        let mut dest = Destination::new(&file).unwrap();
        dest.file.write_all(b"new archive").unwrap();
        dest.keep().unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"new archive");
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 1);
        // A link, a directory and a path in no directory are refused.
        std::os::unix::fs::symlink(&file, tmp.path().join("link")).unwrap();
        assert!(Destination::new(&tmp.path().join("link")).is_err());
        assert!(Destination::new(tmp.path()).is_err());
        assert!(Destination::new(&tmp.path().join("nodir/x.tar")).is_err());
    }
}
