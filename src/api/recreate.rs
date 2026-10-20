//! A machine made anew from a newer image of its reference, with every option it had,
//! and put back as it was when the new one does not come up. What the machine wrote
//! outside its volumes goes with the old image, as when docker recreates a container;
//! it is kept until the new one has proved itself.

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

/// Swaps the machine to the image `fetched` brought, starts it when it ran, and waits for
/// it to be up (healthy, when it has a healthcheck). On a failure the old machine is
/// put back and started again; the error says what failed, and whether the old one is
/// back.
pub async fn recreate(
    ctx: &Context,
    name: &str,
    fetched: &Fetched,
    report: Report<'_>,
) -> Result<()> {
    let sd = ctx.sd().await?;
    let store = &ctx.store;
    let old = store
        .load_image(name)?
        .with_context(|| format!("no machine named {name}"))?;
    if old.backend != BackendChoice::Overlay {
        bail!("{name} is not an overlay machine; only those are updated");
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
    {
        let _lock = store.lock().await?;
        let manifest = store.load_manifest(name)?;
        store.save_rollback(&old, &manifest)?;
        keep_writable_layer(store, name)?;
        let backend = Backend::choose(BackendChoice::Overlay, sd).await?;
        if let Err(e) = swap(ctx, &old, fetched, backend, report).await {
            restore(ctx, name, report).await?;
            return Err(e.context(format!(
                "{name} could not be made anew; it is back as it was"
            )));
        }
    }
    let verdict = if running {
        start_and_watch(ctx, name, report).await
    } else {
        Verdict::Up
    };
    match verdict {
        Verdict::Up => {
            let _lock = store.lock().await?;
            store.forget_rollback(name)?;
            store.gc_layers()?;
            store.gc_blobs()?;
            Ok(())
        }
        Verdict::Failed(why) => {
            note(report, format!("{name}: {why}; putting the old image back"));
            let force = StopRequest {
                name: name.to_string(),
                force: true,
                wait: true,
                timeout: Some(0),
            };
            let _ = crate::api::machines::stop(ctx, &force, report).await;
            {
                let _lock = store.lock().await?;
                restore(ctx, name, report).await?;
            }
            let back =
                match crate::api::machines::start(ctx, &StartRequest::keep(name), report).await {
                    Ok(_) => "it runs on its old image again".to_string(),
                    Err(e) => format!("the old image did not start either: {e:#}"),
                };
            bail!("{why}; rolled back: {back}")
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
    store.record_image(&renewed(old, installed))?;
    enable_at_boot(ctx, old).await
}

async fn enable_at_boot(ctx: &Context, record: &ImageRecord) -> Result<()> {
    if record.restart.enabled_at_boot() {
        let sd = ctx.sd().await?;
        if sd
            .enable_unit(&format!("systemd-nspawn@{}.service", record.name))
            .await?
        {
            sd.reload().await?;
        }
    }
    Ok(())
}

/// Puts back the machine the rollback directory keeps: its image, its record and its
/// writable layer. The caller holds the store lock and the machine is not running.
pub async fn restore(ctx: &Context, name: &str, report: Report<'_>) -> Result<()> {
    let (store, sd) = (&ctx.store, ctx.sd().await?);
    let Some((old, manifest_bytes)) = store.load_rollback(name)? else {
        return Ok(());
    };
    let manifest: OciImageManifest = serde_json::from_slice(&manifest_bytes)
        .with_context(|| format!("reading the manifest kept for {name}"))?;
    let backend = Backend::choose(BackendChoice::Overlay, sd).await?;
    stage_layers(store, backend, &manifest, Some(old.mode), name).await?;
    // The new machine goes, its writable layer with it, unless nothing replaced the old
    // one yet.
    let aside = store.rollback_dir(name);
    let private = store.machines_private_dir().join(name);
    let mut kept = Vec::new();
    for dir in ["upper", "work"] {
        let tmp = store
            .machines_private_dir()
            .join(format!(".rollback-{name}-{dir}"));
        let _ = std::fs::remove_dir_all(&tmp);
        if aside.join(dir).exists() {
            std::fs::rename(aside.join(dir), &tmp)
                .with_context(|| format!("keeping {}", aside.join(dir).display()))?;
            kept.push((dir, tmp));
        }
    }
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
    for (dir, tmp) in kept {
        let target = private.join(dir);
        let _ = std::fs::remove_dir_all(&target);
        std::fs::rename(&tmp, &target)
            .with_context(|| format!("putting back {}", target.display()))?;
    }
    store.record_image(&old)?;
    store.forget_rollback(name)?;
    enable_at_boot(ctx, &old).await
}

/// Starts the machine and waits until it is up: healthy, when it has a healthcheck, or
/// still running a moment after its start otherwise.
async fn start_and_watch(ctx: &Context, name: &str, report: Report<'_>) -> Verdict {
    line(report, format!("{name}: starting on the new image"));
    match crate::api::machines::start(ctx, &StartRequest::keep(name), report).await {
        Ok(StartOutcome::Started) => {}
        Ok(StartOutcome::Ended) => return Verdict::Failed(ended(ctx, name).await),
        // Its policy brings it back, which says nothing of whether it works.
        Ok(StartOutcome::Restarting) => return Verdict::Failed(ended(ctx, name).await),
        Err(e) => return Verdict::Failed(format!("{e:#}")),
    }
    let record = match ctx.store.load_image(name) {
        Ok(Some(r)) => r,
        _ => return Verdict::Failed(format!("the record of {name} is gone")),
    };
    let healthcheck = record
        .effective_healthcheck()
        .filter(|h| h.test.first().map(String::as_str) != Some("NONE"))
        .cloned();
    let deadline = Instant::now() + watch_time(healthcheck.as_ref());
    loop {
        let summary = match crate::api::machines::get(ctx, name).await {
            Ok(s) => s,
            Err(e) => return Verdict::Failed(format!("{e:#}")),
        };
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
    let usec = h.start_period + (interval + timeout) * (retries + 1);
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
    fn time_to_prove_itself() {
        assert_eq!(watch_time(None), Duration::from_secs(10));
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
