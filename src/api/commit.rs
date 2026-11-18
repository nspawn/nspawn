//! commit: what a machine changed over its image, as a new image with one more layer,
//! the machine's settings on top of the image's and the changes asked for on top of
//! those. Volumes, binds and tmpfs are not part of the machine's tree, so not of the
//! layer; nor are the files nspawn and systemd-nspawn write into it for the machine.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::BufWriter;
use std::path::PathBuf;

use anyhow::{bail, Context as _, Result};
use oci_client::manifest::OciImageManifest;

use crate::api::{line, note, snapshot, Context, FailureKind, Report};
use crate::backend::Backend;
use crate::build::dispatch::change;
use crate::build::exec::{oci_media_type, timestamp_now, LAYER_MEDIA_TYPE};
use crate::build::image::{self, History};
use crate::build::layer::{self, DirTree, Tree};
use crate::oci::Mode;
use crate::reference::{validate_machine_name, ImageRef};
use crate::store::{ImageRecord, Ownership};

#[derive(Debug, Clone, Default)]
pub struct Request {
    pub machine: String,
    /// REPOSITORY[:TAG] of the new image.
    pub reference: String,
    pub author: Option<String>,
    pub message: Option<String>,
    /// Dockerfile instructions that change the image's settings.
    pub changes: Vec<String>,
    /// Freeze a running machine while its files are read.
    pub pause: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Committed {
    /// The digest of the new image's config.
    pub id: String,
    /// The local image it was installed as.
    pub name: String,
    pub reference: String,
}

/// The name and reference a commit, a load or an import installs `text` under:
/// REPOSITORY[:TAG], read as every other command reads an image name.
pub fn target(text: &str, registry: &str) -> Result<(String, String)> {
    let image = ImageRef::parse(text, registry)
        .map_err(|e| FailureKind::Invalid.error(format!("{e:#}")))?;
    if image.digest.is_some() {
        return Err(FailureKind::Invalid.error(format!(
            "{text} names a digest; an image made here is named by a tag"
        )));
    }
    let name = image.local_name();
    validate_machine_name(&name).map_err(|e| FailureKind::Invalid.error(format!("{e:#}")))?;
    Ok((name, image.to_string()))
}

/// The settings the machine runs with, on top of the image's: its environment, command,
/// user, working directory, stop signal, labels, healthcheck flags and the ports it
/// publishes. The labels that place a machine in a compose project stay with it: a
/// machine made from the image is none of the project's.
fn machine_settings(config: &mut image::Config, record: &ImageRecord) {
    let s = &mut config.config;
    for var in &record.env {
        let (key, value) = var.split_once('=').unwrap_or((var, ""));
        s.set_env(key, value);
    }
    if record.entrypoint.is_some() || record.cmd.is_some() {
        let entrypoint = record.effective_entrypoint();
        let cmd = record.effective_cmd();
        s.entrypoint = (!entrypoint.is_empty()).then(|| entrypoint.to_vec());
        s.cmd = (!cmd.is_empty()).then(|| cmd.to_vec());
    }
    if let Some(user) = &record.tuning.user {
        s.user.clone_from(user);
    }
    if let Some(dir) = &record.tuning.working_dir {
        s.working_dir.clone_from(dir);
    }
    if let Some(signal) = &record.tuning.stop_signal {
        s.stop_signal.clone_from(signal);
    }
    for (key, value) in &record.labels {
        if !crate::compose::MACHINE_LABELS.contains(&key.as_str()) {
            s.labels.insert(key.clone(), value.clone());
        }
    }
    if let Some(h) = &record.healthcheck {
        let nanos = |micros: u64| i64::try_from(micros.saturating_mul(1000)).unwrap_or(i64::MAX);
        s.healthcheck = Some(image::Healthcheck {
            test: h.test.clone(),
            interval: nanos(h.interval),
            timeout: nanos(h.timeout),
            start_period: nanos(h.start_period),
            start_interval: nanos(h.start_interval),
            retries: i64::from(h.retries),
        });
    }
    for port in &record.ports {
        s.exposed_ports.insert(
            format!("{}/{}", port.container, port.protocol.name()),
            serde_json::json!({}),
        );
    }
}

/// Applies `changes` to the image's settings. The mode the image takes: the machine's,
/// which its command alone need not say (a pull --mode), unless the changes give it
/// another command, which then says.
fn apply_changes(
    config: &mut image::Config,
    changes: &[String],
    mode: Mode,
) -> Result<Option<Mode>> {
    let command = (config.config.entrypoint.clone(), config.config.cmd.clone());
    change::apply(config, changes)?;
    let kept = command == (config.config.entrypoint.clone(), config.config.cmd.clone());
    Ok(kept.then_some(mode))
}

/// Commits machine `req.machine` as image `req.reference`.
pub async fn commit(ctx: &Context, req: &Request, report: Report<'_>) -> Result<Committed> {
    crate::api::require_root("commit")?;
    let (own, reference) = target(&req.reference, &ctx.config.registry)?;
    let machine = req.machine.as_str();
    let snap = snapshot::open(ctx, machine, "commit").await?;
    let store = &ctx.store;
    let sd = ctx.sd().await?;
    let manifest_bytes = store.load_manifest(machine)?;
    let manifest: OciImageManifest =
        serde_json::from_slice(&manifest_bytes).context("reading the machine's manifest")?;
    let config_path = store.blob_path(&manifest.config.digest);
    let base_bytes =
        fs::read(&config_path).with_context(|| format!("reading {}", config_path.display()))?;
    let mut config: image::Config =
        serde_json::from_slice(&base_bytes).context("reading the image's config")?;
    if config.rootfs.diff_ids.len() != manifest.layers.len() {
        anyhow::bail!(
            "the config of {machine}'s image lists {} layers, its manifest {}",
            config.rootfs.diff_ids.len(),
            manifest.layers.len()
        );
    }
    // The settings, the changes applied with the machine's environment: a change nspawn
    // cannot apply fails before anything is read.
    machine_settings(&mut config, &snap.record);
    let mode = apply_changes(&mut config, &req.changes, snap.record.mode)
        .map_err(|e| FailureKind::Invalid.error(format!("{e:#}")))?;
    let mut held: Vec<String> = manifest
        .layers
        .iter()
        .map(|l| l.digest.clone())
        .chain([manifest.config.digest.clone()])
        .collect();
    let hold = {
        let _still = store.holds_still()?;
        store.hold_blobs(&held)?
    };
    // The image's layers as root's, topmost first: an overlay machine's are its lowers;
    // a flat one's are extracted for the comparison.
    let mut lowers: Vec<PathBuf> = Vec::new();
    if snap.upper.is_some() {
        for l in &manifest.layers {
            let dir = store.layer_dir(&l.digest, Ownership::Root);
            if !dir.is_dir() {
                anyhow::bail!(
                    "layer {} of {machine}'s image is not in the store",
                    l.digest
                );
            }
            lowers.push(dir);
        }
    } else {
        line(report, format!("reading the layers of {machine}'s image"));
        let _lock = store.lock().await?;
        for (l, diff_id) in manifest.layers.iter().zip(&config.rootfs.diff_ids) {
            let (s, digest, media_type, diff_id) = (
                store.clone(),
                l.digest.clone(),
                l.media_type.clone(),
                diff_id.clone(),
            );
            let dir = tokio::task::spawn_blocking(move || {
                s.import_layer(
                    &digest,
                    &media_type,
                    &s.blob_path(&digest),
                    Ownership::Root,
                    Some(&diff_id),
                )
            })
            .await
            .context("extracting a layer")??;
            lowers.push(dir);
        }
    }
    lowers.reverse();

    let unit = format!("systemd-nspawn@{machine}.service");
    // Another commit's pause is no pause of the machine's: its thaw comes in the middle.
    let pausing = if snap.running && req.pause {
        match store.mark_pausing(machine)? {
            Some(mark) => Some(mark),
            None => bail!("another commit of {machine} has it paused; wait for it"),
        }
    } else {
        None
    };
    let frozen = snap.running && crate::api::machines::frozen(sd, machine).await;
    // A machine paused already stays so, and is none of this commit's to thaw.
    let pausing = pausing.filter(|_| !frozen);
    let pause_here = pausing.is_some();
    if pause_here {
        sd.freeze_unit(&unit)
            .await
            .with_context(|| format!("pausing {machine}"))?;
    }
    line(report, format!("committing {machine}"));
    let part = crate::api::load::PartFile(
        store
            .blobs_dir()
            .join(format!(".part-commit-{}", crate::store::unique_suffix())),
    );
    let written = part.0.clone();
    let lenient = snap.running && !pause_here && !frozen;
    let snapshot::Snapshot {
        record,
        tree,
        upper,
        ..
    } = snap;
    let kept = record.clone();
    // A tree chowned into a user namespace: its files lost their capabilities with it.
    let shifted = !matches!(
        tree.ids,
        crate::api::copy::Ids::Keep | crate::api::copy::Ids::Shift(0)
    );
    let diffed = tokio::task::spawn_blocking(move || -> Result<_> {
        // The upper directory too is read beneath a descriptor of it: a running machine
        // may put a link where it had a directory.
        let upper = match upper {
            Some(path) => Some(snapshot::RootTree::new(
                nix::fcntl::open(
                    &path,
                    nix::fcntl::OFlag::O_PATH
                        | nix::fcntl::OFlag::O_DIRECTORY
                        | nix::fcntl::OFlag::O_CLOEXEC,
                    nix::sys::stat::Mode::empty(),
                )
                .with_context(|| format!("opening {}", path.display()))?,
            )),
            None => None,
        };
        let trees: Vec<DirTree> = lowers.iter().map(DirTree::new).collect();
        let refs: Vec<&dyn Tree> = trees.iter().map(|t| t as &dyn Tree).collect();
        let changes = layer::machine_changes(
            upper.as_ref().map(|u| u as &dyn Tree),
            &tree,
            &refs,
            shifted,
            lenient,
        )?;
        let changes = snapshot::own_changes(changes, &tree, &kept);
        let file = fs::File::create(&written)
            .with_context(|| format!("creating {}", written.display()))?;
        layer::commit_changes(&tree, &changes, lenient, BufWriter::new(file))
    })
    .await
    .context("reading the machine's files")
    .and_then(|r| r);
    if pause_here {
        if let Err(e) = sd.thaw_unit(&unit).await {
            note(
                report,
                format!("warning: {machine} stays paused, as its unpause failed: {e:#}"),
            );
        }
    }
    drop(pausing);
    let (committed, summary, changed) =
        diffed.map_err(|e| e.context(format!("committing {machine}")))?;
    if !changed.is_empty() {
        note(
            report,
            format!(
                "note: changed while they were read (the ones gone left out): {}",
                changed.join(", ")
            ),
        );
    }
    let empty = summary.entries == 0;
    let created = timestamp_now();
    config.created = Some(created.clone());
    if let Some(author) = &req.author {
        config.author = Some(author.clone());
    }
    config.history.push(History {
        created: Some(created),
        created_by: record.effective_cmd().join(" "),
        author: req.author.clone(),
        comment: req.message.clone().unwrap_or_default(),
        empty_layer: empty,
    });
    let mut layers: Vec<(String, String, u64)> = manifest
        .layers
        .iter()
        .map(|l| {
            (
                l.digest.clone(),
                oci_media_type(&l.media_type),
                u64::try_from(l.size).unwrap_or(0),
            )
        })
        .collect();
    if !empty {
        config.rootfs.diff_ids.push(committed.diff_id.clone());
        layers.push((
            committed.digest.clone(),
            LAYER_MEDIA_TYPE.to_string(),
            committed.size,
        ));
    }
    let config_bytes = config.to_bytes();
    let id = crate::layout::sha256_digest(&config_bytes);
    let new_manifest = image::manifest(&config_bytes, &layers, &BTreeMap::new());
    {
        let _lock = store.lock().await?;
        held.extend([committed.digest.clone(), id.clone()]);
        hold.set(&held)?;
        let blob = store.blob_path(&committed.digest);
        if !empty && !blob.exists() {
            fs::rename(&part.0, &blob).with_context(|| format!("keeping {}", blob.display()))?;
        }
        let path = store.blob_path(&id);
        if !path.exists() {
            fs::write(&path, &config_bytes)
                .with_context(|| format!("writing {}", path.display()))?;
        }
    }
    let backend = Backend::choose(ctx.config.backend, sd).await?;
    let name = crate::install::install_made_here(
        store,
        sd,
        &ctx.config,
        backend,
        (&own, &reference),
        &new_manifest,
        "commit",
        mode,
        &BTreeSet::new(),
        report,
    )
    .await?;
    drop(hold);
    Ok(Committed {
        id,
        name,
        reference,
    })
}

/// Thaws the machines commits froze and an end of the service cut short, at the start
/// of the next service.
pub async fn thaw_left(ctx: &Context) {
    let names = ctx.store.left_paused();
    if names.is_empty() {
        return;
    }
    let sd = match ctx.sd().await {
        Ok(sd) => sd,
        Err(e) => {
            eprintln!("warning: unpausing the machines commits left paused: {e:#}");
            return;
        }
    };
    for name in names {
        if crate::api::machines::frozen(sd, &name).await {
            let unit = format!("systemd-nspawn@{name}.service");
            // Tried once: a pause made after this start is the user's.
            match sd.thaw_unit(&unit).await {
                Ok(()) => eprintln!("unpaused {name}, which a commit cut short left paused"),
                Err(e) => eprintln!(
                    "warning: unpausing {name}, which a commit cut short left paused: {e:#}"
                ),
            }
        }
        ctx.store.forget_paused(&name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_commit_is_named_by_a_tag() {
        let (name, reference) = target("web", "hub.nspawn.org").unwrap();
        assert_eq!(
            (name.as_str(), reference.as_str()),
            ("web", "hub.nspawn.org/web:latest")
        );
        let (name, reference) = target("docker.io/me/app:2", "hub.nspawn.org").unwrap();
        assert_eq!(
            (name.as_str(), reference.as_str()),
            ("me-app-2", "docker.io/me/app:2")
        );
        let digest = format!("web@sha256:{}", "a".repeat(64));
        let e = target(&digest, "hub.nspawn.org").unwrap_err();
        assert_eq!(crate::api::kind_of(&e), Some(FailureKind::Invalid));
    }

    #[test]
    fn the_image_keeps_the_machine_s_mode_unless_given_another_command() {
        let mut config = image::Config::scratch();
        config.config.entrypoint = Some(vec!["/entrypoint.sh".into()]);
        config.config.cmd = Some(vec!["/sbin/init".into()]);
        let changes = |list: &[&str]| list.iter().map(|c| c.to_string()).collect::<Vec<_>>();
        let mut c = config.clone();
        assert_eq!(
            apply_changes(&mut c, &[], Mode::Boot).unwrap(),
            Some(Mode::Boot)
        );
        let mut c = config.clone();
        let kept = apply_changes(&mut c, &changes(&["ENV A=1", "LABEL x=y"]), Mode::Boot);
        assert_eq!(kept.unwrap(), Some(Mode::Boot));
        for given in ["CMD [\"/app\"]", "ENTRYPOINT []"] {
            let mut c = config.clone();
            assert_eq!(
                apply_changes(&mut c, &changes(&[given]), Mode::Boot).unwrap(),
                None
            );
        }
        let mut c = config.clone();
        assert!(apply_changes(&mut c, &changes(&["RUN true"]), Mode::App).is_err());
    }

    #[test]
    fn the_machine_s_settings_go_on_top_of_the_image_s() {
        let mut config = image::Config::scratch();
        config.config.env = vec!["A=1".into(), "B=2".into()];
        config.config.cmd = Some(vec!["sh".into()]);
        let mut record: ImageRecord = serde_json::from_value(serde_json::json!({
            "name": "web", "reference": "hub.nspawn.org/web:latest",
            "manifest_digest": "sha256:0", "layers": [], "backend": "overlay", "created": 0,
            "env": ["B=3", "C=4"],
            "labels": {"x": "y", "com.docker.compose.project": "demo",
                       "com.docker.compose.config-hash": "h"},
            "cmd": ["serve", "--port", "80"],
            "ports": [{"host": 8080, "container": 80, "protocol": "tcp"}],
        }))
        .unwrap();
        record.tuning.user = Some("app".into());
        machine_settings(&mut config, &record);
        let s = &config.config;
        assert_eq!(s.env, ["A=1", "B=3", "C=4"]);
        assert_eq!(
            s.cmd.as_deref(),
            Some(&["serve".to_string(), "--port".into(), "80".into()][..])
        );
        assert_eq!(s.user, "app");
        assert_eq!(s.labels.get("x").map(String::as_str), Some("y"));
        assert_eq!(s.labels.len(), 1, "{:?}", s.labels);
        assert!(s.exposed_ports.contains_key("80/tcp"));
    }
}
