//! The terminal side of machines: the ps table, start, run and stop, and the commands
//! that own the terminal (exec, shell, logs), whose streams come from the service.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::thread;

use anyhow::{bail, Context as _, Result};
use nix::sys::signal::{pthread_sigmask, SigSet, SigmaskHow, Signal};
use zbus::zvariant::{OwnedObjectPath, Value};

use crate::cli::{
    AutoUpdateArgs, EventsArgs, ExecArgs, KillArgs, LogsArgs, ModeChoice, NamesArgs, PortArgs,
    PsArgs, PullPolicy, RestartArgs, RunArgs, ShellArgs, StartArgs, StartOptions, StopArgs,
    TopArgs, UpdateArgs, WaitArgs,
};
use crate::client::{self, Client, Dict, Ended, Options};
use crate::config::Config;
use crate::output::{human_duration, table};
use crate::pty;
use crate::reference::ImageRef;
use crate::store::now_unix;

pub async fn ls(args: PsArgs, client: &Client) -> Result<()> {
    let now = now_unix();
    let filters = crate::dockerapi::filters::Filters::from_pairs(&args.filters)
        .map_err(|e| anyhow::anyhow!("{}", e.0))?;
    filters
        .only(PS_FILTERS)
        .map_err(|e| anyhow::anyhow!("{}", e.0))?;
    // A status or exit code asked for looks at stopped machines too.
    let all = args.all || filters.values("status").is_some() || filters.values("exited").is_some();
    let listed = client
        .manager
        .list_machines(all)
        .await
        .map_err(client::error)?;
    // before= and since= name any machine, a stopped one too, and one that does not
    // exist is an error.
    let references: Vec<&String> = ["before", "since"]
        .iter()
        .filter_map(|k| filters.values(k))
        .flatten()
        .collect();
    let mut created: HashMap<String, u64> = HashMap::new();
    if !references.is_empty() {
        let every = client
            .manager
            .list_machines(true)
            .await
            .map_err(client::error)?;
        for m in &every {
            created.insert(client::string(m, "name"), client::u64(m, "created"));
        }
        if let Some(missing) = references
            .iter()
            .find(|r| !created.contains_key(r.as_str()))
        {
            bail!("No such container: {missing}");
        }
    }
    let machines: Vec<Dict> = listed
        .iter()
        .filter(|m| ps_matches(&filters, m, &created))
        .map(|m| {
            m.iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        v.try_clone()
                            .expect("plain values carry no file descriptor"),
                    )
                })
                .collect()
        })
        .collect();
    if args.quiet {
        for m in &machines {
            println!("{}", client::string(m, "name"));
        }
        return Ok(());
    }
    if let Some(format) = &args.format {
        let rows: Vec<serde_json::Value> = machines
            .iter()
            .map(|m| ps_row(m, now, !args.no_trunc))
            .collect();
        // .Label "k" is a method of the rows, which the template engine gives them.
        let text = crate::gotemplate::format_list(format, PS_TABLE, &ps_headers(), &rows)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        print!("{text}");
        return Ok(());
    }
    if args.output.json {
        client::print_json(&serde_json::Value::Array(
            machines.iter().map(client::dict_to_json).collect(),
        ));
        return Ok(());
    }
    let rows = machines
        .iter()
        .map(|m| {
            let (image, mode, command) = describe(m);
            let started = client::u64(m, "started");
            let leader = client::u64(m, "leader");
            vec![
                client::string(m, "name"),
                image,
                mode,
                command,
                state_column(m),
                if started > 0 {
                    human_duration(now.saturating_sub(started))
                } else {
                    "-".to_string()
                },
                if leader > 0 {
                    leader.to_string()
                } else {
                    "-".to_string()
                },
                network_column(m),
                client::dash(client::string(m, "os")),
            ]
        })
        .collect();
    println!(
        "{}",
        table(
            &["MACHINE", "IMAGE", "MODE", "COMMAND", "STATE", "UP", "PID", "NETWORK", "OS"],
            rows
        )
    );
    Ok(())
}

/// The filters `ps` takes.
const PS_FILTERS: &[&str] = &[
    "ancestor", "before", "expose", "exited", "health", "id", "is-task", "label", "name",
    "network", "publish", "since", "status", "volume",
];

/// The default table of --format table.
const PS_TABLE: &str =
    "table {{.Names}}\t{{.Image}}\t{{.Command}}\t{{.RunningFor}}\t{{.Status}}\t{{.Ports}}";

/// The state word of a machine in a --format row and the status filter.
fn ps_state(machine: &Dict) -> &'static str {
    match client::string(machine, "state").as_str() {
        "running" | "opening" | "closing" => "running",
        "paused" => "paused",
        "restarting" => "restarting",
        "stopped" if machine.contains_key("exit_code") => "exited",
        _ => "created",
    }
}

/// The status of a machine that ran, `Exited (N) X ago` or `Restarting (N) X ago`,
/// where `word` is Exited or Restarting; without the time when the end of the run
/// is not known (`finished` 0).
pub(crate) fn ended_status(word: &str, code: i32, finished: u64, now: u64) -> String {
    if finished == 0 {
        return format!("{word} ({code})");
    }
    format!(
        "{word} ({code}) {} ago",
        crate::dockerapi::util::human_duration(now.saturating_sub(finished))
    )
}

fn exit_code(machine: &Dict) -> Option<i32> {
    machine
        .get("exit_code")
        .and_then(|v| i32::try_from(v.clone()).ok())
}

fn ps_matches(
    filters: &crate::dockerapi::filters::Filters,
    m: &Dict,
    created: &HashMap<String, u64>,
) -> bool {
    let name = client::string(m, "name");
    let json = client::dict_to_json(m);
    let labels: std::collections::BTreeMap<String, String> = json["labels"]
        .as_object()
        .map(|l| {
            l.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                .collect()
        })
        .unwrap_or_default();
    let reference = client::string(m, "reference");
    let familiar = crate::dockerapi::util::familiar(&reference);
    let ports = client::strings(m, "ports");
    let created_of = |n: &str| created.get(n).copied();
    let health = match client::string(m, "health").as_str() {
        "" => "none".to_string(),
        h => h.to_string(),
    };
    filters.exact("status", ps_state(m))
        && filters.matches("name", &name)
        && filters
            .values("id")
            .is_none_or(|ids| ids.iter().any(|i| name.starts_with(i.as_str())))
        && filters.labels("label", &labels)
        && filters.exact("health", &health)
        && filters.values("exited").is_none_or(|codes| {
            ps_state(m) == "exited" && codes.iter().any(|c| c.parse::<i32>().ok() == exit_code(m))
        })
        && filters.values("network").is_none_or(|nets| {
            // A machine on the host's network or on none is under host or none.
            client::strings(m, "networks")
                .iter()
                .any(|n| nets.contains(n))
                || matches!(client::string(m, "network").as_str(), n @ ("host" | "none") if nets.contains(n))
        })
        && filters.values("ancestor").is_none_or(|images| {
            images.iter().any(|i| {
                *i == familiar || *i == reference || familiar.starts_with(&format!("{i}:"))
            })
        })
        && filters.values("volume").is_none_or(|volumes| {
            client::strings(m, "volumes")
                .iter()
                .any(|v| v.split(':').take(2).any(|part| volumes.contains(part)))
        })
        && filters.values("publish").is_none_or(|wanted| {
            ports.iter().any(|p| {
                let host = p.split("->").next().unwrap_or_default();
                let port = host.rsplit(':').next().unwrap_or(host);
                let proto = p.rsplit('/').next().unwrap_or("tcp");
                // A bare port is a tcp one.
                wanted.contains(&format!("{port}/{proto}"))
                    || (proto == "tcp" && wanted.contains(port))
            })
        })
        && filters.values("expose").is_none_or(|wanted| {
            ports.iter().any(|p| {
                let inside = p.split("->").nth(1).unwrap_or_default();
                wanted.contains(inside)
                    || (inside.ends_with("/tcp")
                        && wanted.contains(inside.split('/').next().unwrap_or_default()))
            })
        })
        && !filters
            .values("is-task")
            .is_some_and(|v| v.contains("true"))
        && filters.values("before").is_none_or(|before| {
            before
                .iter()
                .all(|b| created_of(b).is_some_and(|c| client::u64(m, "created") < c))
        })
        && filters.values("since").is_none_or(|since| {
            since
                .iter()
                .all(|s| created_of(s).is_some_and(|c| client::u64(m, "created") > c))
        })
}

/// A machine with the fields --format templates are written for, and nspawn's own;
/// `trunc` cuts what --no-trunc keeps whole.
fn ps_row(m: &Dict, now: u64, trunc: bool) -> serde_json::Value {
    let json = client::dict_to_json(m);
    let created = client::u64(m, "created");
    let started = client::u64(m, "started");
    // A machine nspawn has no record of has no labels: an empty map, not null.
    let labels = match &json["labels"] {
        v @ serde_json::Value::Object(_) => v.clone(),
        _ => serde_json::json!({}),
    };
    // The KEY=VALUE strings are sorted, not the keys: a.b=x comes after a=y.
    let label_text = labels
        .as_object()
        .map(|l| {
            let mut pairs: Vec<String> = l
                .iter()
                .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or_default()))
                .collect();
            pairs.sort();
            pairs.join(",")
        })
        .unwrap_or_default();
    // Quoted, so that a newline of a compose command keeps to its row.
    let command = match client::string(m, "mode").as_str() {
        "boot" => "/sbin/init".to_string(),
        _ => crate::dockerapi::util::command_line(&client::strings(m, "command")),
    };
    let cut = |text: String, max: usize| match trunc {
        true => crate::gotemplate::ellipsis(&text, max),
        false => text,
    };
    let command = crate::gotemplate::go_quote(&cut(command, 20));
    // A reference pinned to a digest shows its tag, without the digest.
    let reference = client::string(m, "reference");
    let image = match reference.split_once('@') {
        Some((named, _)) if trunc => named,
        _ => reference.as_str(),
    };
    let state = ps_state(m);
    let since = |t: u64| crate::dockerapi::util::human_duration(now.saturating_sub(t));
    let status = match state {
        "running" => match client::string(m, "health").as_str() {
            "" => format!("Up {}", since(started)),
            "starting" => format!("Up {} (health: starting)", since(started)),
            h => format!("Up {} ({h})", since(started)),
        },
        "paused" => format!("Up {} (Paused)", since(started)),
        "restarting" | "exited" => {
            let word = if state == "exited" {
                "Exited"
            } else {
                "Restarting"
            };
            ended_status(
                word,
                exit_code(m).unwrap_or(0),
                client::u64(m, "finished"),
                now,
            )
        }
        _ => "Created".to_string(),
    };
    let ports = client::bindings(m);
    serde_json::json!({
        "ID": client::string(m, "name"),
        "Names": client::string(m, "name"),
        "Image": crate::dockerapi::util::familiar(image),
        "Command": command,
        // Go's Time.String in the local zone; nothing for a machine nspawn has no record
        // of, rather than 1970.
        "CreatedAt": if created > 0 { crate::compose::gotime::time_string(created as i64) } else { String::new() },
        "RunningFor": if created > 0 { format!("{} ago", since(created)) } else { String::new() },
        "Ports": ports.join(", "),
        "State": state,
        "Status": status,
        "Size": "",
        "Labels": label_text,
        "LabelsMap": labels,
        "Mounts": client::strings(m, "volumes").iter().map(|v| cut(v.split(':').next().unwrap_or_default().to_string(), 15)).collect::<Vec<_>>().join(","),
        "Networks": client::strings(m, "networks").join(","),
        "LocalVolumes": client::strings(m, "volumes").iter().filter(|v| !v.starts_with('/')).count().to_string(),
        "Mode": client::string(m, "mode"),
        "Address": client::string(m, "address"),
        "OS": client::string(m, "os"),
        "Pid": client::u64(m, "leader"),
    })
}

/// The header of each field in a --format table.
fn ps_headers() -> serde_json::Value {
    serde_json::json!({
        "ID": "CONTAINER ID", "Names": "NAMES", "Image": "IMAGE", "Command": "COMMAND",
        "CreatedAt": "CREATED AT", "RunningFor": "CREATED", "Ports": "PORTS", "State": "STATE",
        "Status": "STATUS", "Size": "SIZE", "Labels": "LABELS", "Mounts": "MOUNTS",
        "Networks": "NETWORKS", "LocalVolumes": "LOCAL VOLUMES", "Mode": "MODE",
        "Address": "ADDRESS", "OS": "OS", "Pid": "PID",
    })
}

/// The state, and the healthcheck's verdict next to it.
fn state_column(machine: &Dict) -> String {
    let state = client::string(machine, "state");
    match client::string(machine, "health").as_str() {
        "" => state,
        "starting" => format!("{state} (health: starting)"),
        health => format!("{state} ({health})"),
    }
}

/// Whether nspawn installed the machine's image: only then is there a record.
fn recorded(machine: &Dict) -> bool {
    machine.contains_key("reference")
}

/// The kind of network, or the address on each bridge network (with the network's name
/// unless it is the default one) and the published ports.
fn network_column(machine: &Dict) -> String {
    if !recorded(machine) {
        return "-".to_string();
    }
    let networks = client::strings(machine, "networks");
    if networks.is_empty() {
        return client::string(machine, "network");
    }
    let addresses = client::dict_to_json(machine);
    let mut parts: Vec<String> = networks
        .iter()
        .map(|network| {
            let address = addresses["addresses"][network].as_str().unwrap_or("");
            match (network.as_str(), address) {
                ("bridge", "") => "bridge".to_string(),
                ("bridge", address) => address.to_string(),
                (network, "") => network.to_string(),
                (network, address) => format!("{network}:{address}"),
            }
        })
        .collect();
    parts.extend(client::strings(machine, "ports"));
    parts.join(" ")
}

/// Image reference, mode and command of a machine, when nspawn installed its image.
fn describe(machine: &Dict) -> (String, String, String) {
    if !recorded(machine) {
        return ("-".to_string(), "-".to_string(), "-".to_string());
    }
    let mode = client::string(machine, "mode");
    let command = if mode == "boot" {
        "init".to_string()
    } else {
        let joined = client::strings(machine, "command").join(" ");
        if joined.chars().count() > 40 {
            format!("{}...", joined.chars().take(37).collect::<String>())
        } else {
            joined
        }
    };
    (client::string(machine, "reference"), mode, command)
}

pub async fn start(args: StartArgs, client: &Client) -> Result<()> {
    let mut options = start_options(args.options, args.command)?;
    if args.image_command {
        options.insert("image_command", Value::from(true));
    }
    start_machine(client, &args.name, options).await
}

/// `options` of a CreateMachine call with the flags of the start that follows it: the
/// service checks them as it makes the machine, so that a start that would be refused (a
/// mount point twice, nothing to run) leaves no machine behind.
fn with_start_flags<'a>(mut options: Options<'a>, start: &Options<'static>) -> Options<'a> {
    for (key, value) in start {
        // What only a start takes.
        if matches!(*key, "wait" | "remove" | "image_command") {
            continue;
        }
        options.insert(
            key,
            value
                .try_clone()
                .expect("plain values carry no file descriptor"),
        );
    }
    options
}

/// The options of a StartMachine call from what start or run was given.
fn start_options(mut args: StartOptions, command: Vec<String>) -> Result<Options<'static>> {
    // --mount is -v or --tmpfs written out; the files come before the flags, which win.
    let (mounted, tmpfs) = crate::mountopt::split(&args.mount)?;
    args.volume.extend(mounted);
    args.tuning.tmpfs.extend(tmpfs);
    args.env = crate::kvfile::env_with_files(&args.env_file, &args.env)?;
    args.label = crate::kvfile::labels_with_files(&args.label_file, &args.label)?;
    let mut options = Options::new();
    options.insert("wait", Value::from(args.wait));
    if !args.network.is_empty() {
        options.insert("networks", Value::from(args.network));
    }
    if !args.network_alias.is_empty() {
        options.insert("aliases", Value::from(args.network_alias));
    }
    if !args.ip.is_empty() {
        options.insert("addresses", Value::from(args.ip));
    }
    if !args.publish.is_empty() {
        options.insert("publish", Value::from(args.publish));
    }
    if let Some(entrypoint) = args.entrypoint {
        options.insert("entrypoint", Value::from(entrypoint));
    }
    if !args.env.is_empty() {
        // A bare VAR means this environment, not the service's.
        options.insert("env", Value::from(crate::volume::expand_env(&args.env)?));
    }
    if !args.volume.is_empty() {
        options.insert("volume", Value::from(args.volume));
    }
    if !args.label.is_empty() {
        options.insert("label", Value::from(args.label));
    }
    if let Some(restart) = args.restart {
        options.insert("restart", Value::from(restart.name()));
    }
    if let Some(memory) = args.memory {
        options.insert("memory", Value::from(memory));
    }
    if let Some(memory_swap) = args.memory_swap {
        options.insert("memory_swap", Value::from(memory_swap));
    }
    if let Some(cpus) = args.cpus {
        options.insert("cpus", Value::from(cpus));
    }
    if let Some(pids) = args.pids_limit {
        options.insert("pids_limit", Value::from(pids));
    }
    if !command.is_empty() {
        options.insert("command", Value::from(command));
    }
    super::put_health(&mut options, args.health);
    super::put_tuning(&mut options, args.tuning);
    Ok(options)
}

async fn start_machine(client: &Client, name: &str, options: Options<'_>) -> Result<()> {
    let (outcome, notes) = client
        .manager
        .start_machine(name, options)
        .await
        .map_err(client::error)?;
    for note in &notes {
        eprintln!("{note}");
    }
    match outcome.as_str() {
        "ended" => println!("{name} ran and ended already"),
        "restarting" => println!(
            "{name} ended right after starting and is being restarted; see nspawn logs {name}"
        ),
        _ => println!("started {name}"),
    }
    Ok(())
}

/// Where run takes a machine from.
#[derive(Debug, PartialEq, Eq)]
enum Source {
    /// A local image with the reference: create, no registry.
    Local(String),
    Registry,
}

/// A record as ListImages has it, for the commands that look for an image by reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Listed {
    pub name: String,
    pub reference: String,
    /// Never started: an image, where a machine that ran has a backend, a mode and a
    /// network of its own, which a machine made from it would take.
    pub image_only: bool,
    /// When it was pulled or built (unix seconds), not when its image was made.
    pub pulled: u64,
}

impl Listed {
    pub(crate) fn all(images: &[client::Dict]) -> Vec<Listed> {
        images
            .iter()
            .map(|i| Listed {
                name: client::string(i, "name"),
                reference: client::string(i, "reference"),
                image_only: client::bool(i, "image_only"),
                pulled: client::u64(i, "pulled"),
            })
            .collect()
    }

    /// The image of `reference` a new machine is made from: one never run, the last
    /// pulled of several, as a tag names the last pull; never `but`.
    pub(crate) fn image_of<'a>(
        images: &'a [Listed],
        reference: &str,
        but: &str,
    ) -> Option<&'a Listed> {
        images
            .iter()
            .filter(|i| i.reference == reference && i.name != but && i.image_only)
            .max_by_key(|i| i.pulled)
    }
}

/// An existing machine of that name is started with start, or made anew with --force; a
/// mode other than auto can only come with a pull.
/// The references a local image of `text` goes by, in the order run and create look for
/// one: under the configured registry, as nspawn names what it pulls and builds, then
/// with Docker Hub as the registry of a name that gives none, for an image compose took
/// from there. Only after both is the registry asked.
pub(crate) fn local_spellings(text: &str, registry: &str) -> Result<Vec<String>> {
    let mut out = vec![ImageRef::parse(text, registry)?.to_string()];
    if let Ok(hub) = ImageRef::normalized(text) {
        let hub = hub.to_string();
        if !out.contains(&hub) {
            out.push(hub);
        }
    }
    Ok(out)
}

fn run_source(
    images: &[Listed],
    references: &[String],
    name: &str,
    force: bool,
    pull: PullPolicy,
    mode_given: bool,
) -> Result<Source> {
    if !force && images.iter().any(|i| i.name == name) {
        bail!("machine {name} exists: nspawn start {name}, or nspawn run --force to make it anew");
    }
    let local = references
        .iter()
        .find_map(|r| Listed::image_of(images, r, name))
        .map(|i| i.name.clone());
    Ok(match (pull, local) {
        (PullPolicy::Always, _) => Source::Registry,
        (PullPolicy::Never, _) if mode_given => {
            bail!("--mode takes a pull of the image; drop it, or use --pull missing")
        }
        (PullPolicy::Never, None) => {
            bail!(
                "no local image of {}; pull it, or use --pull missing",
                references.first().map_or("", String::as_str)
            )
        }
        (_, Some(source)) if !mode_given => Source::Local(source),
        _ => Source::Registry,
    })
}

/// The name of a machine of run --rm without --name: the image's local name and a random
/// part.
fn run_name(base: &str) -> Result<String> {
    let mut bytes = [0u8; 4];
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .context("reading /dev/urandom")?;
    let base: String = base.chars().take(54).collect();
    Ok(format!("{base}-{}", hex::encode(bytes)))
}

/// run: a machine from a local image or from the registry, started with the
/// options given, which it keeps as after start.
pub async fn run(args: RunArgs, client: &Client, config: &Config) -> Result<()> {
    if !args.detach && !args.options.wait {
        bail!("--no-wait goes with -d: an attached run follows the machine anyway");
    }
    if args.detach && (args.tty || args.interactive) {
        bail!("-d runs the machine in the background; -i and -t are for a run that stays attached");
    }
    // What the start takes is read before anything is pulled or made: a file or a
    // variable that is not there must not leave a machine behind.
    let options_of_start = start_options(args.options, args.command)?;
    let registry = super::registry_name(client, config).await;
    let image = ImageRef::parse(&args.reference, &registry)?;
    let name = match &args.name {
        Some(name) => name.clone(),
        // run --rm keeps the image (see below): the machine it removes
        // gets a name of its own.
        None if args.rm => run_name(&image.local_name())?,
        None => image.local_name(),
    };
    let images = Listed::all(&client.manager.list_images().await.map_err(client::error)?);
    let source = run_source(
        &images,
        &local_spellings(&args.reference, &registry)?,
        &name,
        args.force,
        args.pull,
        args.mode != ModeChoice::Auto,
    )?;
    let mut options = client::registry_options(config);
    options.insert("force", Value::from(args.force));
    if args.backend != crate::cli::BackendChoice::Auto {
        options.insert(
            "backend",
            Value::from(format!("{:?}", args.backend).to_lowercase()),
        );
    }
    let manager = &client.manager;
    // A pulled image is a machine, so --rm would remove the image too and the next run
    // would download it again: it is pulled under its own name and kept, and the machine
    // is made from it, when that name is free. A machine of that name, stopped or not,
    // is somebody's: it is neither replaced (--pull always) nor given another mode.
    let base = image.local_name();
    let keep_image = args.rm
        && matches!(source, Source::Registry)
        && base != name
        && args.mode == ModeChoice::Auto
        && images.iter().all(|i| i.name != base);
    match source {
        Source::Local(source) => {
            let options = with_start_flags(options, &options_of_start);
            client
                .run_job_to_stderr(|| manager.create_machine(&source, &name, options))
                .await?;
        }
        Source::Registry if keep_image => {
            let mut create = client::registry_options(config);
            create.insert("force", Value::from(args.force));
            if args.backend != crate::cli::BackendChoice::Auto {
                create.insert(
                    "backend",
                    Value::from(format!("{:?}", args.backend).to_lowercase()),
                );
            }
            options.insert("name", Value::from(base.clone()));
            if args.no_verify {
                options.insert("verify", Value::from(false));
            }
            client
                .run_job_to_stderr(|| manager.pull_image(&args.reference, options))
                .await?;
            let create = with_start_flags(create, &options_of_start);
            client
                .run_job_to_stderr(|| manager.create_machine(&base, &name, create))
                .await?;
        }
        Source::Registry => {
            options.insert("name", Value::from(name.clone()));
            if args.mode != ModeChoice::Auto {
                options.insert(
                    "mode",
                    Value::from(format!("{:?}", args.mode).to_lowercase()),
                );
            }
            if args.no_verify {
                options.insert("verify", Value::from(false));
            }
            client
                .run_job_to_stderr(|| manager.pull_image(&args.reference, options))
                .await?;
        }
    }
    let mut options = options_of_start;
    if args.rm {
        options.insert("remove", Value::from(true));
    }
    let started = if args.detach {
        start_machine(client, &name, options).await.map(|()| None)
    } else {
        run_attached(client, &name, options, args.tty, args.interactive)
            .await
            .map(Some)
    };
    let code = match started {
        Ok(code) => code,
        Err(e) => {
            if args.rm {
                remove_unless_running(client, &name).await;
            }
            return Err(e);
        }
    };
    let Some(code) = code else { return Ok(()) };
    if args.rm {
        removed(client, &name).await;
    }
    std::process::exit(code);
}

/// An attached run of a machine made already: its console or its program's output, a
/// shell for -it on a booted one.
async fn run_attached(
    client: &Client,
    name: &str,
    options: Options<'_>,
    tty: bool,
    interactive: bool,
) -> Result<i32> {
    let booted = client
        .manager
        .get_image(name)
        .await
        .map_err(client::error)
        .map(|image| client::string(&image, "mode") == "boot")?;
    match (booted, tty, interactive) {
        (true, true, true) => booted_shell(client, name, options).await,
        (true, false, false) | (false, _, _) => {
            attached(client, name, options, tty, interactive).await
        }
        (true, _, _) => bail!(
            "{name} boots an init system: without -i and -t run shows its console, with -it it opens a shell"
        ),
    }
}

/// run without -d: the machine's output (or its terminal) until it ends, and its
/// exit code. Ctrl-C and the like go to its program; a third Ctrl-C within a second
/// leaves it running and returns.
pub(crate) async fn attached(
    client: &Client,
    name: &str,
    mut options: Options<'_>,
    tty: bool,
    interactive: bool,
) -> Result<i32> {
    let (rows, cols) = pty::window_size().unwrap_or((24, 80));
    options.insert("tty", Value::from(tty));
    options.insert("rows", Value::from(rows as u64));
    options.insert("cols", Value::from(cols as u64));
    if let Ok(term) = std::env::var("TERM") {
        options.insert("term", Value::from(term));
    }
    let stdin = io::stdin();
    let mut fds = HashMap::new();
    // A terminal reaches the program through a pipe: given the terminal itself, the
    // program could read what is typed after a detach, or push input into the shell. A
    // file or a pipe goes as it is.
    let piped = if interactive && !tty && nix::unistd::isatty(&stdin).unwrap_or(false) {
        let (read, write) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)
            .context("making a pipe for the input")?;
        thread::spawn(move || {
            let mut set = SigSet::empty();
            set.add(Signal::SIGPIPE);
            let _ = pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&set), None);
            let mut to_program = File::from(write);
            let _ = io::copy(&mut io::stdin().lock(), &mut to_program);
        });
        Some(read)
    } else {
        None
    };
    if interactive && !tty {
        let fd = match &piped {
            Some(read) => read.as_fd(),
            None => stdin.as_fd(),
        };
        fds.insert("stdin", zbus::zvariant::Fd::from(fd));
    }
    let (mut handed, process, notes) = client
        .manager
        .run_machine(name, options, fds)
        .await
        .map_err(client::error)?;
    for note in &notes {
        eprintln!("{note}");
    }
    let ended = Ended::watch(&client.connection, process).await?;
    if let Some(master) = handed.remove("tty") {
        tokio::task::block_in_place(|| pty::run_session(OwnedFd::from(master), interactive))?;
        return ended.status().await;
    }
    let Some(output) = handed.remove("stdout") else {
        bail!("the service returned neither a terminal nor the output");
    };
    let output = OwnedFd::from(output);
    let mut copy = tokio::task::spawn_blocking(move || copy(File::from(output), io::stdout()));
    let mut forwarded = Forwarded::new()?;
    loop {
        tokio::select! {
            done = &mut copy => {
                done.map_err(|_| anyhow::anyhow!("the output pump panicked"))??;
                break;
            }
            signal = forwarded.next() => {
                let Some(signal) = signal else { continue };
                if forwarded.detaching() {
                    eprintln!("\n{name} keeps running; nspawn stop {name} stops it");
                    std::process::exit(0);
                }
                if let Err(e) = ended.signal(signal).await {
                    eprintln!("note: {e:#}");
                }
            }
        }
    }
    ended.status().await
}

/// The signals `run` passes on to the program, and the Ctrl-C count that detaches.
struct Forwarded {
    streams: Vec<(i32, tokio::signal::unix::Signal)>,
    interrupts: Vec<std::time::Instant>,
}

impl Forwarded {
    fn new() -> Result<Self> {
        use tokio::signal::unix::{signal, SignalKind};
        let mut streams = Vec::new();
        for (number, kind) in [
            (nix::libc::SIGINT, SignalKind::interrupt()),
            (nix::libc::SIGTERM, SignalKind::terminate()),
            (nix::libc::SIGHUP, SignalKind::hangup()),
            (nix::libc::SIGQUIT, SignalKind::quit()),
        ] {
            streams.push((number, signal(kind).context("catching signals")?));
        }
        Ok(Forwarded {
            streams,
            interrupts: Vec::new(),
        })
    }

    async fn next(&mut self) -> Option<i32> {
        let futures = self.streams.iter_mut().map(|(number, stream)| {
            let number = *number;
            Box::pin(async move { stream.recv().await.map(|_| number) })
        });
        let (signal, _, _) = futures_util::future::select_all(futures).await;
        if signal == Some(nix::libc::SIGINT) {
            let now = std::time::Instant::now();
            self.interrupts
                .retain(|t| now.duration_since(*t) < std::time::Duration::from_secs(1));
            self.interrupts.push(now);
        }
        signal
    }

    /// Three Ctrl-C within a second.
    fn detaching(&self) -> bool {
        self.interrupts.len() >= 3
    }
}

/// run -it of a booted image: a shell once the machine is up, and the machine
/// powered off when the shell ends, whose exit code is the run's.
async fn booted_shell(client: &Client, name: &str, options: Options<'_>) -> Result<i32> {
    let (_, notes) = client
        .manager
        .start_machine(name, options)
        .await
        .map_err(client::error)?;
    for note in &notes {
        eprintln!("{note}");
    }
    let shell = [
        "/bin/sh".to_string(),
        "-c".to_string(),
        "if [ -x /bin/bash ]; then exec /bin/bash -l; else exec /bin/sh -l; fi".to_string(),
    ];
    // -it asked for a terminal, whatever standard output is (run -it | tee).
    let spec = ExecSpec {
        tty: Some(true),
        ..ExecSpec::default()
    };
    let code = run_command(client, name, &shell, "", spec).await;
    let mut stop = Options::new();
    stop.insert("wait", Value::from(true));
    let stopped = client.manager.stop_machine(name, stop).await;
    let code = code?;
    stopped.map_err(client::error)?;
    Ok(code)
}

/// run --rm leaves nothing behind when the start fails. One that runs (the output's
/// reader went, the service restarted) is removed by the service once it ends, as --rm
/// asked.
pub(crate) async fn remove_unless_running(client: &Client, name: &str) {
    let runs = client.manager.get_machine(name).await.is_ok_and(|m| {
        matches!(
            client::string(&m, "state").as_str(),
            "running" | "opening" | "paused" | "starting" | "restarting"
        )
    });
    if !runs {
        let mut options = Options::new();
        options.insert("force", Value::from(true));
        let names = [name.to_string()];
        let _ = client
            .run_job_to_stderr(|| client.manager.remove_machines(&names, options))
            .await;
    }
}

/// run --rm: the machine goes once its unit is down, in a unit of its own; returns once
/// it is gone (or after a while).
pub(crate) async fn removed(client: &Client, name: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while client.manager.get_image(name).await.is_ok() {
        if std::time::Instant::now() > deadline {
            eprintln!("note: {name} is still there; nspawn rm {name} removes it");
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

pub async fn stop(args: StopArgs, client: &Client) -> Result<()> {
    let mut failed = 0;
    for name in &args.names {
        match stop_one(
            client,
            name,
            args.force,
            args.wait,
            args.timeout.map(crate::policy::stop_seconds),
        )
        .await
        {
            Ok(was_running) => {
                if was_running {
                    println!("stopped {name}");
                } else {
                    println!("{name} was not running");
                }
            }
            Err(e) => {
                eprintln!("error: {e:#}");
                failed += 1;
            }
        }
    }
    if failed > 0 {
        bail!(
            "{failed} of {} machines could not be stopped",
            args.names.len()
        );
    }
    Ok(())
}

/// StopMachine for one name; whether it was running.
async fn stop_one(
    client: &Client,
    name: &str,
    force: bool,
    wait: bool,
    timeout: Option<u64>,
) -> Result<bool> {
    let mut options = Options::new();
    options.insert("force", Value::from(force));
    options.insert("wait", Value::from(wait));
    if let Some(timeout) = timeout {
        options.insert("timeout", Value::from(timeout));
    }
    let (outcome, notes) = client
        .manager
        .stop_machine(name, options)
        .await
        .map_err(client::error)?;
    for note in &notes {
        eprintln!("{note}");
    }
    Ok(outcome != "was-not-running")
}

/// restart: a stop and a start with the remembered options, each name in turn.
pub async fn restart(args: RestartArgs, client: &Client) -> Result<()> {
    let mut failed = 0;
    for name in &args.names {
        let outcome: Result<()> = async {
            stop_one(
                client,
                name,
                false,
                true,
                args.timeout.map(crate::policy::stop_seconds),
            )
            .await?;
            let mut options = Options::new();
            options.insert("wait", Value::from(true));
            let (_, notes) = client
                .manager
                .start_machine(name, options)
                .await
                .map_err(client::error)?;
            for note in &notes {
                eprintln!("{note}");
            }
            Ok(())
        }
        .await;
        match outcome {
            Ok(()) => println!("restarted {name}"),
            Err(e) => {
                eprintln!("error: {e:#}");
                failed += 1;
            }
        }
    }
    if failed > 0 {
        bail!(
            "{failed} of {} machines could not be restarted",
            args.names.len()
        );
    }
    Ok(())
}

pub async fn pause(args: NamesArgs, client: &Client, on: bool) -> Result<()> {
    let mut failed = 0;
    for name in &args.names {
        let result = if on {
            client.manager.pause_machine(name).await
        } else {
            client.manager.unpause_machine(name).await
        };
        match result {
            Ok(()) => println!("{name}"),
            Err(e) => {
                eprintln!("error: {:#}", client::error(e));
                failed += 1;
            }
        }
    }
    if failed > 0 {
        bail!(
            "{failed} of {} machines could not be {}",
            args.names.len(),
            if on { "paused" } else { "unpaused" }
        );
    }
    Ok(())
}

pub async fn top(args: TopArgs, client: &Client) -> Result<()> {
    let processes = client
        .manager
        .machine_processes(&args.machine)
        .await
        .map_err(client::error)?;
    let rows = processes
        .iter()
        .map(|p| {
            vec![
                client::u64(p, "pid").to_string(),
                client::string(p, "user"),
                client::string(p, "time"),
                client::string(p, "command"),
            ]
        })
        .collect();
    println!("{}", table(&["PID", "USER", "TIME", "COMMAND"], rows));
    Ok(())
}

/// wait: each machine's exit code once it has stopped, in the order named; a name that
/// is no machine is an error for the end, the others waited for all the same.
pub async fn wait(args: WaitArgs, client: &Client) -> Result<()> {
    // The ends are followed from a moment before anything is looked at, so that one in
    // between is not missed: a machine of run --rm is gone right after, and one with a
    // restart policy runs again.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let mut filters = vec!["type=machine".to_string(), "event=die".to_string()];
    filters.extend(args.names.iter().map(|n| format!("name={n}")));
    let mut options = Options::new();
    options.insert("since", Value::from(format!("@{}", now.saturating_sub(2))));
    options.insert("filters", Value::from(filters));
    let (mut fds, _process) = client
        .manager
        .events(options)
        .await
        .map_err(client::error)?;
    let stdout = fds
        .remove("stdout")
        .context("the service returned no stream for the events")?;
    // (name, unix seconds, exit code) of each end.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, u64, i32)>();
    thread::spawn(move || {
        use std::io::BufRead;
        for line in io::BufReader::new(File::from(OwnedFd::from(stdout))).lines() {
            let Ok(line) = line else { break };
            let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            let name = event["name"].as_str().unwrap_or("").to_string();
            let time = event["time_usec"].as_u64().unwrap_or(0) / 1_000_000;
            let code = event["attributes"]["exit_code"]
                .as_str()
                .and_then(|c| c.parse().ok())
                .unwrap_or(0);
            if tx.send((name, time, code)).is_err() {
                break;
            }
        }
    });
    let mut ends: Vec<(String, u64, i32)> = Vec::new();
    let mut failed = Vec::new();
    for name in &args.names {
        let machine = match client.manager.get_machine(name).await {
            Ok(machine) => machine,
            Err(e) => {
                failed.push(format!("{:#}", client::error(e)));
                continue;
            }
        };
        if client::string(&machine, "state") == "stopped" {
            println!("{}", exit_code_of(&machine));
            continue;
        }
        // An end of this run, not of one before it: the times are whole seconds, so an
        // end in the second this run started counts only once the run is gone. A run
        // that ends without its program's (a failed start, a stop during a restart's
        // wait) is seen by looking again now and then.
        let started = client::u64(&machine, "started");
        let from = ends_from(started, &client::string(&machine, "state"), now);
        let mut look = tokio::time::interval(std::time::Duration::from_secs(2));
        look.tick().await;
        let code = loop {
            if let Some(i) = ends
                .iter()
                .position(|(n, time, _)| n == name && *time >= from)
            {
                let (_, time, code) = ends.remove(i);
                if time == from {
                    if let Ok(m) = client.manager.get_machine(name).await {
                        let state = client::string(&m, "state");
                        if still_this_run(started, &state, client::u64(&m, "started")) {
                            continue;
                        }
                    }
                }
                break code;
            }
            tokio::select! {
                end = rx.recv() => match end {
                    Some(end) => ends.push(end),
                    None => bail!("the service stopped reporting the ends of machines"),
                },
                _ = look.tick() => {
                    match client.manager.get_machine(name).await {
                        Ok(m) if client::string(&m, "state") == "stopped" => {
                            break exit_code_of(&m);
                        }
                        Ok(_) => {}
                        // Gone (a machine of run --rm): its last end seen is its end.
                        Err(e) => match ends.iter().rev().find(|(n, _, _)| n == name) {
                            Some((_, _, code)) => break *code,
                            None => bail!("{:#}", client::error(e)),
                        },
                    }
                }
            }
        };
        // The record has the program's own code, when it is still there and done.
        let code = match client.manager.get_machine(name).await {
            Ok(m) if client::string(&m, "state") == "stopped" => exit_code_of(&m),
            _ => code,
        };
        println!("{code}");
    }
    if !failed.is_empty() {
        bail!("{}", failed.join("\n"));
    }
    Ok(())
}

/// From which second an end is the one `wait` waits for. A machine that has not begun
/// its run yet (a restart's back-off, a start still held) is waited for to its next run,
/// so an end from before the wait is an earlier run's; one closing has its end, from
/// before the wait or not.
fn ends_from(started: u64, state: &str, now: u64) -> u64 {
    if started == 0 && matches!(state, "restarting" | "starting") {
        now
    } else {
        started
    }
}

/// Whether the run `wait` waits for still goes on, as a look again shows the machine:
/// an end in the very second it began was then an earlier run's.
fn still_this_run(started: u64, state: &str, started_now: u64) -> bool {
    if started == 0 {
        matches!(state, "restarting" | "starting")
    } else {
        matches!(state, "running" | "opening" | "paused") && started_now == started
    }
}

fn exit_code_of(machine: &Dict) -> i32 {
    machine
        .get("exit_code")
        .and_then(|v| i32::try_from(v.clone()).ok())
        .unwrap_or(0)
}

/// port: PORT/PROTO -> HOST:PORT for each published port of a running machine, in
/// natural order, or the host's end of one port.
pub async fn port(args: PortArgs, client: &Client) -> Result<()> {
    let machine = client
        .manager
        .get_machine(&args.name)
        .await
        .map_err(client::error)?;
    let running = client::string(&machine, "state") != "stopped";
    // "IP:HOST->CONTAINER/PROTO", an IPv6 IP in brackets.
    let published: Vec<(String, String)> = if running {
        client::bindings(&machine)
            .iter()
            .filter_map(|p| {
                let (host, container) = p.split_once("->")?;
                Some((container.to_string(), host.to_string()))
            })
            .collect()
    } else {
        Vec::new()
    };
    let mut out: Vec<String> = match &args.port {
        Some(wanted) => {
            let key = if wanted.contains('/') {
                wanted.clone()
            } else {
                format!("{wanted}/tcp")
            };
            let hosts: Vec<String> = published
                .iter()
                .filter(|(c, _)| *c == key)
                .map(|(_, h)| h.clone())
                .collect();
            if hosts.is_empty() {
                bail!("no public port '{wanted}' published for {}", args.name);
            }
            hosts
        }
        None => published
            .iter()
            .map(|(c, h)| format!("{c} -> {h}"))
            .collect(),
    };
    out.sort_by(|a, b| natural_order(a, b));
    for line in out {
        println!("{line}");
    }
    Ok(())
}

/// Digits compared as numbers: 8080/tcp after 443/tcp.
pub(crate) fn natural_order(a: &str, b: &str) -> std::cmp::Ordering {
    let split = |s: &str| -> Vec<(bool, String)> {
        let mut parts: Vec<(bool, String)> = Vec::new();
        for c in s.chars() {
            let digit = c.is_ascii_digit();
            match parts.last_mut() {
                Some((d, part)) if *d == digit => part.push(c),
                _ => parts.push((digit, c.to_string())),
            }
        }
        parts
    };
    let (pa, pb) = (split(a), split(b));
    for (x, y) in pa.iter().zip(&pb) {
        let ordering = match (x.0, y.0) {
            (true, true) => {
                let (nx, ny) = (x.1.trim_start_matches('0'), y.1.trim_start_matches('0'));
                nx.len().cmp(&ny.len()).then_with(|| nx.cmp(ny))
            }
            _ => x.1.cmp(&y.1),
        };
        if ordering != std::cmp::Ordering::Equal {
            return ordering;
        }
    }
    pa.len().cmp(&pb.len())
}

/// auto-update: podman's table, the reasons on standard error; fails when a machine
/// failed or went back to its old image, so that the timer's unit says so.
pub async fn auto_update(args: AutoUpdateArgs, client: &Client) -> Result<()> {
    let mut options = Options::new();
    options.insert("dry_run", Value::from(args.dry_run));
    let result = client
        .run_job_to_stderr(|| client.manager.auto_update(&args.names, options))
        .await?;
    let machines: Vec<HashMap<String, String>> = result
        .get("machines")
        .and_then(|v| Vec::<HashMap<String, String>>::try_from(v.try_clone().ok()?).ok())
        .unwrap_or_default();
    let field = |m: &HashMap<String, String>, k: &str| m.get(k).cloned().unwrap_or_default();
    if args.json {
        for m in &machines {
            println!("{}", serde_json::to_string(m)?);
        }
    } else {
        let rows = machines
            .iter()
            .map(|m| {
                vec![
                    format!("systemd-nspawn@{}.service", field(m, "name")),
                    field(m, "name"),
                    field(m, "image"),
                    field(m, "policy"),
                    field(m, "updated"),
                ]
            })
            .collect();
        println!(
            "{}",
            table(&["UNIT", "CONTAINER", "IMAGE", "POLICY", "UPDATED"], rows)
        );
        for m in &machines {
            let reason = field(m, "reason");
            // A machine without the label is skipped quietly unless it was named.
            let named = args.names.contains(&field(m, "name"));
            if !reason.is_empty() && (field(m, "updated") != "skipped" || named) {
                eprintln!("{}: {reason}", field(m, "name"));
            }
        }
    }
    let failed = machines
        .iter()
        .filter(|m| matches!(field(m, "updated").as_str(), "failed" | "rolled back"))
        .count();
    if failed > 0 {
        bail!("{failed} machine(s) could not be updated");
    }
    Ok(())
}

/// kill: every name gets the signal, each one printed once it did; a name that
/// could not be signalled is reported and makes the command fail at the end.
pub async fn kill(args: KillArgs, client: &Client) -> Result<()> {
    let mut failed = 0;
    for name in &args.names {
        let mut options = Options::new();
        options.insert("signal", Value::from(args.signal.clone()));
        match client.manager.kill_machine(name, options).await {
            Ok(notes) => {
                for note in &notes {
                    eprintln!("{note}");
                }
                println!("{name}");
            }
            Err(e) => {
                eprintln!("error: {:#}", client::error(e));
                failed += 1;
            }
        }
    }
    if failed > 0 {
        bail!(
            "{failed} of {} machines could not be signalled",
            args.names.len()
        );
    }
    Ok(())
}

/// update: every name gets the same changes, each one printed once it has them.
pub async fn update(args: UpdateArgs, client: &Client) -> Result<()> {
    let mut failed = 0;
    for name in &args.names {
        let mut options = Options::new();
        if let Some(restart) = args.restart {
            options.insert("restart", Value::from(restart.name()));
        }
        if let Some(memory) = args.memory {
            options.insert("memory", Value::from(memory));
        }
        if let Some(memory_swap) = args.memory_swap {
            options.insert("memory_swap", Value::from(memory_swap));
        }
        if let Some(cpus) = args.cpus {
            options.insert("cpus", Value::from(cpus));
        }
        if let Some(pids) = args.pids_limit {
            options.insert("pids_limit", Value::from(pids));
        }
        super::put_health(&mut options, args.health.clone());
        match client.manager.update_machine(name, options).await {
            Ok(_) => println!("{name}"),
            Err(e) => {
                eprintln!("error: {:#}", client::error(e));
                failed += 1;
            }
        }
    }
    if failed > 0 {
        bail!(
            "{failed} of {} machines could not be updated",
            args.names.len()
        );
    }
    Ok(())
}

/// exec: the service runs the command inside the machine and hands its streams
/// over, a pseudo terminal when this is one, pipes otherwise; the exit status comes
/// back through the process object.
pub async fn exec(args: ExecArgs, client: &Client) -> Result<()> {
    if args.privileged == Some(true) {
        eprintln!(
            "note: --privileged is not used: the command gets the machine's own capabilities"
        );
    }
    let spec = ExecSpec {
        env: crate::volume::expand_env(&args.env)?,
        workdir: args.workdir,
        tty: if args.tty {
            Some(true)
        } else if args.no_tty {
            Some(false)
        } else {
            None
        },
        detach: args.detach,
        no_stdin: false,
    };
    let user = args.user.as_deref().unwrap_or("");
    let code = run_command(client, &args.machine, &args.command, user, spec).await?;
    std::process::exit(code);
}

/// What `exec` asks beyond the command.
#[derive(Debug, Default)]
pub(crate) struct ExecSpec {
    pub env: Vec<String>,
    pub workdir: Option<String>,
    /// None: a terminal when this is one.
    pub tty: Option<bool>,
    pub detach: bool,
    /// The command gets no input: its stdin ends at once, and this one is not read.
    pub no_stdin: bool,
}

/// A shell: the login session machined offers for a booted machine, the namespaces
/// for an app (nothing inside to log in with).
pub async fn shell(args: ShellArgs, client: &Client) -> Result<()> {
    let image = client.manager.get_image(&args.machine).await.ok();
    if image.is_some_and(|i| client::string(&i, "mode") == "app") {
        let shell = vec!["/bin/sh".to_string()];
        let code = run_command(
            client,
            &args.machine,
            &shell,
            &args.user,
            ExecSpec::default(),
        )
        .await?;
        std::process::exit(code);
    }
    let mut options = Options::new();
    if let Some(term) = terminal_env() {
        options.insert("env", Value::from(vec![term]));
    }
    let (fd, _pty) = client
        .manager
        .shell(&args.machine, &args.user, options)
        .await
        .map_err(client::error)?;
    tokio::task::block_in_place(|| pty::run_session(OwnedFd::from(fd), true))
}

/// TERM as this terminal has it, for the machine's side of a pseudo terminal.
fn terminal_env() -> Option<String> {
    std::env::var("TERM")
        .ok()
        .filter(|t| !t.is_empty())
        .map(|t| format!("TERM={t}"))
}

pub(crate) async fn run_command(
    client: &Client,
    machine: &str,
    argv: &[String],
    user: &str,
    spec: ExecSpec,
) -> Result<i32> {
    // A terminal when both ends are one: with the output redirected to a file, a pty
    // would turn every newline into CRLF and tell the command it has a terminal.
    let tty = !spec.detach
        && spec.tty.unwrap_or_else(|| {
            nix::unistd::isatty(io::stdin()).unwrap_or(false)
                && nix::unistd::isatty(io::stdout()).unwrap_or(false)
        });
    let (rows, cols) = pty::window_size().unwrap_or((24, 80));
    let mut options = Options::new();
    options.insert("tty", Value::from(tty));
    options.insert("rows", Value::from(rows as u64));
    options.insert("cols", Value::from(cols as u64));
    let mut env = spec.env;
    if tty {
        if let Some(term) = terminal_env() {
            env.push(term);
        }
    }
    if !env.is_empty() {
        options.insert("env", Value::from(env));
    }
    if let Some(workdir) = spec.workdir {
        options.insert("workdir", Value::from(workdir));
    }
    if spec.detach {
        options.insert("detach", Value::from(true));
    }
    let (mut fds, process): (HashMap<String, zbus::zvariant::OwnedFd>, OwnedObjectPath) = client
        .manager
        .exec(machine, argv, user, options)
        .await
        .map_err(client::error)?;
    if spec.detach {
        return Ok(0);
    }
    let ended = Ended::watch(&client.connection, process).await?;
    let mut take = |name: &str| fds.remove(name).map(OwnedFd::from);
    if let Some(master) = take("tty") {
        tokio::task::block_in_place(|| pty::run_session(master, true))?;
    } else {
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (take("stdin"), take("stdout"), take("stderr"))
        else {
            bail!("the service returned neither a terminal nor pipes");
        };
        if spec.no_stdin {
            drop(stdin);
            tokio::task::block_in_place(|| pump_output(stdout, stderr))?;
        } else {
            tokio::task::block_in_place(|| pump_pipes(stdin, stdout, stderr))?;
        }
    }
    // The streams end when the command (and whatever it left behind) closes them; the
    // exit status may take its time after that.
    ended.status().await
}

/// Copies this process's streams to and from the command's pipes until its output ends.
fn pump_pipes(stdin: OwnedFd, stdout: OwnedFd, stderr: OwnedFd) -> Result<()> {
    let writer = thread::spawn(move || {
        // The command may exit without reading its input; a broken pipe is then an
        // error to see, not a signal to die of.
        let mut set = SigSet::empty();
        set.add(Signal::SIGPIPE);
        let _ = pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&set), None);
        let mut to_command = File::from(stdin);
        let mut buf = [0u8; 8192];
        let mut input = io::stdin().lock();
        loop {
            match input.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if to_command.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let outcome = pump_output(stdout, stderr);
    // Input still pending is of no use once the command is gone.
    drop(writer);
    outcome
}

/// Copies a process's output pipes to ours until both end.
fn pump_output(stdout: OwnedFd, stderr: OwnedFd) -> Result<()> {
    let out = thread::spawn(move || copy(File::from(stdout), io::stdout().lock()));
    let err = thread::spawn(move || copy(File::from(stderr), io::stderr().lock()));
    out.join()
        .map_err(|_| anyhow::anyhow!("stdout pump panicked"))??;
    err.join()
        .map_err(|_| anyhow::anyhow!("stderr pump panicked"))??;
    Ok(())
}

fn copy(mut from: File, mut to: impl Write) -> Result<()> {
    let mut buf = [0u8; 8192];
    loop {
        match from.read(&mut buf) {
            Ok(0) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e).context("reading from the command"),
            Ok(n) => {
                to.write_all(&buf[..n])?;
                to.flush()?;
            }
        }
    }
}

/// events: the service reads the journal and sends one JSON object per event.
pub async fn events(args: EventsArgs, client: &Client) -> Result<()> {
    // A bad filter is said before anything is asked.
    crate::api::events::Filters::parse(&args.filters)?;
    let mut options = Options::new();
    if let Some(since) = args.since {
        options.insert("since", Value::from(since));
    }
    if let Some(until) = args.until {
        options.insert("until", Value::from(until));
    }
    if !args.filters.is_empty() {
        options.insert("filters", Value::from(args.filters));
    }
    let (mut fds, process) = client
        .manager
        .events(options)
        .await
        .map_err(client::error)?;
    let ended = Ended::watch(&client.connection, process).await?;
    let (Some(stdout), Some(stderr)) = (fds.remove("stdout"), fds.remove("stderr")) else {
        bail!("the service returned no streams for the events");
    };
    let json = args.json;
    let err = thread::spawn(move || copy(File::from(OwnedFd::from(stderr)), io::stderr().lock()));
    tokio::task::block_in_place(|| -> Result<()> {
        use std::io::BufRead;
        let mut out = io::stdout().lock();
        for line in io::BufReader::new(File::from(OwnedFd::from(stdout))).lines() {
            let line = line.context("reading the events")?;
            if json {
                writeln!(out, "{line}")?;
            } else if let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) {
                writeln!(out, "{}", event_line(&event))?;
            }
            out.flush()?;
        }
        Ok(())
    })?;
    err.join()
        .map_err(|_| anyhow::anyhow!("stderr pump panicked"))??;
    let code = ended.status().await?;
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

/// An event as one line: time, type, action, name, then its attributes
/// and labels.
fn event_line(event: &serde_json::Value) -> String {
    let text = |key: &str| event.get(key).and_then(|v| v.as_str()).unwrap_or("");
    let mut details: Vec<String> = Vec::new();
    for key in ["attributes", "labels"] {
        if let Some(map) = event.get(key).and_then(|v| v.as_object()) {
            for (k, v) in map {
                details.push(format!("{k}={}", v.as_str().unwrap_or("")));
            }
        }
    }
    let mut line = format!(
        "{} {} {} {}",
        text("time"),
        text("type"),
        text("action"),
        text("name")
    );
    if !details.is_empty() {
        line.push_str(&format!(" ({})", details.join(", ")));
    }
    line
}

/// logs: journalctl's output comes through pipes from the service, and its exit
/// status through the process object.
pub async fn logs(args: LogsArgs, client: &Client) -> Result<()> {
    let mut streams = Vec::new();
    for machine in &args.machines {
        let mut options = Options::new();
        options.insert("follow", Value::from(args.follow));
        if let Some(lines) = args.lines {
            options.insert("lines", Value::from(lines as u64));
        }
        if let Some(since) = &args.since {
            options.insert("since", Value::from(since.clone()));
        }
        if let Some(until) = &args.until {
            options.insert("until", Value::from(until.clone()));
        }
        options.insert("timestamps", Value::from(args.timestamps));
        options.insert("all", Value::from(args.all));
        options.insert("inside", Value::from(args.inside));
        let (mut fds, process) = client
            .manager
            .logs(machine, options)
            .await
            .map_err(client::error)?;
        let ended = Ended::watch(&client.connection, process).await?;
        let (Some(stdout), Some(stderr)) = (fds.remove("stdout"), fds.remove("stderr")) else {
            bail!("the service returned no streams for the logs of {machine}");
        };
        streams.push((
            machine.clone(),
            OwnedFd::from(stdout),
            OwnedFd::from(stderr),
            ended,
        ));
    }
    let mut ended = Vec::new();
    if streams.len() == 1 {
        let (_, stdout, stderr, end) = streams.remove(0);
        tokio::task::block_in_place(|| pump_output(stdout, stderr))?;
        ended.push(end);
    } else {
        let mut pumps = Vec::new();
        for (name, stdout, stderr, end) in streams {
            pumps.push((name, stdout, stderr));
            ended.push(end);
        }
        tokio::task::block_in_place(|| pump_prefixed(pumps))?;
    }
    let mut code = 0;
    for end in ended {
        code = code.max(end.status().await?);
    }
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

/// Several machines' output on this process's streams, every line behind its machine's
/// name. Lines are printed whole, so they never mix.
fn pump_prefixed(streams: Vec<(String, OwnedFd, OwnedFd)>) -> Result<()> {
    let width = streams.iter().map(|(n, _, _)| n.len()).max().unwrap_or(0);
    let mut threads = Vec::new();
    for (name, stdout, stderr) in streams {
        for (fd, to_stderr) in [(stdout, false), (stderr, true)] {
            let prefix = format!("{name:width$} | ");
            threads.push(thread::spawn(move || -> Result<()> {
                let reader = io::BufReader::new(File::from(fd));
                for line in io::BufRead::split(reader, b'\n') {
                    let line = line.context("reading the logs")?;
                    let text = String::from_utf8_lossy(&line);
                    if to_stderr {
                        eprintln!("{prefix}{text}");
                    } else {
                        println!("{prefix}{text}");
                    }
                }
                Ok(())
            }));
        }
    }
    for thread in threads {
        thread
            .join()
            .map_err(|_| anyhow::anyhow!("logs pump panicked"))??;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_takes_the_end_of_the_run_it_waits_for() {
        // Running since 100: an end at 100 or later; one at 100 only once the run is gone.
        assert_eq!(ends_from(100, "running", 130), 100);
        assert!(still_this_run(100, "running", 100));
        assert!(still_this_run(100, "paused", 100));
        assert!(
            !still_this_run(100, "running", 101),
            "a run of its own since"
        );
        assert!(!still_this_run(100, "stopped", 100));
        // In a restart's back-off at 130: the crash that put it there is not waited for.
        assert_eq!(ends_from(0, "restarting", 130), 130);
        // Closing, its program ended: the end before the wait is the one.
        assert_eq!(ends_from(0, "closing", 130), 0);
        assert!(still_this_run(0, "restarting", 0));
        assert!(still_this_run(0, "starting", 0));
        assert!(!still_this_run(0, "running", 131));
    }

    #[test]
    fn a_machine_without_a_record_has_no_labels_for_ps() {
        let row = ps_row(&Dict::new(), 0, true);
        let text = crate::gotemplate::format_list(
            r#"{{.Names}}<{{or (index .LabelsMap "x") ""}}>"#,
            PS_TABLE,
            &ps_headers(),
            &[row],
        )
        .unwrap();
        assert_eq!(text, "<>\n");
    }

    #[test]
    fn label_is_a_method_of_the_rows() {
        let m = Dict::from([(
            "labels".to_string(),
            zbus::zvariant::OwnedValue::try_from(zbus::zvariant::Value::from(
                std::collections::HashMap::from([("app", "web")]),
            ))
            .unwrap(),
        )]);
        let row = ps_row(&m, 0, true);
        for (format, want) in [
            (r#"{{.Label "app"}}"#, "web"),
            ("{{.Label `app`}}", "web"),
            (r#"{{$c := .}}{{$c.Label "app"}}"#, "web"),
            (r#"{{with .Names}}{{$.Label "app"}}{{end}}"#, ""),
            (r#"{{"app" | .Label}}"#, "web"),
            (r#"tag .Label "x" {{.Label "none"}}"#, "tag .Label \"x\" "),
        ] {
            let text = crate::gotemplate::format_list(
                format,
                PS_TABLE,
                &ps_headers(),
                std::slice::from_ref(&row),
            )
            .unwrap();
            assert_eq!(text, format!("{want}\n"), "{format}");
        }
        assert!(
            crate::gotemplate::format_list("{{.Label}}", PS_TABLE, &ps_headers(), &[row])
                .unwrap_err()
                .contains("wrong number of args for Label: want 1 got 0")
        );
    }

    #[test]
    fn ps_filters_match_by_kind_and_key() {
        let value =
            |v: zbus::zvariant::Value<'static>| zbus::zvariant::OwnedValue::try_from(v).unwrap();
        let m = Dict::from([
            ("name".to_string(), value("dns".into())),
            ("network".to_string(), value("host".into())),
            ("ports".to_string(), value(vec!["53->53/udp"].into())),
            ("created".to_string(), value(20u64.into())),
        ]);
        let f = |pairs: &[&str]| {
            crate::dockerapi::filters::Filters::from_pairs(
                &pairs.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
            )
            .unwrap()
        };
        let created = HashMap::from([("db".to_string(), 10u64), ("dns".to_string(), 20)]);
        assert!(ps_matches(&f(&["network=host"]), &m, &created));
        assert!(
            !ps_matches(&f(&["publish=53"]), &m, &created),
            "a bare port is tcp"
        );
        assert!(ps_matches(&f(&["publish=53/udp"]), &m, &created));
        assert!(ps_matches(&f(&["since=db"]), &m, &created));
        assert!(!ps_matches(&f(&["before=db"]), &m, &created));
        // As FilterOpt: an empty one is none, one of blanks is no key=value.
        let pairs = |p: &[&str]| p.iter().map(|p| p.to_string()).collect::<Vec<_>>();
        assert!(crate::dockerapi::filters::Filters::from_pairs(&pairs(&[""])).is_ok());
        assert!(crate::dockerapi::filters::Filters::from_pairs(&pairs(&[" "])).is_err());
    }

    #[test]
    fn labels_are_sorted_as_key_value_strings() {
        let m = Dict::from([(
            "labels".to_string(),
            zbus::zvariant::OwnedValue::try_from(zbus::zvariant::Value::from(
                std::collections::HashMap::from([
                    ("com.docker.compose.project", "demo"),
                    ("com.docker.compose.project.config_files", "/x/compose.yaml"),
                ]),
            ))
            .unwrap(),
        )]);
        assert_eq!(
            ps_row(&m, 0, true)["Labels"],
            "com.docker.compose.project.config_files=/x/compose.yaml,com.docker.compose.project=demo"
        );
        assert_eq!(ps_row(&m, 0, true)["CreatedAt"], "", "no record, no 1970");
    }

    #[test]
    fn a_command_keeps_to_its_row() {
        let m = Dict::from([(
            "command".to_string(),
            zbus::zvariant::OwnedValue::try_from(zbus::zvariant::Value::from(vec![
                "sh",
                "-c",
                "echo \"a\"\nexec sleep infinity",
            ]))
            .unwrap(),
        )]);
        // An argument with a space in single quotes, cut to 20 columns unless
        // --no-trunc, and quoted.
        assert_eq!(
            ps_row(&m, 0, true)["Command"],
            "\"sh -c 'echo \\\"a\\\"\\nexe\u{2026}\""
        );
        assert_eq!(
            ps_row(&m, 0, false)["Command"],
            "\"sh -c 'echo \\\"a\\\"\\nexec sleep infinity'\""
        );
    }

    #[test]
    fn run_gives_its_create_the_flags_of_its_start() {
        use clap::Parser;
        let cli = crate::cli::Cli::try_parse_from([
            "nspawn",
            "run",
            "app",
            "--entrypoint",
            "/app",
            "-v",
            "data:/data",
            "--tmpfs",
            "/data",
            "--",
            "--serve",
        ])
        .unwrap();
        let crate::cli::Command::Run(args) = cli.command else {
            panic!("not run");
        };
        let start = start_options(args.options, args.command).unwrap();
        let mut create = Options::new();
        create.insert("force", Value::from(false));
        let create = with_start_flags(create, &start);
        assert_eq!(
            String::try_from(create["entrypoint"].try_clone().unwrap()).unwrap(),
            "/app"
        );
        assert_eq!(
            Vec::<String>::try_from(create["command"].try_clone().unwrap()).unwrap(),
            ["--serve"]
        );
        // The mounts too, which the service checks for a place taken twice.
        assert_eq!(
            Vec::<String>::try_from(create["volume"].try_clone().unwrap()).unwrap(),
            ["data:/data"]
        );
        assert_eq!(
            Vec::<String>::try_from(create["tmpfs"].try_clone().unwrap()).unwrap(),
            ["/data"]
        );
        // What only a start takes stays out: CreateMachine refuses options it does not know.
        assert!(create.contains_key("force") && !create.contains_key("wait"));
        let mut start = start;
        start.insert("remove", Value::from(true));
        start.insert("image_command", Value::from(true));
        let create = with_start_flags(Options::new(), &start);
        assert!(!["wait", "remove", "image_command"]
            .iter()
            .any(|key| create.contains_key(key)));
    }

    #[test]
    fn ended_runs_say_how_long_ago() {
        assert_eq!(
            ended_status("Exited", 3, 1_000, 1_006),
            "Exited (3) 6 seconds ago"
        );
        assert_eq!(
            ended_status("Restarting", 1, 1_000, 1_090),
            "Restarting (1) About a minute ago"
        );
        assert_eq!(ended_status("Exited", 0, 0, 1_006), "Exited (0)");
        // A clock that went back is less than a second ago.
        assert_eq!(
            ended_status("Exited", 137, 2_000, 1_000),
            "Exited (137) Less than a second ago"
        );
    }

    #[test]
    fn ps_cuts_mounts_and_digests() {
        let value = |v: zbus::zvariant::Value<'_>| zbus::zvariant::OwnedValue::try_from(v).unwrap();
        let m = Dict::from([
            (
                "reference".to_string(),
                value(zbus::zvariant::Value::from(
                    "docker.io/library/nginx:1.27@sha256:0000000000000000000000000000000000000000000000000000000000000000",
                )),
            ),
            (
                "volumes".to_string(),
                value(zbus::zvariant::Value::from(vec![
                    "/srv/www/static-files:/usr/share/nginx/html:ro",
                    "data:/data",
                ])),
            ),
        ]);
        let cut = ps_row(&m, 0, true);
        assert_eq!(cut["Image"], "nginx:1.27");
        assert_eq!(cut["Mounts"], "/srv/www/stati\u{2026},data");
        let whole = ps_row(&m, 0, false);
        assert_eq!(
            whole["Image"],
            "nginx:1.27@sha256:0000000000000000000000000000000000000000000000000000000000000000"
        );
        assert_eq!(whole["Mounts"], "/srv/www/static-files,data");
    }

    #[test]
    fn ports_in_natural_order() {
        let mut lines = vec![
            "9000/tcp -> 0.0.0.0:9000",
            "80/tcp -> 0.0.0.0:8080",
            "443/tcp -> 127.0.0.1:443",
            "80/udp -> 0.0.0.0:8080",
        ];
        lines.sort_by(|a, b| natural_order(a, b));
        assert_eq!(
            lines,
            [
                "80/tcp -> 0.0.0.0:8080",
                "80/udp -> 0.0.0.0:8080",
                "443/tcp -> 127.0.0.1:443",
                "9000/tcp -> 0.0.0.0:9000"
            ]
        );
    }

    /// A machine with every flag `generate` knows, and the same machine before any.
    fn generated_pair() -> (crate::store::ImageRecord, crate::store::ImageRecord) {
        let mut fresh: crate::store::ImageRecord = serde_json::from_value(serde_json::json!({
            "name": "web",
            "reference": "docker.io/library/nginx:1.27",
            "manifest_digest": "sha256:00",
            "layers": [],
            "backend": "overlay",
            "created": 0,
        }))
        .unwrap();
        fresh.mode = crate::oci::Mode::App;
        fresh.run.entrypoint = vec!["/docker-entrypoint.sh".into()];
        fresh.run.cmd = vec!["nginx".into(), "-g".into(), "daemon off;".into()];
        fresh.run.healthcheck = Some(crate::health::Healthcheck {
            test: vec![
                "CMD".into(),
                "curl".into(),
                "-f".into(),
                "http://localhost/".into(),
            ],
            interval: 30_000_000,
            timeout: 5_000_000,
            retries: 3,
            ..Default::default()
        });
        let mut r = fresh.clone();
        crate::api::network::apply(
            &mut r,
            &crate::api::network::choices(&["backend".into(), "front".into()]).unwrap(),
        );
        r.aliases = crate::api::network::parse_aliases(
            &["www".into(), "front=web-front".into()],
            &["backend", "front"],
        )
        .unwrap();
        r.ports = crate::bridge::parse_publish(&["127.0.0.1:8080:80".into(), "5353:53/udp".into()])
            .unwrap();
        r.entrypoint = Some(vec!["/bin/sh".into()]);
        r.cmd = Some(vec!["-c".into(), "echo 'hi there'; exec nginx".into()]);
        r.env = vec!["A=1".into(), "B=two words".into()];
        r.volumes =
            crate::volume::parse_volumes(&["data:/var/lib/data".into(), "/srv/x:/x:ro".into()])
                .unwrap();
        r.labels = [("tier".to_string(), "front end".to_string())].into();
        r.restart = crate::policy::Restart::Always;
        r.limits.memory = 512 << 20;
        r.limits.memory_swap = -1;
        r.limits.milli_cpus = 500;
        r.limits.pids = 100;
        r.healthcheck = Some(
            crate::health::Overrides {
                cmd: Some("test -f /ok".into()),
                interval: Some(1_000_000),
                retries: Some(2),
                ..Default::default()
            }
            .apply(r.run.healthcheck.as_ref())
            .unwrap(),
        );
        crate::tuning::Overrides {
            hostname: Some("w1".into()),
            user: Some("1000:1000".into()),
            working_dir: Some("/srv".into()),
            cap_add: vec!["NET_ADMIN".into()],
            cap_drop: vec!["MKNOD".into()],
            privileged: None,
            read_only: Some(true),
            tmpfs: vec!["/tmp:size=64m".into()],
            shm_size: Some(64 << 20),
            devices: vec!["/dev/null:/dev/nullo:r".into(), "nvidia.com/gpu=all".into()],
            gpus: vec![r#"count=2,"capabilities=compute,utility""#.into()],
            interfaces: vec!["eth9".into()],
            dns: vec!["10.0.0.53".into()],
            dns_search: vec!["example.org".into()],
            extra_hosts: vec![
                "db:10.1.1.1".into(),
                "v6=fd00::1".into(),
                "gw:host-gateway".into(),
            ],
            ulimits: vec!["nofile=64:128".into(), "core=unlimited".into()],
            oom_score_adj: Some(-500),
            stop_signal: Some("SIGQUIT".into()),
            stop_timeout: Some(3),
            init: Some(true),
            sysctls: vec!["net.ipv4.ip_forward=1".into()],
            secrets: vec!["pw:/etc/pw:0400:1000:1000".into()],
            timezone: Some("off".into()),
            log_driver: Some("journal".into()),
            depends_on: vec!["db:healthy".into(), "cache:optional".into()],
        }
        .apply(&mut r.tuning)
        .unwrap();
        (r, fresh)
    }

    #[test]
    fn what_generate_prints_makes_the_same_machine_again() {
        let (r, mut again) = generated_pair();
        let command = crate::api::generate::machine_command(&r, "hub.example", false, false);
        assert_eq!(
            command[0],
            [
                "run",
                "-d",
                "--name",
                "web",
                "--pull",
                "always",
                "docker.io/library/nginx:1.27"
            ]
        );
        assert_eq!(
            command.last().unwrap(),
            &["--", "-c", "echo 'hi there'; exec nginx"]
        );
        let mut argv = vec!["nspawn".to_string()];
        argv.extend(command.concat());
        let cli = <crate::cli::Cli as clap::Parser>::try_parse_from(&argv).unwrap();
        let crate::cli::Command::Run(run) = cli.command else {
            panic!("not run");
        };
        assert!(run.detach && !run.rm && !run.no_verify);
        assert_eq!(run.name.as_deref(), Some("web"));
        assert_eq!(run.reference, r.reference);
        // What run sends the service, read back as the service reads it.
        let options = start_options(run.options, run.command).unwrap();
        let dict: HashMap<String, zbus::zvariant::OwnedValue> = options
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.try_into().unwrap()))
            .collect();
        let mut read = crate::daemon::values::Options::new(&dict);
        let request = crate::daemon::manager::start_request("web".into(), &mut read, true).unwrap();
        read.finish().unwrap();
        crate::api::machines::apply_flags(&mut again, &request).unwrap();
        assert_eq!(
            serde_json::to_value(&again).unwrap(),
            serde_json::to_value(&r).unwrap()
        );
        // And as a script a shell reads.
        let script = crate::api::generate::Script {
            notes: vec![],
            commands: vec![command],
        }
        .render();
        assert!(script.starts_with(
            "nspawn run -d --name web --pull always docker.io/library/nginx:1.27 \\\n  --network backend \\\n  --network front \\\n"
        ), "{script}");
        assert!(
            script.contains("\\\n  --env 'B=two words' \\\n"),
            "{script}"
        );
        assert!(
            script.ends_with("\\\n  -- -c 'echo '\\''hi there'\\''; exec nginx'\n"),
            "{script}"
        );
    }

    #[test]
    fn machines_of_run_rm_get_names_of_their_own() {
        let a = run_name("alpine-3").unwrap();
        let b = run_name("alpine-3").unwrap();
        assert!(
            a.starts_with("alpine-3-") && a.len() == "alpine-3-".len() + 8,
            "{a}"
        );
        assert_ne!(a, b);
        let long = run_name(&"x".repeat(80)).unwrap();
        assert!(long.len() <= 64);
        crate::reference::validate_machine_name(&long).unwrap();
    }

    #[test]
    fn events_are_printed_one_per_line() {
        let event = serde_json::json!({
            "time": "2026-09-24T10:00:00.000001Z",
            "type": "machine",
            "action": "die",
            "name": "web",
            "attributes": {"exit_code": "1", "code": "exited"},
            "labels": {"caddy": "web.example"},
        });
        assert_eq!(
            event_line(&event),
            "2026-09-24T10:00:00.000001Z machine die web (code=exited, exit_code=1, caddy=web.example)"
        );
        let bare = serde_json::json!({"time": "t", "type": "volume", "action": "create", "name": "data", "attributes": {}, "labels": {}});
        assert_eq!(event_line(&bare), "t volume create data");
    }

    #[test]
    fn run_reuses_a_local_image_and_asks_the_registry_otherwise() {
        let nginx = "docker.io/library/nginx:1.27";
        let listed = |name: &str, reference: &str, image_only: bool, pulled: u64| Listed {
            name: name.into(),
            reference: reference.into(),
            image_only,
            pulled,
        };
        let images = vec![
            listed("web", nginx, true, 1),
            listed("fedora-44", "hub.nspawn.org/fedora:44", true, 1),
            listed("vm", "", false, 0),
        ];
        let one = |r: &str| vec![r.to_string()];
        let source = |name: &str, force: bool, pull: PullPolicy, mode: bool| {
            run_source(&images, &one(nginx), name, force, pull, mode)
        };
        assert_eq!(
            source("web2", false, PullPolicy::Missing, false).unwrap(),
            Source::Local("web".to_string()),
            "a local image with the reference is made into another machine"
        );
        assert_eq!(
            source("web2", false, PullPolicy::Always, false).unwrap(),
            Source::Registry
        );
        assert_eq!(
            source("web2", false, PullPolicy::Missing, true).unwrap(),
            Source::Registry,
            "a mode of its own takes a pull"
        );
        let taken = source("web", false, PullPolicy::Missing, false).unwrap_err();
        assert!(taken.to_string().contains("nspawn start web"), "{taken}");
        assert!(
            source("vm", false, PullPolicy::Missing, false).is_err(),
            "an image machined lists takes the name too"
        );
        assert_eq!(
            source("web", true, PullPolicy::Missing, false).unwrap(),
            Source::Registry,
            "made anew from the registry: the one local copy is the machine itself"
        );
        assert!(run_source(
            &images,
            &one("hub.nspawn.org/x:1"),
            "x",
            false,
            PullPolicy::Never,
            false
        )
        .is_err());
        assert_eq!(
            source("web2", false, PullPolicy::Never, false).unwrap(),
            Source::Local("web".to_string())
        );
        assert!(source("web2", false, PullPolicy::Never, true).is_err());
        // A machine that ran is no image: the newest image of the reference, or a pull.
        let ran = vec![
            listed("a", nginx, false, 5),
            listed("old", nginx, true, 1),
            listed("new", nginx, true, 3),
        ];
        assert_eq!(
            run_source(&ran, &one(nginx), "b", false, PullPolicy::Missing, false).unwrap(),
            Source::Local("new".to_string())
        );
        assert_eq!(
            run_source(
                &ran[..1],
                &one(nginx),
                "b",
                false,
                PullPolicy::Missing,
                false
            )
            .unwrap(),
            Source::Registry
        );
        // The registry's own image first, then Docker Hub's that compose pulled, and the
        // registry only when neither is here.
        let app = local_spellings("app", "hub.nspawn.org").unwrap();
        let hub = listed("app", "hub.nspawn.org/app:latest", true, 1);
        let docker_hub = listed("app-2", "docker.io/library/app:latest", true, 2);
        assert_eq!(
            run_source(
                &[hub, docker_hub.clone()],
                &app,
                "m",
                false,
                PullPolicy::Missing,
                false
            )
            .unwrap(),
            Source::Local("app".to_string())
        );
        assert_eq!(
            run_source(&[docker_hub], &app, "m", false, PullPolicy::Missing, false).unwrap(),
            Source::Local("app-2".to_string())
        );
        assert_eq!(
            run_source(&[], &app, "m", false, PullPolicy::Missing, false).unwrap(),
            Source::Registry
        );
    }

    #[test]
    fn a_name_is_looked_for_under_the_registry_then_under_docker_hub() {
        assert_eq!(
            local_spellings("app", "hub.nspawn.org").unwrap(),
            ["hub.nspawn.org/app:latest", "docker.io/library/app:latest"]
        );
        assert_eq!(
            local_spellings("user/app:1", "hub.nspawn.org").unwrap(),
            ["hub.nspawn.org/user/app:1", "docker.io/user/app:1"]
        );
        assert_eq!(
            local_spellings("ghcr.io/x/y:2", "hub.nspawn.org").unwrap(),
            ["ghcr.io/x/y:2"]
        );
        assert_eq!(
            local_spellings("nginx", "docker.io").unwrap(),
            ["docker.io/library/nginx:latest"]
        );
    }
}
