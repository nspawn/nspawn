//! Local images: what machined and the store know about them, and their removal.

use std::collections::HashMap;

use anyhow::{bail, Result};

use crate::api::{line, note, require_root, Context, Report};
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
/// What `remove` did: the names that are gone, and for the others why not.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Removal {
    pub removed: Vec<String>,
    pub failed: Vec<(String, String)>,
}

impl Removal {
    /// The failures as one message, when there were any.
    pub fn error(&self) -> Option<String> {
        if self.failed.is_empty() {
            return None;
        }
        let reasons: Vec<String> = self
            .failed
            .iter()
            .map(|(name, why)| {
                if why.contains(name.as_str()) {
                    why.clone()
                } else {
                    format!("{name}: {why}")
                }
            })
            .collect();
        Some(reasons.join("; "))
    }
}

/// Removes every name it can, like docker rmi: one that cannot be removed does not stop
/// the others, and is reported in the result.
pub async fn remove(ctx: &Context, names: &[String], report: Report<'_>) -> Result<Removal> {
    remove_machines(ctx, names, false, report).await
}

/// `remove`, and with `force` a running machine is stopped first (SIGKILL, like docker
/// rm -f) instead of refused.
pub async fn remove_machines(
    ctx: &Context,
    names: &[String],
    force: bool,
    report: Report<'_>,
) -> Result<Removal> {
    require_root(if force { "rm --force" } else { "rm" })?;
    let sd = ctx.sd().await?;
    let store = &ctx.store;
    let assembler = Assembler { store, sd };
    let mut removal = Removal::default();
    // Stopping takes the store lock itself, so it happens before the removal takes it.
    let mut skip = std::collections::HashSet::new();
    if force {
        for name in names {
            if store.is_starting(name) {
                continue;
            }
            // Running, or its unit restarting it: the stop's job ends that too.
            if !sd.machine_exists(name).await?
                && crate::api::machines::unit_busy(sd, name).await?.is_none()
            {
                continue;
            }
            let stop = crate::api::machines::StopRequest {
                name: name.clone(),
                force: true,
                wait: true,
                timeout: 0,
            };
            if let Err(e) = crate::api::machines::stop(ctx, &stop, report).await {
                removal.failed.push((name.clone(), format!("{e:#}")));
                skip.insert(name.clone());
            }
        }
    }
    let _lock = store.lock().await?;
    let hint = if force { "" } else { ", or use rm --force" };
    for name in names.iter().filter(|n| !skip.contains(*n)) {
        let outcome: Result<()> = async {
            if store.is_starting(name) {
                bail!("machine {name} is starting; wait for it or stop it first");
            }
            if sd.machine_exists(name).await? {
                bail!("machine {name} is running; stop it first{hint}");
            }
            if let Some(why) = crate::api::machines::unit_busy(sd, name).await? {
                bail!("{why}{hint}");
            }
            match store.load_image(name)? {
                Some(rec) => {
                    assembler.remove(name, rec.backend).await?;
                    store.remove_machine_files(name)?;
                    bridge::delete_netns(name);
                    store.remove_record(name)?;
                    // Like docker: named volumes outlive the machine.
                    for volume in rec.volumes.iter().filter(|v| v.is_named()) {
                        note(
                            report,
                            format!(
                                "note: volume {0} kept; nspawn volume rm {0} removes it",
                                volume.source
                            ),
                        );
                    }
                }
                None => {
                    // Not recorded: an image machined knows, or leftovers of ours.
                    assembler.remove_leftovers(name).await?;
                    if sd.list_images().await?.iter().any(|i| i.name == *name) {
                        sd.remove_image(name).await?;
                    }
                }
            }
            Ok(())
        }
        .await;
        match outcome {
            Ok(()) => {
                line(report, format!("removed {name}"));
                removal.removed.push(name.clone());
            }
            Err(e) => removal.failed.push((name.clone(), format!("{e:#}"))),
        }
    }
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
    Ok(removal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removal_failures_read_as_one_message() {
        let mut removal = Removal::default();
        assert_eq!(removal.error(), None);
        removal.failed.push((
            "web".to_string(),
            "machine web is running; stop it first".to_string(),
        ));
        removal
            .failed
            .push(("db".to_string(), "Permission denied".to_string()));
        assert_eq!(
            removal.error().unwrap(),
            "machine web is running; stop it first; db: Permission denied"
        );
    }
}
