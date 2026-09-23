//! Command implementations.

mod bus;
mod login;
mod machines;

use anyhow::Result;

use crate::api::{self, Context, Event};
use crate::cli::{Cli, Command, HubCommand, ImagesCommand, MachinesCommand, NetworkCommand};
use crate::config::Config;
use crate::output::{human_bytes, table};

/// Events on the terminal: lines to stdout, notes to stderr.
pub fn print(event: Event) {
    match event {
        Event::Line(text) => println!("{text}"),
        Event::Note(text) => eprintln!("{text}"),
    }
}

pub async fn run(cli: Cli) -> Result<()> {
    let config = Config::load(
        cli.config.as_deref(),
        cli.registry.clone(),
        cli.ca_cert.clone(),
    )?;
    let ctx = Context::new(config.clone());
    match cli.command {
        Command::Hub(args) => match args.command {
            HubCommand::Ls(a) => {
                let repos = api::hub::repositories(&ctx, a.filter.as_deref(), !a.no_tags).await?;
                if repos.is_empty() {
                    println!("no repositories on {}", ctx.config.registry);
                } else {
                    let rows = repos
                        .into_iter()
                        .map(|r| {
                            vec![
                                r.name,
                                r.tags
                                    .map(|t| t.join(", "))
                                    .unwrap_or_else(|| "-".to_string()),
                            ]
                        })
                        .collect();
                    println!("{}", table(&["REPOSITORY", "TAGS"], rows));
                }
                Ok(())
            }
            HubCommand::Tags(a) => {
                for tag in api::hub::tags(&ctx, &a.repository).await? {
                    println!("{tag}");
                }
                Ok(())
            }
        },
        Command::Pull(a) => {
            let pulled = api::pull::pull(
                &ctx,
                &api::pull::PullRequest {
                    reference: a.reference,
                    name: a.name,
                    backend: a.backend,
                    mode: a.mode.to_mode(),
                    force: a.force,
                },
                &print,
            )
            .await?;
            println!(
                "image {} ({} image) is ready: nspawn start {}",
                pulled.name,
                pulled.mode.name(),
                pulled.name
            );
            Ok(())
        }
        Command::Search(a) => {
            let hits = api::search::search(&ctx, &a.term, a.source, a.limit, &print).await?;
            if hits.is_empty() {
                println!("nothing found for {:?}", a.term);
                return Ok(());
            }
            let rows: Vec<Vec<String>> = hits
                .into_iter()
                .map(|h| {
                    vec![
                        h.source,
                        h.name,
                        shorten(&h.description, 60),
                        match h.stars {
                            Some(n) => n.to_string(),
                            None => "-".to_string(),
                        },
                        if h.official { "yes" } else { "-" }.to_string(),
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
        Command::Login(args) => login::login(args, &ctx).await,
        Command::Logout(args) => login::logout(args, &ctx),
        Command::Build(a) => {
            let built = api::build::build(
                &ctx,
                &api::build::BuildRequest {
                    directory: a.directory,
                    tag: a.tag,
                    name: a.name,
                    distribution: a.distribution,
                    release: a.release,
                    profile: a.profile,
                    backend: a.backend,
                    mode: a.mode.to_mode(),
                    force: a.force,
                    keep_output: a.keep_output,
                    mkosi_args: a.mkosi_args,
                },
                &print,
            )
            .await?;
            if let Some(output) = &built.output {
                println!("mkosi output kept at {}", output.display());
            }
            println!(
                "image {} ({} image) is ready: nspawn start {}, nspawn push {}",
                built.name,
                built.mode.name(),
                built.name,
                built.name
            );
            Ok(())
        }
        Command::Create(a) => {
            let created = api::create::create(
                &ctx,
                &api::create::CreateRequest {
                    source: a.source,
                    name: a.name,
                    backend: a.backend,
                    network: a.network,
                    publish: a.publish,
                    force: a.force,
                    entrypoint: a.entrypoint,
                    env: a.env,
                    volume: a.volume,
                    command: a.command,
                },
                &print,
            )
            .await?;
            println!(
                "machine {} ({} image) is ready: nspawn start {}",
                created.name,
                created.mode.name(),
                created.name
            );
            Ok(())
        }
        Command::Push(a) => {
            let pushed = api::push::push(
                &ctx,
                &api::push::PushRequest {
                    image: a.image,
                    to: a.to,
                },
                &print,
            )
            .await?;
            println!("pushed {}: {}", pushed.destination, pushed.url);
            Ok(())
        }
        Command::Images(args) => match args.command {
            ImagesCommand::Ls => images_ls(&ctx).await,
            ImagesCommand::Rm(a) => api::images::remove(&ctx, &a.names, &print).await,
        },
        Command::Machines(args) => match args.command {
            MachinesCommand::Ls(a) => machines::ls(a, &ctx).await,
        },
        Command::Ps(args) => machines::ls(args, &ctx).await,
        Command::Start(args) => machines::start(args, &ctx).await,
        Command::Stop(args) => machines::stop(args, &ctx).await,
        Command::Exec(args) => machines::exec(args, &ctx).await,
        Command::Shell(args) => machines::shell(args, &ctx).await,
        Command::Logs(args) => machines::logs(args),
        Command::Network(args) => match args.command {
            NetworkCommand::Up => {
                let info = api::network::up(&ctx).await?;
                println!("{} is up: {} on {}", info.bridge, info.gateway, info.subnet);
                Ok(())
            }
            NetworkCommand::Ls => {
                let (info, entries) = api::network::list(&ctx).await?;
                println!(
                    "{} {} (gateway {}, host name {})",
                    info.bridge, info.subnet, info.gateway, info.host_name
                );
                let rows = entries
                    .into_iter()
                    .map(|e| {
                        vec![
                            e.name,
                            e.address
                                .map(|a| a.to_string())
                                .unwrap_or_else(|| "-".into()),
                            if e.ports.is_empty() {
                                "-".to_string()
                            } else {
                                e.ports
                                    .iter()
                                    .map(|p| p.to_string())
                                    .collect::<Vec<_>>()
                                    .join(" ")
                            },
                            if e.running { "running" } else { "stopped" }.to_string(),
                        ]
                    })
                    .collect();
                println!("{}", table(&["MACHINE", "ADDRESS", "PORTS", "STATE"], rows));
                Ok(())
            }
            NetworkCommand::Prepare { name } => api::network::prepare(&ctx, &name).await,
            NetworkCommand::Publish { name } => api::network::publish(&ctx, &name).await,
            NetworkCommand::Release { name } => api::network::release(&ctx, &name),
        },
        Command::Daemon(a) => {
            if a.install {
                for line in crate::daemon::install::install(
                    config.config_path.as_deref(),
                    &config.state_dir,
                )
                .await?
                {
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
    }
}

fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        format!("{}...", text.chars().take(max - 3).collect::<String>())
    }
}

async fn images_ls(ctx: &Context) -> Result<()> {
    let rows = api::images::list(ctx)
        .await?
        .into_iter()
        .map(|i| {
            vec![
                i.name,
                i.kind,
                i.backend
                    .map(|b| format!("{b:?}").to_lowercase())
                    .unwrap_or_else(|| "-".to_string()),
                i.origin.unwrap_or_else(|| "-".to_string()),
                i.reference.unwrap_or_else(|| "-".to_string()),
                i.size.map(human_bytes).unwrap_or_else(|| "-".to_string()),
                if i.read_only { "yes" } else { "no" }.to_string(),
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
