//! Turns blobs in the store into a usable local image: assembly with a backend, the
//! settings file nspawn boots it with, and the record that `images ls`, `images rm`, `start`
//! and `push` rely on.

use std::collections::{BTreeMap, BTreeSet};
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
            host_devices: false,
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
        api_name: None,
        tty: false,
        interactive: false,
        stdin_once: false,
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
        return Err(crate::api::FailureKind::Exists.error(format!(
            "image {name} already exists; use --force to replace it or --name for another name"
        )));
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

/// What holds a name a build would install its image under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Holder {
    /// Nothing, or what an install that did not finish left behind.
    Nothing,
    /// An image of the build's reference that never ran.
    Image,
    /// An image of another reference that never ran.
    Other,
    /// A machine: one that ran, starts, runs or shuts down, one an update cut short keeps
    /// for its rollback, or an image machined lists that nspawn has no record of.
    Machine,
}

/// What the store says holds `name` for a build of `reference`: None without a record
/// of it or anything under way.
fn recorded_holder(store: &Store, name: &str, reference: &str) -> Result<Option<Holder>> {
    if store.is_starting(name) || store.rollback_dir(name).exists() {
        return Ok(Some(Holder::Machine));
    }
    Ok(store.load_image(name)?.map(|record| {
        if !crate::api::disk::image_only(store, &record) {
            Holder::Machine
        } else if record.reference == reference {
            Holder::Image
        } else {
            Holder::Other
        }
    }))
}

/// What holds `name` for a build of `reference`, the store's word checked against
/// systemd's: a record that never ran is a machine all the same while its unit is up.
async fn holder(store: &Store, sd: &Systemd, name: &str, reference: &str) -> Result<Holder> {
    let recorded = recorded_holder(store, name, reference)?;
    if recorded == Some(Holder::Machine)
        || sd.machine_exists(name).await?
        || crate::api::machines::unit_busy(sd, name).await?.is_some()
    {
        return Ok(Holder::Machine);
    }
    if let Some(holder) = recorded {
        return Ok(holder);
    }
    let listed = sd.list_images().await?.into_iter().any(|i| i.name == name);
    Ok(if listed {
        Holder::Machine
    } else {
        Holder::Nothing
    })
}

/// Where a build puts the image of one tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    /// The name the image is installed under, replacing what has it.
    pub name: String,
    /// The tag's own name, when what holds it keeps it and the image goes under `name`.
    pub beside: Option<String>,
}

/// The images a build of `reference` may have put beside what holds the tag's own name
/// `name`, the last made first: built, never run, and under a name free_name gives
/// `name`; none another tag of the build names.
fn kept_beside(
    store: &Store,
    name: &str,
    reference: &str,
    reserved: &BTreeSet<String>,
) -> Result<Vec<String>> {
    let mut found: Vec<ImageRecord> = store
        .list_images()?
        .into_iter()
        .filter(|r| {
            r.origin == "build"
                && r.reference == reference
                && crate::reference::numbered_from(name, &r.name)
                && !reserved.contains(&r.name)
                && crate::api::disk::image_only(store, r)
        })
        .collect();
    found.sort_by_key(|r| std::cmp::Reverse(r.created));
    Ok(found.into_iter().map(|r| r.name).collect())
}

/// The image of `reference` an earlier build kept beside what holds `name`, which the
/// next build of the tag replaces: the last one kept_beside finds that holder still
/// takes for an image of the reference.
async fn image_beside(
    store: &Store,
    sd: &Systemd,
    name: &str,
    reference: &str,
    reserved: &BTreeSet<String>,
) -> Result<Option<String>> {
    for candidate in kept_beside(store, name, reference, reserved)? {
        if holder(store, sd, &candidate, reference).await? == Holder::Image {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

/// Where the image goes, from what holds the tag's own name `name`: there, replacing an
/// image of the reference that never ran as the tag moves to the new build; beside a
/// machine or an image of another reference, which keeps the name, into `beside` (an
/// image of the reference a build kept there before) or else a name `taken` leaves free.
/// A name the build was given (`movable` false) goes nowhere else.
fn decide(
    name: &str,
    own: Holder,
    beside: Option<String>,
    taken: &BTreeSet<String>,
    movable: bool,
) -> Result<Destination> {
    match own {
        Holder::Nothing | Holder::Image => Ok(Destination {
            name: name.to_string(),
            beside: None,
        }),
        Holder::Machine if !movable => Err(crate::api::FailureKind::Exists.error(format!(
            "{name} is a machine, which a build does not replace; use --force to replace it or --name for another name"
        ))),
        Holder::Other if !movable => Err(crate::api::FailureKind::Exists.error(format!(
            "image {name} is of another reference, which a build does not replace; use --force to replace it or --name for another name"
        ))),
        Holder::Machine | Holder::Other => Ok(Destination {
            name: beside.unwrap_or_else(|| crate::reference::free_name(name, taken)),
            beside: Some(name.to_string()),
        }),
    }
}

/// Where a build of `reference` installs its image when its own name is `name`, refused
/// as `ensure_replaceable` refuses with `force`, which replaces whatever has the name.
/// Without it, an image of the reference that never ran gives way to the build; a
/// machine, or an image of another reference, keeps the name while the image goes
/// beside it under the same reference, said in a note: the script that builds and runs
/// its image builds it again without a word. `reserved` names the other tags of the
/// build, which a name chosen here leaves to them; `movable` is false for a name the
/// build was given. Final only under the store lock.
#[allow(clippy::too_many_arguments)]
pub async fn build_destination(
    store: &Store,
    sd: &Systemd,
    name: &str,
    reference: &str,
    force: bool,
    movable: bool,
    reserved: &BTreeSet<String>,
    report: Report<'_>,
) -> Result<Destination> {
    if force {
        ensure_replaceable(store, sd, name, true).await?;
        return Ok(Destination {
            name: name.to_string(),
            beside: None,
        });
    }
    let own = holder(store, sd, name, reference).await?;
    let kept = matches!(own, Holder::Machine | Holder::Other);
    let (beside, mut taken) = if kept && movable {
        let mut taken: BTreeSet<String> =
            store.list_images()?.into_iter().map(|r| r.name).collect();
        taken.extend(sd.list_images().await?.into_iter().map(|i| i.name));
        taken.extend(sd.list_machines().await?.into_iter().map(|m| m.name));
        // A machine an update cut short has no record; the install would drop its
        // rollback.
        taken.extend(store.rollbacks());
        taken.extend(reserved.iter().cloned());
        (
            image_beside(store, sd, name, reference, reserved).await?,
            taken,
        )
    } else {
        (None, BTreeSet::new())
    };
    // Whatever holds it, the tag's own name is not free.
    taken.insert(name.to_string());
    let destination = decide(name, own, beside, &taken, movable)?;
    if destination.beside.is_some() {
        let held = match own {
            Holder::Other => format!("image {name} is of another reference"),
            _ => format!("{name} is a machine"),
        };
        note(
            report,
            format!(
                "note: {held}, which keeps its name; {reference} goes to image {}",
                destination.name
            ),
        );
    }
    Ok(destination)
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
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
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

    fn record(name: &str, reference: &str, created: u64) -> ImageRecord {
        serde_json::from_value(serde_json::json!({
            "name": name, "reference": reference, "manifest_digest": "sha256:m",
            "layers": [], "backend": "overlay", "created": created, "origin": "build",
        }))
        .unwrap()
    }

    #[test]
    fn a_name_is_held_by_an_image_of_the_reference_until_something_ran_from_it() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(&tmp.path().join("m"), &tmp.path().join("s"));
        store.init().unwrap();
        let reference = "hub.nspawn.org/app:latest";
        let held = |name: &str| recorded_holder(&store, name, reference).unwrap();
        assert_eq!(held("app"), None);
        let mut app = record("app", reference, 1);
        store.record_image(&app).unwrap();
        assert_eq!(held("app"), Some(Holder::Image));
        // Pulled, it is an image of the reference all the same.
        app.origin = "pull".to_string();
        store.record_image(&app).unwrap();
        assert_eq!(held("app"), Some(Holder::Image));
        // One of another reference keeps its name: Docker Hub's app, or app:2, which
        // free_name may have put under app-2 beside a machine app.
        app.reference = "docker.io/library/app:latest".to_string();
        store.record_image(&app).unwrap();
        assert_eq!(held("app"), Some(Holder::Other));
        let mut beside = record("app-2", reference, 1);
        store.record_image(&beside).unwrap();
        assert_eq!(
            recorded_holder(&store, "app-2", "hub.nspawn.org/app:2").unwrap(),
            Some(Holder::Other)
        );
        beside.started = Some(3);
        store.record_image(&beside).unwrap();
        assert_eq!(held("app-2"), Some(Holder::Machine));
        let starting = store.mark_starting("app").unwrap();
        assert_eq!(held("app"), Some(Holder::Machine));
        drop(starting);
        app.started = Some(2);
        store.record_image(&app).unwrap();
        assert_eq!(held("app"), Some(Holder::Machine));
        let mut made = record("web", reference, 1);
        made.origin = "create".to_string();
        store.record_image(&made).unwrap();
        assert_eq!(held("web"), Some(Holder::Machine));
        // An update cut short keeps the machine of the name, its record put aside.
        fs::create_dir_all(store.rollback_dir("db")).unwrap();
        assert_eq!(held("db"), Some(Holder::Machine));
    }

    #[test]
    fn a_build_replaces_an_image_of_its_reference_and_goes_beside_anything_else() {
        let taken: BTreeSet<String> = ["app", "app-2"].map(String::from).into();
        for own in [Holder::Nothing, Holder::Image] {
            let there = decide("app", own, None, &taken, true).unwrap();
            assert_eq!((there.name.as_str(), there.beside), ("app", None));
        }
        for own in [Holder::Machine, Holder::Other] {
            let free = decide("app", own, None, &taken, true).unwrap();
            assert_eq!(free.name, "app-3");
            assert_eq!(free.beside.as_deref(), Some("app"));
            let again = decide("app", own, Some("app-2".into()), &taken, true).unwrap();
            assert_eq!(again.name, "app-2");
            // A name the build was given is not moved.
            let given = decide("app", own, None, &taken, false).unwrap_err();
            assert_eq!(
                crate::api::kind_of(&given),
                Some(crate::api::FailureKind::Exists)
            );
        }
        assert_eq!(
            decide("app", Holder::Image, None, &taken, false)
                .unwrap()
                .name,
            "app"
        );
    }

    #[test]
    fn the_images_kept_beside_are_built_ones_of_the_reference_under_a_free_name() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(&tmp.path().join("m"), &tmp.path().join("s"));
        store.init().unwrap();
        let reference = "hub.nspawn.org/app:latest";
        let none = BTreeSet::new();
        let kept =
            |reserved: &BTreeSet<String>| kept_beside(&store, "app", reference, reserved).unwrap();
        let mut machine = record("app", reference, 5);
        machine.started = Some(6);
        store.record_image(&machine).unwrap();
        assert!(kept(&none).is_empty());
        store.record_image(&record("app-2", reference, 2)).unwrap();
        store.record_image(&record("app-3", reference, 3)).unwrap();
        store
            .record_image(&record("app-4", "hub.nspawn.org/app:4", 4))
            .unwrap();
        // An image of the reference the user named, or pulled, is no build's to replace.
        store
            .record_image(&record("app-stable", reference, 8))
            .unwrap();
        let mut pulled = record("app-9", reference, 9);
        pulled.origin = "pull".to_string();
        store.record_image(&pulled).unwrap();
        assert_eq!(kept(&none), ["app-3", "app-2"]);
        // Another tag of the build has the name for its own.
        let reserved: BTreeSet<String> = ["app-3".to_string()].into();
        assert_eq!(kept(&reserved), ["app-2"]);
        let mut ran = record("app-3", reference, 3);
        ran.started = Some(7);
        store.record_image(&ran).unwrap();
        assert_eq!(kept(&none), ["app-2"]);
    }
}
