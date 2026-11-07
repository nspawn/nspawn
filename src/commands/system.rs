//! info, images prune, system df and system prune, their prompts and output as scripts
//! that read them expect.

use std::collections::HashMap;

use anyhow::{anyhow, bail, Result};
use zbus::zvariant::{OwnedValue, Value};

use crate::cli::{
    BuilderDuArgs, BuilderPruneArgs, ImagesPruneArgs, InfoArgs, SystemDfArgs, SystemPruneArgs,
};
use crate::client::{self, Client, Dict, Options};
use crate::output::table;

/// Sizes as go-units' HumanSize writes them: decimal units, four significant digits.
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

/// volume ls --filter: label=KEY or label=KEY=VALUE.
pub fn label_filters(values: &[String]) -> Result<Vec<String>> {
    values
        .iter()
        .map(|value| match value.split_once('=') {
            Some(("label", label)) if !label.is_empty() => Ok(label.to_string()),
            _ => bail!("invalid filter '{value}': label= is taken"),
        })
        .collect()
}

/// Whether a volume (as JSON) has every label of `labels`, KEY or KEY=VALUE.
pub fn labelled(have: &serde_json::Value, labels: &[String]) -> bool {
    selected(have, labels, None)
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
    client::numbers(dict, key)
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
        // The default network is never pruned, used or not.
        .filter(|n| n["name"].as_str() != Some(crate::bridge::DEFAULT_NETWORK))
        .filter(|n| {
            n["machines"].as_array().is_some_and(Vec::is_empty) && selected(n, &labels, until)
        })
        .filter_map(|n| n["name"].as_str().map(str::to_string))
        .collect();
    if unused.is_empty() {
        return Ok(Vec::new());
    }
    let result = client::partly(
        client
            .run_job_silent(|| client.manager.remove_networks(&unused), true)
            .await,
    )?;
    Ok(client::strings(&result, "removed"))
}

/// The unused volumes made without a name (the API makes them), those `labels` select.
/// The unused anonymous volumes removed, and what they held.
async fn anonymous_volumes_prune(client: &Client, labels: &[String]) -> Result<(Vec<String>, u64)> {
    let volumes = client.manager.list_volumes().await.map_err(client::error)?;
    let unused = anonymous_unused(
        &volumes.iter().map(client::dict_to_json).collect::<Vec<_>>(),
        labels,
    );
    if unused.is_empty() {
        return Ok((Vec::new(), 0));
    }
    // Measured before they go.
    let sizes: HashMap<String, u64> = match client.manager.disk_usage().await {
        Ok(usage) => rows(&usage, "volume_rows")
            .iter()
            .map(|r| (client::string(r, "name"), client::u64(r, "size")))
            .collect(),
        Err(_) => HashMap::new(),
    };
    // One a machine took meanwhile is refused by the service and stays.
    let result = client::partly(
        client
            .run_job_silent(|| client.manager.remove_volumes(&unused), true)
            .await,
    )?;
    let removed = client::strings(&result, "removed");
    let freed = removed.iter().filter_map(|name| sizes.get(name)).sum();
    Ok((removed, freed))
}

fn anonymous_unused(volumes: &[serde_json::Value], labels: &[String]) -> Vec<String> {
    volumes
        .iter()
        .filter(|v| {
            v["anonymous"].as_bool() == Some(true)
                && v["used_by"].as_array().is_some_and(Vec::is_empty)
                && labelled(v, labels)
        })
        .filter_map(|v| v["name"].as_str().map(str::to_string))
        .collect()
}

async fn prune_cache(
    client: &Client,
    all: bool,
    until: Option<u64>,
    keep_storage: Option<u64>,
) -> Result<(Vec<String>, u64)> {
    let mut options = Options::new();
    options.insert("all", Value::from(all));
    if let Some(until) = until {
        options.insert("until", Value::from(until));
    }
    if let Some(size) = keep_storage {
        options.insert("keep_storage", Value::from(size));
    }
    let result = client
        .run_job_silent(|| client.manager.prune_build_cache(options), true)
        .await?;
    Ok((
        client::strings(&result, "removed"),
        client::u64(&result, "reclaimed"),
    ))
}

pub async fn builder_prune(args: BuilderPruneArgs, client: &Client) -> Result<()> {
    let (labels, until) = filters(&args.filters)?;
    if !labels.is_empty() {
        bail!("builder prune takes until= alone: the build cache has no labels");
    }
    let warning = if args.all {
        "WARNING! This will remove all build cache."
    } else {
        "WARNING! This will remove all dangling build cache."
    };
    if !confirm(warning, args.force)? {
        return Ok(());
    }
    let (removed, reclaimed) = prune_cache(client, args.all, until, args.keep_storage).await?;
    if !removed.is_empty() {
        println!("Deleted build cache objects:");
        for id in &removed {
            println!("{id}");
        }
        println!();
    }
    println!("Total reclaimed space: {}", human_size(reclaimed));
    Ok(())
}

/// `info`: the command's own version, then the service's answer as text, a template
/// over it or JSON. The service's errors are this command's: a script reads its exit
/// status to learn whether the service is there.
pub async fn info(args: InfoArgs) -> Result<()> {
    let format = args.format.as_deref().filter(|f| !f.is_empty());
    // A template that does not parse is said before the service is asked.
    let template = match format {
        Some(f) if f != "json" => Some(
            crate::gotemplate::Template::parse(f)
                .map_err(|e| anyhow!("template parsing error: {e}"))?,
        ),
        _ => None,
    };
    if format.is_none() {
        print!("{}", client_text(env!("CARGO_PKG_VERSION")));
    }
    let client = Client::connect().await?;
    let answer = client.manager.info().await.map_err(client::error)?;
    match (format, template) {
        (_, Some(template)) => {
            let text = template
                .execute(&client::dict_to_json(&answer))
                .map_err(|e| anyhow!("template parsing error: {e}"))?;
            println!("{text}");
        }
        (Some(_), None) => println!("{}", client::dict_to_json(&answer)),
        (None, None) => print!("{}", server_text(&answer)),
    }
    Ok(())
}

fn client_text(version: &str) -> String {
    format!("Client:\n Version: {version}\n\n")
}

/// The service's answer to Info as `info` prints it.
fn server_text(answer: &Dict) -> String {
    let string = |key: &str| client::string(answer, key);
    let machines = client::numbers(answer, "machines");
    let count = |key: &str| machines.get(key).copied().unwrap_or(0);
    let backend = match (string("backend"), string("auto_backend")) {
        (backend, auto) if backend == "auto" && !auto.is_empty() => format!("auto ({auto})"),
        (backend, _) => backend,
    };
    let mut lines = vec![
        "Server:".to_string(),
        format!(" Version: {}", string("version")),
        format!(" Machines: {}", count("total")),
        format!("  Running: {}", count("running")),
        format!("  Paused: {}", count("paused")),
        format!("  Stopped: {}", count("stopped")),
        format!(" Images: {}", client::u64(answer, "images")),
        format!(" Registry: {}", string("registry")),
        format!(" Storage backend: {backend}"),
        format!(" Log driver: {}", string("log_driver")),
        format!(" Machines directory: {}", string("machines_dir")),
        format!(" State directory: {}", string("state_dir")),
    ];
    let systemd = string("systemd_version");
    if !systemd.is_empty() {
        lines.push(format!(" systemd version: {systemd}"));
    }
    lines.extend([
        format!(" Kernel version: {}", string("kernel")),
        format!(" Operating system: {}", string("os")),
        format!(" Architecture: {}", string("architecture")),
        format!(" CPUs: {}", client::u64(answer, "cpus")),
        format!(
            " Total memory: {}",
            crate::output::human_bytes(client::u64(answer, "memory"))
        ),
        format!(" Name: {}", string("hostname")),
        format!(" ID: {}", string("machine_id")),
    ]);
    lines.join("\n") + "\n"
}

pub async fn builder_du(args: BuilderDuArgs, client: &Client) -> Result<()> {
    let rows = client
        .manager
        .build_cache_usage()
        .await
        .map_err(client::error)?;
    if args.json {
        client::print_json(&serde_json::Value::Array(
            rows.iter().map(client::dict_to_json).collect(),
        ));
        return Ok(());
    }
    let total: u64 = rows.iter().map(|r| client::u64(r, "size")).sum();
    let reclaimable: u64 = rows
        .iter()
        .filter(|r| !client::bool(r, "shared"))
        .map(|r| client::u64(r, "size"))
        .sum();
    if args.verbose {
        let lines = rows
            .iter()
            .map(|r| {
                vec![
                    client::string(r, "id").chars().take(19).collect(),
                    client::string(r, "kind"),
                    if client::bool(r, "shared") {
                        "true"
                    } else {
                        "false"
                    }
                    .to_string(),
                    human_size(client::u64(r, "size")),
                    crate::output::human_duration(
                        crate::store::now_unix().saturating_sub(client::u64(r, "used")),
                    ) + " ago",
                    client::string(r, "description"),
                ]
            })
            .collect();
        println!(
            "{}\n",
            table(
                &["ID", "KIND", "SHARED", "SIZE", "LAST USED", "DESCRIPTION"],
                lines
            )
        );
    }
    println!("Reclaimable:\t{}", human_size(reclaimable));
    println!("Total:\t\t{}", human_size(total));
    Ok(())
}

/// The question of system prune: what goes, and the filters that narrow it.
fn prune_warning(all: bool, volumes: bool, filters: &[String]) -> String {
    let mut warning = String::from("WARNING! This will remove:\n  - all stopped containers\n  - all networks not used by at least one container\n");
    if volumes {
        warning.push_str("  - all anonymous volumes not used by at least one container\n");
    }
    warning.push_str(if all {
        "  - all images without at least one container associated to them\n"
    } else {
        "  - all dangling images\n"
    });
    // -a takes the cache that images use too.
    warning.push_str(if all {
        "  - all build cache\n"
    } else {
        "  - unused build cache\n"
    });
    if !filters.is_empty() {
        let mut filters = filters.to_vec();
        filters.sort_by(|a, b| super::machines::natural_order(a, b));
        warning.push_str("\n  Items to be pruned will be filtered with:\n");
        for filter in filters {
            warning.push_str(&format!("  - {filter}\n"));
        }
    }
    warning
}

pub async fn prune(args: SystemPruneArgs, client: &Client) -> Result<()> {
    let (labels, until) = filters(&args.filters)?;
    if args.volumes && until.is_some() {
        bail!(r#"The "until" filter is not supported with "--volumes""#);
    }
    let warning = prune_warning(args.all, args.volumes, &args.filters);
    if !confirm(&warning, args.force)? {
        return Ok(());
    }
    let manager = &client.manager;
    // Stopped machines made from an image: a pulled or built record is the image too.
    let mut options = Options::new();
    options.insert("made", Value::from(true));
    if !labels.is_empty() {
        options.insert("labels", Value::from(labels.clone()));
    }
    if let Some(until) = until {
        options.insert("until", Value::from(until));
    }
    let pruned = client::partly(
        client
            .run_job_silent(|| manager.prune_machines(options), true)
            .await,
    );
    // What could not go stays; the rest of the prune goes on.
    let (removed_machines, mut reclaimed) = match pruned {
        Ok(result) => (
            client::strings(&result, "removed"),
            client::u64(&result, "reclaimed"),
        ),
        Err(e) => {
            eprintln!("warning: {e:#}");
            (Vec::new(), 0)
        }
    };
    // The service prunes every unused network; with filters, the ones they select.
    let networks = if args.filters.is_empty() {
        client::partly(
            client
                .run_job_silent(|| manager.prune_networks(), true)
                .await,
        )
        .map(|r| client::strings(&r, "removed"))
        .unwrap_or_default()
    } else {
        networks_prune(client, &args.filters)
            .await
            .unwrap_or_default()
    };
    let volumes = if args.volumes {
        let (removed, freed) = anonymous_volumes_prune(client, &labels)
            .await
            .unwrap_or_else(|e| {
                eprintln!("warning: {e:#}");
                (Vec::new(), 0)
            });
        reclaimed += freed;
        removed
    } else {
        Vec::new()
    };
    let (images, freed) = prune_images(client, args.all, &labels, until).await?;
    reclaimed += freed;
    // The build cache has no labels: a prune narrowed by them leaves it.
    let cache = if labels.is_empty() {
        let (removed, freed) = prune_cache(client, args.all, until, None).await?;
        reclaimed += freed;
        removed
    } else {
        Vec::new()
    };
    for (title, list, prefix) in [
        ("Deleted Containers:", &removed_machines, ""),
        ("Deleted Networks:", &networks, ""),
        ("Deleted Volumes:", &volumes, ""),
        ("Deleted Images:", &images, "deleted: "),
        ("Deleted build cache objects:", &cache, ""),
    ] {
        if !list.is_empty() {
            println!("{title}");
            for name in list {
                println!("{prefix}{name}");
            }
            println!();
        }
    }
    println!("Total reclaimed space: {}", human_size(reclaimed));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_print_in_decimal_units() {
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

    fn info_answer(backend: &str, systemd: &str) -> Dict {
        let info = crate::api::info::Info {
            version: "3.0.0".into(),
            machines: crate::api::info::Machines {
                total: 5,
                running: 2,
                paused: 1,
                stopped: 2,
            },
            images: 3,
            machines_dir: "/var/lib/machines".into(),
            state_dir: "/var/lib/nspawn".into(),
            backend: backend.into(),
            auto_backend: "overlay".into(),
            log_driver: "local".into(),
            registry: "hub.nspawn.org".into(),
            architecture: "x86_64".into(),
            systemd_version: systemd.into(),
            host: crate::api::info::Host {
                hostname: "box".into(),
                machine_id: "0123456789abcdef0123456789abcdef".into(),
                kernel: "6.17.1".into(),
                os: "Fedora Linux 44".into(),
                cpus: 4,
                memory: 8 << 30,
            },
        };
        crate::daemon::values::info(&info)
    }

    #[test]
    fn info_prints_the_command_and_the_service() {
        assert_eq!(client_text("3.0.1"), "Client:\n Version: 3.0.1\n\n");
        assert_eq!(
            server_text(&info_answer("auto", "259 (259.7-1.fc44)")),
            "Server:\n Version: 3.0.0\n Machines: 5\n  Running: 2\n  Paused: 1\n  Stopped: 2\n \
             Images: 3\n Registry: hub.nspawn.org\n Storage backend: auto (overlay)\n \
             Log driver: local\n Machines directory: /var/lib/machines\n \
             State directory: /var/lib/nspawn\n systemd version: 259 (259.7-1.fc44)\n \
             Kernel version: 6.17.1\n Operating system: Fedora Linux 44\n \
             Architecture: x86_64\n CPUs: 4\n Total memory: 8.0 GiB\n Name: box\n \
             ID: 0123456789abcdef0123456789abcdef\n"
        );
        // A backend named in the configuration is that one; systemd's version is left
        // out when it could not be read.
        let named = server_text(&info_answer("flat", ""));
        assert!(named.contains("\n Storage backend: flat\n"), "{named}");
        assert!(!named.contains("systemd version"), "{named}");
    }

    #[test]
    fn info_templates_read_the_service_s_keys() {
        let answer = client::dict_to_json(&info_answer("auto", ""));
        let run = |text: &str| {
            crate::gotemplate::Template::parse(text)
                .unwrap()
                .execute(&answer)
                .unwrap()
        };
        assert_eq!(run("{{.version}}"), "3.0.0");
        assert_eq!(
            run("{{.machines.running}} of {{.machines.total}}"),
            "2 of 5"
        );
        assert_eq!(run("{{.backend}}/{{.auto_backend}}"), "auto/overlay");
    }

    #[test]
    fn the_prune_question_says_what_goes() {
        let plain = prune_warning(false, false, &[]);
        assert!(plain.ends_with("  - all dangling images\n  - unused build cache\n"));
        let all = prune_warning(
            true,
            true,
            &["until=24h".to_string(), "label=team=a".to_string()],
        );
        assert!(all.contains("  - all anonymous volumes not used by at least one container\n"));
        assert!(all.ends_with("  - all images without at least one container associated to them\n  - all build cache\n\n  Items to be pruned will be filtered with:\n  - label=team=a\n  - until=24h\n"));
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
    fn volumes_are_listed_by_label() {
        assert_eq!(
            label_filters(&["label=a=b".into(), "label=c".into()]).unwrap(),
            ["a=b", "c"]
        );
        for bad in ["dangling=true", "label=", "label", "until=1"] {
            let e = label_filters(&[bad.to_string()]).unwrap_err();
            assert_eq!(
                format!("{e:#}"),
                format!("invalid filter '{bad}': label= is taken")
            );
        }
        let have = serde_json::json!({ "labels": { "a": "b" }, "created": 50 });
        assert!(labelled(&have, &[]));
        assert!(labelled(&have, &["a".into(), "a=b".into()]));
        assert!(!labelled(&have, &["a=c".into()]));
        assert!(!labelled(
            &serde_json::json!({ "name": "old" }),
            &["a".into()]
        ));
    }

    #[test]
    fn system_prune_takes_unused_anonymous_volumes_only() {
        let volumes = [
            serde_json::json!({ "name": "anon", "anonymous": true, "used_by": [], "labels": { "a": "b" } }),
            serde_json::json!({ "name": "anon-used", "anonymous": true, "used_by": ["m"], "labels": {} }),
            serde_json::json!({ "name": "named", "anonymous": false, "used_by": [], "labels": { "a": "b" } }),
            serde_json::json!({ "name": "anon-other", "anonymous": true, "used_by": [], "labels": { "a": "c" } }),
        ];
        assert_eq!(anonymous_unused(&volumes, &[]), ["anon", "anon-other"]);
        assert_eq!(anonymous_unused(&volumes, &["a=b".into()]), ["anon"]);
        assert!(anonymous_unused(&volumes, &["x".into()]).is_empty());
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
