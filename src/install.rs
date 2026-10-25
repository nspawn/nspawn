//! Turns blobs in the store into a usable local image: assembly with a backend, the
//! settings file nspawn boots it with, and the record that `images ls`, `images rm`, `start`
//! and `push` rely on.

use std::collections::BTreeMap;
use std::fs;

use anyhow::{bail, Context, Result};
use oci_client::manifest::OciImageManifest;

use crate::api::{note, Report};
use crate::backend::{Assembler, Backend, BackendChoice, Layer};
use crate::config::Config;
use crate::oci::{detect_mode, has_init, needs_pid1, Mode, RunSpec};
use crate::settings::{self, MachineSettings, Network};
use crate::store::{now_unix, ImageRecord, Store};
use crate::systemd::Systemd;

pub struct Install<'a> {
    pub name: &'a str,
    pub reference: &'a str,
    pub manifest_bytes: &'a [u8],
    pub manifest: &'a OciImageManifest,
    pub manifest_digest: &'a str,
    /// The index the reference named, for a pull of a tag that names one.
    pub index_digest: Option<&'a str>,
    pub origin: &'a str,
    /// Force boot or app instead of detecting it from the image.
    pub mode: Option<Mode>,
    /// Who signed the image, when its signature was verified, and when it was made.
    pub signed_by: Option<&'a str>,
    pub signed_at: Option<u64>,
}

/// The backend an image of `manifest` is assembled with: `backend`, unless it is mstack
/// and the image's command says it runs a program rather than an init system, in which
/// case overlay, since mstack machines cannot join the bridge network.
fn assembled_with(
    store: &Store,
    backend: Backend,
    manifest: &OciImageManifest,
    mode: Option<Mode>,
    report: Report<'_>,
    name: &str,
) -> Result<(Backend, RunSpec)> {
    let config_path = store.blob_path(&manifest.config.digest);
    let config_blob =
        fs::read(&config_path).with_context(|| format!("reading {}", config_path.display()))?;
    let run = RunSpec::from_config(&config_blob)?;
    let surely_app =
        mode == Some(Mode::App) || (mode.is_none() && detect_mode(true, &run.argv()) == Mode::App);
    if backend == Backend::Mstack && surely_app {
        note(report, format!("note: {name} runs a program rather than an init system; assembling it as overlay, since mstack machines cannot join the bridge network"));
        return Ok((Backend::Overlay, run));
    }
    Ok((backend, run))
}

/// Extracts the layers of `manifest` into the store ahead of `install`, where the same
/// extraction then finds them done: what can fail in it (a media type nobody knows, a
/// hostile archive, a full disk) fails before the machine being replaced is removed. A
/// flat machine's are applied in its staging directory, which install moves into place.
pub async fn stage_layers(
    store: &Store,
    backend: Backend,
    manifest: &OciImageManifest,
    mode: Option<Mode>,
    name: &str,
) -> Result<()> {
    // install says its notes; this pass keeps quiet.
    let (backend, _) = assembled_with(store, backend, manifest, mode, &|_| {}, name)?;
    let diff_ids = diff_ids(store, manifest)?;
    if backend == Backend::Flat {
        let staging = store.flat_staging_dir(name);
        if fs::symlink_metadata(&staging).is_ok() {
            crate::store::remove_tree(&staging)
                .with_context(|| format!("removing {}", staging.display()))?;
        }
        fs::create_dir_all(&staging).with_context(|| format!("creating {}", staging.display()))?;
        for (layer, diff_id) in manifest.layers.iter().zip(diff_ids) {
            let (blob, media_type, digest, target) = (
                store.blob_path(&layer.digest),
                layer.media_type.clone(),
                layer.digest.clone(),
                staging.clone(),
            );
            let applied = tokio::task::spawn_blocking(move || {
                crate::store::extract_layer_checked(
                    &blob,
                    &media_type,
                    &target,
                    crate::store::WhiteoutMode::Apply,
                    crate::store::Ownership::Root,
                    Some(&diff_id),
                )
                .with_context(|| format!("extracting layer {digest}"))
            })
            .await
            .context("extracting a layer")
            .and_then(|r| r);
            if let Err(e) = applied {
                let _ = crate::store::remove_tree(&staging);
                return Err(e);
            }
        }
        return Ok(());
    }
    for (layer, diff_id) in manifest.layers.iter().zip(diff_ids) {
        let store = store.clone();
        let (digest, media_type, blob, ownership) = (
            layer.digest.clone(),
            layer.media_type.clone(),
            store.blob_path(&layer.digest),
            backend.ownership(),
        );
        tokio::task::spawn_blocking(move || {
            store.import_layer(&digest, &media_type, &blob, ownership, Some(&diff_id))
        })
        .await
        .context("extracting a layer")??;
    }
    Ok(())
}

/// The digests the image's config gives its layers' archives, one per layer of the
/// manifest: an image whose two lists differ is refused, as its layers cannot be checked.
pub fn diff_ids(store: &Store, manifest: &OciImageManifest) -> Result<Vec<String>> {
    #[derive(serde::Deserialize, Default)]
    struct Rootfs {
        #[serde(default)]
        diff_ids: Vec<String>,
    }
    #[derive(serde::Deserialize)]
    struct Config {
        #[serde(default)]
        rootfs: Rootfs,
    }
    let path = store.blob_path(&manifest.config.digest);
    let blob = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let config: Config =
        serde_json::from_slice(&blob).with_context(|| format!("reading {}", path.display()))?;
    let ids = config.rootfs.diff_ids;
    if ids.len() != manifest.layers.len() {
        bail!(
            "the image's config names {} layer archives, and its manifest {} layers",
            ids.len(),
            manifest.layers.len()
        );
    }
    Ok(ids)
}

/// All blobs named in the manifest (layers and config) must already be in the store.
pub async fn install(
    store: &Store,
    sd: &Systemd,
    config: &Config,
    backend: Backend,
    spec: Install<'_>,
    report: Report<'_>,
) -> Result<Mode> {
    let layers: Vec<Layer> = spec
        .manifest
        .layers
        .iter()
        .zip(diff_ids(store, spec.manifest)?)
        .map(|(d, diff_id)| Layer {
            digest: d.digest.clone(),
            media_type: d.media_type.clone(),
            blob: store.blob_path(&d.digest),
            diff_id: Some(diff_id),
        })
        .collect();
    // An mstack image runs with managed user namespaces, which cannot join the network
    // namespace prepared for app machines on the bridge; apps get overlay instead. Whether
    // the image is an app is known before extraction when its command says so.
    let (backend, run) =
        assembled_with(store, backend, spec.manifest, spec.mode, report, spec.name)?;
    let assembler = Assembler { store, sd };
    let mut backend = backend;
    let trees = assembler.assemble(backend, spec.name, &layers).await?;
    let mode = spec
        .mode
        .unwrap_or_else(|| detect_mode(has_init(&trees), &run.argv()));
    let pid1 = mode == Mode::App && needs_pid1(&trees);
    if mode == Mode::App && backend == Backend::Mstack {
        // Only known now, without a command in the config: no init inside.
        note(
            report,
            format!(
                "note: {} has no init system; reassembling it as overlay, since mstack machines cannot join the bridge network",
                spec.name
            ),
        );
        assembler.remove(spec.name, BackendChoice::Mstack).await?;
        backend = Backend::Overlay;
        assembler.assemble(backend, spec.name, &layers).await?;
    }
    // Both kinds join the bridge; --network host is one flag away.
    let network = Network::Bridge;
    let route = settings::namespace_route(sd, spec.name, mode, true).await?;
    settings::write(
        &MachineSettings {
            name: spec.name,
            managed_userns: backend == Backend::Mstack,
            mode,
            pid1,
            run: &run,
            command: &run.argv(),
            extra_env: &[],
            binds: &[],
            volume_units: None,
            network,
            no_network: false,
            hostname_file: None,
            bridge: None,
            tuning: &Default::default(),
        },
        &route,
    )?;
    // The unit hooks exist from now on, so that machinectl start or an enabled unit gets
    // the same preparation as nspawn start.
    let app_argv = settings::wrapped_argv(sd, spec.name, mode, &route, &Default::default()).await?;
    if settings::write_hooks(
        spec.name,
        config,
        &route,
        app_argv.as_deref(),
        &settings::HookSpec {
            restart: crate::policy::Restart::No,
            limits: &crate::policy::Limits::default(),
            remove_on_exit: false,
            tuning: &Default::default(),
            api_socket: false,
        },
    )? {
        sd.reload().await?;
    }
    store.save_manifest(spec.name, spec.manifest_bytes)?;
    store.record_image(&ImageRecord {
        name: spec.name.to_string(),
        reference: spec.reference.to_string(),
        manifest_digest: spec.manifest_digest.to_string(),
        index_digest: spec.index_digest.map(str::to_string),
        layers: spec
            .manifest
            .layers
            .iter()
            .map(|l| l.digest.clone())
            .collect(),
        backend: backend.as_choice(),
        created: now_unix(),
        origin: spec.origin.to_string(),
        mode,
        pid1,
        started: None,
        fixed_addresses: Default::default(),
        built: false,
        run,
        network,
        network_name: None,
        address: None,
        ports: Vec::new(),
        entrypoint: None,
        cmd: None,
        env: Vec::new(),
        volumes: Vec::new(),
        labels: BTreeMap::new(),
        restart: Default::default(),
        limits: Default::default(),
        remove_on_exit: false,
        extra_networks: Vec::new(),
        aliases: BTreeMap::new(),
        no_network: false,
        network_container: None,
        healthcheck: None,
        tuning: Default::default(),
        signed_by: spec.signed_by.map(str::to_string),
        signed_at: spec.signed_at,
    })?;
    Ok(mode)
}

/// Refuses early when an image with this name exists and `force` is not set, or when
/// its machine is running. The removal itself waits until the replacement is at hand.
pub async fn ensure_replaceable(
    store: &Store,
    sd: &Systemd,
    name: &str,
    force: bool,
) -> Result<()> {
    ensure_no_pending_rollback(store, name, force)?;
    let existing_record = store.load_image(name)?;
    let existing_image = sd.list_images().await?.into_iter().any(|i| i.name == name);
    if existing_record.is_none() && !existing_image {
        return Ok(());
    }
    if !force {
        anyhow::bail!(
            "image {name} already exists; use --force to replace it or --name for another name"
        );
    }
    if store.is_starting(name) {
        anyhow::bail!("machine {name} is starting; wait for it or stop it first");
    }
    if sd.machine_exists(name).await? {
        anyhow::bail!("machine {name} is running; stop it before replacing its image");
    }
    if let Some(why) = crate::api::machines::unit_busy(sd, name).await? {
        anyhow::bail!("{why}");
    }
    Ok(())
}

/// Refuses a name an update cut short left a rollback of, its old machine's record and
/// writable layer, unless `force` says to let them go: whatever is made under the name
/// takes their place, the machine having no record meanwhile.
pub fn ensure_no_pending_rollback(store: &Store, name: &str, force: bool) -> Result<()> {
    let dir = store.rollback_dir(name);
    if !force && dir.exists() {
        anyhow::bail!(
            "an update of {name} was cut short, and keeps its old machine in {}: nspawn auto-update puts it back, --force lets it go",
            dir.display()
        );
    }
    Ok(())
}

/// Takes the unit of a machine that is gone off the boot list: a link left behind would
/// start a unit with nothing to run at every boot. Best effort, said in a note: the
/// removal is done by then, and machined allows image names systemd refuses as units.
/// Whether there was a link to take.
pub async fn take_off_boot(sd: &Systemd, name: &str, report: Report<'_>) -> bool {
    match sd
        .disable_unit(&format!("systemd-nspawn@{name}.service"))
        .await
    {
        Ok(false) => false,
        Ok(true) => {
            if let Err(e) = sd.reload().await {
                note(report, format!("note: {e:#}"));
            }
            true
        }
        Err(e) => {
            note(
                report,
                format!("note: {name} is gone, but its unit may still be started at boot: {e:#}"),
            );
            false
        }
    }
}

/// Removes whatever exists under this name: a recorded image with its backend's files,
/// or leftovers without a record (a failed install, a hand-deleted record). The unit
/// stays enabled at boot unless nspawn's own restart policy had enabled it: a machine
/// an administrator enabled comes back at boot with the image that replaces it.
pub async fn remove_existing(
    store: &Store,
    sd: &Systemd,
    name: &str,
    report: Report<'_>,
) -> Result<()> {
    if store.is_starting(name) {
        anyhow::bail!("machine {name} is starting; wait for it or stop it first");
    }
    if sd.machine_exists(name).await? {
        anyhow::bail!("machine {name} is running; stop it before replacing its image");
    }
    if let Some(why) = crate::api::machines::unit_busy(sd, name).await? {
        anyhow::bail!("{why}");
    }
    let assembler = Assembler { store, sd };
    match store.load_image(name)? {
        Some(rec) => {
            assembler.remove(name, rec.backend).await?;
            store.remove_record(name)?;
            if rec.restart.enabled_at_boot() {
                take_off_boot(sd, name, report).await;
            }
        }
        None => {
            assembler.remove_leftovers(name).await?;
            if sd.list_images().await?.into_iter().any(|i| i.name == name) {
                sd.remove_image(name).await?;
            }
        }
    }
    store.remove_machine_files(name)?;
    forget_unit_ending(sd, name).await;
    Ok(())
}

/// Takes a machine's unit out of the failed state its last run left: systemd keeps a
/// failed unit loaded with that run's ending, which the next machine of the name would
/// otherwise show before it ever ran.
pub async fn forget_unit_ending(sd: &Systemd, name: &str) {
    let _ = sd
        .reset_failed(&format!("systemd-nspawn@{name}.service"))
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store with a config of `diff_ids` and the given layer blobs, raw tars.
    fn image(store: &Store, layers: &[Vec<u8>], diff_ids: &[String]) -> OciImageManifest {
        let config = serde_json::to_vec(&serde_json::json!({
            "architecture": "amd64", "os": "linux", "config": { "Cmd": ["/bin/true"] },
            "rootfs": { "type": "layers", "diff_ids": diff_ids },
        }))
        .unwrap();
        let put = |bytes: &[u8]| {
            let digest = crate::layout::sha256_digest(bytes);
            fs::create_dir_all(store.blob_path(&digest).parent().unwrap()).unwrap();
            fs::write(store.blob_path(&digest), bytes).unwrap();
            digest
        };
        let config_digest = put(&config);
        let layers: Vec<serde_json::Value> = layers
            .iter()
            .map(|l| serde_json::json!({ "mediaType": "application/vnd.oci.image.layer.v1.tar", "digest": put(l), "size": l.len() }))
            .collect();
        serde_json::from_value(serde_json::json!({
            "schemaVersion": 2,
            "config": { "mediaType": "application/vnd.oci.image.config.v1+json", "digest": config_digest, "size": config.len() },
            "layers": layers,
        }))
        .unwrap()
    }

    fn tar(name: &str, data: &[u8]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_cksum();
        builder.append_data(&mut header, name, data).unwrap();
        builder.into_inner().unwrap()
    }

    #[tokio::test]
    async fn a_flat_machine_is_staged_whole_or_not_at_all() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(&tmp.path().join("m"), &tmp.path().join("s"));
        let (one, two) = (tar("a", b"1"), tar("b", b"2"));
        let ids = [
            crate::layout::sha256_digest(&one),
            crate::layout::sha256_digest(&two),
        ];
        let good = image(&store, &[one.clone(), two.clone()], &ids);
        stage_layers(&store, Backend::Flat, &good, Some(Mode::App), "web")
            .await
            .unwrap();
        let staging = store.flat_staging_dir("web");
        assert!(staging.join("a").is_file() && staging.join("b").is_file());
        // A layer that is not what the config says leaves no staging behind.
        let wrong = [ids[0].clone(), format!("sha256:{}", "0".repeat(64))];
        let bad = image(&store, &[one, two], &wrong);
        assert!(
            stage_layers(&store, Backend::Flat, &bad, Some(Mode::App), "web")
                .await
                .is_err()
        );
        assert!(!staging.exists());
    }

    #[test]
    fn an_image_whose_config_and_manifest_disagree_on_layers_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(&tmp.path().join("m"), &tmp.path().join("s"));
        let config = br#"{"architecture":"amd64","os":"linux","rootfs":{"type":"layers","diff_ids":["sha256:aa"]}}"#;
        let digest = crate::layout::sha256_digest(config);
        fs::create_dir_all(store.blob_path(&digest).parent().unwrap()).unwrap();
        fs::write(store.blob_path(&digest), config).unwrap();
        let layer = |d: &str| serde_json::json!({ "mediaType": "application/vnd.oci.image.layer.v1.tar", "digest": d, "size": 1 });
        let manifest = |layers: Vec<serde_json::Value>| -> OciImageManifest {
            serde_json::from_value(serde_json::json!({
                "schemaVersion": 2,
                "config": { "mediaType": "application/vnd.oci.image.config.v1+json", "digest": digest, "size": config.len() },
                "layers": layers,
            }))
            .unwrap()
        };
        assert_eq!(
            diff_ids(&store, &manifest(vec![layer("sha256:l1")])).unwrap(),
            ["sha256:aa"]
        );
        let e = diff_ids(
            &store,
            &manifest(vec![layer("sha256:l1"), layer("sha256:l2")]),
        )
        .unwrap_err();
        assert!(
            e.to_string()
                .contains("names 1 layer archives, and its manifest 2 layers"),
            "{e}"
        );
    }
}
