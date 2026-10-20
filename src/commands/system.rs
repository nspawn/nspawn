//! images prune, system df and system prune, with docker's prompts and output.

use std::collections::{HashMap, HashSet};

use anyhow::{bail, Result};
use zbus::zvariant::{OwnedValue, Value};

use crate::cli::{ImagesPruneArgs, SystemDfArgs, SystemPruneArgs};
use crate::client::{self, Client, Dict, Options};
use crate::output::table;

/// docker's sizes (go-units HumanSize): decimal units, four significant digits.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 9] = ["B", "kB", "MB", "GB", "TB", "PB", "EB", "ZB", "YB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1000.0 && unit < UNITS.len() - 1 {
        size /= 1000.0;
        unit += 1;
    }
    format!("{}{}", go_g4(size), UNITS[unit])
}

/// Go's %.4g of a value below 10000: four significant digits, trailing zeros dropped.
fn go_g4(value: f64) -> String {
    if value == 0.0 {
        return "0".to_string();
    }
    let exponent = value.abs().log10().floor() as i32;
    let rounded = format!("{:.*e}", 3, value);
    // The exponent after rounding, which can carry (9999.5 is 1.000e4).
    let exponent = rounded
        .split_once('e')
        .and_then(|(_, e)| e.parse::<i32>().ok())
        .unwrap_or(exponent);
    if !(-4..4).contains(&exponent) {
        let (mantissa, _) = rounded.split_once('e').unwrap_or((&rounded, ""));
        let mantissa = trim_zeros(mantissa);
        return format!(
            "{mantissa}e{}{:02}",
            if exponent < 0 { '-' } else { '+' },
            exponent.abs()
        );
    }
    let decimals = (3 - exponent).max(0) as usize;
    trim_zeros(&format!("{value:.decimals$}"))
}

fn trim_zeros(text: &str) -> String {
    if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        text.to_string()
    }
}

/// --filter label=... and until=..., as the bus takes them.
fn filters(values: &[String]) -> Result<(Vec<String>, Option<u64>)> {
    let (mut labels, mut until) = (Vec::new(), None);
    for value in values {
        match value.split_once('=') {
            Some(("label", label)) => labels.push(label.to_string()),
            Some(("until", when)) => {
                until = Some(match when.parse::<u64>() {
                    Ok(seconds) => seconds,
                    Err(_) => {
                        let ago = crate::health::parse_duration(when)? / 1_000_000;
                        crate::store::now_unix().saturating_sub(ago)
                    }
                })
            }
            _ => bail!("invalid filter '{value}': label= and until= are taken"),
        }
    }
    Ok((labels, until))
}

/// Whether a machine or network (as JSON) has every label of `labels` (KEY or
/// KEY=VALUE) and was made before `until`.
fn selected(have: &serde_json::Value, labels: &[String], until: Option<u64>) -> bool {
    labels.iter().all(|l| {
        let (k, v) = l
            .split_once('=')
            .map_or((l.as_str(), None), |(k, v)| (k, Some(v)));
        match have["labels"].get(k).and_then(|x| x.as_str()) {
            Some(value) => v.is_none_or(|v| v == value),
            None => false,
        }
    }) && until.is_none_or(|t| have["created"].as_u64().is_some_and(|c| c < t))
}

fn confirm(warning: &str, force: bool) -> Result<bool> {
    if force {
        return Ok(true);
    }
    eprintln!("{warning}");
    eprint!("Are you sure you want to continue? [y/N] ");
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "Yes" | "YES"))
}

async fn prune_images(
    client: &Client,
    all: bool,
    labels: &[String],
    until: Option<u64>,
) -> Result<(Vec<String>, u64)> {
    let mut options = Options::new();
    options.insert("all", Value::from(all));
    if !labels.is_empty() {
        options.insert("labels", Value::from(labels.to_vec()));
    }
    if let Some(until) = until {
        options.insert("until", Value::from(until));
    }
    let result = client
        .run_job_silent(|| client.manager.prune_images(options), true)
        .await?;
    Ok((
        client::strings(&result, "removed"),
        client::u64(&result, "reclaimed"),
    ))
}

pub async fn images_prune(args: ImagesPruneArgs, client: &Client) -> Result<()> {
    let (labels, until) = filters(&args.filters)?;
    let warning = if args.all {
        "WARNING! This will remove all images without at least one container associated to them."
    } else {
        "WARNING! This will remove all dangling images."
    };
    if !confirm(warning, args.force)? {
        return Ok(());
    }
    let (removed, reclaimed) = prune_images(client, args.all, &labels, until).await?;
    if !removed.is_empty() {
        println!("Deleted Images:");
        for name in &removed {
            println!("deleted: {name}");
        }
        println!();
    }
    println!("Total reclaimed space: {}", human_size(reclaimed));
    Ok(())
}

fn kind(dict: &Dict, key: &str) -> HashMap<String, u64> {
    dict.get(key)
        .and_then(|v| HashMap::<String, u64>::try_from(v.try_clone().ok()?).ok())
        .unwrap_or_default()
}

fn rows(dict: &Dict, key: &str) -> Vec<Dict> {
    dict.get(key)
        .and_then(|v| Vec::<HashMap<String, OwnedValue>>::try_from(v.try_clone().ok()?).ok())
        .unwrap_or_default()
}

fn reclaimable(k: &HashMap<String, u64>) -> String {
    let (size, free) = (
        k.get("size").copied().unwrap_or(0),
        k.get("reclaimable").copied().unwrap_or(0),
    );
    if size == 0 {
        return human_size(free);
    }
    format!("{} ({}%)", human_size(free), free * 100 / size)
}

pub async fn df(args: SystemDfArgs, client: &Client) -> Result<()> {
    let usage = client.manager.disk_usage().await.map_err(client::error)?;
    if args.json {
        client::print_json(&client::dict_to_json(&usage));
        return Ok(());
    }
    let mut lines = Vec::new();
    for (name, key) in [
        ("Images", "images"),
        ("Containers", "containers"),
        ("Local Volumes", "local_volumes"),
        ("Build Cache", "build_cache"),
    ] {
        let k = kind(&usage, key);
        lines.push(vec![
            name.to_string(),
            k.get("total").copied().unwrap_or(0).to_string(),
            k.get("active").copied().unwrap_or(0).to_string(),
            human_size(k.get("size").copied().unwrap_or(0)),
            reclaimable(&k),
        ]);
    }
    if !args.verbose {
        println!(
            "{}",
            table(&["TYPE", "TOTAL", "ACTIVE", "SIZE", "RECLAIMABLE"], lines)
        );
        return Ok(());
    }
    let images = rows(&usage, "image_rows")
        .iter()
        .map(|r| {
            vec![
                client::string(r, "name"),
                client::string(r, "reference"),
                crate::output::human_duration(
                    crate::store::now_unix().saturating_sub(client::u64(r, "created")),
                ) + " ago",
                human_size(client::u64(r, "size")),
                client::u64(r, "containers").to_string(),
            ]
        })
        .collect();
    println!("Images space usage:\n");
    println!(
        "{}\n",
        table(
            &["NAME", "REFERENCE", "CREATED", "SIZE", "CONTAINERS"],
            images
        )
    );
    let machines = rows(&usage, "container_rows")
        .iter()
        .map(|r| {
            vec![
                client::string(r, "name"),
                client::string(r, "reference"),
                human_size(client::u64(r, "size")),
                if client::bool(r, "running") {
                    "running"
                } else {
                    "stopped"
                }
                .to_string(),
            ]
        })
        .collect();
    println!("Containers space usage:\n");
    println!(
        "{}\n",
        table(&["NAME", "IMAGE", "SIZE", "STATUS"], machines)
    );
    let volumes = rows(&usage, "volume_rows")
        .iter()
        .map(|r| {
            vec![
                client::string(r, "name"),
                client::u64(r, "links").to_string(),
                human_size(client::u64(r, "size")),
            ]
        })
        .collect();
    println!("Local Volumes space usage:\n");
    println!("{}\n", table(&["VOLUME NAME", "LINKS", "SIZE"], volumes));
    println!(
        "Build cache usage: {}",
        human_size(
            kind(&usage, "build_cache")
                .get("size")
                .copied()
                .unwrap_or(0)
        )
    );
    Ok(())
}

/// The unused networks `filters` select, removed; their names.
pub async fn networks_prune(client: &Client, filters: &[String]) -> Result<Vec<String>> {
    let (labels, until) = self::filters(filters)?;
    let unused: Vec<String> = client
        .manager
        .list_networks()
        .await
        .map_err(client::error)?
        .iter()
        .map(client::dict_to_json)
        .filter(|n| {
            n["machines"].as_array().is_some_and(Vec::is_empty) && selected(n, &labels, until)
        })
        .filter_map(|n| n["name"].as_str().map(str::to_string))
        .collect();
    if unused.is_empty() {
        return Ok(Vec::new());
    }
    let result = client
        .run_job_silent(|| client.manager.remove_networks(&unused), true)
        .await?;
    Ok(client::strings(&result, "removed"))
}

pub async fn prune(args: SystemPruneArgs, client: &Client) -> Result<()> {
    let (labels, until) = filters(&args.filters)?;
    let mut warning = String::from("WARNING! This will remove:\n  - all stopped containers\n  - all networks not used by at least one container\n");
    if args.volumes {
        warning.push_str("  - all anonymous volumes not used by at least one container\n");
    }
    warning.push_str(if args.all {
        "  - all images without at least one container associated to them\n"
    } else {
        "  - all dangling images\n"
    });
    warning.push_str("  - unused build cache\n");
    if !confirm(&warning, args.force)? {
        return Ok(());
    }
    let manager = &client.manager;
    // Stopped machines made from an image: a pulled or built record is the image too.
    let usage = manager.disk_usage().await.map_err(client::error)?;
    let stopped: HashMap<String, u64> = rows(&usage, "container_rows")
        .iter()
        .filter(|r| client::bool(r, "made") && !client::bool(r, "running"))
        .map(|r| (client::string(r, "name"), client::u64(r, "size")))
        .collect();
    let mut names: Vec<String> = Vec::new();
    if !stopped.is_empty() {
        let known: HashSet<&String> = stopped.keys().collect();
        for machine in manager.list_machines(true).await.map_err(client::error)? {
            let name = client::string(&machine, "name");
            if !known.contains(&name) || client::string(&machine, "state") != "stopped" {
                continue;
            }
            if selected(&client::dict_to_json(&machine), &labels, until) {
                names.push(name);
            }
        }
    }
    let mut reclaimed = 0;
    let mut removed_machines = Vec::new();
    if !names.is_empty() {
        let result = client
            .run_job_silent(|| manager.remove_machines(&names, Options::new()), true)
            .await;
        // What could not go stays; the rest of the prune goes on, as docker's does.
        if let Err(e) = &result {
            eprintln!("warning: {e:#}");
        }
        let left: HashSet<String> = manager
            .list_machines(true)
            .await
            .map_err(client::error)?
            .iter()
            .map(|m| client::string(m, "name"))
            .collect();
        for name in names.iter().filter(|n| !left.contains(*n)) {
            reclaimed += stopped.get(name).copied().unwrap_or(0);
            removed_machines.push(name.clone());
        }
    }
    // The service prunes every unused network; with filters, the ones they select.
    let networks = if args.filters.is_empty() {
        client
            .run_job_silent(|| manager.prune_networks(), true)
            .await
            .map(|r| client::strings(&r, "removed"))
            .unwrap_or_default()
    } else {
        networks_prune(client, &args.filters)
            .await
            .unwrap_or_default()
    };
    let (images, freed) = prune_images(client, args.all, &labels, until).await?;
    reclaimed += freed;
    for (title, list, prefix) in [
        ("Deleted Containers:", &removed_machines, ""),
        ("Deleted Networks:", &networks, ""),
        ("Deleted Images:", &images, "deleted: "),
    ] {
        if !list.is_empty() {
            println!("{title}");
            for name in list {
                println!("{prefix}{name}");
            }
            println!();
        }
    }
    if args.volumes {
        eprintln!("note: nspawn has no anonymous volumes; nspawn volume prune removes named ones no machine uses");
    }
    println!("Total reclaimed space: {}", human_size(reclaimed));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_as_docker_prints_them() {
        for (bytes, text) in [
            (0, "0B"),
            (999, "999B"),
            (1000, "1kB"),
            (1024, "1.024kB"),
            (12_345_678, "12.35MB"),
            (999_950, "1000kB"),
            (1_000_000_000, "1GB"),
            (3_221_225_472, "3.221GB"),
        ] {
            assert_eq!(human_size(bytes), text, "{bytes}");
        }
    }

    #[test]
    fn filters_select_by_label_and_age() {
        let have = serde_json::json!({ "labels": { "a": "b", "c": "" }, "created": 50 });
        assert!(selected(&have, &[], None));
        assert!(selected(&have, &["a=b".into(), "c".into()], Some(51)));
        assert!(!selected(&have, &["a=x".into()], None));
        assert!(!selected(&have, &["d".into()], None));
        assert!(!selected(&have, &[], Some(50)));
    }

    #[test]
    fn prune_filters() {
        let (labels, until) =
            filters(&["label=a=b".into(), "label=c".into(), "until=100".into()]).unwrap();
        assert_eq!(labels, ["a=b", "c"]);
        assert_eq!(until, Some(100));
        assert!(filters(&["dangling=true".into()]).is_err());
    }
}
