//! org.nspawn.Names at /org/nspawn: the names of machines, images, networks and volumes,
//! nothing else about them, for every user and without polkit, as machined lists its
//! machines and images. A shell completing `nspawn stop <TAB>` runs as its user, even
//! under sudo, and cannot answer a password prompt; the names are all it gets.

use std::sync::Arc;

use crate::api;
use crate::daemon::manager::Error;
use crate::daemon::State;

type Result<T> = std::result::Result<T, Error>;

pub struct Names {
    state: Arc<State>,
}

impl Names {
    pub fn new(state: Arc<State>) -> Self {
        Names { state }
    }
}

#[zbus::interface(name = "org.nspawn.Names")]
impl Names {
    /// The machines that run, as `ps` lists them; with `all`, every machine nspawn keeps
    /// a record of too, as `ps -a`.
    async fn machines(&self, all: bool) -> Result<Vec<String>> {
        let _busy = self.state.enter();
        Ok(api::names::machines(&self.state.ctx, all).await?)
    }

    /// The images, as `images ls` lists them: what `start` takes.
    async fn images(&self) -> Result<Vec<String>> {
        let _busy = self.state.enter();
        Ok(api::names::images(&self.state.ctx).await?)
    }

    /// The references of the local images: what `run` takes without a pull.
    async fn references(&self) -> Result<Vec<String>> {
        let _busy = self.state.enter();
        Ok(api::names::references(&self.state.ctx)?)
    }

    /// The networks, "bridge" among them.
    async fn networks(&self) -> Result<Vec<String>> {
        let _busy = self.state.enter();
        Ok(api::names::networks(&self.state.ctx)?)
    }

    /// The named volumes.
    async fn volumes(&self) -> Result<Vec<String>> {
        let _busy = self.state.enter();
        Ok(api::names::volumes(&self.state.ctx)?)
    }
}
