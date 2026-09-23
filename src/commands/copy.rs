//! cp on the command line's side: the host's end of the stream is packed or unpacked
//! here, as the user who runs it, and the machine's end by the service.

use std::ffi::OsString;
use std::fs::{self, File};
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context as _, Result};
use nix::fcntl::OFlag;
use nix::sys::stat::Mode;
use zbus::zvariant::Value;

use crate::api::copy::{
    block_sigpipe, pack, plan, split_source, unpack, Endpoint, Ids, Kind, Stats, Target,
};
use crate::cli::CpArgs;
use crate::client::{self, Client, Options};

pub async fn cp(args: CpArgs, client: &Client) -> Result<()> {
    match (
        Endpoint::parse(&args.source)?,
        Endpoint::parse(&args.destination)?,
    ) {
        (Endpoint::Local(_), Endpoint::Local(_)) => {
            bail!("one side must be MACHINE:PATH; two local paths are cp(1)'s job")
        }
        (Endpoint::Machine { .. }, Endpoint::Machine { .. }) => {
            bail!("copying between machines is not supported; copy to the host first")
        }
        (Endpoint::Machine { name, path }, Endpoint::Local(_)) => {
            copy_out(client, &name, &path, &args.destination).await
        }
        (Endpoint::Local(_), Endpoint::Machine { name, path }) => {
            copy_in(client, &args.source, &name, &path).await
        }
    }
}

fn open_dir(path: &Path) -> Result<OwnedFd> {
    nix::fcntl::open(
        path,
        OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| anyhow!("{}: {}", path.display(), e.desc()))
}

fn local_kind(path: &Path) -> Result<Option<Kind>> {
    match fs::metadata(path) {
        Ok(meta) if meta.is_dir() => Ok(Some(Kind::Directory)),
        Ok(_) => Ok(Some(Kind::Other)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Which failure to show when both ends failed: the one that is not merely the other
/// end having gone away.
fn first_cause(local: anyhow::Error, remote: anyhow::Error) -> anyhow::Error {
    let broken = |e: &anyhow::Error| {
        let text = format!("{e:#}");
        text.contains("Broken pipe") || text.contains("nothing arrived") || text.contains("EOF")
    };
    if broken(&local) && !broken(&remote) {
        remote
    } else {
        local
    }
}

fn skipped(stats: &Stats) {
    for path in &stats.skipped {
        eprintln!("note: {path} is not a file, directory or link; not copied");
    }
}

async fn copy_out(client: &Client, name: &str, path: &str, destination: &str) -> Result<()> {
    let (_, contents) = split_source(path);
    let trailing_slash = destination.len() > 1 && destination.ends_with('/');
    let dst = PathBuf::from(if trailing_slash {
        destination.trim_end_matches('/')
    } else {
        destination
    });
    let dst_kind = local_kind(&dst)?;
    if dst_kind.is_none() {
        if let Some(parent) = dst.parent().filter(|p| !p.as_os_str().is_empty()) {
            if local_kind(parent)? != Some(Kind::Directory) {
                bail!("{}: no such directory", parent.display());
            }
        }
    }
    let watch = client.watch_jobs().await?;
    let (stream, job) = client
        .manager
        .copy_from(name, path, Options::new())
        .await
        .map_err(client::error)?;
    let stream = OwnedFd::from(stream);
    let local = tokio::task::spawn_blocking(move || {
        unpack(
            File::from(stream),
            |is_dir, top| {
                let plan = plan(is_dir, top, contents, &dst, dst_kind, trailing_slash)?;
                Ok(Target {
                    root: open_dir(&plan.dir)?,
                    base: PathBuf::new(),
                    top: plan.top,
                })
            },
            Ids::Keep,
        )
    });
    let remote = watch.finish(job).await;
    let local = local.await.context("the copy stopped")?;
    match (local, remote) {
        (Ok(stats), Ok(_)) => {
            skipped(&stats);
            Ok(())
        }
        (Err(local), Err(remote)) => Err(first_cause(local, remote)),
        (Err(e), Ok(_)) | (Ok(_), Err(e)) => Err(e),
    }
}

async fn copy_in(client: &Client, source: &str, name: &str, path: &str) -> Result<()> {
    let (source, contents) = split_source(source);
    let source = PathBuf::from(source);
    fs::symlink_metadata(&source).with_context(|| format!("{}", source.display()))?;
    // The parent is opened and the last component read from it without following it,
    // like docker; "." and "/" are named by what they are.
    let (parent, leaf, top): (PathBuf, OsString, OsString) = match source.file_name() {
        Some(leaf)
            if source
                .components()
                .next_back()
                .is_some_and(|c| matches!(c, std::path::Component::Normal(_))) =>
        {
            let parent = source
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."));
            (parent, leaf.to_os_string(), leaf.to_os_string())
        }
        _ => {
            let real = fs::canonicalize(&source)
                .with_context(|| format!("resolving {}", source.display()))?;
            match (real.parent(), real.file_name()) {
                (Some(parent), Some(leaf)) => (
                    parent.to_path_buf(),
                    leaf.to_os_string(),
                    leaf.to_os_string(),
                ),
                _ => (real.clone(), OsString::from("."), OsString::from("root")),
            }
        }
    };
    let parent_fd = open_dir(&parent)?;
    let (read, write) = nix::unistd::pipe2(OFlag::O_CLOEXEC).context("creating a pipe")?;
    let watch = client.watch_jobs().await?;
    let mut options = Options::new();
    if contents {
        options.insert("contents", Value::from(true));
    }
    let job = client
        .manager
        .copy_to(name, path, zbus::zvariant::Fd::from(read.as_fd()), options)
        .await
        .map_err(client::error)?;
    drop(read);
    let packer = std::thread::spawn(move || {
        block_sigpipe();
        pack(
            parent_fd.as_fd(),
            &leaf,
            Path::new(&top),
            Ids::Keep,
            File::from(write),
        )
    });
    let remote = watch.finish(job).await;
    let local = packer.join().map_err(|_| anyhow!("the copy stopped"))?;
    match (local, remote) {
        (Ok(stats), Ok(_)) => {
            skipped(&stats);
            Ok(())
        }
        (Err(local), Err(remote)) => Err(first_cause(local, remote)),
        (Err(e), Ok(_)) | (Ok(_), Err(e)) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_end_that_failed_first_is_the_one_shown() {
        let pipe = || anyhow!("writing x: Broken pipe (os error 32)");
        let real = || anyhow!("x: Permission denied");
        assert!(first_cause(pipe(), real())
            .to_string()
            .contains("Permission"));
        assert!(first_cause(real(), pipe())
            .to_string()
            .contains("Permission"));
        assert!(first_cause(anyhow!("nothing arrived to copy"), real())
            .to_string()
            .contains("Permission"));
    }
}
