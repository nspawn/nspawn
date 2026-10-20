//! auto-update: the machines labelled for it get the image their reference names now at
//! its registry, made anew with every option they had and put back as they were when the
//! new image does not come up (see recreate), one machine at a time, the machines they
//! depend on first.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use anyhow::{Context as _, Result};

use crate::api::machines::{StartRequest, StopRequest};
use crate::api::pull::Fetched;
use crate::api::recreate::COMPOSE_IMAGE;
use crate::api::{line, note, Context, Report};
use crate::backend::BackendChoice;
use crate::hub::Hub;
use crate::oci::Mode;
use crate::reference::ImageRef;
use crate::store::ImageRecord;

/// The label that opts a machine in, with the only policy there is, `registry`.
pub const LABEL: &str = "org.nspawn.auto-update";
/// podman's, taken as well, so that machines moved from podman keep updating.
pub const PODMAN_LABEL: &str = "io.containers.autoupdate";

/// One run at a time: two would update the same machine twice.
static RUNNING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Updated {
    /// Its image is the one the registry names.
    No,
    /// A newer image is there (a dry run).
    Pending,
    Yes,
    Failed,
    RolledBack,
    Skipped,
}

impl Updated {
    pub fn word(self) -> &'static str {
        match self {
            Updated::No => "false",
            Updated::Pending => "pending",
            Updated::Yes => "true",
            Updated::Failed => "failed",
            Updated::RolledBack => "rolled back",
            Updated::Skipped => "skipped",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub name: String,
    pub image: String,
    pub policy: String,
    pub updated: Updated,
    /// Why it was skipped, failed or rolled back.
    pub reason: String,
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Request {
    /// The machines to look at; every labelled one without names.
    pub names: Vec<String>,
    pub dry_run: bool,
}

/// The machine's policy: its own label, never one its image carries.
pub fn policy(record: &ImageRecord) -> Option<&str> {
    record
        .labels
        .get(LABEL)
        .or_else(|| record.labels.get(PODMAN_LABEL))
        .map(String::as_str)
}

/// Why a labelled machine is not updated, if it is not.
pub fn skip_reason(record: &ImageRecord, built: &BTreeSet<&str>) -> Option<String> {
    match policy(record) {
        None => return Some(format!("it has no {LABEL} label")),
        Some("registry") => {}
        Some(other) => {
            return Some(format!(
                "unsupported policy {other:?} (registry is the one there is)"
            ))
        }
    }
    if record.origin == "build" || built.contains(record.reference.as_str()) {
        return Some("its image was built on this host".to_string());
    }
    if record.reference.contains('@') {
        return Some("its reference names a digest, which does not change".to_string());
    }
    if record.remove_on_exit {
        return Some("it is removed once it ends (run --rm)".to_string());
    }
    if record.backend != BackendChoice::Overlay {
        return Some("only overlay machines are updated".to_string());
    }
    if record.mode == Mode::Boot {
        return Some("it boots an init system, whose state is its writable layer".to_string());
    }
    None
}

/// Machines after the ones they depend on or take their network from.
pub fn in_dependency_order(records: Vec<ImageRecord>) -> Vec<ImageRecord> {
    let names: BTreeSet<String> = records.iter().map(|r| r.name.clone()).collect();
    let mut pending: BTreeMap<String, ImageRecord> =
        records.into_iter().map(|r| (r.name.clone(), r)).collect();
    let mut out = Vec::new();
    while !pending.is_empty() {
        let ready: Vec<String> = pending
            .values()
            .filter(|r| {
                needs(r)
                    .iter()
                    .all(|n| !names.contains(n) || !pending.contains_key(n))
            })
            .map(|r| r.name.clone())
            .collect();
        // A cycle, which --depends-on refuses: the rest in name order.
        let batch = if ready.is_empty() {
            pending.keys().cloned().collect()
        } else {
            ready
        };
        for name in batch {
            if let Some(r) = pending.remove(&name) {
                out.push(r);
            }
        }
    }
    out
}

fn needs(record: &ImageRecord) -> Vec<String> {
    let mut out: Vec<String> = record
        .tuning
        .depends_on
        .iter()
        .map(|d| d.name.clone())
        .collect();
    out.extend(record.network_container.clone());
    out
}

/// What the registry has for a reference, looked up once per run.
enum Remote {
    Same,
    /// The image the reference names now, brought in when the run is not a dry one.
    Newer {
        digest: String,
        fetched: Option<Arc<Fetched>>,
    },
    Error(String),
}

pub async fn run(ctx: &Context, request: &Request, report: Report<'_>) -> Result<Vec<Outcome>> {
    let _one = RUNNING.lock().await;
    recover(ctx, report).await;
    let store = &ctx.store;
    let records = store.list_images()?;
    let built: BTreeSet<&str> = records
        .iter()
        .filter(|r| r.origin == "build")
        .map(|r| r.reference.as_str())
        .collect();
    let mut chosen = Vec::new();
    if request.names.is_empty() {
        chosen.extend(records.iter().filter(|r| policy(r).is_some()).cloned());
    } else {
        for name in &request.names {
            let record = records
                .iter()
                .find(|r| &r.name == name)
                .with_context(|| format!("no machine named {name}"))?;
            chosen.push(record.clone());
        }
    }
    let mut remotes: BTreeMap<String, Remote> = BTreeMap::new();
    let mut outcomes = Vec::new();
    for record in in_dependency_order(chosen) {
        let mut outcome = Outcome {
            name: record.name.clone(),
            image: record.reference.clone(),
            policy: policy(&record).unwrap_or("").to_string(),
            updated: Updated::Skipped,
            reason: String::new(),
            from: record.manifest_digest.clone(),
            to: String::new(),
        };
        if let Some(why) = skip_reason(&record, &built) {
            outcome.reason = why;
            outcomes.push(outcome);
            continue;
        }
        if !remotes.contains_key(&record.reference) {
            let remote = look_up(ctx, &record, request.dry_run, report).await;
            remotes.insert(record.reference.clone(), remote);
        }
        match &remotes[&record.reference] {
            Remote::Error(why) => {
                outcome.updated = Updated::Failed;
                outcome.reason = why.clone();
            }
            Remote::Same => outcome.updated = Updated::No,
            Remote::Newer { digest, fetched } => {
                outcome.to = digest.clone();
                if *digest == record.manifest_digest {
                    outcome.updated = Updated::No;
                } else if request.dry_run {
                    outcome.updated = Updated::Pending;
                } else if let Some(fetched) = fetched {
                    line(
                        report,
                        format!(
                            "{}: {} moved to {} (was {})",
                            record.name,
                            record.reference,
                            crate::hub::short_digest(digest),
                            crate::hub::short_digest(&record.manifest_digest)
                        ),
                    );
                    match crate::api::recreate::recreate(ctx, &record.name, fetched, report).await {
                        Ok(()) => outcome.updated = Updated::Yes,
                        Err(e) => {
                            let text = format!("{e:#}");
                            outcome.updated = if text.contains("rolled back")
                                || text.contains("back as it was")
                            {
                                Updated::RolledBack
                            } else {
                                Updated::Failed
                            };
                            outcome.reason = text;
                        }
                    }
                    if outcome.updated != Updated::Failed {
                        restart_dependents(ctx, &record, report).await;
                    }
                    if outcome.updated == Updated::Yes {
                        refresh_compose_image(ctx, &record, fetched, report).await;
                    }
                }
            }
        }
        crate::api::events::emit(
            "machine",
            "auto-update",
            &outcome.name,
            &[
                ("image", &outcome.image),
                ("result", outcome.updated.word()),
                ("from", &outcome.from),
                ("to", &outcome.to),
            ],
        );
        outcomes.push(outcome);
    }
    Ok(outcomes)
}

/// Asks the registry what the machine's reference names now: a HEAD for the digest
/// first, which registries do not count against their pull limits, and the manifest
/// for this platform when that differs from what the machine was pulled as.
async fn look_up(ctx: &Context, record: &ImageRecord, dry_run: bool, report: Report<'_>) -> Remote {
    let attempt = async {
        let image = ImageRef::parse(&record.reference, &ctx.config.registry)?;
        let oci = image.to_oci()?;
        let hub = Hub::new(&ctx.config)?;
        let named = hub.digest(&oci).await?;
        let known = record
            .index_digest
            .as_deref()
            .unwrap_or(&record.manifest_digest);
        if named == known {
            return Ok(Remote::Same);
        }
        // A new index may name the same manifest for this platform.
        let resolved = hub.resolve(&oci).await?;
        if resolved.digest == record.manifest_digest {
            if record.index_digest.is_none() || resolved.index_digest != record.index_digest {
                let _lock = ctx.store.lock().await?;
                if let Some(mut current) = ctx.store.load_image(&record.name)? {
                    current.index_digest.clone_from(&resolved.index_digest);
                    ctx.store.record_image(&current)?;
                }
            }
            return Ok(Remote::Same);
        }
        let fetched = if dry_run {
            None
        } else {
            let fetched =
                crate::api::pull::fetch(ctx, &image, true, Some("overlay"), report).await?;
            Some(Arc::new(fetched))
        };
        Ok::<Remote, anyhow::Error>(Remote::Newer {
            digest: resolved.digest,
            fetched,
        })
    };
    attempt
        .await
        .unwrap_or_else(|e: anyhow::Error| Remote::Error(format!("{e:#}")))
}

/// Machines that take this one's network are in the namespace of the run that ended:
/// they start again in the new one. So do compose services whose depends_on asks for a
/// restart with it.
async fn restart_dependents(ctx: &Context, record: &ImageRecord, report: Report<'_>) {
    let Ok(sd) = ctx.sd().await else { return };
    let Ok(records) = ctx.store.list_images() else {
        return;
    };
    let service = record.labels.get("com.docker.compose.service");
    let project = record.labels.get("com.docker.compose.project");
    for other in records.iter().filter(|r| r.name != record.name) {
        let shares = other.network_container.as_deref() == Some(record.name.as_str());
        let compose_restart = match (service, project) {
            (Some(service), Some(project))
                if other.labels.get("com.docker.compose.project") == Some(project) =>
            {
                other
                    .labels
                    .get("com.docker.compose.depends_on")
                    .is_some_and(|deps| {
                        deps.split(',')
                            .any(|d| d.split(':').next() == Some(service) && d.ends_with(":true"))
                    })
            }
            _ => false,
        };
        if !(shares || compose_restart) || !sd.machine_exists(&other.name).await.unwrap_or(false) {
            continue;
        }
        line(
            report,
            format!("{}: restarting, it depends on {}", other.name, record.name),
        );
        let stop = StopRequest {
            name: other.name.clone(),
            force: false,
            wait: true,
            timeout: None,
        };
        if let Err(e) = crate::api::machines::stop(ctx, &stop, report).await {
            note(report, format!("warning: stopping {}: {e:#}", other.name));
            continue;
        }
        if let Err(e) =
            crate::api::machines::start(ctx, &StartRequest::keep(&other.name), report).await
        {
            note(report, format!("warning: starting {}: {e:#}", other.name));
        }
    }
}

/// compose made the machine from an image record of the same reference, which `compose
/// up` compares with the machine's label: that record gets the new image too, when it
/// never ran as a machine of its own (nothing in its writable layer), so that up does
/// not take the machine back to the old image.
async fn refresh_compose_image(
    ctx: &Context,
    record: &ImageRecord,
    fetched: &Fetched,
    report: Report<'_>,
) {
    if !record.labels.contains_key(COMPOSE_IMAGE) {
        return;
    }
    let Ok(records) = ctx.store.list_images() else {
        return;
    };
    let Ok(sd) = ctx.sd().await else { return };
    for image in records.iter().filter(|r| {
        r.name != record.name
            && r.reference == record.reference
            && r.origin == "pull"
            && r.manifest_digest != fetched.manifest_digest
            && !r.labels.contains_key(COMPOSE_IMAGE)
    }) {
        let upper = ctx
            .store
            .machines_private_dir()
            .join(&image.name)
            .join("upper");
        let unused = std::fs::read_dir(&upper).is_ok_and(|mut d| d.next().is_none());
        let running = sd.machine_exists(&image.name).await.unwrap_or(true);
        if running || !unused || image.backend != BackendChoice::Overlay {
            note(
                report,
                format!(
                    "note: compose up would make {} anew from {}, which keeps the old image; nspawn pull --force {} brings the new one",
                    record.name, image.name, image.reference
                ),
            );
            continue;
        }
        let refreshed = async {
            let _lock = ctx.store.lock().await?;
            let backend = crate::backend::Backend::choose(BackendChoice::Overlay, sd).await?;
            crate::install::stage_layers(
                &ctx.store,
                backend,
                &fetched.manifest,
                Some(image.mode),
                &image.name,
            )
            .await?;
            crate::install::remove_existing(&ctx.store, sd, &image.name, report).await?;
            let signed = fetched.signed.as_ref();
            crate::install::install(
                &ctx.store,
                sd,
                &ctx.config,
                backend,
                crate::install::Install {
                    name: &image.name,
                    reference: &image.reference,
                    manifest_bytes: &fetched.manifest_bytes,
                    manifest: &fetched.manifest,
                    manifest_digest: &fetched.manifest_digest,
                    index_digest: fetched.index_digest.as_deref(),
                    origin: "pull",
                    mode: Some(image.mode),
                    signed_by: signed.map(|s| s.signed_by.as_str()),
                    signed_at: signed.and_then(|s| s.signed_at),
                },
                report,
            )
            .await?;
            Ok::<(), anyhow::Error>(())
        };
        if let Err(e) = refreshed.await {
            note(
                report,
                format!("warning: {} kept the old image: {e:#}", image.name),
            );
        }
    }
}

/// What a run cut short left: a machine whose new image runs keeps it, any other gets
/// its old image back.
async fn recover(ctx: &Context, report: Report<'_>) {
    let Ok(sd) = ctx.sd().await else { return };
    for name in ctx.store.rollbacks() {
        if sd.machine_exists(&name).await.unwrap_or(false) {
            let _ = ctx.store.forget_rollback(&name);
            continue;
        }
        let restored = async {
            let _lock = ctx.store.lock().await?;
            crate::api::recreate::restore(ctx, &name, report).await
        };
        match restored.await {
            Ok(()) => note(
                report,
                format!(
                    "note: {name} was left halfway by an earlier update; its old image is back"
                ),
            ),
            Err(e) => note(
                report,
                format!("warning: {name} was left halfway by an earlier update: {e:#}"),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(json: serde_json::Value) -> ImageRecord {
        let mut base = serde_json::json!({
            "name": "web", "reference": "docker.io/library/nginx:1", "manifest_digest": "sha256:a",
            "layers": [], "backend": "overlay", "created": 1, "mode": "app",
            "labels": { LABEL: "registry" },
        });
        for (k, v) in json.as_object().unwrap() {
            base[k] = v.clone();
        }
        serde_json::from_value(base).unwrap()
    }

    #[test]
    fn who_is_updated() {
        let none = BTreeSet::new();
        assert_eq!(skip_reason(&record(serde_json::json!({})), &none), None);
        let podman = record(serde_json::json!({ "labels": { PODMAN_LABEL: "registry" } }));
        assert_eq!(skip_reason(&podman, &none), None);
        // The image's labels do not opt a machine in.
        let image_only =
            record(serde_json::json!({ "labels": {}, "run": { "labels": { LABEL: "registry" } } }));
        assert!(skip_reason(&image_only, &none)
            .unwrap()
            .contains("no org.nspawn.auto-update"));
        let local = record(serde_json::json!({ "labels": { LABEL: "local" } }));
        assert!(skip_reason(&local, &none)
            .unwrap()
            .contains("unsupported policy"));
        assert!(skip_reason(&record(serde_json::json!({ "origin": "build" })), &none).is_some());
        let built = BTreeSet::from(["docker.io/library/nginx:1"]);
        assert!(skip_reason(&record(serde_json::json!({ "origin": "create" })), &built).is_some());
        assert!(skip_reason(
            &record(serde_json::json!({ "reference": "docker.io/library/nginx@sha256:aa" })),
            &none
        )
        .is_some());
        assert!(skip_reason(
            &record(serde_json::json!({ "remove_on_exit": true })),
            &none
        )
        .is_some());
        assert!(skip_reason(&record(serde_json::json!({ "backend": "flat" })), &none).is_some());
        assert!(skip_reason(&record(serde_json::json!({ "mode": "boot" })), &none).is_some());
    }

    #[test]
    fn dependencies_first() {
        let web = record(
            serde_json::json!({ "name": "web", "tuning": { "depends_on": [{ "name": "db", "condition": "started" }] } }),
        );
        let side = record(serde_json::json!({ "name": "aside", "network_container": "web" }));
        let db = record(serde_json::json!({ "name": "db" }));
        let order: Vec<String> = in_dependency_order(vec![side, web, db])
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(order, ["db", "web", "aside"]);
    }
}
