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
    /// -V: the machines made anew get new anonymous volumes.
    pub renew_anonymous: bool,
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
    /// When the machines were about to start, made and pulled and built: the output an
    /// attached up shows begins there: it attaches before they start, so nothing they
    /// print is missed.
    pub starting_at: std::cell::Cell<Option<std::time::SystemTime>>,
}

/// A local image a service runs.
#[derive(Clone)]
pub struct Image {
    /// The record nspawn keeps it under, which machines are made from.
    pub name: String,
    pub digest: String,
    /// Its own command, which an empty entrypoint of the file keeps.
    pub cmd: Vec<String>,
    /// Its VOLUME paths.
    pub volumes: Vec<String>,
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
        // A job's triggers say when it runs, not what it runs as: run takes a scheduled
        // job, and up refuses one with reject_scheduled_jobs.
        if refusal.path == "triggers" || refusal.path.starts_with("triggers.") {
            continue;
        }
        let applies = if !refusal.service.is_empty() {
            project.services.contains_key(&refusal.service)
        } else if let Some((kind, name)) = &refusal.resource {
            // A resource the services to run use, by its whole name.
            match kind.as_str() {
                "networks" => project.networks.contains_key(name),
                "volumes" => project.volumes.contains_key(name),
                "secrets" => project.secrets.contains_key(name),
                "configs" => project.configs.contains_key(name),
                "models" => project.models.contains_key(name),
                _ => true,
            }
        } else {
            // Models as a whole only when a service to run uses one.
            refusal.path != "models" || !project.models.is_empty()
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

/// rejectScheduledJobs: a project that schedules jobs is not brought up, as nothing
/// would run them on their schedule.
pub fn reject_scheduled_jobs(project: &Project) -> Result<()> {
    let names: Vec<&str> = project
        .jobs
        .iter()
        .filter(|(_, job)| {
            job.get(&["triggers", "schedule"])
                .and_then(Value::as_seq)
                .is_some_and(|s| !s.is_empty())
        })
        .map(|(name, _)| name.as_str())
        .collect();
    if !names.is_empty() {
        bail!(
            "scheduled jobs are not supported in this version: {}",
            names.join(", ")
        );
    }
    Ok(())
}

/// validateFlags: the combinations of up's flags docker compose refuses, in its order
/// and its words.
pub fn validate_flags(a: &UpArgs) -> Result<()> {
    if a.wait_timeout.is_some_and(|t| t < 0) {
        bail!("--wait-timeout must be a non-negative integer");
    }
    // --exit-code-from stops everything when one ends, unless failures alone do.
    let cascade_stop =
        a.abort_on_container_exit || (a.exit_code_from.is_some() && !a.abort_on_container_failure);
    if cascade_stop && a.abort_on_container_failure {
        bail!("--abort-on-container-failure cannot be combined with --abort-on-container-exit");
    }
    if a.wait && (a.attach_dependencies || cascade_stop || !a.attach.is_empty()) {
        bail!("--wait cannot be combined with --abort-on-container-exit, --attach or --attach-dependencies");
    }
    if a.build && a.no_build {
        bail!("--build and --no-build are incompatible");
    }
    let detach = a.detach || a.wait;
    if detach
        && (a.attach_dependencies
            || cascade_stop
            || a.abort_on_container_failure
            || !a.attach.is_empty()
            || a.watch)
    {
        if a.wait {
            bail!("--wait cannot be combined with --abort-on-container-exit, --abort-on-container-failure, --attach, --attach-dependencies or --watch");
        }
        bail!("--detach cannot be combined with --abort-on-container-exit, --abort-on-container-failure, --attach, --attach-dependencies or --watch");
    }
    if a.renew_anon_volumes && a.no_recreate {
        bail!("--no-recreate and --renew-anon-volumes are incompatible");
    }
    if a.force_recreate && a.no_recreate {
        bail!("--force-recreate and --no-recreate are incompatible");
    }
    if a.always_recreate_deps && a.no_recreate {
        bail!("--always-recreate-deps and --no-recreate are incompatible");
    }
    if a.no_build && a.watch {
        bail!("--no-build and --watch are incompatible");
    }
    Ok(())
}

pub async fn up(e: &mut Engine<'_>, a: &UpArgs) -> Result<()> {
    validate_flags(a)?;
    if !e.args.all_resources && e.project().services.is_empty() {
        bail!("no service selected");
    }
    let remove_orphans = a
        .remove_orphans
        .unwrap_or_else(|| removes_orphans(e.project()));
    let ignore_orphans = ignores_orphans(e.project());
    if ignore_orphans && remove_orphans {
        bail!("cannot combine COMPOSE_IGNORE_ORPHANS and --remove-orphans");
    }
    if !a.attach.is_empty() && a.attach_dependencies {
        bail!("cannot combine --attach and --attach-dependencies");
    }
    reject_scheduled_jobs(e.project())?;
    check_pull(a.pull.as_deref())?;
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
        // recreateStrategy: -V makes the machines anew as --force-recreate does.
        force_recreate: a.force_recreate || a.renew_anon_volumes,
        renew_anonymous: a.renew_anon_volumes,
        no_recreate: a.no_recreate,
        always_recreate_deps: a.always_recreate_deps,
        remove_orphans,
        scale: scale_overrides(&a.scale)?,
        pull: a.pull.clone(),
        quiet_pull: a.quiet_pull,
        no_deps: a.no_deps,
        start: !a.no_start,
        // --no-start creates and ends there, --wait or not.
        wait: a.wait && !a.no_start,
        wait_timeout: a
            .wait_timeout
            .filter(|t| *t > 0)
            .map(|t| Duration::from_secs(t as u64)),
        timeout: a.timeout.map(crate::policy::stop_seconds),
        build: (!a.no_build).then_some(flags),
        force_build: a.build,
        ..Default::default()
    };
    // An attached up checks what it is to attach to before anything is made.
    let attached = !(a.detach || a.no_start || a.wait);
    if attached {
        super::logs::attach_sets(a, e.project())?;
    }
    let machines = converge_project(e, &converge).await?;
    if !attached {
        return Ok(());
    }
    let since = converge
        .starting_at
        .get()
        .unwrap_or_else(std::time::SystemTime::now);
    super::logs::attach(e, a, &machines, since).await
}

pub async fn create(e: &mut Engine<'_>, a: &CreateArgs) -> Result<()> {
    if a.build && a.no_build {
        bail!("--build and --no-build are incompatible");
    }
    if a.force_recreate && a.no_recreate {
        bail!("--force-recreate and --no-recreate are incompatible");
    }
    reject_scheduled_jobs(e.project())?;
    check_pull(a.pull.as_deref())?;
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
        None => own.to_string(),
        Some("if_not_present") => "missing".to_string(),
        Some(p) => p.to_string(),
    }
}

/// createOptions.Apply: the --pull of up, create and run, one of the policies a service
/// may have.
pub fn check_pull(flag: Option<&str>) -> Result<()> {
    match flag {
        None | Some("always" | "never" | "build" | "missing" | "if_not_present") => Ok(()),
        Some(other) => bail!(
            "invalid --pull option {}",
            crate::compose::gostr::quote(other)
        ),
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

/// isOrphaned: a machine of a service the file no longer has, or a one-off (a job's
/// among them) that ended; one that runs is somebody's run session.
fn is_orphan(m: &Machine, known: &BTreeSet<&String>) -> bool {
    match m.oneoff {
        true => !m.running(),
        false => !known.contains(&m.service),
    }
}

/// up's --remove-orphans when it is not given: COMPOSE_REMOVE_ORPHANS of the project's
/// environment, which has the variables of its .env file and --env-file too, as the
/// command line sets them before up reads it.
fn removes_orphans(project: &Project) -> bool {
    project
        .environment
        .get("COMPOSE_REMOVE_ORPHANS")
        .is_some_and(|v| string_to_bool(v))
}

/// COMPOSE_IGNORE_ORPHANS of the project's environment, its .env file's too.
fn ignores_orphans(project: &Project) -> bool {
    project
        .environment
        .get("COMPOSE_IGNORE_ORPHANS")
        .is_some_and(|v| string_to_bool(v))
}

/// utils.StringToBool: y, or what strconv.ParseBool reads as true, trimmed.
pub(super) fn string_to_bool(text: &str) -> bool {
    matches!(
        text.trim().to_lowercase().as_str(),
        "y" | "1" | "t" | "true"
    )
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
        let reference = e.reference(named)?;
        let full = reference.to_string();
        // Once per image, however it is written (redis, redis:latest, docker.io/redis).
        if pulled.contains(&full) {
            continue;
        }
        let existing = found(&local, &full);
        // --build makes every service with a build section a built one, not pulled.
        let policy = if p.force_build && service.get("build").and_then(Value::as_map).is_some() {
            "build".to_string()
        } else {
            policy(service, p.flag, unset)
        };
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
            let full = e.reference(&image)?.to_string();
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
        let full = e.reference(&image)?.to_string();
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
                cmd: client::strings(&record, "image_cmd"),
                volumes: client::strings(&record, "image_volumes"),
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
/// The networks of the project, made where they are missing. With `renew`, one made by
/// an earlier up whose definition has changed since (its config hash) is made anew, as
/// compose's reconcileNetworks does; the machines of the project on it are removed
/// first, to be made again by the pass, as nspawn cannot keep their place on a network
/// that goes.
pub(super) async fn networks(e: &Engine<'_>, renew: bool, timeout: Option<u64>) -> Result<()> {
    let project = e.project();
    let listed = e
        .client
        .manager
        .list_networks()
        .await
        .map_err(client::error)?;
    let mut existing: BTreeSet<String> = listed.iter().map(|n| client::string(n, "name")).collect();
    let hash_of = |name: &str| -> Option<String> {
        listed
            .iter()
            .find(|n| client::string(n, "name") == name)
            .and_then(|n| n.get("labels"))
            .and_then(|l| std::collections::HashMap::<String, String>::try_from(l.clone()).ok())
            .filter(|l| l.get(label::PROJECT) == Some(&project.name))
            .and_then(|l| l.get(label::CONFIG_HASH).cloned())
    };
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
        let hash = crate::compose::config::network_hash(&Value::Map(def.clone()))
            .map_err(anyhow::Error::msg)?;
        if existing.contains(&name) {
            match hash_of(&name) {
                Some(had) if renew && had != hash => {
                    renew_network(e, &name, timeout).await?;
                    existing.remove(&name);
                }
                _ => continue,
            }
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
            format!("{}={hash}", label::CONFIG_HASH),
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

/// A network of the project that is to be made anew: the project's machines on it go,
/// then it does.
async fn renew_network(e: &Engine<'_>, name: &str, timeout: Option<u64>) -> Result<()> {
    for m in e.machines().await?.iter().filter(|m| {
        client::strings(&m.dict, "networks")
            .iter()
            .any(|n| n == name)
    }) {
        stop_and_remove(e, m, timeout).await?;
    }
    e.progress.network(name, "Removing", Status::Working);
    let names = [name.to_string()];
    let manager = &e.client.manager;
    e.client
        .run_job_silent(|| manager.remove_networks(&names), true)
        .await
        .with_context(|| format!("removing network {name}, whose definition changed"))?;
    e.progress.network(name, "Removed", Status::Done);
    Ok(())
}

pub(super) async fn volumes(e: &Engine<'_>) -> Result<()> {
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
pub(super) async fn secrets(e: &Engine<'_>) -> Result<()> {
    let project = e.project();
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
            // The content changes with the file or the environment: a secret of other
            // content is replaced, and machines read the new one at their next start, as
            // the content is given to containers at theirs. The service compares them:
            // nothing derived from the content is put where readers of secrets see it.
            let name = convert::secret_name(&project.name, kind, key);
            let mut options = Options::new();
            options.insert(
                "labels",
                Bus::from(vec![format!("{}={}", label::PROJECT, project.name)]),
            );
            options.insert("replace_if_changed", Bus::from(true));
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
    // --scale names any service of the project loaded: it applies before --no-deps
    // narrows the selection.
    for name in c.scale.keys() {
        if !e.project().services.contains_key(name) {
            bail!("no such service: {name}");
        }
    }
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
    project
        .check_container_name_unicity()
        .map_err(anyhow::Error::msg)?;
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
    networks(e, true, c.timeout).await?;
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
    // parentNamespaceRecreated: a machine in the network of a service made anew this
    // pass would stay in the namespace of the old one.
    let mut recreated: BTreeSet<String> = BTreeSet::new();
    // The machines stopped for a dependency made anew, each once.
    let mut stopped: BTreeSet<String> = BTreeSet::new();
    for name in &order {
        let service = convert::service(&project, name)?.clone();
        let Some(image) = images.get(name) else {
            continue;
        };
        let parent_recreated = s(&service, "network_mode")
            .strip_prefix("service:")
            .is_some_and(|owner| recreated.contains(owner));
        let want = convert::replicas(&service, &c.scale, name);
        let mut existing: Vec<Machine> = machines
            .iter()
            .filter(|m| m.service == *name && !m.oneoff)
            .cloned()
            .collect();
        existing.sort_by_key(|m| m.number);
        let spec = |number: u32| -> Result<(String, convert::Made)> {
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
                image_cmd: image.cmd.clone(),
                env_files: e.args.env_file.clone(),
                image_volumes: image.volumes.clone(),
            };
            Ok((machine, convert::options(&spec)?))
        };
        // Which replicas are to be made anew, each by what it would be made as now.
        let mut replicas = Vec::new();
        for current in &existing {
            let (_, made) = spec(current.number)?;
            let hash = labels_of(&made).get(label::CONFIG_HASH).cloned();
            // A name it should go by and does not, as a link of another service
            // gives it; one given by hand with network connect stays.
            let has: BTreeSet<String> = client::strings(&current.dict, "aliases")
                .into_iter()
                .collect();
            let stale = parent_recreated
                || current.labels.get(label::CONFIG_HASH) != hash.as_ref()
                || current.label(label::IMAGE) != image.digest
                || !convert::aliases(&made).is_subset(&has);
            replicas.push(Replica {
                number: current.number,
                recreate: recreates(c, name, stale),
                created: current.created,
            });
        }
        let plan = plan_replicas(&replicas, want);
        let by_number = |n: u32| existing.iter().find(|m| m.number == n);
        for number in &plan.remove {
            if let Some(extra) = by_number(*number) {
                stop_and_remove(e, extra, c.timeout).await?;
            }
        }
        for number in &plan.recreate {
            let Some(current) = by_number(*number) else {
                continue;
            };
            let (machine, made) = spec(*number)?;
            if !recreated.contains(name) {
                // planStopDependents: what restarts with this service stops before
                // it is made anew, and starts again once it is what they wait for.
                for dependent in restarted_by(&project, name) {
                    for m in machines
                        .iter()
                        .filter(|m| m.service == dependent && !m.oneoff)
                    {
                        // A stop of the plan, without the pre_stop hooks a stop of the
                        // service runs.
                        if stopped.insert(m.name.clone()) && m.running() {
                            stop_only(e, m, c.timeout).await?;
                        }
                    }
                }
            }
            recreated.insert(name.clone());
            e.progress
                .container(&current.name, "Recreate", Status::Working);
            // Made anew in its place, which the service checks before the old one
            // goes, and which keeps the machines that depend on it pointing at it;
            // under another name, the old one goes first.
            if current.name == machine && !c.renew_anonymous {
                stop_only(e, current, c.timeout).await?;
                make(e, image, &machine, made, true).await?;
            } else {
                // -V: new anonymous volumes, the old ones going with the machine.
                stop_and_remove(e, current, c.timeout).await?;
                make(e, image, &machine, made, false).await?;
            }
            e.progress.container(&machine, "Recreated", Status::Done);
        }
        for number in &plan.create {
            let (machine, made) = spec(*number)?;
            if all.contains(&machine) {
                // Another service of the project made it: their names differ
                // only where machine names write '_' as '-'.
                if let Some(other) = machines.iter().find(|m| m.name == machine) {
                    bail!(
                        "the services {:?} and {name:?} would both make the machine {machine}: machine names take no '_', which is written '-'; rename one of them or its container_name",
                        other.service
                    );
                }
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
    machines = e.machines().await?;
    if !c.start {
        return Ok(machines);
    }
    c.starting_at.set(Some(std::time::SystemTime::now()));
    let started = start_in_order(e, &project, &order, &c.services, c).await?;
    if c.wait {
        wait_running(e, &started, c.wait_timeout).await?;
    }
    Ok(started)
}

/// A replica of a service as up finds it.
#[derive(Debug, Clone, Copy)]
struct Replica {
    number: u32,
    /// Whether this pass makes it anew.
    recreate: bool,
    created: u64,
}

/// What a pass does with a service's replicas, by their numbers.
#[derive(Debug, Default, PartialEq, Eq)]
struct ReplicaPlan {
    remove: Vec<u32>,
    recreate: Vec<u32>,
    create: Vec<u32>,
}

/// reconcileService's replicas: those made anew go first, then the highest numbers,
/// past the count; the ones kept and up to date stay as they are, and new ones take the
/// numbers after the highest there is.
fn plan_replicas(replicas: &[Replica], want: u32) -> ReplicaPlan {
    let mut sorted = replicas.to_vec();
    sorted.sort_by_key(|r| (r.recreate, r.number, std::cmp::Reverse(r.created)));
    let want = want as usize;
    let next = replicas.iter().map(|r| r.number).max().unwrap_or(0) + 1;
    let kept = sorted.len().min(want);
    ReplicaPlan {
        remove: sorted[kept..].iter().map(|r| r.number).collect(),
        recreate: sorted[..kept]
            .iter()
            .filter(|r| r.recreate)
            .map(|r| r.number)
            .collect(),
        create: (0..(want - kept) as u32).map(|i| next + i).collect(),
    }
}

/// GetDependentsForService with restart: the services that depend on `service` with
/// `restart: true`.
fn restarted_by(project: &Project, service: &str) -> Vec<String> {
    project
        .services
        .iter()
        .filter(|(_, s)| s.get(&["depends_on", service, "restart"]) == Some(&Value::Bool(true)))
        .map(|(name, _)| name.clone())
        .collect()
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
    anonymous_volumes(e, &made.anonymous, replace).await?;
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

/// A machine's anonymous volumes, made where missing. Made anew in its place, it keeps
/// those it had; made from nothing, it gets new ones, as a container does: what an
/// earlier machine of the name left goes.
pub(super) async fn anonymous_volumes(
    e: &Engine<'_>,
    names: &[String],
    replace: bool,
) -> Result<()> {
    if names.is_empty() {
        return Ok(());
    }
    let existing: BTreeSet<String> = e
        .client
        .manager
        .list_volumes()
        .await
        .map_err(client::error)?
        .iter()
        .map(|v| client::string(v, "name"))
        .collect();
    let manager = &e.client.manager;
    for name in names {
        if existing.contains(name) {
            if replace {
                continue;
            }
            let list = [name.clone()];
            e.client
                .run_job_silent(|| manager.remove_volumes(&list), true)
                .await
                .with_context(|| format!("removing the anonymous volume {name} a machine left"))?;
        }
        manager
            .create_volume(name)
            .await
            .map_err(client::error)
            .with_context(|| format!("creating the anonymous volume {name}"))?;
    }
    Ok(())
}

/// The anonymous volumes of a machine that is gone, as down -v and rm -v remove them.
pub async fn remove_anonymous(e: &Engine<'_>, machine: &str) -> Result<()> {
    let names: Vec<String> = e
        .client
        .manager
        .list_volumes()
        .await
        .map_err(client::error)?
        .iter()
        .map(|v| client::string(v, "name"))
        .filter(|n| convert::anonymous_of(machine, n))
        .collect();
    if names.is_empty() {
        return Ok(());
    }
    let manager = &e.client.manager;
    e.client
        .run_job_silent(|| manager.remove_volumes(&names), true)
        .await?;
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
        .collect();
    let orphans: Vec<&Machine> = machines.iter().filter(|m| is_orphan(m, &known)).collect();
    if orphans.is_empty() {
        return Ok(());
    }
    let ignore = ignores_orphans(project);
    if c.remove_orphans {
        for m in orphans {
            stop_and_remove(e, m, c.timeout).await?;
        }
    } else if !ignore {
        let names: Vec<&str> = orphans.iter().map(|m| m.name.as_str()).collect();
        e.warn(format!(
            "Found orphan containers ({}) for this project. If you removed or renamed this service in your compose file, you can run this command with the --remove-orphans flag to clean it up.",
            names.join(", ")
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
            wait_dependencies(e, project, &service, name, c.wait_timeout).await?;
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

/// A condition a service waits for on one of the services it depends on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Awaited {
    pub service: String,
    /// service_healthy or service_completed_successfully: service_started is the order
    /// the services start in.
    pub condition: String,
    pub required: bool,
}

/// shouldWaitForDependency over a service's depends_on in the file: the conditions to
/// wait for, without those on a service that is not to run (disabled, as with
/// --no-deps) or has no machine to run (scale 0).
pub fn awaited(project: &Project, service: &Map) -> Vec<Awaited> {
    let Some(deps) = service.get("depends_on").and_then(Value::as_map) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (dep, def) in deps {
        let def = def.as_map().cloned().unwrap_or_default();
        let condition = s(&def, "condition");
        if !matches!(
            condition,
            "service_healthy" | "service_completed_successfully"
        ) {
            continue;
        }
        let Some(of) = project.services.get(dep).and_then(Value::as_map) else {
            continue;
        };
        if convert::replicas(of, &BTreeMap::new(), dep) == 0 {
            continue;
        }
        out.push(Awaited {
            service: dep.clone(),
            condition: condition.to_string(),
            required: def.get("required") != Some(&Value::Bool(false)),
        });
    }
    out
}

/// The conditions of a machine's depends_on label (NAME:CONDITION:RESTART, comma
/// separated), as a project made from the labels has them: every one required.
pub fn awaited_by_label(m: &Machine) -> Vec<Awaited> {
    m.label(label::DEPENDS_ON)
        .split(',')
        .filter_map(|d| {
            let mut parts = d.split(':');
            let service = parts.next().filter(|s| !s.is_empty())?;
            let condition = parts.next().unwrap_or("");
            matches!(
                condition,
                "service_healthy" | "service_completed_successfully"
            )
            .then(|| Awaited {
                service: service.to_string(),
                condition: condition.to_string(),
                required: true,
            })
        })
        .collect()
}

/// Waits for what a service asks of the services it depends on in the file.
pub async fn wait_dependencies(
    e: &Engine<'_>,
    project: &Project,
    service: &Map,
    name: &str,
    timeout: Option<Duration>,
) -> Result<()> {
    wait_for(e, name, awaited(project, service), timeout).await
}

/// waitDependencies: each condition waited for on every machine of its service, saying
/// so (Waiting, then Healthy or Exited), all of them within `timeout` when one is given.
pub async fn wait_for(
    e: &Engine<'_>,
    name: &str,
    awaited: Vec<Awaited>,
    timeout: Option<Duration>,
) -> Result<()> {
    let wait = async {
        for dep in &awaited {
            let machines: Vec<Machine> = e
                .machines()
                .await?
                .into_iter()
                .filter(|m| m.service == dep.service && !m.oneoff)
                .collect();
            if machines.is_empty() {
                if dep.required {
                    bail!("{name} is missing dependency {}", dep.service);
                }
                e.warn(format!("{name} is missing dependency {}", dep.service));
                continue;
            }
            for m in &machines {
                e.progress.container(&m.name, "Waiting", Status::Working);
            }
            for m in &machines {
                let id = format!("Container {}", m.name);
                let failure = match await_condition(e, &m.name, &dep.condition).await {
                    Ok(text) => {
                        e.progress.container(&m.name, text, Status::Done);
                        continue;
                    }
                    Err(failure) => failure,
                };
                let words = failure.words(&dep.service);
                if dep.required {
                    e.progress
                        .event(&id, "Error", &words.details, Status::Error);
                    bail!(words.error);
                }
                e.progress.event(
                    &id,
                    &format!("Skipped: optional dependency {}", words.reason),
                    "",
                    Status::Warning,
                );
                e.warn(words.warning);
            }
        }
        Ok(())
    };
    match timeout {
        Some(t) => tokio::time::timeout(t, wait)
            .await
            .unwrap_or_else(|_| bail!("timeout waiting for dependencies")),
        None => wait.await,
    }
}

/// What failed in a dependency: a start (exited, unhealthy, no healthcheck) with why,
/// or a completion with the code it exited with.
#[derive(Debug)]
enum Failure {
    Start(String),
    Exit(i32),
}

/// How a failed dependency is worded: the error of a required one and its progress
/// line, and the reason and warning of an optional one.
#[derive(Debug, PartialEq, Eq)]
struct Words {
    error: String,
    details: String,
    reason: String,
    warning: String,
}

impl Failure {
    fn words(&self, dep: &str) -> Words {
        let quoted = crate::compose::gostr::quote(dep);
        match self {
            Failure::Start(why) => Words {
                error: format!("dependency failed to start: {why}"),
                details: format!("dependency {dep} failed to start"),
                reason: format!("{quoted} failed to start"),
                warning: format!("optional dependency {quoted} failed to start: {why}"),
            },
            Failure::Exit(code) => {
                let what = format!("{quoted} didn't complete successfully: exit {code}");
                Words {
                    error: format!("service {what}"),
                    details: format!("service {what}"),
                    warning: format!("optional dependency {what}"),
                    reason: what,
                }
            }
        }
    }
}

/// Polls a machine until it is healthy, or has completed with 0.
async fn await_condition(
    e: &Engine<'_>,
    machine: &str,
    condition: &str,
) -> Result<&'static str, Failure> {
    loop {
        let found = e.client.manager.get_machine(machine).await;
        let dict = found.map_err(|err| Failure::Start(format!("{:#}", client::error(err))))?;
        let m = Machine::from_dict(dict);
        if condition == "service_healthy" {
            if m.running() {
                match m.health.as_str() {
                    "healthy" => return Ok("Healthy"),
                    "unhealthy" => {
                        return Err(Failure::Start(format!("container {machine} is unhealthy")))
                    }
                    "" if !checks_health(&m) => {
                        return Err(Failure::Start(format!(
                            "container {machine} has no healthcheck configured"
                        )))
                    }
                    _ => {}
                }
            } else if m.state == "stopped" {
                return Err(Failure::Start(format!(
                    "container {machine} exited ({})",
                    m.exit_code.unwrap_or(0)
                )));
            }
        } else if m.state == "stopped" {
            return match m.exit_code {
                Some(0) | None => Ok("Exited"),
                Some(code) => Err(Failure::Exit(code)),
            };
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Whether a service of the project depends on `service` completing successfully: by
/// the file when one is loaded, by the depends_on labels of `machines` otherwise: the
/// project as its machines tell it.
fn awaited_to_complete(e: &Engine<'_>, machines: &[Machine], service: &str) -> bool {
    match &e.project {
        Some(p) => p.services.values().filter_map(Value::as_map).any(|s| {
            s.get("depends_on")
                .and_then(Value::as_map)
                .and_then(|d| d.get(service))
                .and_then(Value::as_map)
                .is_some_and(|d| {
                    d.get("condition").and_then(Value::as_str)
                        == Some("service_completed_successfully")
                })
        }),
        None => machines.iter().any(|m| completes_for(m, service)),
    }
}

/// Whether `m`'s depends_on label (NAME:CONDITION:RESTART, comma separated) waits for
/// `service` to complete successfully.
fn completes_for(m: &Machine, service: &str) -> bool {
    m.label(label::DEPENDS_ON).split(',').any(|d| {
        let mut parts = d.split(':');
        parts.next() == Some(service) && parts.next() == Some("service_completed_successfully")
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
        .is_some_and(|test| {
            // The service's own rule: a test no probe can run gives no verdict.
            crate::health::Healthcheck {
                test,
                ..Default::default()
            }
            .active()
        })
}

/// --wait: every machine running, and healthy when it has a healthcheck.
pub async fn wait_running(
    e: &Engine<'_>,
    machines: &[Machine],
    timeout: Option<Duration>,
) -> Result<()> {
    let start = Instant::now();
    // Without a file, what waits for a service to complete is in the labels of the
    // project's machines.
    let all = match e.project {
        Some(_) => Vec::new(),
        None => e.machines().await?,
    };
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
                if now.exit_code.unwrap_or(0) == 0 && awaited_to_complete(e, &all, &m.service) {
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

    #[test]
    fn a_network_s_hash_changes_with_its_definition() {
        let hash = |yaml: &str| {
            let p = convert::tests::project(
                "nethash",
                &format!("services:\n  web:\n    image: x\n    networks: [back]\nnetworks:\n  back: {yaml}\n"),
            );
            crate::compose::config::network_hash(&p.networks["back"]).unwrap()
        };
        assert_eq!(hash("{}"), hash("{}"));
        assert_ne!(hash("{}"), hash("{internal: true}"));
        assert_ne!(
            hash("{ipam: {config: [{subnet: 172.30.0.0/24}]}}"),
            hash("{ipam: {config: [{subnet: 172.31.0.0/24}]}}")
        );
    }

    #[test]
    fn a_service_waits_for_the_conditions_of_what_runs() {
        let p = convert::tests::project(
            "awaited",
            "services:\n  db:\n    image: x\n    healthcheck: {test: [CMD, \"true\"]}\n  cache:\n    image: x\n  migrate:\n    image: x\n  idle:\n    image: x\n    scale: 0\n  web:\n    image: x\n    depends_on:\n      db: {condition: service_healthy}\n      migrate: {condition: service_completed_successfully, required: false}\n      idle: {condition: service_healthy}\n      cache: {condition: service_started}\n",
        );
        let web = convert::service(&p, "web").unwrap();
        assert_eq!(
            awaited(&p, web),
            [
                Awaited {
                    service: "db".to_string(),
                    condition: "service_healthy".to_string(),
                    required: true,
                },
                Awaited {
                    service: "migrate".to_string(),
                    condition: "service_completed_successfully".to_string(),
                    required: false,
                },
            ],
            "service_started is the order, a service of scale 0 is not waited for"
        );
        let mut m = super::super::Machine {
            name: "p-web-1".to_string(),
            service: "web".to_string(),
            number: 1,
            oneoff: false,
            state: "stopped".to_string(),
            health: String::new(),
            exit_code: None,
            labels: BTreeMap::new(),
            created: 0,
            started: 0,
            dict: crate::client::Dict::new(),
        };
        m.labels.insert(
            label::DEPENDS_ON.to_string(),
            "db:service_healthy:false,cache:service_started:false,migrate:service_completed_successfully:true".to_string(),
        );
        let by_label: Vec<String> = awaited_by_label(&m)
            .into_iter()
            .map(|a| format!("{}:{}:{}", a.service, a.condition, a.required))
            .collect();
        assert_eq!(
            by_label,
            [
                "db:service_healthy:true",
                "migrate:service_completed_successfully:true"
            ]
        );
    }

    #[test]
    fn a_failed_dependency_is_worded_as_docker_compose_words_it() {
        let start = Failure::Start("container p-db-1 is unhealthy".to_string()).words("db");
        assert_eq!(
            start.error,
            "dependency failed to start: container p-db-1 is unhealthy"
        );
        assert_eq!(start.details, "dependency db failed to start");
        assert_eq!(start.reason, "\"db\" failed to start");
        assert_eq!(
            start.warning,
            "optional dependency \"db\" failed to start: container p-db-1 is unhealthy"
        );
        let exit = Failure::Exit(3).words("migrate");
        assert_eq!(
            exit.error,
            "service \"migrate\" didn't complete successfully: exit 3"
        );
        assert_eq!(exit.details, exit.error);
        assert_eq!(
            exit.reason,
            "\"migrate\" didn't complete successfully: exit 3"
        );
        assert_eq!(
            exit.warning,
            "optional dependency \"migrate\" didn't complete successfully: exit 3"
        );
    }

    use super::*;

    #[test]
    fn a_completion_wait_is_read_from_the_labels_without_a_file() {
        let web = Machine {
            name: "p-web-1".into(),
            service: "web".into(),
            number: 1,
            oneoff: false,
            state: "running".into(),
            health: String::new(),
            exit_code: None,
            labels: BTreeMap::from([(
                label::DEPENDS_ON.to_string(),
                "migrate:service_completed_successfully:false,db:service_healthy:true".to_string(),
            )]),
            created: 0,
            started: 0,
            dict: crate::client::Dict::new(),
        };
        assert!(completes_for(&web, "migrate"));
        assert!(!completes_for(&web, "db"));
        assert!(!completes_for(&web, "web"));
    }

    #[test]
    fn orphans_are_read_as_docker_compose_reads_them() {
        assert!(!super::super::lifecycle::orphans_too(Some(false)));
        assert!(super::super::lifecycle::orphans_too(Some(true)));
        // up's default comes from the project's environment, its .env included.
        let mut project =
            super::super::convert::tests::project("orph", "services:\n  web: {image: nginx}\n");
        assert!(!removes_orphans(&project));
        project
            .environment
            .insert("COMPOSE_REMOVE_ORPHANS".into(), "true".into());
        assert!(removes_orphans(&project));
        let machine = |service: &str, oneoff: bool, state: &str| Machine {
            name: format!("orph-{service}-1"),
            service: service.into(),
            number: 1,
            oneoff,
            state: state.into(),
            health: String::new(),
            exit_code: None,
            labels: BTreeMap::new(),
            created: 0,
            started: 0,
            dict: crate::client::Dict::new(),
        };
        let web = "web".to_string();
        let known = BTreeSet::from([&web]);
        assert!(!is_orphan(&machine("web", false, "running"), &known));
        assert!(is_orphan(&machine("gone", false, "running"), &known));
        // A one-off that ended is one, a running one is somebody's session.
        assert!(is_orphan(&machine("web", true, "stopped"), &known));
        assert!(!is_orphan(&machine("web", true, "running"), &known));
        for text in ["y", " TRUE ", "t", "1", "True"] {
            assert!(string_to_bool(text), "{text}");
        }
        for text in ["yes", "on", "0", ""] {
            assert!(!string_to_bool(text), "{text}");
        }
    }

    #[test]
    fn up_refuses_what_docker_compose_refuses() {
        use clap::Parser;
        let check = |argv: &[&str]| {
            let mut full = vec!["nspawn", "compose", "up"];
            full.extend_from_slice(argv);
            let crate::cli::Command::Compose(c) =
                crate::cli::Cli::try_parse_from(full).unwrap().command
            else {
                unreachable!()
            };
            let Some(crate::compose::cli::ComposeCommand::Up(a)) = c.command else {
                unreachable!()
            };
            validate_flags(&a).err().map(|e| e.to_string())
        };
        for (argv, text) in [
            (&["--wait-timeout", "-1"][..], "--wait-timeout must be a non-negative integer"),
            (
                &["--abort-on-container-exit", "--abort-on-container-failure"],
                "--abort-on-container-failure cannot be combined with --abort-on-container-exit",
            ),
            (
                &["--exit-code-from", "web", "--wait"],
                "--wait cannot be combined with --abort-on-container-exit, --attach or --attach-dependencies",
            ),
            (
                &["--wait", "--attach", "web"],
                "--wait cannot be combined with --abort-on-container-exit, --attach or --attach-dependencies",
            ),
            (
                &["-d", "--attach", "web"],
                "--detach cannot be combined with --abort-on-container-exit, --abort-on-container-failure, --attach, --attach-dependencies or --watch",
            ),
            (
                &["-d", "--exit-code-from", "web"],
                "--detach cannot be combined with --abort-on-container-exit, --abort-on-container-failure, --attach, --attach-dependencies or --watch",
            ),
            (
                &["--always-recreate-deps", "--no-recreate"],
                "--always-recreate-deps and --no-recreate are incompatible",
            ),
            (
                &["-V", "--no-recreate"],
                "--no-recreate and --renew-anon-volumes are incompatible",
            ),
        ] {
            assert_eq!(check(argv).as_deref(), Some(text), "{argv:?}");
        }
        // --exit-code-from with failures alone, and --no-start with --wait, are taken.
        assert_eq!(
            check(&["--exit-code-from", "web", "--abort-on-container-failure"]),
            None
        );
        assert_eq!(check(&["--no-start", "--wait"]), None);
    }

    #[test]
    fn a_test_no_probe_can_run_checks_no_health() {
        use zbus::zvariant::{OwnedValue, Value};
        let machine = |test: &[&str]| {
            let test: Vec<String> = test.iter().map(|w| w.to_string()).collect();
            let hc = crate::client::Dict::from([(
                "test".to_string(),
                OwnedValue::try_from(Value::from(test)).unwrap(),
            )]);
            let dict = crate::client::Dict::from([(
                "healthcheck".to_string(),
                OwnedValue::try_from(Value::from(hc)).unwrap(),
            )]);
            Machine::from_dict(dict)
        };
        assert!(checks_health(&machine(&["CMD", "/check"])));
        assert!(checks_health(&machine(&["CMD-SHELL", "curl -f localhost"])));
        assert!(!checks_health(&machine(&["NONE"])));
        assert!(!checks_health(&machine(&["FOO", "x"])));
        assert!(!checks_health(&machine(&["CMD"])));
        assert!(!checks_health(&Machine::from_dict(
            crate::client::Dict::new()
        )));
    }

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
            policy(&service("never"), Some("if_not_present"), "always"),
            "missing"
        );
        // --pull takes a service's policies; its default word is not one to give.
        for given in ["always", "never", "build", "missing", "if_not_present"] {
            assert!(check_pull(Some(given)).is_ok(), "{given}");
        }
        assert_eq!(
            check_pull(Some("policy")).unwrap_err().to_string(),
            "invalid --pull option \"policy\""
        );
        assert!(check_pull(Some("daily")).is_err());
    }
}
