//! Local images: what machined and the store know about them, and their removal.

use std::collections::HashMap;

use anyhow::{bail, Result};

use crate::api::{line, require_root, Context, Report};
use crate::backend::{Assembler, BackendChoice};
use crate::bridge;

/// One local image as `images ls` shows it: machined's view plus nspawn's record when
/// there is one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageSummary {
    pub name: String,
    /// machined's kind: "directory", "subvolume", "raw".
    pub kind: String,
    pub backend: Option<BackendChoice>,
    /// "pull", "build" or "create".
    pub origin: Option<String>,
    /// The reference it was pulled from or built as.
    pub reference: Option<String>,
    pub size: Option<u64>,
    pub read_only: bool,
}

pub async fn list(ctx: &Context) -> Result<Vec<ImageSummary>> {
    let sd = ctx.sd().await?;
    let records: HashMap<String, _> = ctx
        .store
        .list_images()?
        .into_iter()
        .map(|r| (r.name.clone(), r))
        .collect();
    let mut images = sd.list_images().await?;
    images.retain(|i| !i.name.starts_with('.'));
    images.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(images
        .into_iter()
        .map(|i| {
            let record = records.get(&i.name);
            ImageSummary {
                backend: record.map(|r| r.backend),
                origin: record.map(|r| r.origin.clone()),
                reference: record.map(|r| r.reference.clone()),
                name: i.name,
                kind: i.kind,
                size: i.usage,
                read_only: i.read_only,
            }
        })
        .collect())
}

/// Removes images and whatever they alone kept: layers, blobs, network files. A running
/// machine is refused. What was removed before an error stays removed.
pub async fn remove(ctx: &Context, names: &[String], report: Report<'_>) -> Result<()> {
    require_root("images rm")?;
    let sd = ctx.sd().await?;
    let store = &ctx.store;
    let assembler = Assembler { store, sd };
    let _lock = store.lock()?;
    let outcome: Result<()> = async {
        for name in names {
            if sd.machine_exists(name).await? {
                bail!("machine {name} is running; stop it first");
            }
            match store.load_image(name)? {
                Some(rec) => {
                    assembler.remove(name, rec.backend).await?;
                    store.remove_machine_files(name)?;
                    bridge::delete_netns(name);
                    store.remove_record(name)?;
                }
                None => {
                    // Not recorded: an image machined knows, or leftovers of ours.
                    assembler.remove_leftovers(name).await?;
                    if sd.list_images().await?.iter().any(|i| i.name == *name) {
                        sd.remove_image(name).await?;
                    }
                }
            }
            line(report, format!("removed {name}"));
        }
        Ok(())
    }
    .await;
    // Whatever happened above, what was removed must not pin anything, and a machine that
    // died on its own must not keep its ports.
    bridge::write_hosts_files(store, &ctx.config)?;
    bridge::sync_ports(store, sd).await?;
    let gone = store.gc_layers()?;
    if !gone.is_empty() {
        line(report, format!("freed {} unused layer(s)", gone.len()));
    }
    let blobs = store.gc_blobs()?;
    if !blobs.is_empty() {
        line(report, format!("freed {} unused blob(s)", blobs.len()));
    }
    outcome
}
