//! The commands that tell about a project: ps, ls, images, top, port, volumes, stats
//! and events, their tables and their JSON.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Result};

use super::{label, Engine, Machine};
use crate::client::{self, Dict};
use crate::compose::cli::{
    EventsArgs, ImagesArgs, LsArgs, PortArgs, PsArgs, StatsArgs, TopArgs, VolumesArgs,
};
use crate::compose::variables::table;

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// go-units' HumanDuration of whole seconds.
pub fn human_duration(seconds: u64) -> String {
    let minutes = seconds / 60;
    let hours = ((seconds as f64) / 3600.0 + 0.5) as u64;
    match seconds {
        0 => "Less than a second".to_string(),
        1 => "1 second".to_string(),
        s if s < 60 => format!("{s} seconds"),
        _ if minutes == 1 => "About a minute".to_string(),
        _ if minutes < 60 => format!("{minutes} minutes"),
        _ if hours == 1 => "About an hour".to_string(),
        _ if hours < 48 => format!("{hours} hours"),
        _ if hours < 24 * 7 * 2 => format!("{} days", hours / 24),
        _ if hours < 24 * 30 * 2 => format!("{} weeks", hours / 24 / 7),
        _ if hours < 24 * 365 * 2 => format!("{} months", hours / 24 / 30),
        _ => format!("{} years", seconds / 3600 / 24 / 365),
    }
}

/// go-units' HumanSizeWithPrecision(size, 3): decimal units, for image sizes.
pub fn human_size(size: u64) -> String {
    const UNITS: [&str; 9] = ["B", "kB", "MB", "GB", "TB", "PB", "EB", "ZB", "YB"];
    let mut value = size as f64;
    let mut i = 0;
    while value >= 1000.0 && i < UNITS.len() - 1 {
        value /= 1000.0;
        i += 1;
    }
    // %.4g of the value, trailing zeros left out.
    let digits = if value >= 100.0 {
        0
    } else if value >= 10.0 {
        1
    } else {
        2
    };
    let text = format!("{value:.digits$}");
    let text = if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        text
    };
    format!("{text}{}", UNITS[i])
}

/// An image written short: Docker Hub's without its registry, its official ones
/// without library/.
pub fn short_image(reference: &str) -> String {
    reference
        .strip_prefix("docker.io/library/")
        .or_else(|| reference.strip_prefix("docker.io/"))
        .unwrap_or(reference)
        .to_string()
}

/// The state names ps prints and filters on.
fn state(m: &Machine) -> &'static str {
    match m.state.as_str() {
        "running" | "opening" => "running",
        "paused" => "paused",
        "restarting" => "restarting",
        "starting" => "created",
        "closing" => "removing",
        _ if m.exit_code.is_some() || m.started > 0 => "exited",
        _ => "created",
    }
}

fn status(m: &Machine, now: u64) -> String {
    match state(m) {
        "running" | "paused" => {
            let mut text = format!("Up {}", human_duration(now.saturating_sub(m.started)));
            match m.health.as_str() {
                "healthy" => text.push_str(" (healthy)"),
                "unhealthy" => text.push_str(" (unhealthy)"),
                "starting" => text.push_str(" (health: starting)"),
                _ => {}
            }
            if m.state == "paused" {
                text.push_str(" (Paused)");
            }
            text
        }
        "restarting" => format!("Restarting ({})", m.exit_code.unwrap_or(0)),
        "exited" => format!("Exited ({})", m.exit_code.unwrap_or(0)),
        "removing" => "Removal In Progress".to_string(),
        _ => "Created".to_string(),
    }
}

/// The published ports, as ps lists them.
fn ports(m: &Machine) -> Vec<String> {
    m.strings("ports")
        .iter()
        .map(|p| {
            if p.matches(':').count() == 0 && !p.contains('.') {
                format!("0.0.0.0:{p}")
            } else {
                p.clone()
            }
        })
        .collect()
}

fn command(m: &Machine, trunc: bool) -> String {
    // The program and its arguments joined, one with a space in quotes.
    let text = m
        .strings("command")
        .iter()
        .map(|w| {
            if w.contains(char::is_whitespace) {
                format!("'{w}'")
            } else {
                w.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    let text = if trunc && text.chars().count() > 20 {
        format!("{}...", text.chars().take(17).collect::<String>())
    } else {
        text
    };
    let mut quoted = String::new();
    crate::compose::emit::json_string(&text, false, &mut quoted);
    quoted
}

/// The image of a machine as the file names it, or as its reference reads.
fn image_of(e: &Engine<'_>, m: &Machine) -> String {
    if let Some(project) = &e.project {
        if let Ok(service) = super::convert::service(project, &m.service) {
            if let Some(image) = service.get("image").and_then(|v| v.as_str()) {
                return image.to_string();
            }
        }
    }
    short_image(&m.string("reference"))
}

fn json_line(fields: &[(&str, serde_json::Value)]) -> String {
    let mut sorted: Vec<&(&str, serde_json::Value)> = fields.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    let body: Vec<String> = sorted
        .iter()
        .map(|(k, v)| format!("{}:{}", serde_json::Value::from(*k), v))
        .collect();
    format!("{{{}}}", body.join(","))
}

/// A Go template of --format, `{{.Field}}` replaced; `table` at its start makes a table
/// with the fields' names as headers.
fn template(format: &str, rows: &[BTreeMap<&str, String>]) -> String {
    let (table_mode, body) = match format.strip_prefix("table") {
        Some(rest) => (true, rest.trim_start()),
        None => (false, format),
    };
    let fill = |row: Option<&BTreeMap<&str, String>>| {
        let mut out = String::new();
        let mut rest = body;
        while let Some(at) = rest.find("{{") {
            out.push_str(&rest[..at]);
            let Some(end) = rest[at..].find("}}") else {
                out.push_str(&rest[at..]);
                rest = "";
                break;
            };
            let field = rest[at + 2..at + end].trim().trim_start_matches('.');
            match row {
                Some(row) => out.push_str(row.get(field).map(String::as_str).unwrap_or("")),
                None => out.push_str(&field.to_uppercase()),
            }
            rest = &rest[at + end + 2..];
        }
        out.push_str(rest);
        out.replace("\\t", "\t")
    };
    let mut lines = Vec::new();
    if table_mode {
        lines.push(fill(None));
    }
    for row in rows {
        lines.push(fill(Some(row)));
    }
    let text = lines.join("\n") + "\n";
    if table_mode {
        table(&text, 10, 3)
    } else {
        text
    }
}

pub async fn ps(e: &mut Engine<'_>, a: &PsArgs, out: &mut dyn Write) -> Result<()> {
    let now = now();
    let known: Option<BTreeSet<String>> = e.project.as_ref().map(|p| {
        p.services
            .keys()
            .chain(p.disabled_services.keys())
            .cloned()
            .collect()
    });
    let mut filters: Vec<String> = a.status.clone();
    if let Some(filter) = &a.filter {
        match filter.split_once('=') {
            Some(("status", value)) => filters.push(value.to_string()),
            Some((key, _)) => bail!("invalid filter {key:?}"),
            None => bail!("invalid filter {filter:?}"),
        }
    }
    let machines: Vec<Machine> = e
        .machines()
        .await?
        .into_iter()
        .filter(|m| a.service.is_empty() || a.service.contains(&m.service))
        .filter(|m| a.all || !filters.is_empty() || m.running())
        .filter(|m| filters.is_empty() || filters.iter().any(|f| f == state(m)))
        .filter(|m| {
            a.orphans != Some(false) || known.as_ref().is_none_or(|k| k.contains(&m.service))
        })
        .collect();
    if let Some(project) = &e.project {
        for name in &a.service {
            if !project.services.contains_key(name) && !project.disabled_services.contains_key(name)
            {
                bail!("no such service: {name}");
            }
        }
    }
    if a.services {
        let mut seen = Vec::new();
        for m in &machines {
            if !seen.contains(&m.service) {
                seen.push(m.service.clone());
            }
        }
        for s in seen {
            writeln!(out, "{s}")?;
        }
        return Ok(());
    }
    if a.quiet {
        for m in &machines {
            writeln!(out, "{}", m.name)?;
        }
        return Ok(());
    }
    let format = a.format.clone().unwrap_or_else(|| "table".to_string());
    let rows: Vec<BTreeMap<&str, String>> = machines
        .iter()
        .map(|m| {
            BTreeMap::from([
                ("ID", m.name.clone()),
                ("Name", m.name.clone()),
                ("Names", m.name.clone()),
                ("Image", image_of(e, m)),
                ("Command", command(m, !a.no_trunc)),
                ("Project", m.label(label::PROJECT).to_string()),
                ("Service", m.service.clone()),
                (
                    "RunningFor",
                    format!("{} ago", human_duration(now.saturating_sub(m.created))),
                ),
                ("Status", status(m, now)),
                ("State", state(m).to_string()),
                ("Health", m.health.clone()),
                ("ExitCode", m.exit_code.unwrap_or(0).to_string()),
                ("Ports", ports(m).join(", ")),
                ("Networks", m.strings("networks").join(",")),
            ])
        })
        .collect();
    match format.as_str() {
        "table" => {
            let mut text = String::from("NAME\tIMAGE\tCOMMAND\tSERVICE\tCREATED\tSTATUS\tPORTS\n");
            for r in &rows {
                text.push_str(&format!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                    r["Name"],
                    r["Image"],
                    r["Command"],
                    r["Service"],
                    r["RunningFor"],
                    r["Status"],
                    r["Ports"]
                ));
            }
            out.write_all(table(&text, 10, 3).as_bytes())?;
        }
        "json" => {
            for (m, r) in machines.iter().zip(&rows) {
                let labels: Vec<String> =
                    m.labels.iter().map(|(k, v)| format!("{k}={v}")).collect();
                let publishers: Vec<serde_json::Value> = m
                    .strings("ports")
                    .iter()
                    .filter_map(|p| publisher(p))
                    .collect();
                let fields = [
                    ("Command", serde_json::Value::from(r["Command"].clone())),
                    ("CreatedAt", serde_json::Value::from(created_at(m.created))),
                    (
                        "ExitCode",
                        serde_json::Value::from(m.exit_code.unwrap_or(0)),
                    ),
                    ("Health", serde_json::Value::from(m.health.clone())),
                    ("ID", serde_json::Value::from(m.name.clone())),
                    ("Image", serde_json::Value::from(r["Image"].clone())),
                    ("Labels", serde_json::Value::from(labels.join(","))),
                    (
                        "LocalVolumes",
                        serde_json::Value::from(m.strings("volumes").len().to_string()),
                    ),
                    (
                        "Mounts",
                        serde_json::Value::from(m.strings("volumes").join(",")),
                    ),
                    ("Name", serde_json::Value::from(m.name.clone())),
                    ("Names", serde_json::Value::from(m.name.clone())),
                    ("Networks", serde_json::Value::from(r["Networks"].clone())),
                    ("Ports", serde_json::Value::from(r["Ports"].clone())),
                    ("Project", serde_json::Value::from(r["Project"].clone())),
                    ("Publishers", serde_json::Value::Array(publishers)),
                    (
                        "RunningFor",
                        serde_json::Value::from(r["RunningFor"].clone()),
                    ),
                    ("Service", serde_json::Value::from(m.service.clone())),
                    ("Size", serde_json::Value::from("0B")),
                    ("State", serde_json::Value::from(r["State"].clone())),
                    ("Status", serde_json::Value::from(r["Status"].clone())),
                ];
                writeln!(out, "{}", json_line(&fields))?;
            }
        }
        other => out.write_all(template(other, &rows).as_bytes())?,
    }
    Ok(())
}

/// A published port of the record ("[IP:]HOST->CONTAINER/PROTO") as a Publisher of ps.
fn publisher(port: &str) -> Option<serde_json::Value> {
    let (host, rest) = port.split_once("->")?;
    let (target, protocol) = rest.split_once('/')?;
    let (url, published) = match host.rsplit_once(':') {
        Some((ip, p)) => (ip.to_string(), p),
        None => ("0.0.0.0".to_string(), host),
    };
    Some(serde_json::json!({
        "URL": url,
        "TargetPort": target.parse::<u64>().ok()?,
        "PublishedPort": published.parse::<u64>().ok()?,
        "Protocol": protocol,
    }))
}

/// CreatedAt of ps: "2006-01-02 15:04:05 -0700 MST" in UTC.
fn created_at(unix: u64) -> String {
    let days = (unix / 86_400) as i64;
    let secs = unix % 86_400;
    let (y, m, d) = civil(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} +0000 UTC",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

/// The date of a day counted from 1970-01-01.
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub async fn ls(e: &mut Engine<'_>, a: &LsArgs, out: &mut dyn Write) -> Result<()> {
    if let Some(word) = a.args.first() {
        bail!("unknown command {word:?} for \"docker compose ls\"");
    }
    let mut projects: BTreeMap<String, (Vec<String>, String)> = BTreeMap::new();
    for dict in e
        .client
        .manager
        .list_machines(true)
        .await
        .map_err(client::error)?
    {
        let m = Machine::from_dict(dict);
        let Some(project) = m.labels.get(label::PROJECT).cloned() else {
            continue;
        };
        if client::string(&m.dict, "origin") != "create" {
            continue;
        }
        let entry = projects.entry(project).or_default();
        entry.0.push(state(&m).to_string());
        if entry.1.is_empty() {
            entry.1 = m.label(label::CONFIG_FILES).to_string();
        }
    }
    let mut names: Vec<String> = Vec::new();
    for filter in &a.filter {
        match filter.split_once('=') {
            Some(("name", value)) => names.push(value.to_string()),
            _ => bail!("invalid filter {filter:?}"),
        }
    }
    let rows: Vec<(String, String, String)> = projects
        .into_iter()
        .filter(|(_, (states, _))| a.all || states.iter().any(|s| s == "running"))
        .filter(|(name, _)| names.is_empty() || names.iter().any(|n| name.contains(n.as_str())))
        .map(|(name, (states, files))| {
            let mut counts: BTreeMap<String, usize> = BTreeMap::new();
            for s in states {
                *counts.entry(s).or_default() += 1;
            }
            let status: Vec<String> = counts.iter().map(|(s, n)| format!("{s}({n})")).collect();
            (name, status.join(", "), files)
        })
        .collect();
    if a.quiet {
        for (name, _, _) in &rows {
            writeln!(out, "{name}")?;
        }
        return Ok(());
    }
    match a.format.as_deref().unwrap_or("table") {
        "json" => {
            let items: Vec<String> = rows
                .iter()
                .map(|(n, s, f)| {
                    json_line(&[
                        ("Name", serde_json::Value::from(n.clone())),
                        ("Status", serde_json::Value::from(s.clone())),
                        ("ConfigFiles", serde_json::Value::from(f.clone())),
                    ])
                })
                .collect();
            writeln!(out, "[{}]", items.join(","))?;
        }
        _ => {
            let mut text = String::from("NAME\tSTATUS\tCONFIG FILES\n");
            for (n, s, f) in &rows {
                text.push_str(&format!("{n}\t{s}\t{f}\n"));
            }
            out.write_all(table(&text, 20, 3).as_bytes())?;
        }
    }
    Ok(())
}

pub async fn images(e: &mut Engine<'_>, a: &ImagesArgs, out: &mut dyn Write) -> Result<()> {
    let machines: Vec<Machine> = e
        .machines()
        .await?
        .into_iter()
        .filter(|m| a.service.is_empty() || a.service.contains(&m.service))
        .collect();
    let images: Vec<Dict> = e
        .client
        .manager
        .list_images()
        .await
        .map_err(client::error)?;
    let size_of = |reference: &str| {
        images
            .iter()
            .find(|i| {
                client::string(i, "reference") == reference
                    && matches!(client::string(i, "origin").as_str(), "pull" | "build")
            })
            .map(|i| client::u64(i, "size"))
            .unwrap_or(0)
    };
    let now = now();
    let platform = format!("linux/{}", go_arch());
    let mut rows = Vec::new();
    for m in &machines {
        let reference = short_image(&m.string("reference"));
        let (repository, tag) = match reference.rsplit_once(':') {
            Some((r, t)) if !t.contains('/') => (r.to_string(), t.to_string()),
            _ => (reference.clone(), "latest".to_string()),
        };
        let id: String = m
            .string("digest")
            .trim_start_matches("sha256:")
            .chars()
            .take(12)
            .collect();
        rows.push((
            m.name.clone(),
            repository,
            tag,
            id,
            human_size(size_of(&m.string("reference"))),
            format!("{} ago", human_duration(now.saturating_sub(m.created))),
        ));
    }
    if a.quiet {
        let mut seen = BTreeSet::new();
        for r in &rows {
            if seen.insert(r.3.clone()) {
                writeln!(out, "{}", r.3)?;
            }
        }
        return Ok(());
    }
    if a.format.as_deref() == Some("json") {
        let items: Vec<String> = rows
            .iter()
            .map(|r| {
                json_line(&[
                    ("ContainerName", serde_json::Value::from(r.0.clone())),
                    ("Repository", serde_json::Value::from(r.1.clone())),
                    ("Tag", serde_json::Value::from(r.2.clone())),
                    ("Platform", serde_json::Value::from(platform.clone())),
                    ("ID", serde_json::Value::from(r.3.clone())),
                ])
            })
            .collect();
        writeln!(out, "[{}]", items.join(","))?;
        return Ok(());
    }
    let mut text = String::from("CONTAINER\tREPOSITORY\tTAG\tPLATFORM\tIMAGE ID\tSIZE\tCREATED\n");
    for r in &rows {
        text.push_str(&format!(
            "{}\t{}\t{}\t{platform}\t{}\t{}\t{}\n",
            r.0, r.1, r.2, r.3, r.4, r.5
        ));
    }
    out.write_all(table(&text, 20, 3).as_bytes())?;
    Ok(())
}

/// GOARCH of this host, the name its platform goes by in images.
fn go_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "arm" => "arm",
        "powerpc64" => "ppc64le",
        "s390x" => "s390x",
        "riscv64" => "riscv64",
        other => other,
    }
}

pub async fn top(e: &mut Engine<'_>, a: &TopArgs, out: &mut dyn Write) -> Result<()> {
    let machines: Vec<Machine> = e
        .machines()
        .await?
        .into_iter()
        .filter(|m| a.service.is_empty() || a.service.contains(&m.service))
        .filter(Machine::running)
        .collect();
    let mut text = String::from("SERVICE\t#\tPID\tUSER\tTIME\tCOMMAND\n");
    for m in &machines {
        let processes = e
            .client
            .manager
            .machine_processes(&m.name)
            .await
            .map_err(client::error)?;
        for p in processes {
            text.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\t{}\n",
                m.service,
                m.number,
                client::maybe_u64(&p, "pid").unwrap_or(0),
                client::string(&p, "user"),
                client::string(&p, "time"),
                client::string(&p, "command")
            ));
        }
    }
    if !machines.is_empty() {
        out.write_all(table(&text, 4, 2).as_bytes())?;
    }
    Ok(())
}

pub async fn port(e: &mut Engine<'_>, a: &PortArgs, out: &mut dyn Write) -> Result<()> {
    let count = usize::from(a.service.is_some()) + usize::from(a.private_port.is_some());
    let Some(service) = &a.service else {
        bail!("accepts between 1 and 2 arg(s), received {count}");
    };
    if !e.project().services.contains_key(service) {
        bail!("no such service: {service}");
    }
    let index = a.index.unwrap_or(1).max(1) as u32;
    let protocol = a.protocol.clone().unwrap_or_else(|| "tcp".to_string());
    let machines = e.machines().await?;
    let Some(m) = machines
        .iter()
        .find(|m| m.service == *service && !m.oneoff && m.number == index)
    else {
        bail!("service {service:?} is not running");
    };
    let published = m.strings("ports");
    match &a.private_port {
        Some(private) => {
            let found = published.iter().find(|p| {
                p.split_once("->")
                    .is_some_and(|(_, rest)| rest == format!("{private}/{protocol}"))
            });
            match found {
                Some(p) => {
                    let host = p.split_once("->").map(|(h, _)| h).unwrap_or("");
                    let host = if host.contains(':') {
                        host.to_string()
                    } else {
                        format!("0.0.0.0:{host}")
                    };
                    writeln!(out, "{host}")?;
                }
                None => bail!("no port {private}/{protocol} for container {}", m.name),
            }
        }
        None => {
            for p in &published {
                if let Some((host, rest)) = p.split_once("->") {
                    let host = if host.contains(':') {
                        host.to_string()
                    } else {
                        format!("0.0.0.0:{host}")
                    };
                    writeln!(out, "{rest} -> {host}")?;
                }
            }
        }
    }
    Ok(())
}

pub async fn volumes(e: &mut Engine<'_>, a: &VolumesArgs, out: &mut dyn Write) -> Result<()> {
    let project_volumes: BTreeSet<String> = match &e.project {
        Some(project) => project
            .volumes
            .keys()
            .map(|k| super::convert::volume_name(project, k))
            .collect(),
        None => BTreeSet::new(),
    };
    let machines: Vec<String> = e
        .machines()
        .await?
        .into_iter()
        .filter(|m| a.service.is_empty() || a.service.contains(&m.service))
        .map(|m| m.name)
        .collect();
    let volumes: Vec<String> = e
        .client
        .manager
        .list_volumes()
        .await
        .map_err(client::error)?
        .iter()
        .filter(|v| {
            let name = client::string(v, "name");
            project_volumes.contains(&name)
                || client::strings(v, "used_by")
                    .iter()
                    .any(|u| machines.contains(u))
        })
        .map(|v| client::string(v, "name"))
        .collect();
    if a.quiet {
        for v in &volumes {
            writeln!(out, "{v}")?;
        }
        return Ok(());
    }
    if a.format.as_deref() == Some("json") {
        for v in &volumes {
            writeln!(
                out,
                "{}",
                json_line(&[
                    ("Driver", serde_json::Value::from("local")),
                    ("Name", serde_json::Value::from(v.clone())),
                ])
            )?;
        }
        return Ok(());
    }
    let mut text = String::from("DRIVER\tVOLUME NAME\n");
    for v in &volumes {
        text.push_str(&format!("local\t{v}\n"));
    }
    out.write_all(table(&text, 10, 3).as_bytes())?;
    Ok(())
}

pub async fn stats(e: &mut Engine<'_>, a: &StatsArgs, _out: &mut dyn Write) -> Result<()> {
    let names: Vec<String> = e
        .machines()
        .await?
        .into_iter()
        .filter(|m| a.service.is_empty() || a.service.contains(&m.service))
        .filter(|m| a.all || m.running())
        .map(|m| m.name)
        .collect();
    if names.is_empty() {
        return Ok(());
    }
    let args = crate::cli::StatsArgs {
        names,
        no_stream: a.no_stream,
        json: a.format.as_deref() == Some("json"),
    };
    crate::commands::stats::stats(args, &e.client).await
}

/// The events of the project's machines: a text line each, or a JSON object with the
/// service and the action's name.
pub async fn events(e: &mut Engine<'_>, a: &EventsArgs, out: &mut dyn Write) -> Result<()> {
    use std::io::BufRead;
    use std::os::fd::OwnedFd;
    let mut options = crate::client::Options::new();
    if let Some(since) = &a.since {
        options.insert("since", zbus::zvariant::Value::from(since.clone()));
    }
    if let Some(until) = &a.until {
        options.insert("until", zbus::zvariant::Value::from(until.clone()));
    }
    options.insert(
        "filters",
        zbus::zvariant::Value::from(vec![
            "type=machine".to_string(),
            format!("label={}={}", label::PROJECT, e.name),
        ]),
    );
    let (mut fds, _process) = e
        .client
        .manager
        .events(options)
        .await
        .map_err(client::error)?;
    let Some(stdout) = fds.remove("stdout") else {
        bail!("the service returned no stream for the events");
    };
    let services = a.service.clone();
    let json = a.json;
    tokio::task::block_in_place(|| -> Result<()> {
        let reader = std::io::BufReader::new(std::fs::File::from(OwnedFd::from(stdout)));
        for line in reader.lines() {
            let Ok(event) = serde_json::from_str::<serde_json::Value>(&line?) else {
                continue;
            };
            let text = |key: &str| {
                event
                    .get(key)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string()
            };
            let labels = event
                .get("labels")
                .and_then(|v| v.as_object())
                .cloned()
                .unwrap_or_default();
            let service = labels
                .get(label::SERVICE)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if !services.is_empty() && !services.contains(&service) {
                continue;
            }
            let mut attributes: BTreeMap<String, String> = event
                .get("attributes")
                .and_then(|v| v.as_object())
                .map(|m| {
                    m.iter()
                        .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                        .collect()
                })
                .unwrap_or_default();
            attributes.insert("name".to_string(), text("name"));
            let mut action = text("action");
            if action == "health_status" {
                action = format!(
                    "health_status: {}",
                    attributes.get("status").cloned().unwrap_or_default()
                );
            }
            let usec = event.get("time_usec").and_then(|v| v.as_u64()).unwrap_or(0);
            if json {
                let object = serde_json::json!({
                    "action": action,
                    "attributes": attributes,
                    "id": text("name"),
                    "service": service,
                    "time": text("time"),
                    "type": "container",
                });
                writeln!(out, "{object}")?;
            } else {
                let attrs: Vec<String> =
                    attributes.iter().map(|(k, v)| format!("{k}={v}")).collect();
                writeln!(
                    out,
                    "{} container {action} {} ({})\n",
                    local_time(usec),
                    text("name"),
                    attrs.join(", ")
                )?;
            }
            out.flush()?;
        }
        Ok(())
    })
}

/// A time in microseconds as Go's "2006-01-02 15:04:05.000000" writes it, in the local
/// zone.
fn local_time(usec: u64) -> String {
    let secs = (usec / 1_000_000) as i64;
    let local = secs + crate::compose::gotime::local_offset(secs);
    let (y, m, d) = civil(local.div_euclid(86_400));
    let t = local.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}.{:06}",
        t / 3600,
        t % 3600 / 60,
        t % 60,
        usec % 1_000_000
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_and_sizes_read_as_docker_writes_them() {
        assert_eq!(human_duration(0), "Less than a second");
        assert_eq!(human_duration(2), "2 seconds");
        assert_eq!(human_duration(90), "About a minute");
        assert_eq!(human_duration(600), "10 minutes");
        assert_eq!(human_duration(3600), "About an hour");
        assert_eq!(human_size(4_420_000), "4.42MB");
        assert_eq!(human_size(69_000_000), "69MB");
        assert_eq!(human_size(0), "0B");
        assert_eq!(created_at(1_790_000_000), "2026-09-21 14:13:20 +0000 UTC");
        assert_eq!(
            short_image("docker.io/library/nginx:alpine"),
            "nginx:alpine"
        );
        assert_eq!(short_image("ghcr.io/a/b:1"), "ghcr.io/a/b:1");
    }

    #[test]
    fn templates_fill_the_fields_named() {
        let rows = vec![BTreeMap::from([
            ("Name", "a".to_string()),
            ("State", "running".to_string()),
        ])];
        assert_eq!(template("{{.Name}} {{.State}}", &rows), "a running\n");
        assert_eq!(
            template("table {{.Name}}\\t{{.State}}", &rows),
            "NAME      STATE\na         running\n"
        );
    }
}
