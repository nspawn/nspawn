use anyhow::{bail, Result};

use crate::backend::{Assembler, Backend, Layer};
use crate::cli::{BackendChoice, PullArgs};
use crate::commands::require_root;
use crate::config::Config;
use crate::hub::{short_digest, Hub};
use crate::reference::{validate_machine_name, ImageRef};
use crate::store::{now_unix, ImageRecord, Store};
use crate::systemd::Systemd;

pub async fn run(args: PullArgs, config: &Config) -> Result<()> {
    require_root("pull")?;
    let image = ImageRef::parse(&args.reference, &config.registry)?;
    let oci = image.to_oci()?;
    let name = args.name.clone().unwrap_or_else(|| image.local_name());
    validate_machine_name(&name)?;

    let sd = Systemd::connect().await?;
    let store = Store::new(&config.machines_dir, &config.state_dir);
    store.init()?;
    let assembler = Assembler {
        store: &store,
        sd: &sd,
    };

    let existing_record = store.load_image(&name)?;
    let existing_image = sd.list_images().await?.into_iter().any(|i| i.name == name);
    if existing_record.is_some() || existing_image {
        if !args.force {
            bail!(
                "image {name} already exists; use --force to replace it or --name for another name"
            );
        }
        if sd.machine_exists(&name).await? {
            bail!("machine {name} is running; stop it before replacing its image");
        }
        match existing_record {
            Some(rec) => {
                assembler.remove(&name, rec.backend).await?;
                store.remove_record(&name)?;
            }
            None => sd.remove_image(&name).await?,
        }
    }

    let choice = if args.backend == BackendChoice::Auto {
        config.backend
    } else {
        args.backend
    };
    let backend = Backend::choose(choice, &sd).await?;
    let hub = Hub::new(config)?;
    let (manifest, manifest_digest) = hub.resolve(&oci).await?;
    println!(
        "{image}: manifest {} with {} layer(s), assembling as {}",
        short_digest(&manifest_digest),
        manifest.layers.len(),
        backend.name()
    );

    let mut layers = Vec::new();
    for descriptor in &manifest.layers {
        let blob = store.blob_path(&descriptor.digest);
        if backend != Backend::Flat && store.has_layer(&descriptor.digest) {
            println!(
                "layer {}: already present",
                short_digest(&descriptor.digest)
            );
        } else {
            println!("layer {}: downloading", short_digest(&descriptor.digest));
            hub.download_layer(&oci, descriptor, &blob).await?;
        }
        layers.push(Layer {
            digest: descriptor.digest.clone(),
            media_type: descriptor.media_type.clone(),
            blob,
        });
    }

    assembler.assemble(backend, &name, &layers).await?;
    store.record_image(&ImageRecord {
        name: name.clone(),
        reference: image.to_string(),
        manifest_digest,
        layers: manifest.layers.iter().map(|l| l.digest.clone()).collect(),
        backend: backend.as_choice(),
        created: now_unix(),
    })?;
    println!("image {name} is ready: nspawn start {name}");
    Ok(())
}
