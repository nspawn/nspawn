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

pub async fn down(e: &mut Engine<'_>, a: &DownArgs) -> Result<()> {
    let machines = e.machines().await?;
    let known: Option<BTreeSet<String>> = e.project.as_ref().map(|p| {
        p.services
            .keys()
            .chain(p.disabled_services.keys())
            .chain(p.jobs.keys())
            .cloned()
            .collect()
    });
    let mut going: Vec<Machine> = machines
        .into_iter()
        .filter(|m| a.service.is_empty() || a.service.contains(&m.service))
        .filter(|m| {
            // Orphans and one-offs go with --remove-orphans only.
            let orphan = known.as_ref().is_some_and(|k| !k.contains(&m.service));
            (!orphan && !m.oneoff) || a.remove_orphans
        })
        .collect();
    going = start_order(e, going);
    going.reverse();
    let timeout = a.timeout.map(|t| t.max(0) as u64);
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
        images_down(e, rmi).await?;
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
        if def.get(&["external"]) == Some(&Value::Bool(true)) {
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

/// --rmi: the images the services ran, all of them or those without a tag of their own
/// (`local`), which on nspawn are the same: an image here is the pull of a reference.
async fn images_down(e: &Engine<'_>, rmi: &str) -> Result<()> {
    if !matches!(rmi, "all" | "local") {
        bail!("invalid --rmi option {rmi:?}: must be one of \"all\" or \"local\"");
    }
    let Some(project) = &e.project else {
        return Ok(());
    };
    let local: Vec<(String, String)> = e
        .client
        .manager
        .list_images()
        .await
        .map_err(client::error)?
        .iter()
        .filter(|i| client::string(i, "origin") == "pull")
        .map(|i| (client::string(i, "reference"), client::string(i, "name")))
        .collect();
    let mut names = BTreeSet::new();
    for service in project.services.values().filter_map(Value::as_map) {
        let Some(image) = service.get("image").and_then(Value::as_str) else {
            continue;
        };
        let Ok(reference) = super::image_reference(image) else {
            continue;
        };
        if let Some((_, name)) = local.iter().find(|(r, _)| *r == reference.to_string()) {
            names.insert((image.to_string(), name.clone()));
        }
    }
    for (image, name) in names {
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
    if !m.running() {
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
        stop_one(e, m, a.timeout.map(|t| t.max(0) as u64)).await?;
    }
    Ok(())
}

/// Stops a machine, said for one that is down already too.
async fn stop_one(e: &Engine<'_>, m: &Machine, timeout: Option<u64>) -> Result<()> {
    e.progress.container(&m.name, "Stopping", Status::Working);
    if !m.running() && m.state != "restarting" {
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

pub async fn restart(e: &mut Engine<'_>, a: &RestartArgs) -> Result<()> {
    let machines = selected(e, e.machines().await?, &a.service)?;
    let machines: Vec<Machine> = start_order(e, machines)
        .into_iter()
        .filter(|m| !m.oneoff)
        .collect();
    let timeout = a.timeout.map(|t| t.max(0) as u64);
    for m in &machines {
        e.progress.container(&m.name, "Restarting", Status::Working);
    }
    for m in &machines {
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
    }
    Ok(())
}

pub async fn kill(e: &mut Engine<'_>, a: &KillArgs) -> Result<()> {
    let machines = selected(e, e.machines().await?, &a.service)?;
    let signal = a.signal.clone().unwrap_or_else(|| "SIGKILL".to_string());
    for m in machines.iter().filter(|m| m.running()) {
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
    let stopped: Vec<Machine> = machines
        .into_iter()
        .filter(|m| !m.running() && (a.all || !m.oneoff))
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
        let mut options = Options::new();
        options.insert("force", Bus::from(true));
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
            remove_orphans: true,
            ..Default::default()
        };
        self::down(e, &down).await?;
    }
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}
