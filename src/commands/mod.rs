//! The terminal side: arguments in, tables and lines out. Everything goes through the
//! org.nspawn service on the system bus; only the service itself, its installation and
//! the hooks the machine units call run the library in this process.

mod copy;
mod login;
mod machines;
mod stats;

use anyhow::{bail, Context as _, Result};
use zbus::zvariant::Value;

use crate::api::{self, Context};
use crate::backend::BackendChoice;
use clap::CommandFactory;

use crate::cli::{
    Cli, Command, HubCommand, ImagesCommand, MachinesCommand, NetworkCommand, SecretCommand,
    VolumeCommand,
};
use crate::client::{self, Client, Options};
use crate::config::Config;
use crate::oci::ModeChoice;
use crate::output::{human_bytes, human_duration, table};
use crate::search::SearchSource;

pub async fn run(cli: Cli) -> Result<()> {
    // Before anything else: the machine starts whatever the configuration says.
    if let Command::AttachExec { name, argv } = &cli.command {
        match crate::attach::exec(name, argv)? {}
    }
    let config = Config::load(
        cli.config.as_deref(),
        cli.registry.clone(),
        cli.ca_cert.clone(),
    )?;
    match cli.command {
        // What a shell and man(1) need, written from the command line's own definition
        // so that they cannot drift from it. The packages generate them at build time.
        Command::Completions(a) => {
            let mut command = Cli::command();
            let name = command.get_name().to_string();
            clap_complete::generate(a.shell, &mut command, name, &mut std::io::stdout());
            Ok(())
        }
        Command::Manpage => clap_mangen::Man::new(Cli::command())
            .render(&mut std::io::stdout())
            .context("writing the manual page"),
        Command::Daemon(a) => {
            if a.install {
                for line in crate::daemon::install::install(config.config_path.as_deref()).await? {
                    println!("{line}");
                }
                println!(
                    "{} is available on the system bus; the bus starts it on demand",
                    crate::daemon::BUS_NAME
                );
                return Ok(());
            }
            let idle = (a.idle_exit > 0).then(|| std::time::Duration::from_secs(a.idle_exit));
            crate::daemon::run(config, idle).await
        }
        Command::RemoveAfterExit { name, invocation } => {
            api::network::remove_after_exit(&Context::new(config), &name, &invocation).await
        }
        Command::HealthRun { name } => crate::health::run(&Context::new(config), &name).await,
        // The unit hooks must not depend on the service: a machine starts on its own.
        Command::Network(args) => match args.command {
            NetworkCommand::Prepare { name } => {
                api::network::prepare(&Context::new(config), &name).await
            }
            NetworkCommand::Publish { name } => {
                api::network::publish(&Context::new(config), &name).await
            }
            NetworkCommand::Release { name } => {
                api::network::release(&Context::new(config), &name).await
            }
            NetworkCommand::Up => {
                let client = Client::connect().await?;
                let info = client.manager.network_up().await.map_err(client::error)?;
                for note in client::strings(&info, "notes") {
                    eprintln!("{note}");
                }
                println!(
                    "{} is up: {} on {}",
                    client::string(&info, "bridge"),
                    client::string(&info, "gateway"),
                    client::string(&info, "subnet")
                );
                let others: Vec<String> = client::strings(&info, "networks")
                    .into_iter()
                    .filter(|n| n != crate::bridge::DEFAULT_NETWORK)
                    .collect();
                if !others.is_empty() {
                    println!("and {} are up", others.join(", "));
                }
                Ok(())
            }
            NetworkCommand::Ls(output) => {
                let client = Client::connect().await?;
                let networks = client
                    .manager
                    .list_networks()
                    .await
                    .map_err(client::error)?;
                if output.json {
                    client::print_json(&serde_json::Value::Array(
                        networks.iter().map(client::dict_to_json).collect(),
                    ));
                    return Ok(());
                }
                let rows = networks
                    .iter()
                    .map(|n| {
                        vec![
                            client::string(n, "name"),
                            client::string(n, "interface"),
                            client::string(n, "subnet"),
                            if client::bool(n, "internal") {
                                "yes"
                            } else {
                                "no"
                            }
                            .to_string(),
                            client::dash(client::strings(n, "machines").join(" ")),
                        ]
                    })
                    .collect();
                println!(
                    "{}",
                    table(
                        &["NETWORK", "INTERFACE", "SUBNET", "INTERNAL", "MACHINES"],
                        rows
                    )
                );
                Ok(())
            }
            NetworkCommand::Inspect { names } => {
                let client = Client::connect().await?;
                let mut out = Vec::new();
                for name in &names {
                    let (info, entries) = client
                        .manager
                        .get_network(name)
                        .await
                        .map_err(client::error)?;
                    let mut network = client::dict_to_json(&info);
                    network["machines"] = serde_json::Value::Array(
                        entries.iter().map(client::dict_to_json).collect(),
                    );
                    out.push(network);
                }
                client::print_json(&serde_json::Value::Array(out));
                Ok(())
            }
            NetworkCommand::Create(a) => {
                let client = Client::connect().await?;
                let mut options = Options::new();
                put_opt(&mut options, "subnet", a.subnet);
                put(&mut options, "internal", a.internal);
                put_all(&mut options, "labels", a.label);
                let info = client
                    .manager
                    .create_network(&a.name, options)
                    .await
                    .map_err(client::error)?;
                for note in client::strings(&info, "notes") {
                    eprintln!("{note}");
                }
                println!(
                    "{} is up: {} on {}",
                    client::string(&info, "name"),
                    client::string(&info, "interface"),
                    client::string(&info, "subnet")
                );
                Ok(())
            }
            NetworkCommand::Rm { names } => {
                let client = Client::connect().await?;
                client
                    .run_job(|| client.manager.remove_networks(&names))
                    .await?;
                Ok(())
            }
            NetworkCommand::Prune { force } => {
                if !force && !ask("Remove every network no machine uses?", "network prune -f")? {
                    return Ok(());
                }
                let client = Client::connect().await?;
                client.run_job(|| client.manager.prune_networks()).await?;
                Ok(())
            }
        },
        command => {
            let client = Client::connect().await?;
            through_the_service(command, &client, &config).await
        }
    }
}

/// The registry a call went to: ours when we named one, the service's otherwise.
pub(super) async fn registry_name(client: &Client, config: &Config) -> String {
    if config.registry_set {
        return config.registry.clone();
    }
    client
        .manager
        .registry()
        .await
        .unwrap_or_else(|_| config.registry.clone())
}

fn lowercase<T: std::fmt::Debug>(value: T) -> String {
    format!("{value:?}").to_lowercase()
}

/// Adds the values the command line was given to the options of a call.
fn put(options: &mut Options<'_>, key: &'static str, value: impl Into<Value<'static>>) {
    options.insert(key, value.into());
}

fn put_opt(options: &mut Options<'_>, key: &'static str, value: Option<String>) {
    if let Some(value) = value {
        put(options, key, value);
    }
}

/// docker's other flags as the service takes them.
pub fn put_tuning(options: &mut Options<'_>, t: crate::cli::TuningArgs) {
    put_opt(options, "hostname", t.hostname);
    put_opt(options, "user", t.user);
    put_opt(options, "working_dir", t.workdir);
    put_opt(options, "stop_signal", t.stop_signal);
    for (key, values) in [
        ("cap_add", t.cap_add),
        ("cap_drop", t.cap_drop),
        ("tmpfs", t.tmpfs),
        ("devices", t.device),
        ("dns", t.dns),
        ("dns_search", t.dns_search),
        ("extra_hosts", t.add_host),
        ("ulimits", t.ulimit),
        ("sysctls", t.sysctl),
        ("secrets", t.secret),
    ] {
        put_all(options, key, values);
    }
    for (key, value) in [
        ("privileged", t.privileged),
        ("read_only", t.read_only),
        ("init", t.init),
    ] {
        if let Some(value) = value {
            put(options, key, value);
        }
    }
    for (key, value) in [("shm_size", t.shm_size), ("stop_timeout", t.stop_timeout)] {
        if let Some(value) = value {
            put(options, key, value);
        }
    }
    if let Some(adj) = t.oom_score_adj {
        put(options, "oom_score_adj", adj);
    }
}

/// The --health-* flags as the service takes them.
pub fn put_health(options: &mut Options<'_>, health: crate::cli::HealthArgs) {
    put_opt(options, "health_cmd", health.health_cmd);
    for (key, value) in [
        ("health_interval", health.health_interval),
        ("health_timeout", health.health_timeout),
        ("health_start_period", health.health_start_period),
        ("health_start_interval", health.health_start_interval),
    ] {
        if let Some(value) = value {
            put(options, key, value);
        }
    }
    if let Some(retries) = health.health_retries {
        put(options, "health_retries", retries);
    }
    if health.no_healthcheck {
        put(options, "no_healthcheck", true);
    }
}

fn put_all(options: &mut Options<'_>, key: &'static str, values: Vec<String>) {
    if !values.is_empty() {
        put(options, key, values);
    }
}

fn backend(options: &mut Options<'_>, choice: BackendChoice) {
    if choice != BackendChoice::Auto {
        put(options, "backend", lowercase(choice));
    }
}

fn mode(options: &mut Options<'_>, choice: ModeChoice) {
    if choice != ModeChoice::Auto {
        put(options, "mode", lowercase(choice));
    }
}

async fn through_the_service(command: Command, client: &Client, config: &Config) -> Result<()> {
    let manager = &client.manager;
    match command {
        Command::Hub(args) => match args.command {
            HubCommand::Ls(a) => {
                let repos = manager
                    .list_repositories(
                        a.filter.as_deref().unwrap_or(""),
                        !a.no_tags,
                        client::registry_options(config),
                    )
                    .await
                    .map_err(client::error)?;
                if repos.is_empty() {
                    println!("no repositories on {}", registry_name(client, config).await);
                } else {
                    let rows = repos
                        .iter()
                        .map(|r| {
                            vec![
                                client::string(r, "name"),
                                if a.no_tags {
                                    "-".to_string()
                                } else {
                                    client::strings(r, "tags").join(", ")
                                },
                            ]
                        })
                        .collect();
                    println!("{}", table(&["REPOSITORY", "TAGS"], rows));
                }
                Ok(())
            }
            HubCommand::Tags(a) => {
                for tag in manager
                    .list_tags(&a.repository, client::registry_options(config))
                    .await
                    .map_err(client::error)?
                {
                    println!("{tag}");
                }
                Ok(())
            }
        },
        Command::Search(a) => {
            let source = match a.source {
                None => "",
                Some(SearchSource::Hub) => "hub",
                Some(SearchSource::Dockerhub) => "dockerhub",
            };
            let (hits, notes) = manager
                .search_images(
                    &a.term,
                    source,
                    a.limit as u32,
                    client::registry_options(config),
                )
                .await
                .map_err(client::error)?;
            for note in &notes {
                eprintln!("{note}");
            }
            if hits.is_empty() {
                println!("nothing found for {:?}", a.term);
                return Ok(());
            }
            let rows: Vec<Vec<String>> = hits
                .iter()
                .map(|h| {
                    let stars = client::u64(h, "stars");
                    vec![
                        client::string(h, "source"),
                        client::string(h, "name"),
                        shorten(&client::string(h, "description"), 60),
                        if stars == 0 {
                            "-".to_string()
                        } else {
                            stars.to_string()
                        },
                        if client::bool(h, "official") {
                            "yes"
                        } else {
                            "-"
                        }
                        .to_string(),
                    ]
                })
                .collect();
            println!(
                "{}",
                table(
                    &["SOURCE", "NAME", "DESCRIPTION", "STARS", "OFFICIAL"],
                    rows
                )
            );
            Ok(())
        }
        Command::Login(args) => login::login(args, client, config).await,
        Command::Logout(args) => login::logout(args, client, config).await,
        Command::Pull(a) => {
            let mut options = client::registry_options(config);
            put_opt(&mut options, "name", a.name);
            backend(&mut options, a.backend);
            mode(&mut options, a.mode);
            put(&mut options, "force", a.force);
            let done = client
                .run_job(|| manager.pull_image(&a.reference, options))
                .await?;
            println!(
                "image {} ({} image) is ready: nspawn start {}",
                client::string(&done, "name"),
                client::string(&done, "mode"),
                client::string(&done, "name")
            );
            Ok(())
        }
        Command::Build(a) => {
            let mut options = client::registry_options(config);
            put_opt(&mut options, "name", a.name);
            put_opt(&mut options, "distribution", a.distribution);
            put_opt(&mut options, "release", a.release);
            put_all(&mut options, "profile", a.profile);
            backend(&mut options, a.backend);
            mode(&mut options, a.mode);
            put(&mut options, "force", a.force);
            put(&mut options, "keep_output", a.keep_output);
            put_all(&mut options, "mkosi_args", a.mkosi_args);
            // The service resolves the directory; give it an absolute one.
            let directory = std::fs::canonicalize(&a.directory).unwrap_or(a.directory);
            let directory = directory.to_string_lossy().into_owned();
            let done = client
                .run_job(|| manager.build_image(&directory, &a.tag, options))
                .await?;
            let output = client::string(&done, "output");
            if !output.is_empty() {
                println!("mkosi output kept at {output}");
            }
            let name = client::string(&done, "name");
            println!(
                "image {name} ({} image) is ready: nspawn start {name}, nspawn push {name}",
                client::string(&done, "mode")
            );
            Ok(())
        }
        Command::Create(a) => {
            let mut options = client::registry_options(config);
            backend(&mut options, a.backend);
            put_all(&mut options, "networks", a.network);
            put_all(&mut options, "aliases", a.network_alias);
            put_all(&mut options, "publish", a.publish);
            put(&mut options, "force", a.force);
            put_opt(&mut options, "entrypoint", a.entrypoint);
            put_all(&mut options, "env", crate::volume::expand_env(&a.env)?);
            put_all(&mut options, "volume", a.volume);
            put_all(&mut options, "label", a.label);
            if let Some(restart) = a.restart {
                put(&mut options, "restart", restart.name());
            }
            if let Some(memory) = a.memory {
                put(&mut options, "memory", memory);
            }
            if let Some(cpus) = a.cpus {
                put(&mut options, "cpus", cpus);
            }
            if let Some(pids) = a.pids_limit {
                put(&mut options, "pids_limit", pids);
            }
            put_all(&mut options, "command", a.command);
            put_health(&mut options, a.health);
            put_tuning(&mut options, a.tuning);
            // A reference that is not local is pulled first, under its own name, as run
            // does: the machine is then made from that image.
            let images: Vec<(String, String)> = manager
                .list_images()
                .await
                .map_err(client::error)?
                .iter()
                .map(|i| (client::string(i, "name"), client::string(i, "reference")))
                .collect();
            let registry = registry_name(client, config).await;
            let source = match create_source(&images, &a.source, &registry)? {
                CreateSource::Local(source) => source,
                CreateSource::Pull { reference, base } => {
                    let mut pull = client::registry_options(config);
                    put(&mut pull, "name", base.clone());
                    backend(&mut pull, a.backend);
                    client
                        .run_job_to_stderr(|| manager.pull_image(&reference, pull))
                        .await?;
                    base
                }
            };
            let done = client
                .run_job(|| manager.create_machine(&source, &a.name, options))
                .await?;
            let name = client::string(&done, "name");
            println!(
                "machine {name} ({} image) is ready: nspawn start {name}",
                client::string(&done, "mode")
            );
            Ok(())
        }
        Command::Push(a) => {
            let mut options = client::registry_options(config);
            put_opt(&mut options, "to", a.to);
            let done = client
                .run_job(|| manager.push_image(&a.image, options))
                .await?;
            println!(
                "pushed {}: {}",
                client::string(&done, "destination"),
                client::string(&done, "url")
            );
            Ok(())
        }
        Command::Images(args) => match args.command {
            ImagesCommand::Ls(output) => {
                let images = manager.list_images().await.map_err(client::error)?;
                if output.json {
                    client::print_json(&serde_json::Value::Array(
                        images.iter().map(client::dict_to_json).collect(),
                    ));
                    return Ok(());
                }
                let rows = images
                    .iter()
                    .map(|i| {
                        let size = client::u64(i, "size");
                        vec![
                            client::string(i, "name"),
                            client::string(i, "kind"),
                            client::dash(client::string(i, "backend")),
                            client::dash(client::string(i, "origin")),
                            client::dash(client::string(i, "reference")),
                            if size == 0 {
                                "-".to_string()
                            } else {
                                human_bytes(size)
                            },
                            if client::bool(i, "read_only") {
                                "yes"
                            } else {
                                "no"
                            }
                            .to_string(),
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
            ImagesCommand::Rm(a) => {
                // A job: what was removed is printed as it happens, and a name that
                // could not be removed fails it at the end.
                client.run_job(|| manager.remove_images(&a.names)).await?;
                Ok(())
            }
        },
        Command::Rm(a) => {
            let mut options = Options::new();
            put(&mut options, "force", a.force);
            client
                .run_job(|| manager.remove_machines(&a.names, options))
                .await?;
            Ok(())
        }
        Command::Volume(args) => match args.command {
            VolumeCommand::Ls(output) => {
                let volumes = manager.list_volumes().await.map_err(client::error)?;
                if output.json {
                    client::print_json(&serde_json::Value::Array(
                        volumes.iter().map(client::dict_to_json).collect(),
                    ));
                    return Ok(());
                }
                let now = crate::store::now_unix();
                let rows = volumes
                    .iter()
                    .map(|v| {
                        let created = client::u64(v, "created");
                        vec![
                            client::string(v, "name"),
                            client::dash(client::strings(v, "used_by").join(" ")),
                            if created > 0 {
                                format!("{} ago", human_duration(now.saturating_sub(created)))
                            } else {
                                "-".to_string()
                            },
                            client::string(v, "path"),
                        ]
                    })
                    .collect();
                println!("{}", table(&["VOLUME", "USED BY", "CREATED", "PATH"], rows));
                Ok(())
            }
            VolumeCommand::Create { name } => {
                manager.create_volume(&name).await.map_err(client::error)?;
                println!("{name}");
                Ok(())
            }
            VolumeCommand::Rm { names } => {
                client.run_job(|| manager.remove_volumes(&names)).await?;
                Ok(())
            }
            VolumeCommand::Prune { force } => {
                if !force && !ask("Remove every volume no machine uses?", "volume prune -f")? {
                    return Ok(());
                }
                client.run_job(|| manager.prune_volumes()).await?;
                Ok(())
            }
        },
        Command::Secret(args) => match args.command {
            SecretCommand::Ls(output) => {
                let secrets = manager.list_secrets().await.map_err(client::error)?;
                if output.json {
                    client::print_json(&serde_json::Value::Array(
                        secrets.iter().map(client::dict_to_json).collect(),
                    ));
                    return Ok(());
                }
                let now = crate::store::now_unix();
                let rows = secrets
                    .iter()
                    .map(|s| {
                        let created = client::u64(s, "created");
                        vec![
                            client::string(s, "name"),
                            human_bytes(client::u64(s, "size")),
                            if created > 0 {
                                format!("{} ago", human_duration(now.saturating_sub(created)))
                            } else {
                                "-".to_string()
                            },
                            client::dash(client::strings(s, "used_by").join(" ")),
                        ]
                    })
                    .collect();
                println!("{}", table(&["SECRET", "SIZE", "CREATED", "USED BY"], rows));
                Ok(())
            }
            SecretCommand::Create { name, file, label } => {
                let content = match &file {
                    Some(path) => std::fs::read(path)
                        .with_context(|| format!("reading {}", path.display()))?,
                    None => {
                        let mut bytes = Vec::new();
                        std::io::Read::read_to_end(&mut std::io::stdin(), &mut bytes)
                            .context("reading the secret from standard input")?;
                        bytes
                    }
                };
                let mut options = Options::new();
                put_all(&mut options, "labels", label);
                manager
                    .create_secret(&name, &content, options)
                    .await
                    .map_err(client::error)?;
                println!("{name}");
                Ok(())
            }
            SecretCommand::Inspect { names } => {
                let mut out = Vec::new();
                for name in &names {
                    out.push(client::dict_to_json(
                        &manager.get_secret(name).await.map_err(client::error)?,
                    ));
                }
                client::print_json(&serde_json::Value::Array(out));
                Ok(())
            }
            SecretCommand::Rm { names } => {
                client.run_job(|| manager.remove_secrets(&names)).await?;
                Ok(())
            }
        },
        Command::Machines(args) => match args.command {
            MachinesCommand::Ls(a) => machines::ls(a, client).await,
        },
        Command::Ps(args) => machines::ls(args, client).await,
        Command::Inspect(args) => {
            let mut out = Vec::new();
            for name in &args.names {
                let machine = manager.get_machine(name).await.map_err(client::error)?;
                out.push(client::dict_to_json(&machine));
            }
            client::print_json(&serde_json::Value::Array(out));
            Ok(())
        }
        Command::Start(args) => machines::start(args, client).await,
        Command::Run(args) => machines::run(args, client, config).await,
        Command::Stop(args) => machines::stop(args, client).await,
        Command::Restart(args) => machines::restart(args, client).await,
        Command::Kill(args) => machines::kill(args, client).await,
        Command::Pause(args) => machines::pause(args, client, true).await,
        Command::Unpause(args) => machines::pause(args, client, false).await,
        Command::Top(args) => machines::top(args, client).await,
        Command::Update(args) => machines::update(args, client).await,
        Command::Stats(args) => stats::stats(args, client).await,
        Command::Events(args) => machines::events(args, client).await,
        Command::Exec(args) => machines::exec(args, client).await,
        Command::Shell(args) => machines::shell(args, client).await,
        Command::Logs(args) => machines::logs(args, client).await,
        Command::Cp(args) => copy::cp(args, client).await,
        Command::Daemon(_)
        | Command::Network(_)
        | Command::Completions(_)
        | Command::Manpage
        | Command::RemoveAfterExit { .. }
        | Command::HealthRun { .. }
        | Command::AttachExec { .. } => {
            unreachable!("handled before")
        }
    }
}

/// What `create` makes the machine from.
#[derive(Debug, PartialEq, Eq)]
enum CreateSource {
    /// A local image, by its name or its reference (the service finds it by either).
    Local(String),
    /// Not local: pulled first under `base`, the image's own name, as run does.
    Pull { reference: String, base: String },
}

/// `images` are the local images as (name, reference). A reference is compared as the
/// service records it (`fedora:44` is `hub.example/fedora:44` there), so that an image
/// pulled by a short reference is not pulled again under a name it already has.
fn create_source(
    images: &[(String, String)],
    source: &str,
    registry: &str,
) -> Result<CreateSource> {
    if images.iter().any(|(n, r)| n == source || r == source) {
        return Ok(CreateSource::Local(source.to_string()));
    }
    // What is no reference is left to the service to refuse.
    let Ok(image) = crate::reference::ImageRef::parse(source, registry) else {
        return Ok(CreateSource::Local(source.to_string()));
    };
    let reference = image.to_string();
    if images.iter().any(|(_, r)| *r == reference) {
        return Ok(CreateSource::Local(reference));
    }
    let base = image.local_name();
    if images.iter().any(|(n, r)| *n == base && *r != reference) {
        bail!(
            "{base} is an image of another reference; pull {image} under a name of your own first (nspawn pull --name)"
        );
    }
    Ok(CreateSource::Pull {
        reference: source.to_string(),
        base,
    })
}

fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        format!("{}...", text.chars().take(max - 3).collect::<String>())
    }
}

/// Whether the answer to a [y/N] question is a yes.
/// Like docker: the answer is read from standard input whatever it is, and anything but
/// a yes is a no, the end of a script's input too.
fn ask(question: &str, without_asking: &str) -> Result<bool> {
    eprint!("{question} [y/N] ");
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .context("reading the answer")?;
    if confirmed(&answer) {
        return Ok(true);
    }
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        eprintln!("\nnothing removed; {without_asking} removes without asking");
    }
    Ok(false)
}

fn confirmed(answer: &str) -> bool {
    matches!(answer.trim(), "y" | "Y" | "yes" | "Yes" | "YES")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_pulls_only_what_is_not_local_by_any_spelling() {
        let images = vec![
            ("fedora-44".to_string(), "hub.example/fedora:44".to_string()),
            (
                "web".to_string(),
                "docker.io/library/nginx:1.27".to_string(),
            ),
            ("vm".to_string(), String::new()),
        ];
        let source = |s: &str| create_source(&images, s, "hub.example");
        assert_eq!(
            source("fedora-44").unwrap(),
            CreateSource::Local("fedora-44".to_string()),
            "a local name"
        );
        assert_eq!(
            source("hub.example/fedora:44").unwrap(),
            CreateSource::Local("hub.example/fedora:44".to_string()),
            "the reference as recorded"
        );
        assert_eq!(
            source("fedora:44").unwrap(),
            CreateSource::Local("hub.example/fedora:44".to_string()),
            "the short reference of a local image"
        );
        assert_eq!(
            source("nginx:1.27").unwrap(),
            CreateSource::Pull {
                reference: "nginx:1.27".to_string(),
                base: "nginx-1.27".to_string()
            },
            "the hub's nginx is not Docker Hub's"
        );
        assert_eq!(
            source("fedora:45").unwrap(),
            CreateSource::Pull {
                reference: "fedora:45".to_string(),
                base: "fedora-45".to_string()
            }
        );
        let taken = vec![(
            "fedora-45".to_string(),
            "other.example/fedora:45".to_string(),
        )];
        let clash = create_source(&taken, "fedora:45", "hub.example").unwrap_err();
        assert!(clash.to_string().contains("nspawn pull --name"), "{clash}");
        assert_eq!(
            source("not a reference").unwrap(),
            CreateSource::Local("not a reference".to_string()),
            "the service says what is wrong with it"
        );
    }

    #[test]
    fn only_a_yes_confirms() {
        for yes in ["y\n", "Y", "yes\n", " yes "] {
            assert!(confirmed(yes), "{yes:?}");
        }
        for no in ["", "\n", "n\n", "no", "yess", "y y"] {
            assert!(!confirmed(no), "{no:?}");
        }
    }
}
