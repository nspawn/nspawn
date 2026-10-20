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
    /// Who signed it, when the pull verified a signature.
    pub signed_by: Option<String>,
    /// Pulled or built and never run: nothing of a machine is lost when it is replaced.
    pub image_only: bool,
    /// When the image was made, as its config says (unix seconds).
    pub created: Option<u64>,
    /// When it was pulled or built here (unix seconds).
    pub pulled: Option<u64>,
}

/// The `created` of a record's image config, what docker shows as an image's CREATED.
fn config_created(store: &crate::store::Store, record: &crate::store::ImageRecord) -> Option<u64> {
    let manifest: oci_client::manifest::OciImageManifest =
        serde_json::from_slice(&store.load_manifest(&record.name).ok()?).ok()?;
    let blob = std::fs::read(store.blob_path(&manifest.config.digest)).ok()?;
    let config: serde_json::Value = serde_json::from_slice(&blob).ok()?;
    let created = crate::compose::gotime::parse_rfc3339(config.get("created")?.as_str()?)?;
    u64::try_from(created).ok()
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
                signed_by: record.and_then(|r| r.signed_by.clone()),
                image_only: record.is_some_and(|r| crate::api::disk::image_only(&ctx.store, r)),
                created: record.and_then(|r| config_created(&ctx.store, r)),
                pulled: record.map(|r| r.created),
                name: i.name,
                kind: i.kind,
                size: i.usage,
                read_only: i.read_only,
            }
        })
        .collect())
}

/// The machines of `names` that another machine needs, with why: one takes its network
/// (--network container:) or depends on it (--depends-on), and does not go too.
fn still_needed(
    records: &[crate::store::ImageRecord],
    names: &[String],
    skip: &std::collections::HashSet<String>,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for name in names.iter().filter(|n| !skip.contains(*n)) {
        let sharing: Vec<&str> = records
            .iter()
            .filter(|r| r.network_container.as_deref() == Some(name.as_str()))
            .filter(|r| !names.contains(&r.name))
            .map(|r| r.name.as_str())
            .collect();
        if !sharing.is_empty() {
            let sharing = sharing.join(", ");
            out.push((
                name.clone(),
                format!("{name} is the network of {sharing} (--network container:{name}); remove them or give them a network of their own first"),
            ));
            continue;
        }
        let dependents = crate::api::dependencies::dependents(records, name, names);
        if !dependents.is_empty() {
            let (verb, them, their) = if dependents.len() == 1 {
                ("depends", "it", "its")
            } else {
                ("depend", "them", "their")
            };
            out.push((
                name.clone(),
                format!("{} {verb} on {name} (--depends-on); remove {them} too, or drop the dependency with --depends-on none at {their} next start", dependents.join(", ")),
            ));
        }
    }
    out
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

/// Removes every name it can: one that cannot be removed does not stop
/// the others, and is reported in the result.
pub async fn remove(ctx: &Context, names: &[String], report: Report<'_>) -> Result<Removal> {
    require_root("images rm")?;
    remove_machines(ctx, names, false, report).await
}

/// `remove`, and with `force` a running machine is stopped first (SIGKILL) instead of
/// refused.
pub async fn remove_machines(
    ctx: &Context,
    names: &[String],
    force: bool,
    report: Report<'_>,
) -> Result<Removal> {
    remove_machines_if(ctx, names, force, report, &|_, _, _| None).await
}

/// Why a name of a removal stays, said under the lock from the records as they are
/// then; None for one that goes.
pub type Keep =
    dyn Fn(&crate::store::Store, &[crate::store::ImageRecord], &str) -> Option<String> + Sync;

/// `remove_machines`, `keep` saying under the lock, from the records as they are then,
/// why a name now stays: what a prune chose may have become a machine's source.
pub async fn remove_machines_if(
    ctx: &Context,
    names: &[String],
    force: bool,
    report: Report<'_>,
    keep: &Keep,
) -> Result<Removal> {
    require_root(if force { "rm --force" } else { "rm" })?;
    let sd = ctx.sd().await?;
    let store = &ctx.store;
    let assembler = Assembler { store, sd };
    let mut removal = Removal::default();
    // Stopping takes the store lock itself, so it happens before the removal takes it.
    let mut skip = std::collections::HashSet::new();
    // A name is joined to paths below: one that is not a plain entry is refused first.
    for name in names {
        if let Err(e) = crate::reference::validate_entry_name(name) {
            removal.failed.push((name.clone(), format!("{e:#}")));
            skip.insert(name.clone());
        }
    }
    // Machines others need stay: checked now, to know what to stop, and again under the
    // lock, as a machine may have come to need one meanwhile.
    let records = store.list_images()?;
    for (name, why) in still_needed(&records, names, &skip) {
        removal.failed.push((name.clone(), why));
        skip.insert(name);
    }
    if force {
        // Each machine before the ones it depends on: one that a refused machine depends
        // on stays, and is not killed for nothing.
        let order = crate::api::dependencies::removal_order(&records, names);
        let mut starting = std::collections::HashSet::new();
        for name in order
            .iter()
            .filter(|n| !skip.contains(*n))
            .cloned()
            .collect::<Vec<_>>()
        {
            let name = &name;
            if crate::api::dependencies::holders(&records, name, &[])
                .iter()
                .any(|d| skip.contains(*d) || starting.contains(*d))
            {
                continue;
            }
            // Refused below, and what it needs stays with it.
            if store.is_starting(name) {
                starting.insert(name.clone());
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
                timeout: Some(0),
            };
            if let Err(e) = crate::api::machines::stop(ctx, &stop, report).await {
                removal.failed.push((name.clone(), format!("{e:#}")));
                skip.insert(name.clone());
            }
        }
    }
    let _lock = store.lock().await?;
    let records = store.list_images()?;
    for (name, why) in still_needed(&records, names, &skip) {
        removal.failed.push((name.clone(), why));
        skip.insert(name);
    }
    for name in names {
        if skip.contains(name) {
            continue;
        }
        if let Some(why) = keep(store, &records, name) {
            removal.failed.push((name.clone(), why));
            skip.insert(name.clone());
        }
    }
    let hint = if force {
        ""
    } else {
        ", or use nspawn rm --force"
    };
    // A machine before the ones it depends on, which go only once it has: one that
    // stays keeps them.
    let order = crate::api::dependencies::removal_order(&records, names);
    for name in order.iter().filter(|n| !skip.contains(*n)) {
        let outcome: Result<Option<(String, u64)>> = async {
            let staying = crate::api::dependencies::holders(&records, name, &removal.removed);
            if !staying.is_empty() {
                let (stays, needs) = if staying.len() == 1 {
                    ("stays", "needs")
                } else {
                    ("stay", "need")
                };
                bail!(
                    "{} {stays} and {needs} {name} (--depends-on or --network container:); {name} stays with it",
                    staying.join(", ")
                );
            }
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
                    crate::api::machines::release_machine(name, Some(&rec))?;
                    store.remove_record(name)?;
                    // An update that was still deciding about it has nothing left to decide.
                    store.forget_rollback(name)?;
                    crate::install::take_off_boot(sd, name, report).await;
                    crate::install::forget_unit_ending(sd, name).await;
                    // Named volumes outlive the machine.
                    for volume in rec.volumes.iter().filter(|v| v.is_named()) {
                        note(
                            report,
                            format!(
                                "note: volume {0} kept; nspawn volume rm {0} removes it",
                                volume.source
                            ),
                        );
                    }
                    Ok(Some((rec.reference, rec.created)))
                }
                None => {
                    // Not recorded: an image machined knows, or leftovers of ours. A name
                    // with nothing at all behind it is a mistake worth hearing about.
                    let mut found = assembler.remove_leftovers(name).await?;
                    if sd.list_images().await?.iter().any(|i| i.name == *name) {
                        sd.remove_image(name).await?;
                        found = true;
                    }
                    found |= crate::install::take_off_boot(sd, name, report).await;
                    // An update cut short left only its rollback: rm lets it go too, or
                    // the next recover would bring back what was removed.
                    if store.rollback_dir(name).exists() {
                        store.forget_rollback(name)?;
                        found = true;
                    }
                    if !found {
                        bail!("no machine or image named {name}");
                    }
                    Ok(None)
                }
            }
        }
        .await;
        match outcome {
            Ok(image) => {
                line(report, format!("removed {name}"));
                // The record is gone: what the event says of the machine has to travel
                // with it.
                let created = image.as_ref().map(|(_, c)| c.to_string());
                let mut attributes: Vec<(&str, &str)> = Vec::new();
                if let (Some((reference, _)), Some(created)) = (&image, &created) {
                    attributes.push(("image", reference.as_str()));
                    attributes.push(("created", created.as_str()));
                }
                crate::api::events::emit("machine", "remove", name, &attributes);
                removal.removed.push(name.clone());
            }
            Err(e) => removal.failed.push((name.clone(), format!("{e:#}"))),
        }
    }
    // Whatever happened above, what was removed must not pin anything, and a machine that
    // died on its own must not keep its ports.
    // A problem here belongs to other images and the next collection: the removals
    // happened, and say so.
    let tidy = async {
        bridge::write_hosts_files(store, &crate::api::network::all(store, &ctx.config)?)?;
        bridge::sync_ports(store, sd).await?;
        // Trees of any size: off the runtime's workers.
        let gone = tokio::task::block_in_place(|| store.gc_layers())?;
        if !gone.is_empty() {
            line(report, format!("freed {} unused layer(s)", gone.len()));
        }
        let blobs = tokio::task::block_in_place(|| store.gc_blobs())?;
        if !blobs.is_empty() {
            line(report, format!("freed {} unused blob(s)", blobs.len()));
        }
        Ok::<(), anyhow::Error>(())
    };
    if let Err(e) = tidy.await {
        note(report, format!("warning: {e:#}"));
    }
    Ok(removal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machines_others_need_stay() {
        let record = |json: serde_json::Value| -> crate::store::ImageRecord {
            let mut base = serde_json::json!({
                "name": "x", "reference": "r", "manifest_digest": "sha256:a", "layers": [],
                "backend": "overlay", "created": 1, "origin": "create",
            });
            for (k, v) in json.as_object().unwrap() {
                base[k] = v.clone();
            }
            serde_json::from_value(base).unwrap()
        };
        let records = [
            record(serde_json::json!({ "name": "db" })),
            record(serde_json::json!({ "name": "vpn" })),
            record(serde_json::json!({
                "name": "web",
                "tuning": { "depends_on": [{ "name": "db", "condition": "started" }] },
            })),
            record(serde_json::json!({ "name": "torrent", "network_container": "vpn" })),
        ];
        let names = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let none = std::collections::HashSet::new();
        let needed = still_needed(&records, &names(&["db", "vpn"]), &none);
        assert_eq!(needed.len(), 2, "{needed:?}");
        assert!(needed[0].1.contains("web depends on db"));
        assert!(needed[1].1.contains("network of torrent"));
        // Going together, nothing stays behind them.
        assert!(still_needed(&records, &names(&["db", "web"]), &none).is_empty());
    }

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
