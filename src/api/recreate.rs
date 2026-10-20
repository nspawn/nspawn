//! A machine made anew from a newer image of its reference, with every option it had,
//! and put back as it was when the new one does not come up. What the machine wrote
//! outside its volumes goes with the old image; it is kept until the new one has proved
//! itself.

use std::time::{Duration, Instant};

use anyhow::{bail, Context as _, Result};
use oci_client::manifest::OciImageManifest;

use crate::api::machines::{StartOutcome, StartRequest, StopRequest};
use crate::api::pull::Fetched;
use crate::api::{line, note, Context, Report};
use crate::backend::{Backend, BackendChoice};
use crate::install::{install, remove_existing, stage_layers, Install};
use crate::store::{ImageRecord, Store};

/// The label compose compares with the digest of the image a service's machines come
/// from.
pub const COMPOSE_IMAGE: &str = "com.docker.compose.image";

/// The machine's record on the new image: the image's part (its layers, digests, run
/// config and signature) is the new one's, everything the machine was given, its
/// creation time among it, is the old one's.
pub fn renewed(old: &ImageRecord, installed: ImageRecord) -> ImageRecord {
    let mut record = ImageRecord {
        manifest_digest: installed.manifest_digest,
        index_digest: installed.index_digest,
        layers: installed.layers,
        backend: installed.backend,
        pid1: installed.pid1,
        run: installed.run,
        signed_by: installed.signed_by,
        signed_at: installed.signed_at,
        ..old.clone()
    };
    if record.labels.contains_key(COMPOSE_IMAGE) {
        record
            .labels
            .insert(COMPOSE_IMAGE.to_string(), record.manifest_digest.clone());
    }
    record
}

/// How the new machine came up, or why it did not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Up,
    Failed(String),
}

/// Where an update of a machine got to, kept in its rollback directory: what a run cut
/// short is undone from it, and nothing of the old machine is let go before it says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    /// The old record and manifest are kept; nothing else changed.
    Saved,
    /// The old writable layer is going, or has gone, into the rollback directory.
    Aside,
    /// The machine is on the new image.
    Installed,
    /// The old machine is being made again from the rollback: its writable layer is back
    /// once the directory holds none.
    Restoring,
    /// The new image failed its watch: the old machine comes back, whatever is found
    /// running meanwhile.
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Progress {
    pub phase: Phase,
    /// It ran before the update, and is started again whichever image it ends up on.
    pub running: bool,
    /// Its unit was enabled at boot.
    pub enabled: bool,
}

fn progress_path(store: &Store, name: &str) -> std::path::PathBuf {
    store.rollback_dir(name).join("progress.json")
}

fn write_progress(store: &Store, name: &str, progress: Progress) -> Result<()> {
    let path = progress_path(store, name);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(&progress)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))
}

/// The progress of an update of `name`, from what its rollback directory holds when it
/// says nothing.
pub fn progress(store: &Store, name: &str) -> Progress {
    std::fs::read(progress_path(store, name))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_else(|| Progress {
            phase: if has_layer_aside(store, name) {
                Phase::Aside
            } else {
                Phase::Saved
            },
            running: false,
            enabled: enabled_at_boot_now(name),
        })
}

fn has_layer_aside(store: &Store, name: &str) -> bool {
    let aside = store.rollback_dir(name);
    ["upper", "work"].iter().any(|d| aside.join(d).exists())
}

/// Whether the machine's unit is enabled at boot now, by nspawn or by hand.
fn enabled_at_boot_now(name: &str) -> bool {
    std::path::Path::new(&format!(
        "/etc/systemd/system/machines.target.wants/systemd-nspawn@{name}.service"
    ))
    .symlink_metadata()
    .is_ok()
}

/// Refuses while the machine runs, starts or its unit is busy: nothing of it is moved
/// under it. A start systemd made of its own meanwhile (a dependent's Wants=, a restart
/// policy, the boot) is stopped first: it waits in its hook, which refuses it while the
/// update is under way, and systemd would go on trying.
async fn ensure_down(store: &Store, sd: &crate::systemd::Systemd, name: &str) -> Result<()> {
    if store.is_starting(name) || sd.machine_exists(name).await? {
        bail!("{name} is running");
    }
    let unit = format!("systemd-nspawn@{name}.service");
    let state = sd.unit_status(&unit).await?;
    // A start waiting in systemd's queue for the machines it depends on too.
    if state.busy() || state.restarting() || sd.start_queued(&unit).await? {
        sd.stop_unit(&unit)
            .await
            .with_context(|| format!("stopping the start systemd made of {name}"))?;
        if sd.machine_exists(name).await? {
            bail!("{name} is running");
        }
    }
    if let Some(why) = crate::api::machines::unit_busy(sd, name).await? {
        bail!("{why}");
    }
    Ok(())
}

/// The mark of an update that failed and was undone: the machine is back on its old
/// image. Told by its type, not by the words of the error, which carry a probe's
/// output.
#[derive(Debug)]
pub struct RolledBack(pub String);

impl std::fmt::Display for RolledBack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RolledBack {}

/// What an update did to a machine.
pub struct Recreated {
    pub result: Result<()>,
    /// It was started again, on its new image or its old one: what takes its network
    /// follows it into the new namespace.
    pub restarted: bool,
}

/// Swaps the machine to the image `fetched` brought, starts it when it ran, and waits for
/// it to be up (healthy, when it has a healthcheck). On a failure the old machine is
/// put back and started again; the error says what failed, and whether the old one is
/// back.
/// `looked_at` is the record the run decided on: the machine is updated only while it is
/// still that one.
pub async fn recreate(
    ctx: &Context,
    looked_at: &ImageRecord,
    fetched: &Fetched,
    report: Report<'_>,
) -> Recreated {
    let mut restarted = false;
    let result = update(ctx, looked_at, fetched, report, &mut restarted).await;
    Recreated { result, restarted }
}

async fn update(
    ctx: &Context,
    looked_at: &ImageRecord,
    fetched: &Fetched,
    report: Report<'_>,
    restarted: &mut bool,
) -> Result<()> {
    let name = looked_at.name.as_str();
    let sd = ctx.sd().await?;
    let store = &ctx.store;
    let old = store
        .load_image(name)?
        .with_context(|| format!("no machine named {name}"))?;
    // A run watches each machine for minutes: one changed meanwhile, or another machine
    // of the name, is no longer what the image was looked up for.
    if serde_json::to_value(&old)? != serde_json::to_value(looked_at)? {
        bail!("it changed since this run looked at it; the next run updates it");
    }
    if old.backend != BackendChoice::Overlay {
        bail!("{name} is not an overlay machine; only those are updated");
    }
    let backend = Backend::choose(BackendChoice::Overlay, sd).await?;
    // The new image's layers come in before anything stops: a failure there leaves the
    // machine as it was. The swap stages them again, in case a gc took them meanwhile.
    {
        let _lock = store.lock().await?;
        stage_layers(store, backend, &fetched.manifest, Some(old.mode), name).await?;
    }
    let unit = format!("systemd-nspawn@{name}.service");
    let running = sd.machine_exists(name).await? || sd.unit_status(&unit).await?.restarting();
    if running {
        line(report, format!("{name}: stopping"));
        let stop = StopRequest {
            name: name.to_string(),
            force: false,
            wait: true,
            timeout: None,
        };
        crate::api::machines::stop(ctx, &stop, report).await?;
    }
    let enabled = enabled_at_boot_now(name);
    let mut back = true;
    let swapped = async {
        let _lock = store.lock().await?;
        // A dependent's start or a restart policy may have brought it back meanwhile.
        ensure_down(store, sd, name)
            .await
            .context("it started again while it was being updated")?;
        // Read before the stop and without the lock: what was changed meanwhile (an
        // update of its limits, a network connected) would be undone by the swap.
        let now = store
            .load_image(name)?
            .context("it went while it was being updated")?;
        if serde_json::to_value(&now)? != serde_json::to_value(&old)? {
            bail!("it changed while it was being updated; the next run updates it");
        }
        let manifest = store.load_manifest(name)?;
        let mut progress = Progress {
            phase: Phase::Saved,
            running,
            enabled,
        };
        // Nothing of the machine is moved yet: a rollback that cannot be kept whole is
        // let go, which the hook would refuse every start of it for.
        let saved = store.save_rollback(&old, &manifest).and_then(|()| {
            write_progress(store, name, progress)?;
            progress.phase = Phase::Aside;
            write_progress(store, name, progress)
        });
        if let Err(e) = saved {
            let _ = store.forget_rollback(name);
            return Err(e);
        }
        let swap = match keep_writable_layer(store, name) {
            Ok(()) => swap(ctx, &old, fetched, backend, &mut progress, report).await,
            Err(e) => Err(e),
        };
        if let Err(e) = swap {
            if let Err(again) = restore(ctx, name, report).await {
                back = false;
                return Err(e.context(format!(
                    "{name} could not be made anew, nor put back as it was ({again:#}); its old record and writable layer are in {}",
                    store.rollback_dir(name).display()
                )));
            }
            return Err(e.context(RolledBack(format!(
                "{name} could not be made anew; it is back as it was"
            ))));
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;
    if let Err(e) = swapped {
        if running && back {
            *restarted = true;
            if let Err(start) =
                crate::api::machines::start(ctx, &StartRequest::keep(name), report).await
            {
                note(report, format!("warning: starting {name} again: {start:#}"));
            }
        }
        return Err(e);
    }
    let verdict = if running {
        *restarted = true;
        start_and_watch(ctx, name, report).await
    } else {
        Verdict::Up
    };
    match verdict {
        Verdict::Up => {
            // The machine is on its new image whatever happens here.
            let cleaned = async {
                let _lock = store.lock().await?;
                tokio::task::block_in_place(|| {
                    store.forget_rollback(name)?;
                    store.gc_layers()?;
                    store.gc_blobs()
                })?;
                Ok::<(), anyhow::Error>(())
            };
            if let Err(e) = cleaned.await {
                note(
                    report,
                    format!("warning: {name} is updated; cleaning up after it: {e:#}"),
                );
            }
            Ok(())
        }
        Verdict::Failed(why) => {
            note(report, format!("{name}: {why}; putting the old image back"));
            // Said first: whatever stops this now, the old machine is what comes back.
            let mut rejected = progress(store, name);
            rejected.phase = Phase::Rejected;
            if let Err(e) = write_progress(store, name, rejected) {
                note(report, format!("warning: {e:#}"));
            }
            let force = StopRequest {
                name: name.to_string(),
                force: true,
                wait: true,
                timeout: Some(0),
            };
            let _ = crate::api::machines::stop(ctx, &force, report).await;
            {
                let _lock = store.lock().await?;
                restore(ctx, name, report).await.with_context(|| {
                    format!(
                        "{why}; the old image could not be put back; its old record and writable layer are in {}",
                        store.rollback_dir(name).display()
                    )
                })?;
            }
            let back =
                match crate::api::machines::start(ctx, &StartRequest::keep(name), report).await {
                    Ok(_) => "it runs on its old image again".to_string(),
                    Err(e) => format!("the old image did not start either: {e:#}"),
                };
            Err(anyhow::Error::new(RolledBack(format!(
                "{why}; rolled back: {back}"
            ))))
        }
    }
}

/// The old machine's upper and work directories, aside in its rollback directory: the
/// new one starts with a writable layer of its own.
fn keep_writable_layer(store: &Store, name: &str) -> Result<()> {
    let private = store.machines_private_dir().join(name);
    let aside = store.rollback_dir(name);
    for dir in ["upper", "work"] {
        let from = private.join(dir);
        let to = aside.join(dir);
        if to.exists() {
            // A previous attempt put it aside already.
            continue;
        }
        if from.exists() {
            std::fs::rename(&from, &to)
                .with_context(|| format!("moving {} to {}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

/// The machine on the new image, its record carrying the old one's options.
async fn swap(
    ctx: &Context,
    old: &ImageRecord,
    fetched: &Fetched,
    backend: Backend,
    progress: &mut Progress,
    report: Report<'_>,
) -> Result<()> {
    let (store, sd) = (&ctx.store, ctx.sd().await?);
    stage_layers(store, backend, &fetched.manifest, Some(old.mode), &old.name).await?;
    remove_existing(store, sd, &old.name, report).await?;
    let signed = fetched.signed.as_ref();
    install(
        store,
        sd,
        &ctx.config,
        backend,
        Install {
            name: &old.name,
            reference: &old.reference,
            manifest_bytes: &fetched.manifest_bytes,
            manifest: &fetched.manifest,
            manifest_digest: &fetched.manifest_digest,
            index_digest: fetched.index_digest.as_deref(),
            origin: &old.origin,
            mode: Some(old.mode),
            signed_by: signed.map(|s| s.signed_by.as_str()),
            signed_at: signed.and_then(|s| s.signed_at),
        },
        report,
    )
    .await?;
    let installed = store
        .load_image(&old.name)?
        .context("the record of the new machine is missing")?;
    let renewed = renewed(old, installed);
    store.record_image(&renewed)?;
    // install wrote an image's unit drop-in; the machine's policy, limits and
    // dependencies are what systemd needs for a start of its own (boot, a dependent).
    crate::api::machines::rewrite_hooks(ctx, sd, &renewed).await?;
    progress.phase = Phase::Installed;
    write_progress(store, &old.name, *progress)?;
    enable_if(ctx, &old.name, progress.enabled).await
}

/// The unit enabled at boot again when it was before the machine was made anew.
async fn enable_if(ctx: &Context, name: &str, enabled: bool) -> Result<()> {
    if enabled {
        let sd = ctx.sd().await?;
        if sd
            .enable_unit(&format!("systemd-nspawn@{name}.service"))
            .await?
        {
            sd.reload().await?;
        }
    }
    Ok(())
}

/// The old writable layer moved back from the rollback directory: over the one the new
/// machine has with `replace`, else only where there is none.
fn put_back(store: &Store, name: &str, replace: bool) -> Result<()> {
    let aside = store.rollback_dir(name);
    let private = store.machines_private_dir().join(name);
    // The upper directory last: the old machine's data, whose place says how far a put
    // back cut short got. Overlay's work directory is scratch.
    for dir in ["work", "upper"] {
        let from = aside.join(dir);
        if !from.exists() {
            continue;
        }
        let target = private.join(dir);
        if target.exists() {
            if !replace {
                bail!(
                    "{} and {} both exist; left as they are",
                    from.display(),
                    target.display()
                );
            }
            crate::store::remove_tree(&target)
                .with_context(|| format!("removing {}", target.display()))?;
        }
        std::fs::create_dir_all(&private)
            .with_context(|| format!("creating {}", private.display()))?;
        std::fs::rename(&from, &target)
            .with_context(|| format!("putting back {}", target.display()))?;
    }
    Ok(())
}

/// Puts back the machine the rollback directory keeps, as far as the update got: its
/// image, its record and its writable layer. The caller holds the store lock; a machine
/// that runs is left alone.
pub async fn restore(ctx: &Context, name: &str, report: Report<'_>) -> Result<()> {
    let (store, sd) = (&ctx.store, ctx.sd().await?);
    let Some((old, manifest_bytes)) = store.load_rollback(name)? else {
        if has_layer_aside(store, name) {
            bail!(
                "{} keeps a writable layer but no record; left as it is",
                store.rollback_dir(name).display()
            );
        }
        return store.forget_rollback(name);
    };
    ensure_down(store, sd, name).await?;
    let progress = progress(store, name);
    let aside = store.rollback_dir(name);
    let private = store.machines_private_dir().join(name);
    let untouched = private.exists()
        && store
            .load_image(name)?
            .is_some_and(|c| c.manifest_digest == old.manifest_digest && c.layers == old.layers);
    match progress.phase {
        Phase::Saved => {}
        // The old machine is all there but for its writable layer.
        Phase::Aside if untouched => put_back(store, name, false)?,
        // A restore cut short after its writable layer went back: the machine is the
        // old one but for its record and unit; making it again would take the layer.
        Phase::Restoring if !aside.join("upper").exists() && private.exists() => {
            let work = aside.join("work");
            if work.exists() {
                crate::store::remove_tree(&work)
                    .with_context(|| format!("removing {}", work.display()))?;
            }
            store.record_image(&old)?;
            crate::api::machines::rewrite_hooks(ctx, sd, &old).await?;
            enable_if(ctx, name, progress.enabled).await?;
        }
        _ => {
            let manifest: OciImageManifest = serde_json::from_slice(&manifest_bytes)
                .with_context(|| format!("reading the manifest kept for {name}"))?;
            let backend = Backend::choose(BackendChoice::Overlay, sd).await?;
            stage_layers(store, backend, &manifest, Some(old.mode), name).await?;
            write_progress(
                store,
                name,
                Progress {
                    phase: Phase::Restoring,
                    ..progress
                },
            )?;
            remove_existing(store, sd, name, report).await?;
            install(
                store,
                sd,
                &ctx.config,
                backend,
                Install {
                    name,
                    reference: &old.reference,
                    manifest_bytes: &manifest_bytes,
                    manifest: &manifest,
                    manifest_digest: &old.manifest_digest,
                    index_digest: old.index_digest.as_deref(),
                    origin: &old.origin,
                    mode: Some(old.mode),
                    signed_by: old.signed_by.as_deref(),
                    signed_at: old.signed_at,
                },
                report,
            )
            .await?;
            store.record_image(&old)?;
            put_back(store, name, true)?;
            crate::api::machines::rewrite_hooks(ctx, sd, &old).await?;
            enable_if(ctx, name, progress.enabled).await?;
        }
    }
    store.forget_rollback(name)
}

/// Starts the machine and waits until it is up: healthy, when it has a healthcheck, or
/// still running a moment after its start otherwise.
async fn start_and_watch(ctx: &Context, name: &str, report: Report<'_>) -> Verdict {
    line(report, format!("{name}: starting on the new image"));
    // systemd may have started it of its own once the swap let go of the lock (a
    // dependent's Wants=): that start is on the new image as well, and is watched.
    let unit = format!("systemd-nspawn@{name}.service");
    let started_by_systemd = match ctx.sd().await {
        Ok(sd) => {
            sd.machine_exists(name).await.unwrap_or(false)
                || sd.unit_status(&unit).await.is_ok_and(|s| s.busy())
        }
        Err(_) => false,
    };
    if !started_by_systemd {
        match crate::api::machines::start(ctx, &StartRequest::keep(name), report).await {
            Ok(StartOutcome::Started) => {}
            Ok(StartOutcome::Ended) => return Verdict::Failed(ended(ctx, name).await),
            // Its policy brings it back, which says nothing of whether it works.
            Ok(StartOutcome::Restarting) => return Verdict::Failed(ended(ctx, name).await),
            Err(e) => return Verdict::Failed(format!("{e:#}")),
        }
    }
    let record = match ctx.store.load_image(name) {
        Ok(Some(r)) => r,
        _ => return Verdict::Failed(format!("the record of {name} is gone")),
    };
    let healthcheck = record
        .effective_healthcheck()
        .filter(|h| h.active())
        .cloned();
    let deadline = Instant::now() + watch_time(healthcheck.as_ref());
    loop {
        let summary = match crate::api::machines::get(ctx, name).await {
            Ok(s) => s,
            Err(e) => return Verdict::Failed(format!("{e:#}")),
        };
        // A start systemd made is still on its way for a while.
        if summary.state == "starting" && Instant::now() <= deadline {
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        }
        if summary.state != "running" && summary.state != "opening" {
            return Verdict::Failed(ended(ctx, name).await);
        }
        match (
            &healthcheck,
            summary.health.as_ref().map(|h| h.status.as_str()),
        ) {
            (Some(_), Some("healthy")) => {
                line(report, format!("{name}: healthy"));
                return Verdict::Up;
            }
            (Some(_), Some("unhealthy")) => {
                let output = summary
                    .health
                    .as_ref()
                    .and_then(|h| h.log.last())
                    .map(|p| p.output.trim().to_string())
                    .unwrap_or_default();
                return Verdict::Failed(format!("{name} is unhealthy: {output}"));
            }
            (None, _) if Instant::now() > deadline => {
                line(report, format!("{name}: running"));
                return Verdict::Up;
            }
            _ => {}
        }
        if Instant::now() > deadline {
            return Verdict::Failed(format!(
                "{name} did not become healthy in {} seconds",
                watch_time(healthcheck.as_ref()).as_secs()
            ));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// How long a new machine has to show it is up: its healthcheck's start period and as
/// many probes as make it unhealthy, ten minutes at most; ten seconds of running without
/// one.
pub fn watch_time(healthcheck: Option<&crate::health::Healthcheck>) -> Duration {
    let Some(h) = healthcheck else {
        return Duration::from_secs(10);
    };
    let or = |usec: u64, default: u64| if usec == 0 { default } else { usec };
    let interval = or(h.interval, 30_000_000);
    let timeout = or(h.timeout, 30_000_000);
    let retries = u64::from(if h.retries == 0 { 3 } else { h.retries });
    // The values are the image's, as large as it likes.
    let usec = interval
        .saturating_add(timeout)
        .saturating_mul(retries.saturating_add(1))
        .saturating_add(h.start_period);
    Duration::from_micros(usec.min(600_000_000))
}

async fn ended(ctx: &Context, name: &str) -> String {
    let code = match ctx.sd().await {
        Ok(sd) => match ctx.store.load_image(name) {
            Ok(Some(r)) => crate::api::machines::last_exit_code(sd, &ctx.store, &r).await,
            _ => None,
        },
        Err(_) => None,
    };
    match code {
        Some(code) => {
            format!("{name} exited with code {code} (journalctl -u systemd-nspawn@{name}.service)")
        }
        None => format!("{name} ended (journalctl -u systemd-nspawn@{name}.service)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(json: serde_json::Value) -> ImageRecord {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn a_renewed_machine_keeps_what_it_was_given() {
        let old = record(serde_json::json!({
            "name": "web", "reference": "docker.io/library/nginx:1", "manifest_digest": "sha256:old",
            "index_digest": "sha256:oldindex", "layers": ["sha256:a"], "backend": "overlay", "created": 100,
            "origin": "pull", "mode": "app", "run": { "cmd": ["nginx"], "env": ["V=1"] },
            "network": "bridge", "network_name": "front", "address": "10.99.1.5",
            "ports": [{ "host": 8080, "container": 80, "protocol": "tcp" }],
            "env": ["A=b"], "volumes": [{ "source": "data", "target": "/data" }],
            "labels": { "org.nspawn.auto-update": "registry", "com.docker.compose.image": "sha256:old" },
            "restart": "always", "limits": { "memory": 1048576000 },
            "tuning": { "hostname": "h", "cap_add": ["NET_ADMIN"] },
            "healthcheck": { "test": ["CMD-SHELL", "true"] },
        }));
        let installed = record(serde_json::json!({
            "name": "web", "reference": "docker.io/library/nginx:1", "manifest_digest": "sha256:new",
            "index_digest": "sha256:newindex", "layers": ["sha256:b"], "backend": "overlay", "created": 999,
            "origin": "pull", "mode": "app", "run": { "cmd": ["nginx", "-g"], "env": ["V=2"] },
        }));
        let new = renewed(&old, installed);
        assert_eq!(new.manifest_digest, "sha256:new");
        assert_eq!(new.index_digest.as_deref(), Some("sha256:newindex"));
        assert_eq!(new.layers, ["sha256:b"]);
        assert_eq!(new.run.env, ["V=2"]);
        assert_eq!(new.created, 100, "the container ID stays");
        assert_eq!(new.labels["com.docker.compose.image"], "sha256:new");
        // Everything the machine was given is what generate prints of it, the image's
        // label for compose aside.
        let flags = |r: &ImageRecord| crate::api::generate::flags(r);
        let mut same = new.clone();
        same.labels
            .insert(COMPOSE_IMAGE.to_string(), "sha256:old".to_string());
        assert_eq!(flags(&same), flags(&old));
        assert_eq!(new.address, old.address);
        assert_eq!(new.restart, old.restart);
        assert_eq!(new.ports, old.ports);
    }

    #[test]
    fn an_update_cut_short_is_undone_from_where_it_got() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(&dir.path().join("m"), &dir.path().join("s"));
        let aside = store.rollback_dir("web");
        std::fs::create_dir_all(&aside).unwrap();
        assert_eq!(progress(&store, "web").phase, Phase::Saved);
        std::fs::create_dir_all(aside.join("upper")).unwrap();
        std::fs::write(aside.join("upper").join("data"), "old").unwrap();
        // Only the layer says where it got.
        assert_eq!(progress(&store, "web").phase, Phase::Aside);
        let written = Progress {
            phase: Phase::Installed,
            running: true,
            enabled: false,
        };
        write_progress(&store, "web", written).unwrap();
        assert_eq!(progress(&store, "web"), written);
        // The old layer goes back only over nothing, unless it replaces the new one's.
        let private = store.machines_private_dir().join("web");
        std::fs::create_dir_all(private.join("upper")).unwrap();
        std::fs::write(private.join("upper").join("data"), "new").unwrap();
        assert!(put_back(&store, "web", false).is_err());
        assert!(
            aside.join("upper").exists(),
            "a refused put back moves nothing"
        );
        put_back(&store, "web", true).unwrap();
        assert_eq!(
            std::fs::read_to_string(private.join("upper").join("data")).unwrap(),
            "old"
        );
        assert!(!aside.join("upper").exists());
        // A restore is a phase of its own, and once the layer went back nothing is aside.
        let restoring = Progress {
            phase: Phase::Restoring,
            ..written
        };
        write_progress(&store, "web", restoring).unwrap();
        assert_eq!(progress(&store, "web").phase, Phase::Restoring);
        assert!(!has_layer_aside(&store, "web"));
        // A new image that failed its watch says so before anything else happens.
        let rejected = Progress {
            phase: Phase::Rejected,
            ..written
        };
        write_progress(&store, "web", rejected).unwrap();
        assert_eq!(progress(&store, "web"), rejected);
        // A name with a rollback kept is not made anew without --force.
        let refused = crate::install::ensure_no_pending_rollback(&store, "web", false);
        assert!(refused
            .unwrap_err()
            .to_string()
            .contains("--force lets it go"));
        crate::install::ensure_no_pending_rollback(&store, "web", true).unwrap();
        crate::install::ensure_no_pending_rollback(&store, "other", false).unwrap();
        // Let go of, it is gone from the list at once, the deletion after.
        store.forget_rollback("web").unwrap();
        assert!(store.rollbacks().is_empty());
        let left = store.rollback_dir("web").with_file_name(".gone-web-1.1");
        std::fs::create_dir_all(left.join("upper")).unwrap();
        assert!(
            store.rollbacks().is_empty(),
            "what is being let go is no rollback"
        );
        store.sweep_trash();
        assert!(!left.exists());
    }

    #[test]
    fn a_rollback_is_told_by_its_type() {
        let back = anyhow::anyhow!("unhealthy").context(RolledBack("web is back".into()));
        assert!(back.downcast_ref::<RolledBack>().is_some());
        let plain = anyhow::anyhow!("web is unhealthy: rolled back by hand");
        assert!(plain.downcast_ref::<RolledBack>().is_none());
        let new = anyhow::Error::new(RolledBack("why; rolled back: it runs".into()));
        assert!(new.downcast_ref::<RolledBack>().is_some());
    }

    #[test]
    fn time_to_prove_itself() {
        assert_eq!(watch_time(None), Duration::from_secs(10));
        let huge = crate::health::Healthcheck {
            test: vec!["CMD".into(), "true".into()],
            interval: u64::MAX,
            start_period: u64::MAX,
            retries: u32::MAX,
            ..Default::default()
        };
        assert_eq!(watch_time(Some(&huge)), Duration::from_secs(600));
        let quick = crate::health::Healthcheck {
            test: vec!["CMD".into(), "true".into()],
            interval: 2_000_000,
            timeout: 1_000_000,
            start_period: 5_000_000,
            start_interval: 0,
            retries: 3,
        };
        assert_eq!(watch_time(Some(&quick)), Duration::from_secs(5 + 3 * 4));
        let defaults = crate::health::Healthcheck {
            test: vec!["CMD".into(), "true".into()],
            interval: 0,
            timeout: 0,
            start_period: 0,
            start_interval: 0,
            retries: 0,
        };
        assert_eq!(watch_time(Some(&defaults)), Duration::from_secs(60 * 4));
        let slow = crate::health::Healthcheck {
            start_period: 3_600_000_000,
            ..defaults
        };
        assert_eq!(watch_time(Some(&slow)), Duration::from_secs(600));
    }
}
