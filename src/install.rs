//! Turns blobs in the store into a usable local image: assembly with a backend, the
//! settings file nspawn boots it with, and the record that `images ls`, `images rm`, `start`
//! and `push` rely on.

use std::fs;

use anyhow::{Context, Result};
use oci_client::manifest::OciImageManifest;

use crate::backend::{Assembler, Backend, Layer};
use crate::oci::{detect_mode, has_init, Mode, RunSpec};
use crate::settings::{self, MachineSettings, Network};
use crate::store::{now_unix, ImageRecord, Store};
use crate::systemd::Systemd;

pub struct Install<'a> {
    pub name: &'a str,
    pub reference: &'a str,
    pub manifest_bytes: &'a [u8],
    pub manifest: &'a OciImageManifest,
    pub manifest_digest: &'a str,
    pub origin: &'a str,
    /// Force boot or app instead of detecting it from the image.
    pub mode: Option<Mode>,
}

/// All blobs named in the manifest (layers and config) must already be in the store.
pub async fn install(
    store: &Store,
    sd: &Systemd,
    backend: Backend,
    spec: Install<'_>,
) -> Result<Mode> {
    let layers: Vec<Layer> = spec
        .manifest
        .layers
        .iter()
        .map(|d| Layer {
            digest: d.digest.clone(),
            media_type: d.media_type.clone(),
            blob: store.blob_path(&d.digest),
        })
        .collect();
    let config_path = store.blob_path(&spec.manifest.config.digest);
    let config =
        fs::read(&config_path).with_context(|| format!("reading {}", config_path.display()))?;
    let run = RunSpec::from_config(&config)?;
    // An mstack image runs with managed user namespaces, which cannot join the network
    // namespace prepared for app machines on the bridge; apps get overlay instead. Whether
    // the image is an app is known before extraction when its command says so.
    let surely_app = spec.mode == Some(Mode::App)
        || (spec.mode.is_none() && detect_mode(true, &run.command) == Mode::App);
    let backend = if backend == Backend::Mstack && surely_app {
        eprintln!("note: {} runs a program rather than an init system; assembling it as overlay, since mstack machines cannot join the bridge network", spec.name);
        Backend::Overlay
    } else {
        backend
    };
    let assembler = Assembler { store, sd };
    let trees = assembler.assemble(backend, spec.name, &layers).await?;
    let mode = spec
        .mode
        .unwrap_or_else(|| detect_mode(has_init(&trees), &run.command));
    // Both kinds join the bridge, like docker; --network host is one flag away.
    let network = Network::Bridge;
    settings::write(&MachineSettings {
        name: spec.name,
        managed_userns: backend == Backend::Mstack,
        mode,
        run: &run,
        command_override: None,
        network,
        bridge: None,
    })?;
    store.save_manifest(spec.name, spec.manifest_bytes)?;
    store.record_image(&ImageRecord {
        name: spec.name.to_string(),
        reference: spec.reference.to_string(),
        manifest_digest: spec.manifest_digest.to_string(),
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
        run,
        network,
        address: None,
        ports: Vec::new(),
    })?;
    Ok(mode)
}

/// Removes an existing image with the same name, or refuses when it is running or
/// `force` is not set.
pub async fn replace_existing(store: &Store, sd: &Systemd, name: &str, force: bool) -> Result<()> {
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
    if sd.machine_exists(name).await? {
        anyhow::bail!("machine {name} is running; stop it before replacing its image");
    }
    match existing_record {
        Some(rec) => {
            Assembler { store, sd }.remove(name, rec.backend).await?;
            store.remove_record(name)?;
        }
        None => sd.remove_image(name).await?,
    }
    Ok(())
}
