//! The terminal side: arguments in, tables and lines out. Everything goes through the
//! org.nspawn service on the system bus; only the service itself, its installation and
//! the hooks the machine units call run the library in this process.

mod login;
mod machines;

use anyhow::Result;
use zbus::zvariant::Value;

use crate::api::{self, Context};
use crate::backend::BackendChoice;
use crate::cli::{Cli, Command, HubCommand, ImagesCommand, MachinesCommand, NetworkCommand};
use crate::client::{self, Client, Options};
use crate::config::Config;
use crate::oci::ModeChoice;
use crate::output::{human_bytes, table};
use crate::search::SearchSource;

pub async fn run(cli: Cli) -> Result<()> {
    let config = Config::load(
        cli.config.as_deref(),
        cli.registry.clone(),
        cli.ca_cert.clone(),
    )?;
    match cli.command {
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
        // The unit hooks must not depend on the service: a machine starts on its own.
        Command::Network(args) => match args.command {
            NetworkCommand::Prepare { name } => {
                api::network::prepare(&Context::new(config), &name).await
            }
            NetworkCommand::Publish { name } => {
                api::network::publish(&Context::new(config), &name).await
            }
            NetworkCommand::Release { name } => api::network::release(&Context::new(config), &name),
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
                Ok(())
            }
            NetworkCommand::Ls => {
                let client = Client::connect().await?;
                let (info, entries) = client.manager.list_network().await.map_err(client::error)?;
                println!(
                    "{} {} (gateway {}, host name {})",
                    client::string(&info, "bridge"),
                    client::string(&info, "subnet"),
                    client::string(&info, "gateway"),
                    client::string(&info, "host_name")
                );
                let rows = entries
                    .iter()
                    .map(|e| {
                        vec![
                            client::string(e, "name"),
                            client::dash(client::string(e, "address")),
                            client::dash(client::strings(e, "ports").join(" ")),
                            if client::bool(e, "running") {
                                "running"
                            } else {
                                "stopped"
                            }
                            .to_string(),
                        ]
                    })
                    .collect();
                println!("{}", table(&["MACHINE", "ADDRESS", "PORTS", "STATE"], rows));
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
async fn registry_name(client: &Client, config: &Config) -> String {
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
            if let Some(network) = a.network {
                put(&mut options, "network", lowercase(network));
            }
            put_all(&mut options, "publish", a.publish);
            put(&mut options, "force", a.force);
            put_opt(&mut options, "entrypoint", a.entrypoint);
            put_all(&mut options, "env", crate::volume::expand_env(&a.env)?);
            put_all(&mut options, "volume", a.volume);
            put_all(&mut options, "command", a.command);
            let done = client
                .run_job(|| manager.create_machine(&a.source, &a.name, options))
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
            ImagesCommand::Ls => {
                let rows = manager
                    .list_images()
                    .await
                    .map_err(client::error)?
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
        Command::Machines(args) => match args.command {
            MachinesCommand::Ls(a) => machines::ls(a, client).await,
        },
        Command::Ps(args) => machines::ls(args, client).await,
        Command::Start(args) => machines::start(args, client).await,
        Command::Stop(args) => machines::stop(args, client).await,
        Command::Exec(args) => machines::exec(args, client).await,
        Command::Shell(args) => machines::shell(args, client).await,
        Command::Logs(args) => machines::logs(args, client).await,
        Command::Daemon(_) | Command::Network(_) => unreachable!("handled before"),
    }
}

fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        format!("{}...", text.chars().take(max - 3).collect::<String>())
    }
}
