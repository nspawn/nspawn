//! The commands that work on the machines a project has: down, start, stop, restart,
//! kill, pause and unpause, rm, wait. Each finds them by the project's label, the
//! project loaded or named.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, IsTerminal, Write};

use anyhow::{bail, Result};
use zbus::zvariant::Value as Bus;

use super::progress::Status;
use super::up::{self, stop_and_remove};
use super::{label, Engine, Machine};
use crate::client::{self, Options};
use crate::compose::cli::{DownArgs, KillArgs, RestartArgs, RmArgs, StartArgs, StopArgs, WaitArgs};
use crate::compose::value::Value;

/// The machines of the named services (every service's when none is named); a name
/// the project does not have is an error.
fn selected(e: &Engine<'_>, machines: Vec<Machine>, services: &[String]) -> Result<Vec<Machine>> {
    if let Some(project) = &e.project {
        for name in services {
            if !project.services.contains_key(name) && !project.disabled_services.contains_key(name)
            {
                bail!("no such service: {name}");
            }
        }
    }
    Ok(machines
        .into_iter()
        .filter(|m| services.is_empty() || services.contains(&m.service))
        .collect())
}

/// Machines in the order they start: each after those its service depends on, by the
/// project when it is loaded, by the labels otherwise.
fn start_order(e: &Engine<'_>, mut machines: Vec<Machine>) -> Vec<Machine> {
    let order: Vec<String> = match &e.project {
        Some(project) => up::order(project),
        None => {
            let mut graph: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for m in &machines {
                let deps = m
                    .label(label::DEPENDS_ON)
                    .split(',')
                    .filter_map(|d| d.split(':').next())
                    .filter(|d| !d.is_empty())
                    .map(String::from)
                    .collect();
                graph.insert(m.service.clone(), deps);
            }
            let mut out = Vec::new();
            let mut seen = BTreeSet::new();
            fn visit(
                n: &str,
                graph: &BTreeMap<String, Vec<String>>,
                seen: &mut BTreeSet<String>,
                out: &mut Vec<String>,
            ) {
                if !seen.insert(n.to_string()) {
                    return;
                }
                for d in graph.get(n).into_iter().flatten() {
                    visit(d, graph, seen, out);
                }
                out.push(n.to_string());
            }
            for n in graph.keys() {
                visit(n, &graph, &mut seen, &mut out);
            }
            out
        }
    };
    let rank = |m: &Machine| {
        order
            .iter()
            .position(|s| *s == m.service)
            .unwrap_or(usize::MAX)
    };
    machines.sort_by(|a, b| (rank(a), a.number, &a.name).cmp(&(rank(b), b.number, &b.name)));
    machines
}

/// Whether down removes a machine. One-offs count only with --remove-orphans. A
/// service's machines go when it is selected; with --remove-orphans so do one-offs that
/// ended and machines of services the file lacks, but not those of a profile not
/// enabled.
fn down_takes(
    m: &Machine,
    services: &[String],
    remove_orphans: bool,
    known: Option<&BTreeSet<String>>,
    disabled: &BTreeSet<String>,
) -> bool {
    if m.oneoff && !remove_orphans {
        return false;
    }
    let in_project = known.is_none_or(|k| k.contains(&m.service));
    if in_project && (services.is_empty() || services.contains(&m.service)) {
        return true;
    }
    if !remove_orphans {
        return false;
    }
    if m.oneoff {
        return !m.running();
    }
    !in_project && !disabled.contains(&m.service)
}

/// down's and kill's --remove-orphans, whose default is COMPOSE_REMOVE_ORPHANS of the
/// process's environment, as compose makes it the flag's default.
/// --remove-orphans, COMPOSE_REMOVE_ORPHANS of the process when it is not given, as
/// docker compose takes the variable for the flag's default.
pub(super) fn orphans_too(flag: Option<bool>) -> bool {
    flag.unwrap_or_else(|| {
        std::env::var("COMPOSE_REMOVE_ORPHANS").is_ok_and(|v| up::string_to_bool(&v))
    })
}

pub async fn down(e: &mut Engine<'_>, a: &DownArgs) -> Result<()> {
    let remove_orphans = orphans_too(a.remove_orphans);
    // Checked before anything goes.
    if let Some(rmi) = &a.rmi {
        if !matches!(rmi.as_str(), "all" | "local") {
            bail!("invalid --rmi option {rmi:?}: must be one of \"all\" or \"local\"");
        }
    }
    let machines = e.machines().await?;
    if let Some(project) = &e.project {
        if let Some(name) = a
            .service
            .iter()
            .find(|s| !project.services.contains_key(*s) && !project.jobs.contains_key(*s))
        {
            bail!("no such service: {name}");
        }
    }
    let (known, disabled): (Option<BTreeSet<String>>, BTreeSet<String>) = match &e.project {
        Some(p) => (
            Some(p.services.keys().chain(p.jobs.keys()).cloned().collect()),
            p.disabled_services.keys().cloned().collect(),
        ),
        None => (None, BTreeSet::new()),
    };
    let mut going: Vec<Machine> = machines
        .into_iter()
        .filter(|m| down_takes(m, &a.service, remove_orphans, known.as_ref(), &disabled))
        .collect();
    going = start_order(e, going);
    going.reverse();
    let timeout = a.timeout.map(crate::policy::stop_seconds);
    for m in &going {
        pre_stop(e, m).await;
        stop_and_remove(e, m, timeout).await?;
    }
    if !a.service.is_empty() {
        return Ok(());
    }
    networks_down(e).await?;
    if a.volumes {
        volumes_down(e).await?;
    }
    secrets_down(e).await?;
    if let Some(rmi) = &a.rmi {
        images_down(e, rmi, remove_orphans).await?;
    }
    Ok(())
}

async fn networks_down(e: &Engine<'_>) -> Result<()> {
    let networks: Vec<String> = e
        .client
        .manager
        .list_networks()
        .await
        .map_err(client::error)?
        .iter()
        .filter(|n| {
            n.get("labels")
                .and_then(|l| std::collections::HashMap::<String, String>::try_from(l.clone()).ok())
                .is_some_and(|l| l.get(label::PROJECT) == Some(&e.name))
        })
        .map(|n| client::string(n, "name"))
        .collect();
    for name in networks {
        e.progress.network(&name, "Removing", Status::Working);
        let names = [name.clone()];
        let manager = &e.client.manager;
        match e
            .client
            .run_job_silent(|| manager.remove_networks(&names), true)
            .await
        {
            Ok(_) => e.progress.network(&name, "Removed", Status::Done),
            Err(err) => {
                e.progress.event(
                    &format!("Network {name}"),
                    "Resource is still in use",
                    "",
                    Status::Warning,
                );
                e.warn(format!("{err:#}"));
            }
        }
    }
    Ok(())
}

/// The project's own volumes, with -v: nspawn's volumes carry no labels, so only a
/// project loaded from its files knows them.
async fn volumes_down(e: &Engine<'_>) -> Result<()> {
    let Some(project) = &e.project else {
        e.warn(
            "volumes are only known from the project's files: run down -v with them".to_string(),
        );
        return Ok(());
    };
    let existing: BTreeSet<String> = e
        .client
        .manager
        .list_volumes()
        .await
        .map_err(client::error)?
        .iter()
        .map(|v| client::string(v, "name"))
        .collect();
    for (key, def) in &project.volumes {
        // The host directory a volume binds stays, as with docker.
        if def.get(&["external"]) == Some(&Value::Bool(true))
            || !matches!(super::convert::volume_device(project, key), Ok(None))
        {
            continue;
        }
        let name = super::convert::volume_name(project, key);
        if !existing.contains(&name) {
            continue;
        }
        e.progress.volume(&name, "Removing", Status::Working);
        let names = [name.clone()];
        let manager = &e.client.manager;
        match e
            .client
            .run_job_silent(|| manager.remove_volumes(&names), true)
            .await
        {
            Ok(_) => e.progress.volume(&name, "Removed", Status::Done),
            Err(err) => {
                e.progress.volume(&name, "Error", Status::Error);
                e.warn(format!("{err:#}"));
            }
        }
    }
    Ok(())
}

async fn secrets_down(e: &Engine<'_>) -> Result<()> {
    let secrets: Vec<String> = e
        .client
        .manager
        .list_secrets()
        .await
        .map_err(client::error)?
        .iter()
        .filter(|s| {
            s.get("labels")
                .and_then(|l| std::collections::HashMap::<String, String>::try_from(l.clone()).ok())
                .is_some_and(|l| l.get(label::PROJECT) == Some(&e.name))
        })
        .map(|s| client::string(s, "name"))
        .collect();
    if secrets.is_empty() {
        return Ok(());
    }
    let manager = &e.client.manager;
    if let Err(err) = e
        .client
        .run_job_silent(|| manager.remove_secrets(&secrets), true)
        .await
    {
        e.warn(format!("{err:#}"));
    }
    Ok(())
}

/// --rmi: `local` removes the images the project built for its services, and those of
/// services without an image named as compose names them; `all` the images the
/// services name as well.
/// The records of a reference down --rmi removes: the images that never ran, as up makes
/// its machines from those (a copy pulled beside one that ran among them); else the ones
/// that ran, which are kept with a word.
fn records_of(
    records: &[(String, String, String)],
    ran: &BTreeSet<String>,
    reference: &str,
) -> Vec<String> {
    let of: Vec<&String> = records
        .iter()
        .filter(|(r, _, _)| r == reference)
        .map(|(_, name, _)| name)
        .collect();
    let images: Vec<String> = of
        .iter()
        .filter(|name| !ran.contains(name.as_str()))
        .map(|&name| name.clone())
        .collect();
    if images.is_empty() {
        of.into_iter().cloned().collect()
    } else {
        images
    }
}

async fn images_down(e: &Engine<'_>, rmi: &str, orphans: bool) -> Result<()> {
    if !matches!(rmi, "all" | "local") {
        bail!("invalid --rmi option {rmi:?}: must be one of \"all\" or \"local\"");
    }
    let Some(project) = &e.project else {
        e.warn(
            "images are only known from the project's files: run down --rmi with them".to_string(),
        );
        return Ok(());
    };
    // Machines made from an image keep it, as containers keep theirs: the project's
    // own are gone by now.
    let users: Vec<(String, String)> = e
        .client
        .manager
        .list_machines(true)
        .await
        .map_err(client::error)?
        .iter()
        .filter(|m| client::string(m, "origin") == "create")
        .map(|m| (client::string(m, "digest"), client::string(m, "name")))
        .collect();
    let listed = e
        .client
        .manager
        .list_images()
        .await
        .map_err(client::error)?;
    let records: Vec<(String, String, String)> = listed
        .iter()
        .filter(|i| matches!(client::string(i, "origin").as_str(), "pull" | "build"))
        .map(|i| {
            (
                client::string(i, "reference"),
                client::string(i, "name"),
                client::string(i, "origin"),
            )
        })
        .collect();
    // A record that ran is a machine of its own, whatever image it is.
    let ran: BTreeSet<String> = listed
        .iter()
        .filter(|i| !client::bool(i, "image_only"))
        .map(|i| client::string(i, "name"))
        .collect();
    let find = |image: &str| -> Vec<String> {
        match super::image_reference(image) {
            Ok(reference) => records_of(&records, &ran, &reference.to_string()),
            Err(_) => Vec::new(),
        }
    };
    let mut names = BTreeSet::new();
    for (key, service) in &project.services {
        let Some(service) = service.as_map() else {
            continue;
        };
        let named = service.get("image").and_then(Value::as_str).unwrap_or("");
        if named.is_empty() || rmi == "all" {
            let image = super::build::image_of(&project.name, key, service);
            for name in find(&image) {
                names.insert((image.clone(), name));
            }
        }
    }
    for (reference, name, _) in records.iter().filter(|(_, _, o)| o == "build") {
        let Ok(record) = e.client.manager.get_image(name).await else {
            continue;
        };
        let labels: BTreeMap<String, String> = record
            .get("labels")
            .and_then(|v| std::collections::HashMap::<String, String>::try_from(v.clone()).ok())
            .map(|m| m.into_iter().collect())
            .unwrap_or_default();
        if labels.get(label::PROJECT) != Some(&project.name) {
            continue;
        }
        let service = labels.get(label::SERVICE).map(String::as_str).unwrap_or("");
        if orphans || project.services.contains_key(service) {
            names.insert((reference.clone(), name.clone()));
        }
    }
    let mut seen = BTreeSet::new();
    for (image, name) in names {
        if !seen.insert(name.clone()) {
            continue;
        }
        let digest = match e.client.manager.get_image(&name).await {
            Ok(record) => client::string(&record, "digest"),
            Err(_) => continue,
        };
        if let Some((_, user)) = users.iter().find(|(d, _)| *d == digest) {
            e.warn(format!(
                "image {image} is kept: machine {user} was made from it"
            ));
            continue;
        }
        if ran.contains(&name) {
            e.warn(format!("image {image} is kept: {name} ran as a machine"));
            continue;
        }
        let id = format!("Image {image}");
        e.progress.event(&id, "Removing", "", Status::Working);
        let list = [name];
        let manager = &e.client.manager;
        match e
            .client
            .run_job_silent(|| manager.remove_images(&list), true)
            .await
        {
            Ok(_) => e.progress.event(&id, "Removed", "", Status::Done),
            Err(err) => {
                e.progress.event(&id, "Error", "", Status::Error);
                e.warn(format!("{err:#}"));
            }
        }
    }
    Ok(())
}

/// The pre_stop hooks of a machine's service, run inside it before it stops; a failure
/// is a warning, as the machine goes all the same.
async fn pre_stop(e: &Engine<'_>, m: &Machine) {
    // A paused machine runs nothing, its hooks included: the stop goes on without them.
    if !m.running() || m.state == "paused" {
        return;
    }
    let Some(project) = &e.project else { return };
    let Ok(service) = super::convert::service(project, &m.service) else {
        return;
    };
    for hook in service
        .get("pre_stop")
        .and_then(Value::as_seq)
        .into_iter()
        .flatten()
        .filter_map(Value::as_map)
    {
        if let Err(err) = super::logs::hook(e, &m.name, hook, "pre_stop").await {
            e.warn(format!("{err:#}"));
        }
    }
}

pub async fn start(e: &mut Engine<'_>, a: &StartArgs) -> Result<()> {
    // With a file, as up and create.
    if let Some(project) = &e.project {
        super::up::reject_scheduled_jobs(project)?;
    }
    let machines = selected(e, e.machines().await?, &a.service)?;
    let machines: Vec<Machine> = machines.into_iter().filter(|m| !m.oneoff).collect();
    for name in &a.service {
        if !machines.iter().any(|m| m.service == *name) {
            bail!("service {name:?} has no container to start");
        }
    }
    if machines.is_empty() {
        bail!("no containers to start");
    }
    let machines = start_order(e, machines);
    for m in &machines {
        if m.running() {
            continue;
        }
        up::start_one(e, m).await?;
    }
    if a.wait {
        let timeout = a
            .wait_timeout
            .filter(|t| *t > 0)
            .map(|t| std::time::Duration::from_secs(t as u64));
        up::wait_running(e, &machines, timeout).await?;
    }
    Ok(())
}

pub async fn stop(e: &mut Engine<'_>, a: &StopArgs) -> Result<()> {
    let machines = selected(e, e.machines().await?, &a.service)?;
    let mut machines = start_order(e, machines);
    machines.reverse();
    for m in machines.iter().filter(|m| !m.oneoff) {
        pre_stop(e, m).await;
        stop_one(e, m, a.timeout.map(crate::policy::stop_seconds)).await?;
    }
    Ok(())
}

/// Stops a machine, said for one that is down already too.
async fn stop_one(e: &Engine<'_>, m: &Machine, timeout: Option<u64>) -> Result<()> {
    e.progress.container(&m.name, "Stopping", Status::Working);
    // One that waits in its start for what it depends on is stopped too: the stop job
    // ends the start.
    if !m.running() && !matches!(m.state.as_str(), "restarting" | "starting") {
        e.progress.container(&m.name, "Stopped", Status::Done);
        return Ok(());
    }
    let mut options = Options::new();
    if let Some(t) = timeout {
        options.insert("timeout", Bus::from(t));
    }
    e.client
        .manager
        .stop_machine(&m.name, options)
        .await
        .map_err(client::error)?;
    e.progress.container(&m.name, "Stopped", Status::Done);
    Ok(())
}

/// compose restart's services: those named, and the ones that depend on them with
/// `restart: true`, unless --no-deps; every service when none is named.
fn restart_services(
    project: &crate::compose::project::Project,
    named: &[String],
    no_deps: bool,
) -> Vec<String> {
    let mut out = named.to_vec();
    if named.is_empty() || no_deps {
        return out;
    }
    let mut i = 0;
    while i < out.len() {
        let current = out[i].clone();
        for (name, service) in &project.services {
            let restarts =
                service.get(&["depends_on", &current, "restart"]) == Some(&Value::Bool(true));
            if restarts && !out.contains(name) {
                out.push(name.clone());
            }
        }
        i += 1;
    }
    out
}

/// Whether a restart of `services` (every one when none is named) restarts `dependency`.
fn restarted_with(services: &[String], dependency: &str) -> bool {
    services.is_empty() || services.iter().any(|s| s == dependency)
}

pub async fn restart(e: &mut Engine<'_>, a: &RestartArgs) -> Result<()> {
    let services = match &e.project {
        Some(project) => restart_services(project, &a.service, a.no_deps),
        None => a.service.clone(),
    };
    let machines = selected(e, e.machines().await?, &services)?;
    let machines: Vec<Machine> = start_order(e, machines)
        .into_iter()
        .filter(|m| !m.oneoff)
        .collect();
    let timeout = a.timeout.map(crate::policy::stop_seconds);
    for m in &machines {
        e.progress.container(&m.name, "Restarting", Status::Working);
    }
    for m in &machines {
        // Only the dependencies a restart concerns are waited for: those with
        // restart: true that it restarts too, as the project narrowed to its services
        // keeps them (every service when none is named).
        if let Some(project) = &e.project {
            if let Ok(service) = super::convert::service(project, &m.service) {
                let mut service = service.clone();
                if let Some(Value::Map(deps)) = service.get_mut("depends_on") {
                    deps.retain(|name, d| {
                        d.get(&["restart"]) == Some(&Value::Bool(true))
                            && restarted_with(&services, name)
                    });
                }
                up::wait_dependencies(e, project, &service, &m.service).await?;
            }
        }
        if m.running() {
            pre_stop(e, m).await;
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
        let mut options = Options::new();
        options.insert("wait", Bus::from(true));
        e.client
            .manager
            .start_machine(&m.name, options)
            .await
            .map_err(client::error)?;
        e.progress.container(&m.name, "Started", Status::Done);
        up::post_start(e, m).await?;
    }
    Ok(())
}

/// The machines kill signals, as docker compose's do: running ones, and those between two
/// runs of their restart policy, which docker counts as running; one still starting only
/// for SIGKILL, which ends its start.
fn kills(m: &Machine, sigkill: bool) -> bool {
    m.running() || m.state == "restarting" || (sigkill && m.state == "starting")
}

pub async fn kill(e: &mut Engine<'_>, a: &KillArgs) -> Result<()> {
    let machines = selected(e, e.machines().await?, &a.service)?;
    let signal = a.signal.clone().unwrap_or_else(|| "SIGKILL".to_string());
    // Machines of services the file no longer has are left alone unless asked for.
    let orphan = |m: &Machine| {
        e.project
            .as_ref()
            .is_some_and(|p| !p.services.contains_key(&m.service))
    };
    let sigkill = crate::api::machines::signal_number(&signal).ok() == Some(nix::libc::SIGKILL);
    for m in machines
        .iter()
        .filter(|m| kills(m, sigkill) && (orphans_too(a.remove_orphans) || !orphan(m)))
    {
        e.progress.container(&m.name, "Killing", Status::Working);
        let mut options = Options::new();
        options.insert("signal", Bus::from(signal.clone()));
        e.client
            .manager
            .kill_machine(&m.name, options)
            .await
            .map_err(client::error)?;
        e.progress.container(&m.name, "Killed", Status::Done);
    }
    Ok(())
}

pub async fn pause(e: &mut Engine<'_>, services: &[String], on: bool) -> Result<()> {
    let machines = selected(e, e.machines().await?, services)?;
    for m in machines.iter().filter(|m| m.running()) {
        let manager = &e.client.manager;
        if on {
            manager
                .pause_machine(&m.name)
                .await
                .map_err(client::error)?;
            e.progress.container(&m.name, "Paused", Status::Done);
        } else {
            manager
                .unpause_machine(&m.name)
                .await
                .map_err(client::error)?;
            e.progress.container(&m.name, "Unpaused", Status::Done);
        }
    }
    Ok(())
}

pub async fn rm(e: &mut Engine<'_>, a: &RmArgs) -> Result<()> {
    if a.stop {
        let stop = StopArgs {
            service: a.service.clone(),
            ..Default::default()
        };
        self::stop(e, &stop).await?;
    }
    let machines = selected(e, e.machines().await?, &a.service)?;
    // Only what is stopped: a machine waiting to be restarted, starting or shutting down
    // runs. One-offs stay, -a or not, and so do machines of services the file lacks.
    let stopped: Vec<Machine> = machines
        .into_iter()
        .filter(|m| m.state == "stopped" && !m.oneoff)
        .filter(|m| {
            e.project
                .as_ref()
                .is_none_or(|p| p.services.contains_key(&m.service))
        })
        .collect();
    if stopped.is_empty() {
        eprintln!("No stopped containers");
        return Ok(());
    }
    let names: Vec<&str> = stopped.iter().map(|m| m.name.as_str()).collect();
    let message = format!("Going to remove {}", names.join(", "));
    if a.force {
        println!("{message}");
    } else if !confirm(&message)? {
        return Ok(());
    }
    for m in &stopped {
        e.progress.container(&m.name, "Removing", Status::Working);
        let options = Options::new();
        let list = [m.name.clone()];
        let manager = &e.client.manager;
        e.client
            .run_job_silent(|| manager.remove_machines(&list, options), true)
            .await?;
        e.progress.container(&m.name, "Removed", Status::Done);
    }
    Ok(())
}

/// A yes or no: on a terminal, asked until it is one; from a pipe, one word.
pub fn confirm(message: &str) -> Result<bool> {
    let mut stdout = std::io::stdout();
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        loop {
            write!(stdout, "{message} [y/N]: ")?;
            stdout.flush()?;
            let mut line = String::new();
            if stdin.lock().read_line(&mut line)? == 0 {
                return Ok(false);
            }
            match line.trim().to_lowercase().as_str() {
                "" | "n" | "no" => return Ok(false),
                "y" | "yes" => return Ok(true),
                _ => {}
            }
        }
    }
    write!(stdout, "{message}")?;
    stdout.flush()?;
    let mut line = String::new();
    stdin.lock().read_line(&mut line)?;
    let word = line.split_whitespace().next().unwrap_or("").to_lowercase();
    Ok(matches!(word.as_str(), "1" | "t" | "true"))
}

pub async fn wait(e: &mut Engine<'_>, a: &WaitArgs) -> Result<()> {
    if e.args.dry_run {
        bail!("wait is not supported in dry-run mode");
    }
    let all = e.machines().await?;
    let mut machines: Vec<Machine> = all
        .iter()
        .filter(|m| a.service.is_empty() || a.service.contains(&m.service))
        .filter(|m| m.running())
        .cloned()
        .collect();
    if machines.is_empty() {
        machines = all
            .into_iter()
            .filter(|m| a.service.is_empty() || a.service.contains(&m.service))
            .collect();
    }
    if machines.is_empty() {
        bail!("no containers for project {:?}", e.name);
    }
    let mut code = 0;
    for m in &machines {
        loop {
            let dict = e
                .client
                .manager
                .get_machine(&m.name)
                .await
                .map_err(client::error)?;
            let now = Machine::from_dict(dict);
            if now.state == "stopped" {
                code = now.exit_code.unwrap_or(0);
                println!("container {:?} exited with status code {code}", m.name);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }
    if a.down_project {
        let down = DownArgs {
            remove_orphans: Some(true),
            ..Default::default()
        };
        self::down(e, &down).await?;
    }
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kill_takes_a_machine_between_restarts() {
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
            started: 0,
            dict: Default::default(),
        };
        assert!(kills(&m("running"), false));
        assert!(kills(&m("restarting"), false));
        assert!(!kills(&m("starting"), false));
        assert!(kills(&m("starting"), true));
        assert!(!kills(&m("stopped"), true));
    }

    #[test]
    fn down_removes_the_copy_up_made_and_keeps_what_ran() {
        let reference = "docker.io/library/postgres:18";
        let record = |name: &str| (reference.to_string(), name.to_string(), "pull".to_string());
        let records = [record("postgres-18"), record("postgres-18-2")];
        let ran = BTreeSet::from(["postgres-18".to_string()]);
        assert_eq!(records_of(&records, &ran, reference), ["postgres-18-2"]);
        // Only the one that ran: it is the one said to be kept.
        assert_eq!(records_of(&records[..1], &ran, reference), ["postgres-18"]);
        assert!(records_of(&records, &ran, "docker.io/library/redis:8").is_empty());
    }

    #[test]
    fn a_restart_takes_the_services_that_restart_with_it() {
        let p = crate::compose::engine::convert::tests::project(
            "restart",
            "services:\n  db:\n    image: x\n  web:\n    image: x\n    depends_on:\n      db: {condition: service_started, restart: true}\n  proxy:\n    image: x\n    depends_on:\n      web: {condition: service_started, restart: true}\n  cron:\n    image: x\n    depends_on: [db]\n",
        );
        let named = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            restart_services(&p, &named(&["db"]), false),
            named(&["db", "web", "proxy"])
        );
        assert_eq!(restart_services(&p, &named(&["db"]), true), named(&["db"]));
        assert!(restart_services(&p, &[], false).is_empty());
        // What each restarted machine waits for: a dependency restarted with it only.
        assert!(restarted_with(&[], "db"), "every service restarts");
        assert!(restarted_with(&named(&["db", "web"]), "db"));
        assert!(!restarted_with(&named(&["web"]), "db"));
    }

    #[test]
    fn down_takes_what_docker_compose_takes() {
        let m = |service: &str, oneoff: bool, state: &str| Machine {
            name: format!("p-{service}"),
            service: service.to_string(),
            number: 1,
            oneoff,
            state: state.to_string(),
            health: String::new(),
            exit_code: None,
            labels: BTreeMap::new(),
            created: 0,
            started: 0,
            dict: crate::client::Dict::new(),
        };
        let known: BTreeSet<String> = ["db", "web"].map(String::from).into();
        let disabled: BTreeSet<String> = ["extra"].map(String::from).into();
        let takes = |m: &Machine, services: &[&str], orphans: bool| {
            let services: Vec<String> = services.iter().map(|s| s.to_string()).collect();
            down_takes(m, &services, orphans, Some(&known), &disabled)
        };
        assert!(takes(&m("db", false, "running"), &[], false));
        assert!(!takes(&m("db", false, "running"), &["web"], false));
        // A profile not enabled is left alone, orphans or not.
        assert!(!takes(&m("extra", false, "running"), &[], true));
        // Orphans go with --remove-orphans only.
        assert!(!takes(&m("gone", false, "running"), &[], false));
        assert!(takes(&m("gone", false, "running"), &[], true));
        // One-offs: of a selected service with --remove-orphans, or ended.
        assert!(!takes(&m("db", true, "stopped"), &[], false));
        assert!(takes(&m("db", true, "running"), &[], true));
        assert!(takes(&m("gone", true, "stopped"), &[], true));
        assert!(!takes(&m("gone", true, "running"), &[], true));
        // Without the files, every service counts as the project's.
        assert!(down_takes(
            &m("any", false, "stopped"),
            &[],
            false,
            None,
            &BTreeSet::new()
        ));
    }
}
