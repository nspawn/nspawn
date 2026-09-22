//! The library behind the command line: typed operations on images, machines and the
//! network. Nothing here prints. What an operation has to say while it works goes
//! through a `Report`, what it found comes back as a value, and the command line and
//! the D-Bus service are two clients of the same functions.

pub mod build;
pub mod create;
pub mod hub;
pub mod images;
pub mod login;
pub mod machines;
pub mod network;
pub mod pull;
pub mod push;
pub mod search;

use anyhow::{bail, Context as _, Result};
use tokio::sync::OnceCell;

use crate::config::Config;
use crate::store::Store;
use crate::systemd::Systemd;

/// Everything an operation needs: the configuration, the store and, when it is used,
/// the connection to systemd and machined.
pub struct Context {
    pub config: Config,
    pub store: Store,
    sd: OnceCell<Systemd>,
}

impl Context {
    pub fn new(config: Config) -> Self {
        let store = Store::new(&config.machines_dir, &config.state_dir);
        Context {
            config,
            store,
            sd: OnceCell::new(),
        }
    }

    /// The bus connection, made on first use: listing a hub or forgetting credentials
    /// must work where there is no bus.
    pub async fn sd(&self) -> Result<&Systemd> {
        self.sd
            .get_or_try_init(Systemd::connect)
            .await
            .context("connecting to systemd")
    }
}

/// What an operation says while it works.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Progress or a partial result: "blob 1cfa4e2b09e1: downloading".
    Line(String),
    /// A remark beside the result: "note: ..." or "warning: ...".
    Note(String),
}

/// Where events go: the terminal, a D-Bus job, a log.
pub type Report<'a> = &'a (dyn Fn(Event) + Send + Sync);

pub fn line(report: Report, text: impl Into<String>) {
    report(Event::Line(text.into()));
}

pub fn note(report: Report, text: impl Into<String>) {
    report(Event::Note(text.into()));
}

pub fn require_root(action: &str) -> Result<()> {
    if !nix::unistd::geteuid().is_root() {
        bail!("{action} needs root privileges (it writes below /var/lib/machines and /etc/systemd/system)");
    }
    Ok(())
}
