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
use crate::compose::variables::{columns_table, table};

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
        // A machine shutting down still runs, as a container being stopped does.
        "running" | "opening" | "closing" => "running",
        "paused" => "paused",
        "restarting" => "restarting",
        "starting" => "created",
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
        "restarting" | "exited" => {
            let word = if state(m) == "exited" {
                "Exited"
            } else {
                "Restarting"
            };
            let finished = client::u64(&m.dict, "finished");
            crate::commands::machines::ended_status(word, m.exit_code.unwrap_or(0), finished, now)
        }
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
    let text = crate::dockerapi::util::command_line(&m.strings("command"));
    let text = if trunc {
        crate::gotemplate::ellipsis(&text, 20)
    } else {
        text
    };
    crate::gotemplate::go_quote(&text)
}

/// The image a machine was made from: as the file names it while that is the image, as
/// its reference reads otherwise (the file names another one now, or none).
fn image_of(e: &Engine<'_>, m: &Machine) -> String {
    let reference = m.string("reference");
    if let Some(project) = &e.project {
        if let Ok(service) = super::convert::service(project, &m.service) {
            if let Some(image) = service.get("image").and_then(|v| v.as_str()) {
                if crate::reference::ImageRef::normalized(image)
                    .is_ok_and(|r| r.to_string() == reference)
                {
                    return image.to_string();
                }
            }
        }
    }
    short_image(&reference)
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
        // Without -a: what runs or restarts, one-offs left out.
        .filter(|m| {
            a.all
                || !filters.is_empty()
                || (!m.oneoff && matches!(state(m), "running" | "paused" | "restarting"))
        })
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
            out.write_all(columns_table(&text, 10, 3).as_bytes())?;
        }
        "json" => {
            for (m, r) in machines.iter().zip(&rows) {
                writeln!(out, "{}", json_line(&ps_fields(m, r)))?;
            }
        }
        other => {
            let objects: Vec<serde_json::Value> = machines
                .iter()
                .zip(&rows)
                .map(|(m, r)| {
                    let mut row: serde_json::Map<String, serde_json::Value> = ps_fields(m, r)
                        .into_iter()
                        .map(|(k, v)| (k.to_string(), v))
                        .collect();
                    // What the rows' Label method reads.
                    row.insert("LabelsMap".into(), serde_json::json!(m.labels));
                    serde_json::Value::Object(row)
                })
                .collect();
            let headers = serde_json::json!({
                "ID": "CONTAINER ID", "Name": "NAME", "Project": "PROJECT", "Service": "SERVICE",
                "Image": "IMAGE", "Command": "COMMAND", "CreatedAt": "CREATED AT",
                "RunningFor": "CREATED", "Status": "STATUS", "Size": "SIZE", "Labels": "LABELS",
                "State": "STATE", "Ports": "PORTS",
            });
            let text = crate::gotemplate::format_list(other, PS_TABLE, &headers, &objects)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            out.write_all(text.as_bytes())?;
        }
    }
    Ok(())
}

/// The default table of ps, as docker compose has it.
const PS_TABLE: &str =
    "table {{.Name}}\t{{.Image}}\t{{.Command}}\t{{.Service}}\t{{.RunningFor}}\t{{.Status}}\t{{.Ports}}";

/// A machine as ps --format sees it, the fields in docker compose's order.
fn ps_fields(m: &Machine, r: &BTreeMap<&str, String>) -> Vec<(&'static str, serde_json::Value)> {
    let labels: Vec<String> = m.labels.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let publishers: Vec<serde_json::Value> = m
        .strings("ports")
        .iter()
        .filter_map(|p| publisher(p))
        .collect();
    vec![
        ("Command", serde_json::Value::from(r["Command"].clone())),
        (
            "CreatedAt",
            serde_json::Value::from(crate::compose::gotime::time_string(m.created as i64)),
        ),
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
    ]
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
        if client::string(&m.dict, "origin") != "create"
            || !m.labels.contains_key(label::CONFIG_HASH)
        {
            continue;
        }
        // Without --all: running, paused or restarting machines.
        let state = state(&m).to_string();
        if !a.all && !matches!(state.as_str(), "running" | "paused" | "restarting") {
            continue;
        }
        let entry = projects.entry(project).or_default();
        entry.0.push(state);
        // combinedConfigFiles: every machine's, each once, in order.
        let mut files: Vec<String> = entry
            .1
            .split(',')
            .filter(|f| !f.is_empty())
            .map(str::to_string)
            .collect();
        for file in m.label(label::CONFIG_FILES).split(',') {
            if !file.is_empty() && !files.iter().any(|f| f == file) {
                files.push(file.to_string());
            }
        }
        entry.1 = files.join(",");
    }
    let mut names: Vec<String> = Vec::new();
    for filter in &a.filter {
        match filter.split_once('=') {
            Some(("name", value)) => names.push(value.to_string()),
            _ => bail!("invalid filter {filter:?}"),
        }
    }
    // filters.Match: each value a regular expression, one that does not compile no
    // match.
    let names: Vec<regex::Regex> = names
        .iter()
        .filter_map(|n| regex::Regex::new(n).ok())
        .collect();
    let filtered = !a.filter.is_empty();
    let rows: Vec<(String, String, String)> = projects
        .into_iter()
        .filter(|(name, _)| !filtered || names.iter().any(|n| n.is_match(name)))
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
    // The image a machine was made from, while it is here: its size, platform and times.
    let image_of = |reference: &str| {
        images.iter().find(|i| {
            client::string(i, "reference") == reference
                && matches!(client::string(i, "origin").as_str(), "pull" | "build")
        })
    };
    let mut rows: Vec<ImageRow> = machines
        .iter()
        .map(|m| {
            let reference = m.string("reference");
            let (repository, tag) = repository_and_tag(&reference);
            let image = image_of(&reference);
            ImageRow {
                container: m.name.clone(),
                id: m.string("digest"),
                repository,
                tag,
                platform: image
                    .map_or_else(|| "unknown".to_string(), |_| format!("linux/{}", go_arch())),
                size: image.map_or(0, |i| client::u64(i, "size")),
                created: image.map(|i| client::u64(i, "created")).filter(|t| *t > 0),
                tagged: image.map_or(0, |i| client::u64(i, "pulled")),
            }
        })
        .collect();
    rows.sort_by(|a, b| a.container.cmp(&b.container));
    if a.quiet {
        // Whole IDs without their algorithm, each once.
        let mut seen = BTreeSet::new();
        for row in &rows {
            let id = row
                .id
                .split_once(':')
                .map_or(row.id.as_str(), |(_, hex)| hex);
            if seen.insert(id.to_string()) {
                writeln!(out, "{id}")?;
            }
        }
        return Ok(());
    }
    let format = a.format.as_deref().unwrap_or("");
    // JSON as Go's encoder writes a line, then Println's newline.
    if format == "json" {
        let text = if rows.is_empty() {
            "null".to_string()
        } else {
            let items: Vec<String> = rows.iter().map(|r| r.listed()).collect();
            format!("[{}]", items.join(","))
        };
        writeln!(out, "{text}\n")?;
        return Ok(());
    }
    match format.to_lowercase().as_str() {
        "table" | "pretty" | "" => {
            let now = now();
            let mut text =
                String::from("CONTAINER\tREPOSITORY\tTAG\tPLATFORM\tIMAGE ID\tSIZE\tCREATED\n");
            for r in &rows {
                let none = |s: &str| {
                    if s.is_empty() {
                        "<none>".to_string()
                    } else {
                        s.to_string()
                    }
                };
                let id: String =
                    r.id.trim_start_matches("sha256:")
                        .chars()
                        .take(12)
                        .collect();
                let created = r.created.map_or_else(
                    || "N/A".to_string(),
                    |t| format!("{} ago", human_duration(now.saturating_sub(t))),
                );
                text.push_str(&format!(
                    "{}\t{}\t{}\t{}\t{id}\t{}\t{created}\n",
                    r.container,
                    none(&r.repository),
                    none(&r.tag),
                    r.platform,
                    human_size(r.size)
                ));
            }
            out.write_all(table(&text, 20, 3).as_bytes())?;
        }
        // The map by container, which the formatter indents for both.
        "{{json.}}" | "json" => writeln!(out, "{}\n", images_map(&rows))?,
        _ => bail!(
            "format value {} could not be parsed: parsing failed",
            crate::compose::golib::go_quote(format)
        ),
    }
    Ok(())
}

/// A machine's image as compose images shows it.
struct ImageRow {
    container: String,
    id: String,
    repository: String,
    tag: String,
    platform: String,
    size: u64,
    /// When the image was made, as its config says.
    created: Option<u64>,
    /// When it was pulled or built here, 0 when it is gone.
    tagged: u64,
}

impl ImageRow {
    fn created_json(&self) -> Option<String> {
        self.created
            .map(|t| crate::compose::gotime::rfc3339(t as i64, 0))
    }

    fn tagged_json(&self) -> String {
        if self.tagged == 0 {
            return "0001-01-01T00:00:00Z".to_string();
        }
        let t = self.tagged as i64;
        crate::compose::gotime::rfc3339(t, crate::compose::gotime::local_offset(t))
    }

    /// One entry of `--format json`, the fields in docker compose's order.
    fn listed(&self) -> String {
        let q = |s: &str| serde_json::Value::from(s).to_string();
        let mut out = format!(
            "{{\"ID\":{},\"ContainerName\":{},\"Repository\":{},\"Tag\":{},\"Platform\":{},\"Size\":{}",
            q(&self.id),
            q(&self.container),
            q(&self.repository),
            q(&self.tag),
            q(&self.platform),
            self.size
        );
        if let Some(created) = self.created_json() {
            out.push_str(&format!(",\"Created\":{}", q(&created)));
        }
        if self.tagged > 0 {
            out.push_str(&format!(",\"LastTagTime\":{}", q(&self.tagged_json())));
        }
        out.push('}');
        out
    }
}

/// The images by container, as compose's formatter writes a map: indented by four spaces
/// (ToStandardJSON), for `{{json.}}` and the spellings of json other than `json`, which
/// compose writes as a list of its own.
fn images_map(rows: &[ImageRow]) -> String {
    let q = |s: &str| serde_json::Value::from(s).to_string();
    let (nl, i1, i2, i3, colon) = ("\n", "    ", "        ", "            ", ": ");
    if rows.is_empty() {
        return "{}".to_string();
    }
    let mut out = format!("{{{nl}");
    for (n, r) in rows.iter().enumerate() {
        let (arch, os) = match r.platform.split_once('/') {
            Some((os, arch)) => (arch.to_string(), os.to_string()),
            None => (String::new(), String::new()),
        };
        let created = r
            .created_json()
            .map_or_else(|| "null".to_string(), |c| q(&c));
        let fields = [
            ("ID", q(&r.id)),
            ("Repository", q(&r.repository)),
            ("Tag", q(&r.tag)),
            (
                "Platform",
                format!(
                    "{{{nl}{i3}\"architecture\"{colon}{},{nl}{i3}\"os\"{colon}{}{nl}{i2}}}",
                    q(&arch),
                    q(&os)
                ),
            ),
            ("Size", r.size.to_string()),
            ("Created", created),
            ("LastTagTime", q(&r.tagged_json())),
        ];
        let body: Vec<String> = fields
            .iter()
            .map(|(k, v)| format!("{i2}{}{colon}{v}", q(k)))
            .collect();
        out.push_str(&format!(
            "{i1}{}{colon}{{{nl}{}{nl}{i1}}}",
            q(&r.container),
            body.join(&format!(",{nl}"))
        ));
        if n + 1 < rows.len() {
            out.push(',');
        }
        out.push_str(nl);
    }
    out.push('}');
    out
}

/// The repository and tag of a machine's image as docker compose reads them from the
/// image it was made from: its familiar name, and no tag for one by digest.
fn repository_and_tag(reference: &str) -> (String, String) {
    match crate::reference::ImageRef::normalized(reference) {
        Ok(r) => (
            crate::dockerapi::util::familiar(&format!("{}/{}", r.registry, r.repository)),
            if r.digest.is_some() {
                String::new()
            } else {
                r.tag.unwrap_or_default()
            },
        ),
        Err(_) => (String::new(), String::new()),
    }
}

/// GOARCH of this host, the name its platform goes by in images.
pub(super) fn go_arch() -> &'static str {
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

/// PRIVATE_PORT of port, as compose reads it before anything else: a number up to 65535.
pub fn private_port(text: &str) -> Result<u16> {
    use crate::compose::gostr;
    gostr::parse_uint(text, 10, 16)
        .map(|n| n as u16)
        .map_err(|e| anyhow::anyhow!(gostr::num_error("ParseUint", text, e)))
}

pub async fn port(e: &mut Engine<'_>, a: &PortArgs, out: &mut dyn Write) -> Result<()> {
    let count = usize::from(a.service.is_some()) + usize::from(a.private_port.is_some());
    let Some(service) = &a.service else {
        bail!("accepts between 1 and 2 arg(s), received {count}");
    };
    if e.project
        .as_ref()
        .is_some_and(|p| !p.services.contains_key(service))
    {
        bail!("no such service: {service}");
    }
    // getSpecifiedContainer: running machines of the service, the one of --index, else
    // the lowest number, one-offs last.
    let index = a.index.filter(|i| *i > 0).map(|i| i as u32);
    let protocol = a.protocol.as_deref().unwrap_or("tcp").to_lowercase();
    let machines = e.machines().await?;
    let mut candidates: Vec<&Machine> = machines
        .iter()
        .filter(|m| m.service == *service && m.running())
        .filter(|m| index.is_none_or(|i| m.number == i))
        .collect();
    candidates.sort_by_key(|m| (m.oneoff, m.number));
    let Some(m) = candidates.first() else {
        match index {
            Some(i) => bail!("service {service:?} is not running container #{i}"),
            None => bail!("service {service:?} is not running"),
        }
    };
    let published = m.strings("ports");
    match &a.private_port {
        Some(private) => {
            let private = private_port(private)?;
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
                    // --protocol narrows the listing.
                    if a.protocol.is_some() && !rest.ends_with(&format!("/{protocol}")) {
                        continue;
                    }
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
    let listed: Vec<(String, String)> = e
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
        .map(|v| (client::string(v, "name"), client::string(v, "path")))
        .collect();
    if a.quiet {
        for (v, _) in &listed {
            writeln!(out, "{v}")?;
        }
        return Ok(());
    }
    // A volume as --format templates see one.
    let fields = |name: &str, path: &str| {
        let na = || serde_json::Value::from("N/A");
        vec![
            ("Availability", na()),
            ("Driver", serde_json::Value::from("local")),
            ("Group", na()),
            ("Labels", serde_json::Value::from("")),
            ("Links", na()),
            ("Mountpoint", serde_json::Value::from(path)),
            ("Name", serde_json::Value::from(name)),
            ("Scope", serde_json::Value::from("local")),
            ("Size", na()),
            ("Status", na()),
        ]
    };
    match a.format.as_deref() {
        Some("json") => {
            for (name, path) in &listed {
                writeln!(out, "{}", json_line(&fields(name, path)))?;
            }
        }
        None | Some("table") => {
            let mut text = String::from("DRIVER\tVOLUME NAME\n");
            for (v, _) in &listed {
                text.push_str(&format!("local\t{v}\n"));
            }
            out.write_all(columns_table(&text, 10, 3).as_bytes())?;
        }
        Some(format) => {
            let objects: Vec<serde_json::Value> = listed
                .iter()
                .map(|(name, path)| {
                    let mut row: serde_json::Map<String, serde_json::Value> = fields(name, path)
                        .into_iter()
                        .map(|(k, v)| (k.to_string(), v))
                        .collect();
                    // nspawn's volumes have no labels: Label reads "".
                    row.insert("LabelsMap".into(), serde_json::json!({}));
                    serde_json::Value::Object(row)
                })
                .collect();
            let headers = serde_json::json!({
                "ID": "ID", "Name": "VOLUME NAME", "Group": "GROUP", "Driver": "DRIVER",
                "Scope": "SCOPE", "Availability": "AVAILABILITY", "Mountpoint": "MOUNTPOINT",
                "Labels": "LABELS", "Links": "LINKS", "Size": "SIZE", "Status": "STATUS",
            });
            let text = crate::gotemplate::format_list(
                format,
                "table {{.Driver}}\t{{.Name}}",
                &headers,
                &objects,
            )
            .map_err(|e| anyhow::anyhow!("{e}"))?;
            out.write_all(text.as_bytes())?;
        }
    }
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
    if let Some(format) = a
        .format
        .as_deref()
        .filter(|f| !matches!(*f, "json" | "table"))
    {
        bail!("stats --format {format:?}: nspawn compose prints the table or json");
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
            // One-off machines are not the project's services to compose's events.
            if labels.get(label::ONEOFF).and_then(|v| v.as_str()) == Some("True") {
                continue;
            }
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
            // The machine's labels, which container events carry, compose's own left
            // out.
            for (k, v) in &labels {
                if !k.starts_with("com.docker.compose.") {
                    attributes
                        .entry(k.clone())
                        .or_insert_with(|| v.as_str().unwrap_or("").to_string());
                }
            }
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
        // docker/cli's Ellipsis: 19 columns and the ellipsis.
        assert_eq!(
            crate::gotemplate::ellipsis("/docker-entrypoint.sh nginx -g daemon off;", 20),
            "/docker-entrypoint.\u{2026}"
        );
        assert_eq!(crate::gotemplate::ellipsis("short", 20), "short");
        assert_eq!(
            crate::gotemplate::ellipsis("\u{4e2d}\u{6587}abc", 4),
            "\u{4e2d}\u{2026}"
        );
        // PRIVATE_PORT as strconv.ParseUint reads it.
        assert_eq!(private_port("053").unwrap(), 53);
        assert_eq!(
            private_port("70000").unwrap_err().to_string(),
            "strconv.ParseUint: parsing \"70000\": value out of range"
        );
        assert_eq!(
            private_port("+5").unwrap_err().to_string(),
            "strconv.ParseUint: parsing \"+5\": invalid syntax"
        );
        // Repository and tag of the image a machine was made from.
        assert_eq!(
            repository_and_tag("docker.io/library/nginx:1.27"),
            ("nginx".to_string(), "1.27".to_string())
        );
        let digest = format!("ghcr.io/o/app@sha256:{}", "a".repeat(64));
        assert_eq!(
            repository_and_tag(&digest),
            ("ghcr.io/o/app".to_string(), String::new())
        );
        assert_eq!(
            short_image("docker.io/library/nginx:alpine"),
            "nginx:alpine"
        );
        assert_eq!(short_image("ghcr.io/a/b:1"), "ghcr.io/a/b:1");
    }

    #[test]
    fn images_print_as_docker_compose_prints_them() {
        let row = |container: &str, created: Option<u64>, tagged: u64| ImageRow {
            container: container.to_string(),
            id: "sha256:abc".to_string(),
            repository: "nginx".to_string(),
            tag: "1.27".to_string(),
            platform: "linux/amd64".to_string(),
            size: 5,
            created,
            tagged,
        };
        let r = row("web", Some(1_729_771_200), 0);
        assert_eq!(
            r.listed(),
            r#"{"ID":"sha256:abc","ContainerName":"web","Repository":"nginx","Tag":"1.27","Platform":"linux/amd64","Size":5,"Created":"2024-10-24T12:00:00Z"}"#
        );
        assert_eq!(
            images_map(&[row("web", None, 0)]),
            "{\n    \"web\": {\n        \"ID\": \"sha256:abc\",\n        \"Repository\": \"nginx\",\n        \"Tag\": \"1.27\",\n        \"Platform\": {\n            \"architecture\": \"amd64\",\n            \"os\": \"linux\"\n        },\n        \"Size\": 5,\n        \"Created\": null,\n        \"LastTagTime\": \"0001-01-01T00:00:00Z\"\n    }\n}"
        );
        assert_eq!(images_map(&[]), "{}");
        // A row's labels by its Label method, and not in its JSON.
        let rows = [serde_json::json!({
            "Name": "web",
            "LabelsMap": { "com.docker.compose.service": "web" },
        })];
        let headers = serde_json::json!({ "Name": "NAME" });
        let label = r#"{{.Name}} {{.Label "com.docker.compose.service"}}{{.Label "x"}}"#;
        assert_eq!(
            crate::gotemplate::format_list(label, PS_TABLE, &headers, &rows).unwrap(),
            "web web\n"
        );
        assert_eq!(
            crate::gotemplate::format_list("{{json .}}", PS_TABLE, &headers, &rows).unwrap(),
            "{\"Name\":\"web\"}\n"
        );
    }

    #[test]
    fn a_machine_shutting_down_still_runs() {
        let m = |state: &str| Machine {
            name: "m".to_string(),
            service: "s".to_string(),
            number: 1,
            oneoff: false,
            state: state.to_string(),
            health: String::new(),
            exit_code: None,
            labels: BTreeMap::new(),
            created: 0,
            started: 1,
            dict: crate::client::Dict::new(),
        };
        assert_eq!(state(&m("closing")), "running");
        assert_eq!(state(&m("restarting")), "restarting");
        assert_eq!(state(&m("stopped")), "exited");
    }

    #[test]
    fn ps_formats_are_go_templates_with_docker_composes_headers() {
        let m = Machine {
            name: "p-web-1".to_string(),
            service: "web".to_string(),
            number: 1,
            oneoff: false,
            state: "running".to_string(),
            health: "healthy".to_string(),
            exit_code: None,
            labels: BTreeMap::new(),
            created: 0,
            started: 0,
            dict: crate::client::Dict::new(),
        };
        let r = BTreeMap::from([
            ("Command", "\"sleep\"".to_string()),
            ("Image", "busybox".to_string()),
            ("Networks", String::new()),
            ("Ports", String::new()),
            ("Project", "p".to_string()),
            ("RunningFor", "1 second ago".to_string()),
            ("State", "running".to_string()),
            ("Status", "Up".to_string()),
        ]);
        let row = serde_json::Value::Object(
            ps_fields(&m, &r)
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        );
        let headers = serde_json::json!({"Name": "NAME", "RunningFor": "CREATED"});
        let text = crate::gotemplate::format_list(
            "table {{.Name}}\t{{.RunningFor}}\t{{.Health | upper}}",
            PS_TABLE,
            &headers,
            &[row],
        )
        .unwrap();
        assert!(text.starts_with("NAME"), "{text}");
        assert!(text.contains("CREATED"), "{text}");
        assert!(
            text.contains("p-web-1") && text.contains("HEALTHY"),
            "{text}"
        );
    }
}
