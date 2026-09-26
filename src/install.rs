//! Turns blobs in the store into a usable local image: assembly with a backend, the
//! settings file nspawn boots it with, and the record that `images ls`, `images rm`, `start`
//! and `push` rely on.

use std::collections::BTreeMap;
use std::fs;

use anyhow::{Context, Result};
use oci_client::manifest::OciImageManifest;

use crate::api::{note, Report};
use crate::backend::{Assembler, Backend, BackendChoice, Layer};
use crate::config::Config;
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
    config: &Config,
    backend: Backend,
    spec: Install<'_>,
    report: Report<'_>,
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
    let config_blob =
        fs::read(&config_path).with_context(|| format!("reading {}", config_path.display()))?;
    let run = RunSpec::from_config(&config_blob)?;
    // An mstack image runs with managed user namespaces, which cannot join the network
    // namespace prepared for app machines on the bridge; apps get overlay instead. Whether
    // the image is an app is known before extraction when its command says so.
    let surely_app = spec.mode == Some(Mode::App)
        || (spec.mode.is_none() && detect_mode(true, &run.argv()) == Mode::App);
    let backend = if backend == Backend::Mstack && surely_app {
        note(report, format!("note: {} runs a program rather than an init system; assembling it as overlay, since mstack machines cannot join the bridge network", spec.name));
        Backend::Overlay
    } else {
        backend
    };
    let assembler = Assembler { store, sd };
    let mut backend = backend;
    let trees = assembler.assemble(backend, spec.name, &layers).await?;
    let mode = spec
        .mode
        .unwrap_or_else(|| detect_mode(has_init(&trees), &run.argv()));
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
    // Both kinds join the bridge, like docker; --network host is one flag away.
    let network = Network::Bridge;
    let route = settings::namespace_route(sd, spec.name, mode, true).await?;
    settings::write(
        &MachineSettings {
            name: spec.name,
            managed_userns: backend == Backend::Mstack,
            mode,
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
    let app_argv = settings::app_argv(sd, spec.name, mode, &route).await?;
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
        },
    )? {
        sd.reload().await?;
    }
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

/// Takes the unit of a machine that is gone off the boot list: a link left behind would
/// start a unit with nothing to run at every boot. Best effort, said in a note: the
/// removal is done by then, and machined allows image names systemd refuses as units.
pub async fn take_off_boot(sd: &Systemd, name: &str, report: Report<'_>) {
    match sd
        .disable_unit(&format!("systemd-nspawn@{name}.service"))
        .await
    {
        Ok(false) => {}
        Ok(true) => {
            if let Err(e) = sd.reload().await {
                note(report, format!("note: {e:#}"));
            }
        }
        Err(e) => note(
            report,
            format!("note: {name} is gone, but its unit may still be started at boot: {e:#}"),
        ),
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
    Ok(())
}
