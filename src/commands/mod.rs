//! The terminal side: arguments in, tables and lines out. Everything goes through the
//! org.nspawn service on the system bus; only the service itself, its installation and
//! the hooks the machine units call run the library in this process.

pub(crate) mod build;
pub(crate) mod copy;
mod login;
pub(crate) mod machines;
pub(crate) mod stats;
pub(crate) mod system;

use anyhow::{bail, Context as _, Result};
use zbus::zvariant::Value;

use crate::api::{self, Context};
use crate::backend::BackendChoice;
use clap::CommandFactory;

use crate::cli::{
    Cli, Command, HubCommand, ImagesCommand, MachinesCommand, NetworkCommand, SecretCommand,
    SystemCommand, VolumeCommand,
};
use crate::client::{self, Client, Options};
use crate::config::Config;
use crate::oci::ModeChoice;
use crate::output::{human_bytes, human_duration, table};
use crate::search::SearchSource;

pub async fn run(cli: Cli) -> Result<()> {
    // Before anything else: the machine starts whatever the configuration says.
    if let Command::AttachExec {
        name,
        journal_namespace,
        argv,
    } = &cli.command
    {
        match crate::attach::exec(name, journal_namespace.as_deref(), argv)? {}
    }
    // What a shell and man(1) need, written from the command line's own definition so
    // that they cannot drift from it, and without a configuration: the packages
    // generate them at build time.
    match &cli.command {
        Command::Completions(a) => {
            let script = crate::completion::registration(a.shell)
                .with_context(|| format!("writing the {} completions", a.shell))?;
            return std::io::Write::write_all(&mut std::io::stdout(), &script)
                .context("writing the completions");
        }
        Command::Manpage => {
            return clap_mangen::Man::new(Cli::command())
                .render(&mut std::io::stdout())
                .context("writing the manual page");
        }
        _ => {}
    }
    let config = match Config::load(
        cli.config.as_deref(),
        cli.registry.clone(),
        cli.ca_cert.clone(),
    ) {
        Ok(config) => config,
        // The release hook cleans up after a machine that ended, and the removal of a
        // --rm machine follows it: a configuration broken since the machine started must
        // not leave its namespace, its rules, its secrets or the machine itself, nor take
        // the IPv6 side of its ports down.
        Err(e) if cleaning_up(&cli.command) => {
            eprintln!("warning: {e:#}; going by the settings of it that are still valid");
            Config::load_leniently(
                cli.config.as_deref(),
                cli.registry.clone(),
                cli.ca_cert.clone(),
            )
        }
        Err(e) => return Err(e),
    };
    match cli.command {
        Command::Completions(_) | Command::Manpage => unreachable!("handled before"),
        // main hands nspawn compose its arguments before clap reads them: clap's
        // definition of it is for the help, the manual page and the completions.
        Command::Compose(_) => unreachable!("nspawn compose is read in main"),
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
            let removed =
                api::network::remove_after_exit(&Context::new(config), &name, &invocation).await;
            // The layers it let go are deleted before the transient unit ends.
            crate::store::finish_trash();
            removed
        }
        Command::HealthRun { name } => crate::health::run(&Context::new(config), &name).await,
        Command::ApiServe { name } => crate::dockerapi::serve(name.as_deref(), config).await,
        Command::PortProxy { name } => crate::portproxy::serve(&Context::new(config), &name).await,
        Command::AwaitDependencies { name } => {
            api::dependencies::await_all(&Context::new(config), &name).await
        }
        // The command's own part is printed before the service is reached, which may fail.
        Command::Info(args) => system::info(args).await,
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
            NetworkCommand::Connect {
                network,
                machine,
                alias,
                ip,
            } => {
                let client = Client::connect().await?;
                let machine = read_name(&machine, MACHINES, &client, &config).await;
                let mut options = Options::new();
                put_all(&mut options, "aliases", alias);
                put_opt(&mut options, "address", ip);
                client
                    .run_job(|| client.manager.connect_network(&network, &machine, options))
                    .await?;
                Ok(())
            }
            NetworkCommand::Disconnect {
                network,
                machine,
                force,
            } => {
                let client = Client::connect().await?;
                let machine = read_name(&machine, MACHINES, &client, &config).await;
                let mut options = Options::new();
                put(&mut options, "force", force);
                client
                    .run_job(|| {
                        client
                            .manager
                            .disconnect_network(&network, &machine, options)
                    })
                    .await?;
                Ok(())
            }
            NetworkCommand::Prune { force, filters } => {
                if !force && !ask("Remove every network no machine uses?", "network prune -f")? {
                    return Ok(());
                }
                let client = Client::connect().await?;
                if filters.is_empty() {
                    client.run_job(|| client.manager.prune_networks()).await?;
                } else {
                    for name in system::networks_prune(&client, &filters).await? {
                        println!("{name}");
                    }
                }
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

/// The per-machine settings as the service takes them.
pub fn put_tuning(options: &mut Options<'_>, t: crate::cli::TuningArgs) {
    for warning in t.warnings() {
        eprintln!("warning: {warning}");
    }
    put_opt(options, "hostname", t.hostname);
    put_opt(options, "user", t.user);
    put_opt(options, "working_dir", t.workdir);
    put_opt(options, "stop_signal", t.stop_signal);
    put_opt(options, "timezone", t.timezone);
    put_opt(options, "log_driver", t.log_driver);
    for (key, values) in [
        ("cap_add", t.cap_add),
        ("cap_drop", t.cap_drop),
        ("tmpfs", t.tmpfs),
        ("devices", t.device),
        ("device_cgroup_rules", t.device_cgroup_rule),
        ("gpus", t.gpus),
        ("interfaces", t.interface),
        ("dns", t.dns),
        ("dns_search", t.dns_search),
        ("extra_hosts", t.add_host),
        ("ulimits", t.ulimit),
        ("sysctls", t.sysctl),
        ("secrets", t.secret),
        ("depends_on", t.depends_on),
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
    if let Some(size) = t.shm_size {
        put(options, "shm_size", size);
    }
    if let Some(seconds) = t.stop_timeout {
        put(
            options,
            "stop_timeout",
            crate::policy::stop_seconds(seconds),
        );
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

async fn through_the_service(mut command: Command, client: &Client, config: &Config) -> Result<()> {
    read_names(&mut command, client, config).await;
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
            if a.no_verify {
                put(&mut options, "verify", false);
            }
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
            if build::kind(&a)? == build::Kind::Dockerfile {
                return build::dockerfile(a, client, config).await;
            }
            let [tag] = a.tag.as_slice() else {
                bail!("an mkosi build takes one --tag");
            };
            let tag = tag.clone();
            let mut options = client::registry_options(config);
            put_opt(&mut options, "name", a.mkosi.name);
            put_opt(&mut options, "distribution", a.mkosi.distribution);
            put_opt(&mut options, "release", a.mkosi.release);
            put_all(&mut options, "profile", a.mkosi.profile);
            backend(&mut options, a.backend);
            mode(&mut options, a.mode);
            put(&mut options, "force", a.force);
            put(&mut options, "keep_output", a.mkosi.keep_output);
            put_all(&mut options, "mkosi_args", a.mkosi.mkosi_args);
            // The service resolves the directory; give it an absolute one.
            let directory = std::fs::canonicalize(&a.directory).unwrap_or(a.directory);
            let directory = directory.to_string_lossy().into_owned();
            let done = client
                .run_job(|| manager.build_image(&directory, &tag, options))
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
        Command::Create(mut a) => {
            // --mount is -v or --tmpfs written out; the files come before the flags.
            (a.volume, a.tuning.tmpfs) = crate::mountopt::with_mounts(
                std::mem::take(&mut a.volume),
                std::mem::take(&mut a.tuning.tmpfs),
                &a.mount,
            )?;
            a.env = crate::kvfile::env_with_files(&a.env_file, &a.env)?;
            a.label = crate::kvfile::labels_with_files(&a.label_file, &a.label)?;
            let mut options = client::registry_options(config);
            backend(&mut options, a.backend);
            put_all(&mut options, "networks", a.network);
            put_all(&mut options, "aliases", a.network_alias);
            put_all(&mut options, "addresses", a.ip);
            put_all(&mut options, "publish", a.publish);
            if a.publish_all {
                put(&mut options, "publish_all", true);
            }
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
            if let Some(memory_swap) = a.memory_swap {
                put(&mut options, "memory_swap", memory_swap);
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
            let images =
                machines::Listed::all(&manager.list_images().await.map_err(client::error)?);
            let registry = registry_name(client, config).await;
            let local = machines::local_image(client, config, &a.source, &a.name)
                .await?
                .map(|(found, _)| found);
            let source =
                match create_source(&images, &a.source, &a.name, &registry, a.force, local)? {
                    CreateSource::Local(source) => source,
                    CreateSource::Pull { reference, base } => {
                        let mut pull = client::registry_options(config);
                        put(&mut pull, "name", base.clone());
                        backend(&mut pull, a.backend);
                        if a.no_verify {
                            put(&mut pull, "verify", false);
                        }
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
        Command::Images(args) => match args.command.unwrap_or(ImagesCommand::Ls(args.ls)) {
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
            ImagesCommand::Inspect(a) => {
                let mut found = Vec::new();
                let mut missing = Vec::new();
                for name in &a.names {
                    match manager.get_image(name).await.map_err(client::error) {
                        Ok(record) => found.push(client::dict_to_json(&record)),
                        Err(e) => missing.push(e),
                    }
                }
                print!("{}", inspected(&found, a.format.as_deref(), &mut missing)?);
                last_error(missing)
            }
            ImagesCommand::Prune(a) => system::images_prune(a, client).await,
        },
        Command::System(args) => match args.command {
            SystemCommand::Df(a) => system::df(a, client).await,
            SystemCommand::Prune(a) => system::prune(a, client).await,
        },
        Command::Builder(args) => match args.command {
            crate::cli::BuilderCommand::Prune(a) => system::builder_prune(a, client).await,
            crate::cli::BuilderCommand::Du(a) => system::builder_du(a, client).await,
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
            VolumeCommand::Ls(args) => {
                let labels = system::label_filters(&args.filters)?;
                let volumes: Vec<_> = manager
                    .list_volumes()
                    .await
                    .map_err(client::error)?
                    .into_iter()
                    .filter(|v| system::labelled(&client::dict_to_json(v), &labels))
                    .collect();
                if args.output.json {
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
            VolumeCommand::Create { name, label } => {
                let mut options = Options::new();
                put_all(&mut options, "labels", label);
                let volume = manager
                    .create_volume(&name, options)
                    .await
                    .map_err(client::error)?;
                println!("{}", client::string(&volume, "name"));
                Ok(())
            }
            VolumeCommand::Inspect { names } => {
                let mut out = Vec::new();
                for name in &names {
                    out.push(client::dict_to_json(
                        &manager.get_volume(name).await.map_err(client::error)?,
                    ));
                }
                client::print_json(&serde_json::Value::Array(out));
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
            MachinesCommand::Ls(a) => machines::ls(a, client, config).await,
        },
        Command::Ps(args) => machines::ls(args, client, config).await,
        Command::Generate(args) => {
            let script = manager
                .generate(&args.names, Options::new())
                .await
                .map_err(client::error)?;
            print!("{script}");
            Ok(())
        }
        Command::Inspect(args) => {
            // What is found is printed, what is not said after it.
            let mut out = Vec::new();
            let mut missing = Vec::new();
            for name in &args.names {
                match manager.get_machine(name).await.map_err(client::error) {
                    Ok(machine) => out.push(client::dict_to_json(&machine)),
                    Err(e) => missing.push(e),
                }
            }
            print!("{}", inspected(&out, args.format.as_deref(), &mut missing)?);
            last_error(missing)
        }
        Command::Start(args) => machines::start(args, client).await,
        Command::Run(args) => machines::run(args, client, config).await,
        Command::Stop(args) => machines::stop(args, client).await,
        Command::Restart(args) => machines::restart(args, client).await,
        Command::Kill(args) => machines::kill(args, client).await,
        Command::Pause(args) => machines::pause(args, client, true).await,
        Command::Unpause(args) => machines::pause(args, client, false).await,
        Command::Top(args) => machines::top(args, client).await,
        Command::Wait(args) => machines::wait(args, client).await,
        Command::Port(args) => machines::port(args, client).await,
        Command::Update(args) => machines::update(args, client).await,
        Command::Stats(args) => stats::stats(args, client).await,
        Command::AutoUpdate(args) => machines::auto_update(args, client).await,
        Command::Events(args) => machines::events(args, client).await,
        Command::Exec(args) => machines::exec(args, client).await,
        Command::Shell(args) => machines::shell(args, client).await,
        Command::Logs(args) => machines::logs(args, client).await,
        Command::Cp(args) => copy::cp(args, client).await,
        Command::Daemon(_)
        | Command::Compose(_)
        | Command::Network(_)
        | Command::Completions(_)
        | Command::Manpage
        | Command::RemoveAfterExit { .. }
        | Command::HealthRun { .. }
        | Command::ApiServe { .. }
        | Command::PortProxy { .. }
        | Command::AwaitDependencies { .. }
        | Command::Info(_)
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

/// Every name of a machine or an image `command` was given, as the record it names
/// (`read_name`). What the command compares with the service's answers (wait, stats) is
/// then that record's name. `network connect` and `network disconnect`, dispatched
/// before, read their machine with `read_name` too; cp's MACHINE of MACHINE:PATH can only
/// be a name, which the service takes as it is.
async fn read_names(command: &mut Command, client: &Client, config: &Config) {
    let (texts, reads) = names_in(command);
    for text in texts {
        *text = read_name(text, reads, client, config).await;
    }
}

/// The record `text` names, as ResolveImage with names answers it (`record_name`), as
/// `reads` says.
async fn read_name(text: &str, reads: Reads, client: &Client, config: &Config) -> String {
    let mut options = client::registry_options(config);
    options.insert("names", Value::from(true));
    options.insert("images_only", Value::from(reads.images_only));
    let resolve = |text: String| async move { client.manager.resolve_image(&text, options).await };
    let registry = config.registry_set.then_some(config.registry.as_str());
    let listed = || async {
        client
            .manager
            .list_images()
            .await
            .map(|images| images.iter().map(|i| client::string(i, "name")).collect())
            .unwrap_or_default()
    };
    record_name(text, reads.bare_references, registry, resolve, listed).await
}

/// How a command reads the names it takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Reads {
    /// A reference names the image alone: where the image is only read, a machine made
    /// from it holds it too.
    images_only: bool,
    /// A name nothing here has is the reference of an image too, as for the commands that
    /// take an image (`images rm busybox` is the image of busybox:latest, whatever its
    /// name).
    bare_references: bool,
}

/// The commands that take a machine: a name is that record or none.
const MACHINES: Reads = Reads {
    images_only: true,
    bare_references: false,
};

/// The names of machines and images `command` takes whole, and how it reads them.
fn names_in(command: &mut Command) -> (Vec<&mut String>, Reads) {
    let of_images = |images_only| Reads {
        images_only,
        bare_references: true,
    };
    match command {
        Command::Inspect(a) => (
            a.names.iter_mut().collect(),
            Reads {
                images_only: false,
                ..MACHINES
            },
        ),
        Command::Images(images) => match &mut images.command {
            Some(ImagesCommand::Inspect(a)) => (a.names.iter_mut().collect(), of_images(false)),
            Some(ImagesCommand::Rm(a)) => (a.names.iter_mut().collect(), of_images(true)),
            _ => (Vec::new(), MACHINES),
        },
        Command::Rm(a) => (a.names.iter_mut().collect(), MACHINES),
        Command::Generate(a) => (a.names.iter_mut().collect(), MACHINES),
        Command::Start(a) => (vec![&mut a.name], MACHINES),
        Command::Stop(a) => (a.names.iter_mut().collect(), MACHINES),
        Command::Restart(a) => (a.names.iter_mut().collect(), MACHINES),
        Command::Kill(a) => (a.names.iter_mut().collect(), MACHINES),
        Command::Pause(a) | Command::Unpause(a) => (a.names.iter_mut().collect(), MACHINES),
        Command::Top(a) => (vec![&mut a.machine], MACHINES),
        Command::Wait(a) => (a.names.iter_mut().collect(), MACHINES),
        Command::Port(a) => (vec![&mut a.name], MACHINES),
        Command::Update(a) => (a.names.iter_mut().collect(), MACHINES),
        Command::Stats(a) => (a.names.iter_mut().collect(), MACHINES),
        Command::AutoUpdate(a) => (a.names.iter_mut().collect(), MACHINES),
        Command::Exec(a) => (vec![&mut a.machine], MACHINES),
        Command::Shell(a) => (vec![&mut a.machine], MACHINES),
        Command::Logs(a) => (a.machines.iter_mut().collect(), MACHINES),
        _ => (Vec::new(), MACHINES),
    }
}

/// The record `text` names, as the methods of the bus read it (`api::resolve::name_of`):
/// a name that can be a machine's as it is, the record of that name or none, so that a
/// machine that went meanwhile is never taken for another image by its reference, but
/// with `bare_references` (the commands of images), where a name nothing has is the
/// reference of an image too; any other text as `resolve` (ResolveImage) answers it. A reference nothing here goes by is
/// sent on in full under `registry`, the one the command line was given, for the service
/// to read the same reference, unless an image of `listed` (machined's, say
/// `my_box`) has that very name; a text that gets no answer goes on as it is, for the
/// command's own call to say what is wrong with it.
async fn record_name<R, RF, L, LF>(
    text: &str,
    bare_references: bool,
    registry: Option<&str>,
    resolve: R,
    listed: L,
) -> String
where
    R: FnOnce(String) -> RF,
    RF: std::future::Future<Output = zbus::Result<client::Dict>>,
    L: FnOnce() -> LF,
    LF: std::future::Future<Output = Vec<String>>,
{
    let name = crate::reference::validate_machine_name(text).is_ok();
    if name && !bare_references {
        return text.to_string();
    }
    let e = match resolve(text.to_string()).await {
        Ok(found) => return client::string(&found, "name"),
        Err(e) => client::error(e),
    };
    // A name nothing has goes on as the name it is, which the service takes as such.
    match (client::error_name(&e), registry) {
        _ if name => text.to_string(),
        (Some("NoSuchImage"), Some(registry)) if !listed().await.iter().any(|n| n == text) => {
            crate::reference::ImageRef::parse(text, registry)
                .map_or_else(|_| text.to_string(), |r| r.to_string())
        }
        _ => text.to_string(),
    }
}

/// What inspect prints of the objects it found: JSON, or a line per object of a Go
/// template. An object the template fails on joins `missing`, the others printed still;
/// a template that does not parse fails the command before anything is printed.
fn inspected(
    found: &[serde_json::Value],
    format: Option<&str>,
    missing: &mut Vec<anyhow::Error>,
) -> Result<String> {
    let array = || serde_json::Value::Array(found.to_vec());
    Ok(match format {
        // An empty template is no template.
        Some(format) if !format.is_empty() && format != "json" => {
            let template = crate::gotemplate::Template::parse(format)
                .map_err(|e| anyhow::anyhow!("template parsing error: {e}"))?;
            let mut text = String::new();
            for object in found {
                match template.execute(object) {
                    Ok(line) => {
                        text.push_str(&line);
                        text.push('\n');
                    }
                    Err(e) => missing.push(anyhow::anyhow!("template parsing error: {e}")),
                }
            }
            text
        }
        Some("json") => format!("{}\n", array()),
        _ => format!(
            "{}\n",
            serde_json::to_string_pretty(&array()).unwrap_or_else(|_| "null".to_string())
        ),
    })
}

/// What was not found is said after what was printed, the last error failing the command
/// so that it exits with 1.
fn last_error(mut missing: Vec<anyhow::Error>) -> Result<()> {
    match missing.pop() {
        Some(last) => {
            for e in missing {
                eprintln!("error: {e:#}");
            }
            Err(last)
        }
        None => Ok(()),
    }
}

fn cleaning_up(command: &Command) -> bool {
    matches!(
        command,
        Command::Network(crate::cli::NetworkArgs {
            command: NetworkCommand::Release { .. }
        }) | Command::RemoveAfterExit { .. }
            | Command::PortProxy { .. }
    )
}

/// A source named is taken as it is; one given by its reference is `local`, the image of
/// it never run that the service resolves (machines::local_image), so that an image
/// pulled by a short reference is not pulled again, or else a pull: a machine that ran is
/// no image, and would give the new one its backend, mode and network.
fn create_source(
    images: &[machines::Listed],
    source: &str,
    name: &str,
    registry: &str,
    force: bool,
    local: Option<String>,
) -> Result<CreateSource> {
    // Refused before anything is pulled, as the service would refuse it after.
    if !force && images.iter().any(|i| i.name == name) {
        bail!("image {name} already exists; use --force to replace it");
    }
    if images.iter().any(|i| i.name == source) {
        return Ok(CreateSource::Local(source.to_string()));
    }
    // What is no reference is left to the service to refuse.
    let Ok(image) = crate::reference::ImageRef::parse(source, registry) else {
        return Ok(CreateSource::Local(source.to_string()));
    };
    let reference = image.to_string();
    if let Some(found) = local {
        return Ok(CreateSource::Local(found));
    }
    let mut base = image.local_name();
    if images
        .iter()
        .any(|i| i.name == base && i.reference != reference)
    {
        bail!(
            "{base} is an image of another reference; pull {image} under a name of your own first (nspawn pull --name)"
        );
    }
    // A machine of the reference that ran holds its name: the image goes beside it.
    let taken = |n: &str| images.iter().any(|i| i.name == n);
    if taken(&base) {
        let stem = base.clone();
        base = (2..)
            .map(|n| format!("{stem}-{n}"))
            .find(|candidate| !taken(candidate) && candidate != name)
            .expect("a free name");
    }
    // Pulled under the machine's own name, it could not be made from it: said before the
    // download.
    if base == name {
        bail!(
            "{base} is the name {source} is pulled under; give the machine another name, or pull the image under a name of your own first (nspawn pull --name)"
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
/// The answer is read from standard input whatever it is, and anything but
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

    fn service_error(name: &str, message: &str) -> zbus::Error {
        let call = zbus::message::Message::method_call("/org/nspawn", "Test")
            .unwrap()
            .build(&())
            .unwrap();
        zbus::Error::MethodError(
            zbus::names::OwnedErrorName::try_from(format!("org.nspawn.Error.{name}")).unwrap(),
            Some(message.to_string()),
            call,
        )
    }

    fn dict(pairs: &[(&str, &str)]) -> client::Dict {
        pairs
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string(),
                    zbus::zvariant::OwnedValue::try_from(Value::from(*v)).unwrap(),
                )
            })
            .collect()
    }

    /// ResolveImage with names as the service answers it, and the texts it was asked.
    struct Service {
        resolves: Vec<(&'static str, &'static str)>,
        resolved: std::cell::RefCell<Vec<String>>,
    }

    impl Service {
        fn resolve(&self, text: String) -> std::future::Ready<zbus::Result<client::Dict>> {
            self.resolved.borrow_mut().push(text.clone());
            std::future::ready(if text.contains(' ') {
                Err(service_error("Invalid", "invalid reference format"))
            } else if text == "denied" {
                Err(service_error("NotAuthorized", "org.nspawn.inspect refused"))
            } else {
                match self.resolves.iter().find(|(r, _)| *r == text) {
                    Some((_, name)) => Ok(dict(&[("name", name), ("reference", "full")])),
                    None => Err(service_error(
                        "NoSuchImage",
                        &format!("no local image of {text}"),
                    )),
                }
            })
        }
    }

    #[tokio::test]
    async fn every_name_a_command_takes_is_the_record_the_service_reads_it_as() {
        let service = Service {
            // The service's answers: a record's name is that record, a reference the
            // image of it; which one is the service's to say.
            resolves: vec![
                ("web", "web"),
                ("busybox-1.37", "busybox-1.37"),
                ("busybox:1.37", "busybox-1.37"),
                ("docker.io/library/busybox:1.37", "busybox-1.37"),
            ],
            resolved: Default::default(),
        };
        let nothing = || std::future::ready(Vec::new());
        let name =
            |text: &'static str| record_name(text, false, None, |t| service.resolve(t), nothing);
        // A name that can be a machine's is that record or none, as the service takes it:
        // asked of nobody, so that a machine that went is never read as a reference.
        for text in ["web", "busybox-1.37", "gone", "busybox"] {
            assert_eq!(name(text).await, text);
        }
        assert!(service.resolved.borrow().is_empty());
        // A reference is the image the service says.
        for text in ["busybox:1.37", "docker.io/library/busybox:1.37"] {
            assert_eq!(name(text).await, "busybox-1.37", "{text}");
        }
        // One nothing here goes by goes on as it is, or in full under the registry the
        // command line was given; what is no reference or gets no answer as it is.
        assert_eq!(name("alpine:3").await, "alpine:3");
        assert_eq!(
            record_name(
                "alpine:3",
                false,
                Some("zot.lan"),
                |t| service.resolve(t),
                nothing
            )
            .await,
            "zot.lan/alpine:3"
        );
        // An image machined has under that very name goes on as it is.
        let machined = || std::future::ready(vec!["my_box".to_string()]);
        assert_eq!(
            record_name(
                "my_box",
                false,
                Some("zot.lan"),
                |t| service.resolve(t),
                machined
            )
            .await,
            "my_box"
        );
        assert_eq!(name("Bad Name").await, "Bad Name");
        let refused = Service {
            resolves: Vec::new(),
            resolved: Default::default(),
        };
        let denied = |t: String| {
            refused.resolved.borrow_mut().push(t);
            std::future::ready(Err(service_error("NotAuthorized", "refused")))
        };
        assert_eq!(
            record_name("x:1", false, Some("zot.lan"), denied, nothing).await,
            "x:1"
        );
        assert_eq!(service.resolved.borrow().len(), 6);
        // The commands of images read a name nothing has as the reference it is too: `images
        // rm busybox` is the image of busybox:latest, whatever its name; a name nothing is
        // behind goes on as the name it is, whatever registry was given.
        let image = |text: &'static str| {
            record_name(text, true, Some("zot.lan"), |t| service.resolve(t), nothing)
        };
        assert_eq!(image("web").await, "web");
        assert_eq!(image("busybox:1.37").await, "busybox-1.37");
        let bare = Service {
            resolves: vec![("busybox", "busybox-latest")],
            resolved: Default::default(),
        };
        let image = |text: &'static str| {
            record_name(text, true, Some("zot.lan"), |t| bare.resolve(t), nothing)
        };
        assert_eq!(image("busybox").await, "busybox-latest");
        assert_eq!(image("gone").await, "gone");
    }

    #[test]
    fn every_command_that_takes_a_machine_or_an_image_has_its_names_read() {
        use clap::Parser;
        let read = |args: &[&str]| {
            let mut command = Cli::try_parse_from([&["nspawn"], args].concat())
                .unwrap()
                .command;
            let (texts, reads) = names_in(&mut command);
            let texts: Vec<String> = texts.into_iter().map(|t| t.clone()).collect();
            (texts, reads)
        };
        let two = || vec!["a".to_string(), "b".to_string()];
        // Where the image is only read, a reference takes a machine made from it too; the
        // commands of images read a name nothing has as a reference.
        let reads = |images_only, bare_references| Reads {
            images_only,
            bare_references,
        };
        assert_eq!(read(&["inspect", "a", "b"]), (two(), reads(false, false)));
        assert_eq!(
            read(&["images", "inspect", "a", "b"]),
            (two(), reads(false, true))
        );
        assert_eq!(
            read(&["images", "rm", "a", "b"]),
            (two(), reads(true, true))
        );
        for args in [
            &["rm", "a", "b"][..],
            &["generate", "a", "b"],
            &["stop", "a", "b"],
            &["restart", "a", "b"],
            &["kill", "a", "b"],
            &["pause", "a", "b"],
            &["unpause", "a", "b"],
            &["wait", "a", "b"],
            &["update", "--memory", "1g", "a", "b"],
            &["stats", "a", "b"],
            &["auto-update", "a", "b"],
            &["logs", "a", "b"],
        ] {
            assert_eq!(read(args), (two(), MACHINES), "{args:?}");
        }
        for args in [
            &["start", "a"][..],
            &["top", "a"],
            &["port", "a"],
            &["exec", "a", "true"],
            &["shell", "a"],
        ] {
            assert_eq!(read(args), (vec!["a".into()], MACHINES), "{args:?}");
        }
        // cp's MACHINE of MACHINE:PATH is a name, as the service takes it; network connect
        // and disconnect read theirs where they are dispatched.
        for args in [
            &["cp", "a:/etc/hosts", "."][..],
            &["network", "connect", "net", "a"],
            &["images", "ls"],
            &["ps"],
        ] {
            assert!(read(args).0.is_empty(), "{args:?}");
        }
    }

    #[test]
    fn inspect_prints_json_or_a_template_line_per_object() {
        let found = vec![
            serde_json::json!({"name": "web", "labels": "none"}),
            serde_json::json!({"name": "db", "labels": {"a": "b"}}),
        ];
        let mut missing = Vec::new();
        let pretty = inspected(&found, None, &mut missing).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&pretty).unwrap(),
            serde_json::Value::Array(found.clone())
        );
        assert!(pretty.ends_with("]\n") && pretty.lines().count() > 2);
        assert_eq!(inspected(&found, Some(""), &mut missing).unwrap(), pretty);
        let compact = inspected(&found, Some("json"), &mut missing).unwrap();
        assert_eq!(compact.lines().count(), 1);
        assert_eq!(
            inspected(&found, Some("{{.name}}"), &mut missing).unwrap(),
            "web\ndb\n"
        );
        assert!(missing.is_empty());
        // An object the template fails on is said with the missing ones, the others
        // printed still.
        assert_eq!(
            inspected(&found, Some("{{.labels.a}}"), &mut missing).unwrap(),
            "b\n"
        );
        assert_eq!(missing.len(), 1);
        assert!(missing[0].to_string().starts_with("template parsing error"));
        assert!(inspected(&found, Some("{{.name"), &mut missing)
            .unwrap_err()
            .to_string()
            .starts_with("template parsing error"));
        assert!(last_error(Vec::new()).is_ok());
        let last = last_error(vec![anyhow::anyhow!("one"), anyhow::anyhow!("two")]).unwrap_err();
        assert_eq!(last.to_string(), "two");
    }

    #[test]
    fn only_the_cleanup_goes_on_without_its_configuration() {
        use clap::Parser;
        let parse = |args: &[&str]| Cli::try_parse_from(args).unwrap().command;
        assert!(cleaning_up(&parse(&[
            "nspawn", "network", "release", "web"
        ])));
        assert!(cleaning_up(&parse(&[
            "nspawn",
            "remove-after-exit",
            "web",
            "0123456789abcdef"
        ])));
        assert!(!cleaning_up(&parse(&[
            "nspawn", "network", "prepare", "web"
        ])));
        assert!(!cleaning_up(&parse(&["nspawn", "ps"])));
    }

    #[test]
    fn create_pulls_only_what_is_not_local() {
        let listed = |name: &str, reference: &str, image_only: bool| machines::Listed {
            name: name.into(),
            reference: reference.into(),
            image_only,
            pulled: 1,
        };
        let images = vec![
            listed("fedora-44", "hub.example/fedora:44", true),
            listed("web", "docker.io/library/nginx:1.27", false),
            listed("vm", "", false),
        ];
        let source = |s: &str, local: Option<&str>| {
            create_source(
                &images,
                s,
                "machine",
                "hub.example",
                false,
                local.map(str::to_string),
            )
        };
        assert_eq!(
            source("fedora-44", None).unwrap(),
            CreateSource::Local("fedora-44".to_string()),
            "a local name"
        );
        assert_eq!(
            source("fedora:44", Some("fedora-44")).unwrap(),
            CreateSource::Local("fedora-44".to_string()),
            "the image the service resolves the reference to"
        );
        // Nothing here goes by it: a pull, beside a machine of the reference that ran.
        let ran = vec![listed("nginx-1.27", "hub.example/nginx:1.27", false)];
        assert_eq!(
            create_source(&ran, "nginx:1.27", "machine", "hub.example", false, None).unwrap(),
            CreateSource::Pull {
                reference: "nginx:1.27".to_string(),
                base: "nginx-1.27-2".to_string()
            }
        );
        assert_eq!(
            source("fedora:45", None).unwrap(),
            CreateSource::Pull {
                reference: "fedora:45".to_string(),
                base: "fedora-45".to_string()
            }
        );
        let taken = vec![listed("fedora-45", "other.example/fedora:45", true)];
        let clash =
            create_source(&taken, "fedora:45", "machine", "hub.example", false, None).unwrap_err();
        assert!(clash.to_string().contains("nspawn pull --name"), "{clash}");
        let own = create_source(
            &images,
            "fedora:45",
            "fedora-45",
            "hub.example",
            false,
            None,
        )
        .unwrap_err();
        assert!(
            own.to_string()
                .starts_with("fedora-45 is the name fedora:45 is pulled under"),
            "{own}"
        );
        assert_eq!(
            source("not a reference", None).unwrap(),
            CreateSource::Local("not a reference".to_string()),
            "the service says what is wrong with it"
        );
        // A name taken is refused before anything is pulled, unless --force.
        let pulled = vec![listed("nginx-1.27", "hub.example/nginx:1.27", true)];
        let taken = create_source(
            &pulled,
            "nginx:1.27",
            "nginx-1.27",
            "hub.example",
            false,
            None,
        )
        .unwrap_err();
        assert!(taken.to_string().contains("already exists"), "{taken}");
        assert!(create_source(
            &pulled,
            "nginx:1.27",
            "nginx-1.27",
            "hub.example",
            true,
            None
        )
        .is_ok());
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
