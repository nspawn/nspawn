//! auto-update: the machines labelled for it get the image their reference names now at
//! its registry, made anew with every option they had and put back as they were when the
//! new image does not come up (see recreate), one machine at a time, the machines they
//! depend on first.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use anyhow::{bail, Context as _, Result};

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
    let value = record
        .labels
        .get(LABEL)
        .or_else(|| record.labels.get(PODMAN_LABEL))?;
    match value.as_str() {
        // podman's values for no updates.
        "" | "disabled" => None,
        // podman's older name of registry.
        "image" => Some("registry"),
        other => Some(other),
    }
}

/// A policy that is none of the known ones, a misspelt one: said as a failure, so that
/// the run and its timer show it.
pub fn invalid_policy(record: &ImageRecord) -> Option<String> {
    match policy(record)? {
        "registry" | "local" => None,
        other => Some(format!(
            "invalid auto-update policy {other:?}: valid policies are registry, image, local and disabled"
        )),
    }
}

/// Why a labelled machine is not updated, if it is not.
pub fn skip_reason(record: &ImageRecord, built: &BTreeSet<&str>) -> Option<String> {
    match policy(record) {
        None => return Some(format!("it has no {LABEL} label, or a disabled one")),
        Some("registry") => {}
        Some(other) => {
            return Some(format!(
                "unsupported policy {other:?} (registry is the one there is)"
            ))
        }
    }
    if record.origin == "build" || record.built || built.contains(record.reference.as_str()) {
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
    if let Some(path) = record.run.volumes.iter().find(|p| !covered(record, p)) {
        return Some(format!(
            "its image keeps data in {path}, which no volume of the machine covers: it is in the writable layer a new image starts without"
        ));
    }
    None
}

/// Whether a volume of the machine holds `path`.
fn covered(record: &ImageRecord, path: &str) -> bool {
    let path = path.trim_end_matches('/');
    record.volumes.iter().any(|v| {
        let target = v.target.trim_end_matches('/');
        path == target || target.is_empty() || path.starts_with(&format!("{target}/"))
    })
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

/// What the registry says of a reference, asked once per run whatever the number of
/// machines that have it.
#[derive(Default)]
struct Answer {
    /// The digest the reference names (an index's for a tag of several platforms).
    named: Option<String>,
    /// The manifest for this platform, asked for only when `named` is not a machine's.
    resolved: Option<crate::hub::Resolved>,
    /// The new image, brought in once for every machine that takes it.
    fetched: Option<Arc<Fetched>>,
}

/// What the registry has for one machine.
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
    // A dry run only looks: what an earlier run left halfway waits for a real one.
    if !request.dry_run {
        recover(ctx, report).await;
    }
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
    let mut answers: BTreeMap<String, Answer> = BTreeMap::new();
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
        if let Some(why) = invalid_policy(&record) {
            outcome.updated = Updated::Failed;
            outcome.reason = why;
            outcomes.push(outcome);
            continue;
        }
        if let Some(why) = skip_reason(&record, &built) {
            outcome.reason = why;
            outcomes.push(outcome);
            continue;
        }
        let answer = answers.entry(record.reference.clone()).or_default();
        let remote = look_up(ctx, &record, answer, request.dry_run, report).await;
        match &remote {
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
                    let recreated =
                        crate::api::recreate::recreate(ctx, &record, fetched, report).await;
                    match recreated.result {
                        Ok(()) => outcome.updated = Updated::Yes,
                        Err(e) => {
                            outcome.updated = if e
                                .downcast_ref::<crate::api::recreate::RolledBack>()
                                .is_some()
                            {
                                Updated::RolledBack
                            } else {
                                Updated::Failed
                            };
                            outcome.reason = format!("{e:#}");
                        }
                    }
                    if recreated.restarted {
                        restart_dependents(ctx, &record, report).await;
                    }
                    if outcome.updated == Updated::Yes {
                        refresh_compose_image(ctx, &record, fetched, report).await;
                    }
                }
            }
        }
        crate::api::events::emit_machine(
            &ctx.store,
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
/// for this platform when that differs from what the machine was pulled as. What the
/// registry said is kept in `answer` for the next machine of the reference.
async fn look_up(
    ctx: &Context,
    record: &ImageRecord,
    answer: &mut Answer,
    dry_run: bool,
    report: Report<'_>,
) -> Remote {
    let attempt = async {
        let image = ImageRef::parse(&record.reference, &ctx.config.registry)?;
        let oci = image.to_oci()?;
        let hub = Hub::new(&ctx.config)?;
        let named = match &answer.named {
            Some(named) => named.clone(),
            None => {
                let named = hub.digest(&oci).await?;
                answer.named = Some(named.clone());
                named
            }
        };
        let known = record
            .index_digest
            .as_deref()
            .unwrap_or(&record.manifest_digest);
        if named == known {
            return Ok(Remote::Same);
        }
        // A new index may name the same manifest for this platform.
        if answer.resolved.is_none() {
            answer.resolved = Some(hub.resolve(&oci).await?);
        }
        let resolved = answer.resolved.as_ref().expect("resolved above");
        if resolved.digest == record.manifest_digest {
            if !dry_run
                && (record.index_digest.is_none() || resolved.index_digest != record.index_digest)
            {
                let _lock = ctx.store.lock().await?;
                if let Some(mut current) = ctx.store.load_image(&record.name)? {
                    current.index_digest.clone_from(&resolved.index_digest);
                    ctx.store.record_image(&current)?;
                }
            }
            return Ok(Remote::Same);
        }
        if !dry_run && answer.fetched.is_none() {
            let fetched =
                crate::api::pull::fetch(ctx, &image, true, Some("overlay"), report).await?;
            answer.fetched = Some(Arc::new(fetched));
        }
        // What is installed is what the fetch brought, which resolved the tag on its own:
        // that is the digest compared and told.
        let digest = match &answer.fetched {
            Some(fetched) => fetched.manifest_digest.clone(),
            None => resolved.digest.clone(),
        };
        if digest == record.manifest_digest {
            return Ok(Remote::Same);
        }
        Ok::<Remote, anyhow::Error>(Remote::Newer {
            digest,
            fetched: answer.fetched.clone(),
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
        // Only an image nothing ran from: a pulled record started as a machine has its
        // own options and data, which a reinstall would take.
        let unused = crate::api::disk::image_only(&ctx.store, image);
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
            // Looked at again under the lock: started, updated or pulled again while the
            // lock was waited for, it is no image to reinstall any more.
            let current = ctx
                .store
                .load_image(&image.name)?
                .context("it went meanwhile")?;
            if current.manifest_digest != image.manifest_digest
                || current.origin != "pull"
                || current.labels.contains_key(COMPOSE_IMAGE)
                || current.backend != BackendChoice::Overlay
                || !crate::api::disk::image_only(&ctx.store, &current)
                || ctx.store.is_starting(&image.name)
                || sd.machine_exists(&image.name).await?
            {
                bail!(
                    "it changed meanwhile; compose up makes {} anew from the old image",
                    record.name
                );
            }
            let image = &current;
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

/// What a run cut short left (a service that went away, a host that went down): a
/// machine on its new image that runs keeps it, any other gets its old image back, and
/// runs again if it ran. One that runs on its old writable layer is left for when it
/// stops: that layer is in its rollback directory then.
async fn recover(ctx: &Context, report: Report<'_>) {
    use crate::api::recreate::Phase;
    let Ok(sd) = ctx.sd().await else { return };
    for name in ctx.store.rollbacks() {
        let progress = crate::api::recreate::progress(&ctx.store, &name);
        // What cannot be told now is decided at the next run: the rollback is what an
        // update promised to keep.
        let Ok(runs) = sd.machine_exists(&name).await else {
            continue;
        };
        if ctx.store.is_starting(&name) {
            continue;
        }
        if runs {
            // Running on its new image after the update itself went through: the
            // rollback only had its cleanup left. Anything else is put back once it
            // stops, a restore that failed included.
            let renewed = matches!(
                (ctx.store.load_image(&name), ctx.store.load_rollback(&name)),
                (Ok(Some(now)), Ok(Some((old, _)))) if now.manifest_digest != old.manifest_digest
            );
            if progress.phase == Phase::Saved || (progress.phase == Phase::Installed && renewed) {
                let _lock = ctx.store.lock().await;
                let _ = ctx.store.forget_rollback(&name);
            } else {
                note(
                    report,
                    format!("warning: {name} runs while an earlier update of it was cut short; it is put back once it stops"),
                );
            }
            continue;
        }
        let restored = async {
            let _lock = ctx.store.lock().await?;
            crate::api::recreate::restore(ctx, &name, report).await
        };
        match restored.await {
            Ok(()) => {
                note(
                    report,
                    format!(
                        "note: {name} was left halfway by an earlier update; its old image is back"
                    ),
                );
                if progress.running {
                    if let Err(e) =
                        crate::api::machines::start(ctx, &StartRequest::keep(&name), report).await
                    {
                        note(report, format!("warning: starting {name}: {e:#}"));
                    }
                }
            }
            Err(e) => note(
                report,
                format!("warning: {name} was left halfway by an earlier update: {e:#}"),
            ),
        }
    }
}

/// `recover` at the start of the service, whose end may have cut an update short.
pub async fn recover_at_start(ctx: &Context, report: Report<'_>) {
    let _one = RUNNING.lock().await;
    recover(ctx, report).await;
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
        assert_eq!(invalid_policy(&local), None);
        // podman's values: image is registry, disabled and empty are no updates.
        let image = record(serde_json::json!({ "labels": { PODMAN_LABEL: "image" } }));
        assert_eq!(
            (policy(&image), skip_reason(&image, &none)),
            (Some("registry"), None)
        );
        for off in ["disabled", ""] {
            assert_eq!(
                policy(&record(serde_json::json!({ "labels": { LABEL: off } }))),
                None
            );
        }
        // A misspelt one fails.
        let typo = record(serde_json::json!({ "labels": { LABEL: "regsitry" } }));
        assert!(invalid_policy(&typo)
            .unwrap()
            .contains("invalid auto-update policy"));
        assert!(skip_reason(&record(serde_json::json!({ "origin": "build" })), &none).is_some());
        // A machine of a built image, its build's record gone.
        assert!(skip_reason(
            &record(serde_json::json!({ "origin": "create", "built": true })),
            &none
        )
        .is_some());
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
        // What the image keeps in a VOLUME is in the writable layer unless a volume of the
        // machine holds it.
        let data = |volumes: serde_json::Value| {
            record(serde_json::json!({
                "run": { "volumes": ["/var/lib/postgresql/data"] },
                "volumes": volumes,
            }))
        };
        assert!(skip_reason(&data(serde_json::json!([])), &none)
            .unwrap()
            .contains("/var/lib/postgresql/data"));
        for target in [
            "/var/lib/postgresql/data",
            "/var/lib/postgresql",
            "/var/lib/postgresql/data/",
        ] {
            let covered = data(serde_json::json!([{ "source": "pg", "target": target }]));
            assert_eq!(skip_reason(&covered, &none), None, "{target}");
        }
        let elsewhere =
            data(serde_json::json!([{ "source": "pg", "target": "/var/lib/postgres" }]));
        assert!(skip_reason(&elsewhere, &none).is_some());
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
