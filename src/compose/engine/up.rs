//! up, create, pull and scale: what the file asks for made to exist: images, networks,
//! volumes and secrets first, then each service's
//! machines, kept, made or made anew, then started in the order of their dependencies.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use anyhow::{bail, Context as _, Result};
use zbus::zvariant::Value as Bus;

use super::convert::{self, Spec};
use super::progress::Status;
use super::{label, machine_name, valid_machine_name, Engine, Machine};
use crate::client::{self, Options};
use crate::compose::cli::{CreateArgs, PullArgs, ScaleArgs, UpArgs};
use crate::compose::project::Project;
use crate::compose::value::{Map, Value};

/// What a convergence is asked: the options of create, and those of start.
#[derive(Default)]
pub struct Converge {
    pub services: Vec<String>,
    pub force_recreate: bool,
    pub no_recreate: bool,
    pub always_recreate_deps: bool,
    pub remove_orphans: bool,
    pub scale: BTreeMap<String, u32>,
    pub pull: Option<String>,
    pub quiet_pull: bool,
    pub no_deps: bool,
    pub start: bool,
    pub wait: bool,
    pub wait_timeout: Option<Duration>,
    pub timeout: Option<u64>,
    /// How images are built where a service builds its own; none with --no-build.
    pub build: Option<super::build::Flags>,
    /// --build: every service with a build section is built.
    pub force_build: bool,
}

/// A local image a service runs.
#[derive(Clone)]
pub struct Image {
    /// The record nspawn keeps it under, which machines are made from.
    pub name: String,
    pub digest: String,
}

fn s<'a>(map: &'a Map, key: &str) -> &'a str {
    map.get(key).and_then(Value::as_str).unwrap_or("")
}

/// --scale SERVICE=NUM, as up and create read it.
pub fn scale_overrides(values: &[String]) -> Result<BTreeMap<String, u32>> {
    let mut overrides = BTreeMap::new();
    for value in values {
        let Some((service, n)) = value.split_once('=') else {
            bail!("invalid --scale option {value:?}. Should be SERVICE=NUM");
        };
        let n: u32 = n
            .parse()
            .with_context(|| format!("invalid --scale option {value:?}. Should be SERVICE=NUM"))?;
        overrides.insert(service.to_string(), n);
    }
    Ok(overrides)
}

/// The services of the project, each after those it depends on.
pub fn order(project: &Project) -> Vec<String> {
    fn visit(name: &str, project: &Project, seen: &mut BTreeSet<String>, out: &mut Vec<String>) {
        if !seen.insert(name.to_string()) {
            return;
        }
        if let Some(service) = project.services.get(name).and_then(Value::as_map) {
            for dep in dependencies(service) {
                if project.services.contains_key(&dep) {
                    visit(&dep, project, seen, out);
                }
            }
        }
        out.push(name.to_string());
    }
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for name in project.services.keys() {
        visit(name, project, &mut seen, &mut out);
    }
    out
}

/// What a service waits for: its depends_on, and the service whose network it takes.
pub fn dependencies(service: &Map) -> Vec<String> {
    let mut deps: Vec<String> = service
        .get("depends_on")
        .and_then(Value::as_map)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    if let Some(owner) = s(service, "network_mode").strip_prefix("service:") {
        if !deps.iter().any(|d| d == owner) {
            deps.push(owner.to_string());
        }
    }
    deps
}

/// Fails with what the support table refuses in the services that are to run and the
/// resources they use.
pub fn check_refusals(project: &Project) -> Result<()> {
    let mut lines = Vec::new();
    for refusal in &project.refused {
        let applies = if refusal.service.is_empty() {
            // The name may hold dots itself: a resource applies when the path goes on
            // from its whole name.
            let (kind, rest) = refusal.path.split_once('.').unwrap_or((&refusal.path, ""));
            let named = |map: &Map| {
                map.keys().any(|k| {
                    rest == k
                        || rest
                            .strip_prefix(k.as_str())
                            .is_some_and(|r| r.starts_with('.'))
                })
            };
            match kind {
                "networks" => named(&project.networks),
                "volumes" => named(&project.volumes),
                "secrets" => named(&project.secrets),
                "configs" => named(&project.configs),
                _ => true,
            }
        } else {
            project.services.contains_key(&refusal.service)
        };
        if applies {
            lines.push(refusal.line.clone());
        }
    }
    if !lines.is_empty() {
        bail!(lines.join("\n"));
    }
    Ok(())
}

pub async fn up(e: &mut Engine<'_>, a: &UpArgs) -> Result<()> {
    if a.detach && (a.abort_on_container_exit || a.abort_on_container_failure) {
        bail!("--abort-on-container-exit and --abort-on-container-failure are incompatible with --detach");
    }
    if a.force_recreate && a.no_recreate {
        bail!("--force-recreate and --no-recreate are incompatible");
    }
    if a.no_start && a.wait {
        bail!("--no-start and --wait are incompatible");
    }
    if a.build && a.no_build {
        bail!("--build and --no-build are incompatible");
    }
    let jobs: Vec<&str> = e.project().jobs.keys().map(String::as_str).collect();
    if !jobs.is_empty() {
        e.warn(format!(
            "jobs are not started by up; trigger them with `nspawn compose run`: {}",
            jobs.join(", ")
        ));
    }
    let flags = super::build::Flags {
        quiet: a.quiet_build,
        ..Default::default()
    };
    let converge = Converge {
        services: a.service.clone(),
        force_recreate: a.force_recreate,
        no_recreate: a.no_recreate,
        always_recreate_deps: a.always_recreate_deps,
        // COMPOSE_REMOVE_ORPHANS only when the flag was not given, false included.
        remove_orphans: a.remove_orphans.unwrap_or_else(|| {
            e.project()
                .environment
                .get("COMPOSE_REMOVE_ORPHANS")
                .is_some_and(|v| string_to_bool(v))
        }),
        scale: scale_overrides(&a.scale)?,
        pull: a.pull.clone(),
        quiet_pull: a.quiet_pull,
        no_deps: a.no_deps,
        start: !a.no_start,
        wait: a.wait,
        wait_timeout: a
            .wait_timeout
            .filter(|t| *t > 0)
            .map(|t| Duration::from_secs(t as u64)),
        timeout: a.timeout.map(|t| t.max(0) as u64),
        build: (!a.no_build).then_some(flags),
        force_build: a.build,
    };
    // Attached output starts before anything starts, as docker compose attaches first.
    let since = std::time::SystemTime::now();
    let machines = converge_project(e, &converge).await?;
    if a.detach || a.no_start || a.wait {
        return Ok(());
    }
    super::logs::attach(e, a, &machines, since).await
}

pub async fn create(e: &mut Engine<'_>, a: &CreateArgs) -> Result<()> {
    if a.force_recreate && a.no_recreate {
        bail!("--force-recreate and --no-recreate are incompatible");
    }
    if a.build && a.no_build {
        bail!("--build and --no-build are incompatible");
    }
    let converge = Converge {
        services: a.service.clone(),
        force_recreate: a.force_recreate,
        no_recreate: a.no_recreate,
        remove_orphans: a.remove_orphans,
        scale: scale_overrides(&a.scale)?,
        pull: a.pull.clone(),
        quiet_pull: a.quiet_pull,
        build: (!a.no_build).then(Default::default),
        force_build: a.build,
        ..Default::default()
    };
    converge_project(e, &converge).await.map(|_| ())
}

pub async fn scale(e: &mut Engine<'_>, a: &ScaleArgs) -> Result<()> {
    let mut overrides = BTreeMap::new();
    let mut services = Vec::new();
    for value in &a.service {
        let Some((service, n)) = value.split_once('=') else {
            bail!("invalid scale specifier: {value}");
        };
        let n: u32 = n
            .parse()
            .with_context(|| format!("invalid scale specifier: {value}"))?;
        if !e.project().services.contains_key(service) {
            bail!("no such service: {service}");
        }
        overrides.insert(service.to_string(), n);
        services.push(service.to_string());
    }
    // The services scaled, and theirs unless --no-deps: the others are left alone.
    let selected = e
        .project()
        .clone()
        .with_selected_services(&services, a.no_deps)
        .map_err(anyhow::Error::msg)?;
    e.project = Some(selected);
    let converge = Converge {
        services,
        scale: overrides,
        no_deps: a.no_deps,
        start: true,
        ..Default::default()
    };
    converge_project(e, &converge).await.map(|_| ())
}

pub async fn pull(e: &mut Engine<'_>, a: &PullArgs) -> Result<()> {
    let project = e.project().clone();
    let mut names: Vec<String> = if a.include_deps || a.service.is_empty() {
        project.services.keys().cloned().collect()
    } else {
        a.service.clone()
    };
    // --ignore-buildable leaves every service that can build its image.
    names.retain(|n| {
        project
            .services
            .get(n)
            .and_then(Value::as_map)
            .is_some_and(|s| !(a.ignore_buildable && s.contains_key("build")))
    });
    if let Some(policy) = &a.policy {
        if !matches!(policy.as_str(), "always" | "missing") {
            bail!("invalid pull policy {policy:?}: must be one of \"always\" or \"missing\"");
        }
    }
    let pulls = Pulls {
        flag: a.policy.as_deref(),
        quiet: a.quiet,
        ignore_failures: a.ignore_pull_failures,
        pull_only: true,
        ..Default::default()
    };
    images(e, &names, &pulls).await?;
    Ok(())
}

/// The pull policy a service gets: --pull, else its own; `unset` for a service without
/// one (missing for up, always for pull).
fn policy(service: &Map, flag: Option<&str>, unset: &str) -> String {
    let own = match s(service, "pull_policy") {
        "" => unset,
        "if_not_present" | "missing" => "missing",
        "always" | "refresh" | "daily" | "weekly" => "always",
        p if p.starts_with("every_") => "always",
        p => p,
    };
    match flag {
        None | Some("policy") => own.to_string(),
        Some(p) => p.to_string(),
    }
}

/// Whether a service's image is pulled: one it names, as its policy says.
fn pulls(policy: &str, named: bool, present: bool) -> Result<bool> {
    Ok(match policy {
        _ if !named => false,
        "always" => true,
        "missing" => !present,
        "never" | "build" => false,
        other => bail!("invalid pull policy {other:?}"),
    })
}

/// Whether a service's image is built, once a pull did what it could: with its policy
/// build, or when it is not here.
fn rebuilds(policy: &str, buildable: bool, present: bool) -> bool {
    buildable && (policy == "build" || !present)
}

/// What `images` is asked: the pull policy given on the command line and how images
/// are built, if at all.
#[derive(Default)]
pub struct Pulls<'a> {
    pub flag: Option<&'a str>,
    pub quiet: bool,
    pub ignore_failures: bool,
    pub build: Option<&'a super::build::Flags>,
    pub force_build: bool,
    /// compose pull: an image that is not there and can be built is left for compose
    /// build.
    pub pull_only: bool,
}

/// A pulled or built record of the store.
struct Local {
    reference: String,
    name: String,
    image_only: bool,
}

/// The record of a reference: an image only before one that ran as a machine.
fn found<'a>(local: &'a [Local], full: &str) -> Option<&'a Local> {
    let mut of = local.iter().filter(|l| l.reference == full);
    let first = of.next()?;
    Some(if first.image_only {
        first
    } else {
        of.find(|l| l.image_only).unwrap_or(first)
    })
}

/// compose's utils.StringToBool: strconv.ParseBool of the lowered text, false when it
/// is none.
pub(super) fn string_to_bool(text: &str) -> bool {
    matches!(text.to_lowercase().as_str(), "1" | "t" | "true")
}

/// `base`, or `base-2`, `base-3`... when a record has it.
fn free_name(base: &str, taken: &BTreeSet<String>) -> String {
    if !taken.contains(base) {
        return base.to_string();
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|name| !taken.contains(name))
        .expect("a free name")
}

/// The local image of each service, pulled or built where compose would: an image a
/// service names is pulled as its policy says, its failure only a warning when a
/// service of the project builds that image; then each image a build section makes is
/// built once, when the policy of its service is build, with --build, or when the
/// image is not there after the pulls.
pub async fn images(
    e: &Engine<'_>,
    services: &[String],
    p: &Pulls<'_>,
) -> Result<BTreeMap<String, Image>> {
    let project = e.project();
    // Each record by reference and name, and whether it is an image only; the names of
    // all of them, machines too.
    let list = || async {
        let all = e
            .client
            .manager
            .list_images()
            .await
            .map_err(client::error)?;
        let names: BTreeSet<String> = all.iter().map(|i| client::string(i, "name")).collect();
        let local: Vec<Local> = all
            .iter()
            .filter(|i| matches!(client::string(i, "origin").as_str(), "pull" | "build"))
            .map(|i| Local {
                reference: client::string(i, "reference"),
                name: client::string(i, "name"),
                image_only: client::bool(i, "image_only"),
            })
            .collect();
        Ok::<_, anyhow::Error>((local, names))
    };
    let find = |local: &[Local], full: &str| found(local, full).map(|l| l.name.clone());
    let unset = if p.pull_only { "always" } else { "missing" };
    // The service that builds each image, the first one of the project to.
    let mut builders: BTreeMap<String, String> = BTreeMap::new();
    for (name, service) in &project.services {
        let Some(service) = service.as_map() else {
            continue;
        };
        if service.get("build").and_then(Value::as_map).is_some() {
            builders
                .entry(super::build::image_of(&project.name, name, service))
                .or_insert_with(|| name.clone());
        }
    }
    let (mut local, mut names) = list().await?;
    // The pulls.
    let mut pulled = BTreeSet::new();
    let mut failed = Vec::new();
    let mut must_build = Vec::new();
    for name in services {
        let service = convert::service(project, name)?;
        let named = s(service, "image");
        if named.is_empty() {
            continue;
        }
        let reference = super::image_reference(named)?;
        let full = reference.to_string();
        // Once per image, however it is written (redis, redis:latest, docker.io/redis).
        if pulled.contains(&full) {
            continue;
        }
        let existing = found(&local, &full);
        let policy = policy(service, p.flag, unset);
        if !pulls(&policy, true, existing.is_some()).with_context(|| format!("service {name:?}"))? {
            continue;
        }
        pulled.insert(full.clone());
        let id = format!("Image {named}");
        e.progress.event(&id, "Pulling", "", Status::Working);
        // A pull replaces only a record nothing ran from: one the user ran as a machine
        // stays, and the image comes under a name of its own.
        let (into, force) = match existing {
            Some(l) if l.image_only => (l.name.clone(), true),
            _ => (free_name(&reference.local_name(), &names), false),
        };
        names.insert(into.clone());
        let mut options = Options::new();
        options.insert("name", Bus::from(into));
        options.insert("force", Bus::from(force));
        let manager = &e.client.manager;
        let result = e
            .client
            .run_job_silent(|| manager.pull_image(&full, options), p.quiet)
            .await;
        match result {
            Ok(_) => e.progress.event(&id, "Pulled", "", Status::Done),
            Err(err) if builders.contains_key(named) => {
                e.progress
                    .event(&id, "Warning", &format!("{err:#}"), Status::Warning);
                if p.pull_only {
                    must_build.push(builders[named].clone());
                    failed.push(err.context(format!("pulling {named}")));
                }
            }
            Err(err) => {
                e.progress
                    .event(&id, "Error", &format!("{err:#}"), Status::Error);
                if p.ignore_failures {
                    continue;
                }
                if !p.pull_only {
                    return Err(err.context(format!("pulling {named}")));
                }
                failed.push(err.context(format!("pulling {named}")));
            }
        }
    }
    (local, _) = list().await?;
    // The builds, each image once.
    if let Some(flags) = p.build {
        let mut built = BTreeSet::new();
        for name in services {
            let service = convert::service(project, name)?;
            if service.get("build").and_then(Value::as_map).is_none() {
                continue;
            }
            let image = super::build::image_of(&project.name, name, service);
            if built.contains(&image) {
                continue;
            }
            let full = super::image_reference(&image)?.to_string();
            let mut policy = policy(service, p.flag, unset);
            if p.force_build {
                policy = "build".to_string();
            }
            if rebuilds(&policy, true, find(&local, &full).is_some()) {
                super::build::build(e, name, service, &image, flags).await?;
                built.insert(image);
            }
        }
        (local, _) = list().await?;
    }
    // What each service runs.
    let mut out = BTreeMap::new();
    for name in services {
        let service = convert::service(project, name)?;
        let named = s(service, "image");
        let buildable = service.get("build").and_then(Value::as_map).is_some();
        if named.is_empty() && !buildable {
            continue;
        }
        let image = super::build::image_of(&project.name, name, service);
        let full = super::image_reference(&image)?.to_string();
        let Some(local_name) = find(&local, &full) else {
            if p.pull_only || (p.ignore_failures && !named.is_empty()) {
                continue;
            }
            if buildable || builders.contains_key(&image) {
                bail!(
                    "service {name:?}: image {image} is not on this host; compose build makes it"
                );
            }
            bail!(
                "service {name:?}: image {image} is not on this host and its pull policy is never"
            );
        };
        let record = e
            .client
            .manager
            .get_image(&local_name)
            .await
            .map_err(client::error)?;
        out.insert(
            name.clone(),
            Image {
                name: local_name,
                digest: client::string(&record, "digest"),
            },
        );
    }
    if !must_build.is_empty() {
        eprintln!(
            "WARNING: Some service image(s) must be built from source by running:\n    nspawn compose build {}",
            must_build.join(" ")
        );
    }
    if !failed.is_empty() && !p.ignore_failures {
        let text: Vec<String> = failed.iter().map(|e| format!("{e:#}")).collect();
        bail!("{}", text.join("\n"));
    }
    Ok(out)
}

/// The networks the project's services join, made where they are not, and the
/// external ones there.
async fn networks(e: &Engine<'_>) -> Result<()> {
    let project = e.project();
    let existing: BTreeSet<String> = e
        .client
        .manager
        .list_networks()
        .await
        .map_err(client::error)?
        .iter()
        .map(|n| client::string(n, "name"))
        .collect();
    // An nspawn network has one subnet: a second IPv4 one is refused before any is made.
    for (key, def) in &project.networks {
        let v4: Vec<&str> = def
            .get(&["ipam", "config"])
            .and_then(Value::as_seq)
            .into_iter()
            .flatten()
            .filter_map(Value::as_map)
            .map(|c| s(c, "subnet"))
            .filter(|s| !s.is_empty() && !s.contains(':'))
            .collect();
        if v4.len() > 1 {
            bail!(
                "network {key:?}: a second IPv4 subnet ({}): an nspawn network has one",
                v4[1]
            );
        }
    }
    for (key, def) in &project.networks {
        let def = def.as_map().cloned().unwrap_or_default();
        let name = convert::network_name(project, key);
        if def.get("external") == Some(&Value::Bool(true)) {
            if !existing.contains(&name) {
                bail!("network {name} declared as external, but could not be found");
            }
            continue;
        }
        if existing.contains(&name) {
            continue;
        }
        e.progress.network(&name, "Creating", Status::Working);
        let mut options = Options::new();
        // The file's labels, then compose's own, which win as compose adds them last.
        let mut labels: Vec<String> = match def.get("labels") {
            Some(Value::Map(map)) => map.iter().map(|(k, v)| format!("{k}={v}")).collect(),
            Some(Value::Seq(list)) => list.iter().map(|v| v.to_string()).collect(),
            _ => Vec::new(),
        };
        labels.extend([
            format!("{}={}", label::PROJECT, project.name),
            format!("{}={key}", label::NETWORK),
            format!("{}={}", label::VERSION, super::compose_version()),
        ]);
        options.insert("labels", Bus::from(labels));
        if def.get("internal") == Some(&Value::Bool(true)) {
            options.insert("internal", Bus::from(true));
        }
        let subnet = def
            .get("ipam")
            .and_then(|i| i.get(&["config"]))
            .and_then(Value::as_seq)
            .into_iter()
            .flatten()
            .filter_map(Value::as_map)
            .map(|c| s(c, "subnet").to_string())
            .find(|s| !s.is_empty() && !s.contains(':'));
        if let Some(subnet) = subnet {
            options.insert("subnet", Bus::from(subnet));
        }
        e.client
            .manager
            .create_network(&name, options)
            .await
            .map_err(client::error)
            .with_context(|| format!("creating network {name}"))?;
        e.progress.network(&name, "Created", Status::Done);
    }
    Ok(())
}

async fn volumes(e: &Engine<'_>) -> Result<()> {
    let project = e.project();
    let existing: BTreeSet<String> = e
        .client
        .manager
        .list_volumes()
        .await
        .map_err(client::error)?
        .iter()
        .map(|n| client::string(n, "name"))
        .collect();
    for (key, def) in &project.volumes {
        let def = def.as_map().cloned().unwrap_or_default();
        let name = convert::volume_name(project, key);
        if def.get("external") == Some(&Value::Bool(true)) {
            if !existing.contains(&name) {
                bail!("external volume {name:?} not found");
            }
            continue;
        }
        // One that binds a host directory is no volume of nspawn's.
        if existing.contains(&name) || convert::volume_device(project, key)?.is_some() {
            continue;
        }
        e.progress.volume(&name, "Creating", Status::Working);
        e.client
            .manager
            .create_volume(&name)
            .await
            .map_err(client::error)
            .with_context(|| format!("creating volume {name}"))?;
        e.progress.volume(&name, "Created", Status::Done);
    }
    Ok(())
}

/// The secrets and configs with no file behind them, made as nspawn secrets.
/// The label of a project's secret that says what content it holds.
const CONTENT_LABEL: &str = "org.nspawn.compose.content";

async fn secrets(e: &Engine<'_>) -> Result<()> {
    let project = e.project();
    // Each secret there, with the digest of its content when compose made it.
    let existing: BTreeMap<String, String> = e
        .client
        .manager
        .list_secrets()
        .await
        .map_err(client::error)?
        .iter()
        .map(|n| {
            let digest = n
                .get("labels")
                .and_then(|l| std::collections::HashMap::<String, String>::try_from(l.clone()).ok())
                .and_then(|l| l.get(CONTENT_LABEL).cloned())
                .unwrap_or_default();
            (client::string(n, "name"), digest)
        })
        .collect();
    for (kind, defs) in [("secret", &project.secrets), ("config", &project.configs)] {
        for (key, def) in defs {
            let def = def.as_map().cloned().unwrap_or_default();
            if !s(&def, "file").is_empty() {
                continue;
            }
            let content = if def.contains_key("content") {
                s(&def, "content").to_string()
            } else if !s(&def, "environment").is_empty() {
                let var = s(&def, "environment");
                project.environment.get(var).cloned().with_context(|| {
                    format!("environment variable {var:?} required by {kind} {key:?} is not set")
                })?
            } else {
                continue;
            };
            // The content changes with the file or the environment: a secret made from
            // other content is replaced, and machines read the new one at their next
            // start, as the content is given to containers at theirs.
            let digest = crate::layout::sha256_digest(content.as_bytes());
            let name = convert::secret_name(&project.name, kind, key);
            if existing.get(&name).is_some_and(|d| *d == digest) {
                continue;
            }
            let mut options = Options::new();
            options.insert(
                "labels",
                Bus::from(vec![
                    format!("{}={}", label::PROJECT, project.name),
                    format!("{CONTENT_LABEL}={digest}"),
                ]),
            );
            options.insert("replace", Bus::from(existing.contains_key(&name)));
            e.client
                .manager
                .create_secret(&name, content.as_bytes(), options)
                .await
                .map_err(client::error)
                .with_context(|| format!("creating the {kind} {key}"))?;
        }
    }
    Ok(())
}

/// Before anything is made: replicas of a service with a name of its own cannot be,
/// and two services cannot have one machine name, which they would when they differ by
/// a '_' and a '-' alone.
pub(super) fn unique_names(project: &Project, scale: &BTreeMap<String, u32>) -> Result<()> {
    let mut taken: BTreeMap<String, &str> = BTreeMap::new();
    for (name, service) in &project.services {
        let Some(service) = service.as_map() else {
            continue;
        };
        let custom = s(service, "container_name");
        let replicas = convert::replicas(service, scale, name);
        if !custom.is_empty() && replicas > 1 {
            bail!("the {name:?} service is using the custom container name {custom:?}; each machine needs a name of its own: remove the custom name to scale the service");
        }
        for number in 1..=replicas.max(1) {
            let machine = super::machine_name(&project.name, name, custom, number);
            if let Some(other) = taken.insert(machine.clone(), name) {
                bail!("the services {other:?} and {name:?} would both make the machine {machine}: machine names take no '_', which is written '-'; rename one of them or its container_name");
            }
        }
    }
    Ok(())
}

/// Whether a machine is made again: the services named as --force-recreate says, the
/// others, there as their dependencies, as --always-recreate-deps does; any of them when
/// it changed, unless --no-recreate.
fn recreates(c: &Converge, service: &str, stale: bool) -> bool {
    let targeted = c.services.is_empty() || c.services.iter().any(|s| s == service);
    let force = if targeted {
        c.force_recreate
    } else {
        c.always_recreate_deps
    };
    !c.no_recreate && (force || stale)
}

/// Makes everything the project needs and its services' machines, and starts them when
/// asked; the machines of the services, in the order they started.
pub async fn converge_project(e: &mut Engine<'_>, c: &Converge) -> Result<Vec<Machine>> {
    // --no-deps: the services named alone, their dependencies left as they are.
    if c.no_deps && !c.services.is_empty() {
        let selected = e
            .project()
            .clone()
            .with_selected_services(&c.services, true)
            .map_err(anyhow::Error::msg)?;
        e.project = Some(selected);
    }
    let project = e.project().clone();
    check_refusals(&project)?;
    for name in c.scale.keys() {
        if !project.services.contains_key(name) {
            bail!("no such service: {name}");
        }
    }
    unique_names(&project, &c.scale)?;
    let order = order(&project);
    let pulls = Pulls {
        flag: c.pull.as_deref(),
        quiet: c.quiet_pull,
        build: c.build.as_ref(),
        force_build: c.force_build,
        ..Default::default()
    };
    let images = images(e, &order, &pulls).await?;
    networks(e).await?;
    volumes(e).await?;
    secrets(e).await?;
    let mut machines = e.machines().await?;
    orphans(e, &project, &machines, c).await?;
    let all: Vec<String> = e
        .client
        .manager
        .list_machines(true)
        .await
        .map_err(client::error)?
        .iter()
        .map(|m| client::string(m, "name"))
        .collect();
    for name in &order {
        let service = convert::service(&project, name)?.clone();
        let Some(image) = images.get(name) else {
            continue;
        };
        let want = convert::replicas(&service, &c.scale, name);
        let mut existing: Vec<Machine> = machines
            .iter()
            .filter(|m| m.service == *name && !m.oneoff)
            .cloned()
            .collect();
        existing.sort_by_key(|m| m.number);
        // Replicas past the count go, the highest first.
        for extra in existing.iter().rev().filter(|m| m.number > want) {
            stop_and_remove(e, extra, c.timeout).await?;
        }
        for number in 1..=want {
            let machine = machine_name(&project.name, name, s(&service, "container_name"), number);
            if !valid_machine_name(&machine) {
                bail!(
                    "service {name:?}: {machine:?} is not a name machined takes: letters, digits, '-' and '.', up to 64"
                );
            }
            let spec = Spec {
                project: &project,
                service: name,
                number,
                name: machine.clone(),
                oneoff: false,
                image_digest: image.digest.clone(),
                no_deps: c.no_deps,
                env_files: e.args.env_file.clone(),
                scale: &c.scale,
            };
            let made = convert::options(&spec)?;
            let found = existing
                .iter()
                .find(|m| m.number == number || m.name == machine);
            match found {
                Some(current) => {
                    let hash = labels_of(&made).get(label::CONFIG_HASH).cloned();
                    let stale = current.labels.get(label::CONFIG_HASH) != hash.as_ref()
                        || current.label(label::IMAGE) != image.digest;
                    let recreate = recreates(c, name, stale);
                    if recreate {
                        e.progress
                            .container(&current.name, "Recreate", Status::Working);
                        // Made anew in its place, which the service checks before the
                        // old one goes, and which keeps the machines that depend on it
                        // pointing at it; under another name, the old one goes first.
                        if current.name == machine {
                            stop_only(e, current, c.timeout).await?;
                            make(e, image, &machine, made, true).await?;
                        } else {
                            stop_and_remove(e, current, c.timeout).await?;
                            make(e, image, &machine, made, false).await?;
                        }
                        e.progress.container(&machine, "Recreated", Status::Done);
                    }
                }
                None => {
                    if all.contains(&machine) {
                        bail!(
                            "machine {machine} exists and is not one of project {}'s; remove it or rename the service",
                            project.name
                        );
                    }
                    e.progress.container(&machine, "Creating", Status::Working);
                    make(e, image, &machine, made, false).await?;
                    e.progress.container(&machine, "Created", Status::Done);
                }
            }
        }
    }
    machines = e.machines().await?;
    if !c.start {
        return Ok(machines);
    }
    let started = start_in_order(e, &project, &order, &c.services, c).await?;
    if c.wait {
        wait_running(e, &started, c.wait_timeout).await?;
    }
    Ok(started)
}

fn labels_of(made: &convert::Made) -> BTreeMap<String, String> {
    match made.options.get("label") {
        Some(Bus::Array(items)) => items
            .iter()
            .filter_map(|v| String::try_from(v.clone()).ok())
            .filter_map(|l| {
                l.split_once('=')
                    .map(|(k, v)| (k.to_string(), v.to_string()))
            })
            .collect(),
        _ => BTreeMap::new(),
    }
}

/// CreateMachine for one replica, with what has to be there first.
async fn make(
    e: &Engine<'_>,
    image: &Image,
    machine: &str,
    made: convert::Made,
    replace: bool,
) -> Result<()> {
    for note in &made.notes {
        e.warn(note.clone());
    }
    for path in &made.host_paths {
        std::fs::create_dir_all(path).with_context(|| format!("creating {path}"))?;
    }
    let manager = &e.client.manager;
    let mut options = made.options;
    if replace {
        options.insert("force", Bus::from(true));
    }
    e.client
        .run_job_silent(
            || manager.create_machine(&image.name, machine, options),
            true,
        )
        .await
        .with_context(|| format!("creating {machine}"))?;
    Ok(())
}

/// Stops a machine and removes it.
pub async fn stop_and_remove(e: &Engine<'_>, m: &Machine, timeout: Option<u64>) -> Result<()> {
    stop_only(e, m, timeout).await?;
    e.progress.container(&m.name, "Removing", Status::Working);
    let mut options = Options::new();
    options.insert("force", Bus::from(true));
    let names = [m.name.clone()];
    let manager = &e.client.manager;
    e.client
        .run_job_silent(|| manager.remove_machines(&names, options), true)
        .await?;
    e.progress.container(&m.name, "Removed", Status::Done);
    Ok(())
}

/// Stops a machine that runs.
async fn stop_only(e: &Engine<'_>, m: &Machine, timeout: Option<u64>) -> Result<()> {
    e.progress.container(&m.name, "Stopping", Status::Working);
    if m.running() || m.state == "restarting" || m.state == "starting" {
        let mut options = Options::new();
        if let Some(t) = timeout {
            options.insert("timeout", Bus::from(t));
        }
        e.client
            .manager
            .stop_machine(&m.name, options)
            .await
            .map_err(client::error)?;
    }
    e.progress.container(&m.name, "Stopped", Status::Done);
    Ok(())
}

/// The machines of services the file no longer has: a warning, or gone with
/// --remove-orphans.
async fn orphans(
    e: &Engine<'_>,
    project: &Project,
    machines: &[Machine],
    c: &Converge,
) -> Result<()> {
    let known: BTreeSet<&String> = project
        .services
        .keys()
        .chain(project.disabled_services.keys())
        .chain(project.jobs.keys())
        .collect();
    let orphans: Vec<&Machine> = machines
        .iter()
        .filter(|m| !m.oneoff && !known.contains(&m.service))
        .collect();
    if orphans.is_empty() {
        return Ok(());
    }
    let ignore = project
        .environment
        .get("COMPOSE_IGNORE_ORPHANS")
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
    if c.remove_orphans {
        for m in orphans {
            stop_and_remove(e, m, c.timeout).await?;
        }
    } else if !ignore {
        let names: Vec<&str> = orphans.iter().map(|m| m.name.as_str()).collect();
        e.warn(format!(
            "Found orphan containers ([{}]) for this project. If you removed or renamed this service in your compose file, you can run this command with the --remove-orphans flag to clean it up.",
            names.join(" ")
        ));
    }
    Ok(())
}

/// Starts the machines of the services, each service once those it depends on are what
/// it waits for; the machines started, or running already.
async fn start_in_order(
    e: &Engine<'_>,
    project: &Project,
    order: &[String],
    asked: &[String],
    c: &Converge,
) -> Result<Vec<Machine>> {
    let machines = e.machines().await?;
    let mut out = Vec::new();
    for name in order {
        let service = convert::service(project, name)?.clone();
        let mine: Vec<&Machine> = machines
            .iter()
            .filter(|m| m.service == *name && !m.oneoff)
            .collect();
        if mine.is_empty() {
            continue;
        }
        if !c.no_deps || asked.is_empty() {
            wait_dependencies(e, project, &service, name).await?;
        }
        for m in &mine {
            if m.running() {
                e.progress.container(&m.name, "Running", Status::Done);
            } else {
                start_one(e, m).await?;
            }
            out.push((*m).clone());
        }
    }
    Ok(out)
}

pub async fn start_one(e: &Engine<'_>, m: &Machine) -> Result<()> {
    e.progress.container(&m.name, "Starting", Status::Working);
    let mut options = Options::new();
    options.insert("wait", Bus::from(true));
    let started = e.client.manager.start_machine(&m.name, options).await;
    match started {
        Ok((_, notes)) => {
            for note in notes {
                e.warn(note);
            }
        }
        Err(err) => {
            let err = client::error(err);
            e.progress.container(&m.name, "Error", Status::Error);
            return Err(err.context(format!("starting {}", m.name)));
        }
    }
    e.progress.container(&m.name, "Started", Status::Done);
    post_start(e, m).await
}

/// The post_start hooks of a machine's service, run inside it once it is up.
pub(super) async fn post_start(e: &Engine<'_>, m: &Machine) -> Result<()> {
    let Some(project) = &e.project else {
        return Ok(());
    };
    let Ok(service) = convert::service(project, &m.service) else {
        return Ok(());
    };
    for hook in service
        .get("post_start")
        .and_then(Value::as_seq)
        .into_iter()
        .flatten()
        .filter_map(Value::as_map)
    {
        super::logs::hook(e, &m.name, hook, "post_start").await?;
    }
    Ok(())
}

/// Waits for what a service asks of the services it depends on, saying so: Waiting,
/// then Healthy or Exited.
pub async fn wait_dependencies(
    e: &Engine<'_>,
    project: &Project,
    service: &Map,
    name: &str,
) -> Result<()> {
    let Some(deps) = service.get("depends_on").and_then(Value::as_map) else {
        return Ok(());
    };
    for (dep, def) in deps {
        let def = def.as_map().cloned().unwrap_or_default();
        let condition = s(&def, "condition");
        let required = def.get("required") != Some(&Value::Bool(false));
        if !matches!(
            condition,
            "service_healthy" | "service_completed_successfully"
        ) {
            continue;
        }
        if !project.services.contains_key(dep) {
            continue;
        }
        let machines: Vec<Machine> = e
            .machines()
            .await?
            .into_iter()
            .filter(|m| m.service == *dep && !m.oneoff)
            .collect();
        for m in machines {
            e.progress.container(&m.name, "Waiting", Status::Working);
            let outcome = await_condition(e, &m.name, condition).await;
            match outcome {
                Ok(text) => e.progress.container(&m.name, text, Status::Done),
                Err(why) if !required => {
                    e.progress.container(&m.name, "Error", Status::Warning);
                    e.warn(format!("optional dependency {dep:?} failed: {why}"));
                }
                Err(why) => {
                    e.progress.container(&m.name, "Error", Status::Error);
                    bail!("dependency failed to start: {why}");
                }
            }
        }
        let _ = name;
    }
    Ok(())
}

/// Polls a machine until it is healthy, or has completed with 0.
async fn await_condition(
    e: &Engine<'_>,
    machine: &str,
    condition: &str,
) -> Result<&'static str, String> {
    loop {
        let found = e.client.manager.get_machine(machine).await;
        let dict = found.map_err(|err| format!("{:#}", client::error(err)))?;
        let m = Machine::from_dict(dict);
        if condition == "service_healthy" {
            if m.running() {
                match m.health.as_str() {
                    "healthy" => return Ok("Healthy"),
                    "unhealthy" => return Err(format!("container {machine} is unhealthy")),
                    "" if !checks_health(&m) => {
                        return Err(format!("container {machine} has no healthcheck configured"))
                    }
                    _ => {}
                }
            } else if m.state == "stopped" {
                return Err(format!(
                    "container {machine} exited ({})",
                    m.exit_code.unwrap_or(0)
                ));
            }
        } else if m.state == "stopped" {
            return match m.exit_code {
                Some(0) | None => Ok("Exited"),
                Some(code) => Err(format!(
                    "service {:?} didn't complete successfully: exit {code}",
                    m.service
                )),
            };
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Whether a service of the project depends on `service` completing successfully.
fn awaited_to_complete(e: &Engine<'_>, service: &str) -> bool {
    e.project.as_ref().is_some_and(|p| {
        p.services.values().filter_map(Value::as_map).any(|s| {
            s.get("depends_on")
                .and_then(Value::as_map)
                .and_then(|d| d.get(service))
                .and_then(Value::as_map)
                .is_some_and(|d| {
                    d.get("condition").and_then(Value::as_str)
                        == Some("service_completed_successfully")
                })
        })
    })
}

/// Whether a machine has a healthcheck that runs: one disabled (NONE) gives no verdict.
fn checks_health(m: &Machine) -> bool {
    m.dict
        .get("healthcheck")
        .and_then(|h| {
            std::collections::HashMap::<String, zbus::zvariant::OwnedValue>::try_from(h.clone())
                .ok()
        })
        .and_then(|h| {
            h.get("test")
                .and_then(|t| Vec::<String>::try_from(t.clone()).ok())
        })
        .is_some_and(|test| test.first().is_some_and(|t| t != "NONE"))
}

/// --wait: every machine running, and healthy when it has a healthcheck.
pub async fn wait_running(
    e: &Engine<'_>,
    machines: &[Machine],
    timeout: Option<Duration>,
) -> Result<()> {
    let start = Instant::now();
    for m in machines {
        e.progress.container(&m.name, "Waiting", Status::Working);
        loop {
            if timeout.is_some_and(|t| start.elapsed() > t) {
                bail!(
                    "application not healthy after {}",
                    crate::compose::golib::duration_string(
                        timeout.map(|t| t.as_nanos() as i64).unwrap_or(0)
                    )
                );
            }
            let dict = e
                .client
                .manager
                .get_machine(&m.name)
                .await
                .map_err(client::error)?;
            let now = Machine::from_dict(dict);
            if now.state == "stopped" {
                // Ending is what a service others wait to complete does; any other
                // was to keep running.
                if now.exit_code.unwrap_or(0) == 0 && awaited_to_complete(e, &m.service) {
                    e.progress.container(&m.name, "Exited", Status::Done);
                    break;
                }
                bail!(
                    "container {} exited ({})",
                    m.name,
                    now.exit_code.unwrap_or(0)
                );
            }
            // A machine with a healthcheck has no verdict until its first probe.
            if now.running()
                && (now.health == "healthy" || (now.health.is_empty() && !checks_health(&now)))
            {
                e.progress.container(
                    &m.name,
                    if now.health.is_empty() {
                        "Running"
                    } else {
                        "Healthy"
                    },
                    Status::Done,
                );
                break;
            }
            if now.health == "unhealthy" {
                bail!("container {} is unhealthy", m.name);
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn booleans_of_the_environment_as_compose_reads_them() {
        assert!(string_to_bool("TRUE") && string_to_bool("1") && string_to_bool("t"));
        assert!(!string_to_bool("yes") && !string_to_bool(""));
    }

    #[test]
    fn a_disabled_healthcheck_is_none() {
        let machine = |test: &[&str]| {
            let check: std::collections::HashMap<String, zbus::zvariant::OwnedValue> =
                std::collections::HashMap::from([(
                    "test".to_string(),
                    crate::daemon::values::v(
                        test.iter().map(|t| t.to_string()).collect::<Vec<_>>(),
                    ),
                )]);
            let mut dict = crate::client::Dict::new();
            dict.insert("healthcheck".to_string(), crate::daemon::values::v(check));
            Machine {
                name: "m".to_string(),
                service: "s".to_string(),
                number: 1,
                oneoff: false,
                state: "running".to_string(),
                health: String::new(),
                exit_code: None,
                labels: BTreeMap::new(),
                created: 0,
                started: 0,
                dict,
            }
        };
        assert!(checks_health(&machine(&["CMD", "true"])));
        assert!(!checks_health(&machine(&["NONE"])));
        assert!(!checks_health(&machine(&[])));
    }

    #[test]
    fn recreation_follows_the_services_named() {
        let c = Converge {
            services: vec!["web".to_string()],
            force_recreate: true,
            ..Default::default()
        };
        assert!(recreates(&c, "web", false));
        assert!(!recreates(&c, "db", false));
        assert!(recreates(&c, "db", true));
        let deps = Converge {
            services: vec!["web".to_string()],
            always_recreate_deps: true,
            ..Default::default()
        };
        assert!(!recreates(&deps, "web", false));
        assert!(recreates(&deps, "db", false));
        let all = Converge {
            force_recreate: true,
            ..Default::default()
        };
        assert!(recreates(&all, "db", false));
        let never = Converge {
            force_recreate: true,
            no_recreate: true,
            ..Default::default()
        };
        assert!(!recreates(&never, "web", true));
    }

    #[test]
    fn a_pull_never_replaces_a_machine() {
        let local = |name: &str, image_only: bool| Local {
            reference: "docker.io/library/postgres:16".to_string(),
            name: name.to_string(),
            image_only,
        };
        let full = "docker.io/library/postgres:16";
        // The user's machine comes first by name; the image only is the one taken.
        let both = [local("db", false), local("postgres-16-2", true)];
        assert_eq!(found(&both, full).unwrap().name, "postgres-16-2");
        let machine = [local("db", false)];
        assert!(!found(&machine, full).unwrap().image_only);
        assert!(found(&machine, "docker.io/library/redis:7").is_none());
        let taken: BTreeSet<String> = ["postgres-16".to_string(), "postgres-16-2".to_string()]
            .into_iter()
            .collect();
        assert_eq!(free_name("postgres-16", &taken), "postgres-16-3");
        assert_eq!(free_name("redis-7", &taken), "redis-7");
    }

    #[test]
    fn images_are_pulled_and_built_as_compose_decides() {
        // An image a service names, by its policy.
        assert!(pulls("always", true, true).unwrap());
        assert!(pulls("missing", true, false).unwrap());
        assert!(!pulls("missing", true, true).unwrap());
        assert!(!pulls("never", true, false).unwrap());
        assert!(!pulls("build", true, false).unwrap());
        // One it does not name is never pulled.
        assert!(!pulls("always", false, false).unwrap());
        assert!(pulls("sometimes", true, false).is_err());
        // Built when missing after the pull, or every time with the policy build.
        assert!(rebuilds("missing", true, false));
        assert!(!rebuilds("missing", true, true));
        assert!(!rebuilds("always", true, true));
        assert!(rebuilds("build", true, true));
        assert!(rebuilds("never", true, false));
        assert!(!rebuilds("build", false, false));
    }

    #[test]
    fn scale_options_read_as_docker_compose_reads_them() {
        assert_eq!(
            scale_overrides(&["web=3".to_string()]).unwrap(),
            BTreeMap::from([("web".to_string(), 3)])
        );
        assert!(scale_overrides(&["web".to_string()]).is_err());
    }

    #[test]
    fn pull_policies_follow_the_flag_then_the_service() {
        let service = |p: &str| Map::from([("pull_policy".to_string(), Value::str(p))]);
        assert_eq!(policy(&Map::new(), None, "missing"), "missing");
        assert_eq!(policy(&Map::new(), None, "always"), "always");
        assert_eq!(policy(&service("missing"), None, "always"), "missing");
        assert_eq!(policy(&service("daily"), None, "missing"), "always");
        assert_eq!(
            policy(&service("never"), Some("always"), "missing"),
            "always"
        );
        assert_eq!(
            policy(&service("never"), Some("policy"), "missing"),
            "never"
        );
    }
}
