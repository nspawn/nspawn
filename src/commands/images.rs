use std::collections::HashMap;

use anyhow::{bail, Result};

use crate::backend::Assembler;
use crate::bridge;
use crate::cli::ImagesRmArgs;
use crate::commands::require_root;
use crate::config::Config;
use crate::output::{human_bytes, table};
use crate::store::Store;
use crate::systemd::Systemd;

pub async fn ls(config: &Config) -> Result<()> {
    let sd = Systemd::connect().await?;
    let store = Store::new(&config.machines_dir, &config.state_dir);
    let records: HashMap<String, _> = store
        .list_images()?
        .into_iter()
        .map(|r| (r.name.clone(), r))
        .collect();
    let mut images = sd.list_images().await?;
    images.retain(|i| !i.name.starts_with('.'));
    images.sort_by(|a, b| a.name.cmp(&b.name));
    let rows = images
        .into_iter()
        .map(|i| {
            let (backend, origin, source) = match records.get(&i.name) {
                Some(r) => (
                    format!("{:?}", r.backend).to_lowercase(),
                    r.origin.clone(),
                    r.reference.clone(),
                ),
                None => ("-".to_string(), "-".to_string(), "-".to_string()),
            };
            vec![
                i.name,
                i.kind,
                backend,
                origin,
                source,
                i.usage.map(human_bytes).unwrap_or_else(|| "-".to_string()),
                if i.read_only {
                    "yes".into()
                } else {
                    "no".into()
                },
            ]
        })
        .collect();
    println!(
        "{}",
        table(
            &["NAME", "TYPE", "BACKEND", "ORIGIN", "SOURCE", "SIZE", "RO"],
            rows
        )
    );
    Ok(())
}

pub async fn rm(args: ImagesRmArgs, config: &Config) -> Result<()> {
    require_root("images rm")?;
    let sd = Systemd::connect().await?;
    let store = Store::new(&config.machines_dir, &config.state_dir);
    let assembler = Assembler {
        store: &store,
        sd: &sd,
    };
    for name in &args.names {
        if sd.machine_exists(name).await? {
            bail!("machine {name} is running; stop it first");
        }
        match store.load_image(name)? {
            Some(rec) => {
                assembler.remove(name, rec.backend).await?;
                store.remove_machine_files(name)?;
                store.remove_record(name)?;
            }
            None => sd.remove_image(name).await?,
        }
        println!("removed {name}");
    }
    bridge::write_hosts_files(&store, config)?;
    let gone = store.gc_layers()?;
    if !gone.is_empty() {
        println!("freed {} unused layer(s)", gone.len());
    }
    let blobs = store.gc_blobs()?;
    if !blobs.is_empty() {
        println!("freed {} unused blob(s)", blobs.len());
    }
    Ok(())
}
