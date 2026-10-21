//! The commands of nspawn compose that drive machines: a project's services become
//! machines of nspawn's, its networks, volumes and secrets nspawn's own, all through
//! the org.nspawn bus as the command line drives them. A project is known by the
//! labels of its machines.

mod build;
mod convert;
mod info;
mod lifecycle;
mod logs;
mod progress;
mod up;

use std::collections::BTreeMap;

use anyhow::{bail, Result};

use super::cli::{ComposeArgs, ComposeCommand};
use super::config::Host;
use super::project::Project;
use super::Log;
use crate::client::{self, Client, Dict};

pub use progress::Progress;

/// The labels a project puts on what it makes.
pub mod label {
    pub const PROJECT: &str = "com.docker.compose.project";
    pub const SERVICE: &str = "com.docker.compose.service";
    pub const NUMBER: &str = "com.docker.compose.container-number";
    pub const ONEOFF: &str = "com.docker.compose.oneoff";
    pub const CONFIG_HASH: &str = "com.docker.compose.config-hash";
    pub const WORKING_DIR: &str = "com.docker.compose.project.working_dir";
    pub const CONFIG_FILES: &str = "com.docker.compose.project.config_files";
    pub const ENV_FILE: &str = "com.docker.compose.project.environment_file";
    pub const DEPENDS_ON: &str = "com.docker.compose.depends_on";
    pub const IMAGE: &str = "com.docker.compose.image";
    pub const VERSION: &str = "com.docker.compose.version";
    pub const NETWORK: &str = "com.docker.compose.network";
}

/// The docker compose release nspawn compose stamps its machines with.
pub(super) fn compose_version() -> &'static str {
    super::support::DOCKER_COMPOSE.trim_start_matches('v')
}

/// What a command works with: the bus, the project (or its name alone, for the
/// commands that work from the labels), and where its progress goes.
pub struct Engine<'a> {
    pub client: Client,
    pub name: String,
    pub project: Option<Project>,
    pub progress: Progress,
    pub args: &'a ComposeArgs,
    pub log: &'a dyn Log,
    /// The command line's working directory, as Go's os.Getwd gives it.
    pub cwd: String,
}

/// A machine of the project, as ListMachines has it.
#[derive(Debug, Clone)]
pub struct Machine {
    pub name: String,
    pub service: String,
    pub number: u32,
    pub oneoff: bool,
    /// machined's state, or the service's for one it does not list: running, stopped,
    /// starting, restarting, closing, paused.
    pub state: String,
    pub health: String,
    pub exit_code: Option<i32>,
    pub labels: BTreeMap<String, String>,
    pub created: u64,
    pub started: u64,
    pub dict: Dict,
}

impl Machine {
    fn from_dict(dict: Dict) -> Machine {
        let labels: BTreeMap<String, String> = dict
            .get("labels")
            .and_then(|v| std::collections::HashMap::<String, String>::try_from(v.clone()).ok())
            .map(|m| m.into_iter().collect())
            .unwrap_or_default();
        let number = labels
            .get(label::NUMBER)
            .and_then(|n| n.parse().ok())
            .unwrap_or(0);
        Machine {
            name: client::string(&dict, "name"),
            service: labels.get(label::SERVICE).cloned().unwrap_or_default(),
            number,
            oneoff: labels.get(label::ONEOFF).is_some_and(|v| v == "True"),
            state: client::string(&dict, "state"),
            health: client::string(&dict, "health"),
            exit_code: dict
                .get("exit_code")
                .and_then(|v| i32::try_from(v.clone()).ok()),
            labels,
            created: client::u64(&dict, "created"),
            started: client::u64(&dict, "started"),
            dict,
        }
    }

    pub fn running(&self) -> bool {
        matches!(self.state.as_str(), "running" | "paused" | "opening")
    }

    pub fn label(&self, key: &str) -> &str {
        self.labels.get(key).map(String::as_str).unwrap_or("")
    }

    pub fn string(&self, key: &str) -> String {
        client::string(&self.dict, key)
    }

    pub fn strings(&self, key: &str) -> Vec<String> {
        client::strings(&self.dict, key)
    }
}

impl Engine<'_> {
    /// Every machine of the project, one-offs too, by name.
    pub async fn machines(&self) -> Result<Vec<Machine>> {
        let mut machines: Vec<Machine> = self
            .client
            .manager
            .list_machines(true)
            .await
            .map_err(client::error)?
            .into_iter()
            .map(Machine::from_dict)
            // An image the project built carries its labels too, and is no machine; nor
            // is one made by hand from that image, as only compose gives the config hash.
            .filter(|m| client::string(&m.dict, "origin") == "create")
            .filter(|m| m.labels.get(label::PROJECT) == Some(&self.name))
            .filter(|m| m.labels.contains_key(label::CONFIG_HASH))
            .collect();
        machines.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(machines)
    }

    /// The project, for the commands that need one.
    pub fn project(&self) -> &Project {
        self.project
            .as_ref()
            .expect("the commands that make machines load their project")
    }

    pub fn warn(&self, text: String) {
        self.log.warn(text);
    }
}

/// The name of the machine of a service's replica: `container_name` where it has one,
/// `PROJECT-SERVICE-N` otherwise, each `_` written `-` since machined takes none.
pub fn machine_name(project: &str, service: &str, container_name: &str, number: u32) -> String {
    let name = if container_name.is_empty() {
        format!("{project}-{service}-{number}")
    } else {
        container_name.to_string()
    };
    name.replace('_', "-")
}

/// Whether machined takes a name: letters, digits, `-` and `.`, up to 64, as a host name.
pub fn valid_machine_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
        && !name.starts_with(['-', '.'])
}

/// An image of a compose file as nspawn's registry client reads it: one that names no
/// registry comes from Docker Hub, an official one under library/.
pub fn image_reference(image: &str) -> Result<crate::reference::ImageRef> {
    crate::reference::ImageRef::normalized(image)
}

/// Runs a command that drives machines; the error is the line to print.
pub fn execute(
    args: &ComposeArgs,
    host: &mut Host,
    flags: &super::options::Flags,
    log: &dyn Log,
    out: &mut dyn std::io::Write,
) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    runtime
        .block_on(dispatch(args, host, flags, log, out))
        .map_err(|e| format!("{e:#}"))
}

/// The services a command names.
fn named(command: &ComposeCommand) -> Vec<String> {
    use ComposeCommand as C;
    match command {
        C::Create(c) => c.service.clone(),
        C::Down(c) => c.service.clone(),
        C::Events(c) => c.service.clone(),
        C::Images(c) => c.service.clone(),
        C::Kill(c) => c.service.clone(),
        C::Logs(c) => c.service.clone(),
        C::Pause(c) => c.service.clone(),
        C::Ps(c) => c.service.clone(),
        C::Pull(c) => c.service.clone(),
        C::Build(c) => c.service.clone(),
        C::Rm(c) => c.service.clone(),
        C::Start(c) => c.service.clone(),
        C::Stats(c) => c.service.clone(),
        C::Stop(c) => c.service.clone(),
        C::Top(c) => c.service.clone(),
        C::Unpause(c) => c.service.clone(),
        C::Up(c) => c.service.clone(),
        C::Volumes(c) => c.service.clone(),
        C::Exec(c) => c.service.iter().cloned().collect(),
        C::Port(c) => c.service.iter().cloned().collect(),
        C::Run(c) => c.service.iter().cloned().collect(),
        // SERVICE=REPLICAS: the services it scales, whose environment is resolved.
        C::Scale(c) => c
            .service
            .iter()
            .map(|s| {
                s.split_once('=')
                    .map_or(s.as_str(), |(name, _)| name)
                    .to_string()
            })
            .collect(),
        // restart and wait load the whole project: restart also restarts the services
        // that depend on the ones it names.
        _ => Vec::new(),
    }
}

/// How a command finds its project: loaded with its services (up, create, run, pull,
/// scale), or by name when no file is given and -p is (down, ps, logs and the others,
/// projectOrName), or by name alone (ls).
enum Wants {
    Project,
    ProjectOrName,
    Nothing,
}

fn wants(command: &ComposeCommand) -> Wants {
    use ComposeCommand as C;
    match command {
        // exec, port, cp, images, top and stats find their project by -p NAME alone,
        // as down and ps do.
        C::Up(_) | C::Create(_) | C::Run(_) | C::Pull(_) | C::Build(_) | C::Scale(_) => {
            Wants::Project
        }
        C::Ls(_) => Wants::Nothing,
        _ => Wants::ProjectOrName,
    }
}

/// A project named by -p alone, as the command looks for it: lowercased by the commands
/// whose docker compose counterparts lowercase it, as given by the others.
fn named_as_given(command: &ComposeCommand, name: String) -> String {
    use ComposeCommand as C;
    match command {
        C::Down(_)
        | C::Stop(_)
        | C::Start(_)
        | C::Restart(_)
        | C::Rm(_)
        | C::Kill(_)
        | C::Pause(_)
        | C::Unpause(_)
        | C::Ps(_)
        | C::Top(_)
        | C::Port(_)
        | C::Exec(_)
        | C::Cp(_)
        | C::Events(_)
        | C::Images(_) => name.to_lowercase(),
        _ => name,
    }
}

async fn dispatch(
    args: &ComposeArgs,
    host: &mut Host,
    flags: &super::options::Flags,
    log: &dyn Log,
    out: &mut dyn std::io::Write,
) -> Result<()> {
    let command = args.command.as_ref().expect("a command");
    check_arguments(command)?;
    let services = named(command);
    let mut warnings = Vec::new();
    let (name, project) = {
        let mut warn = |w: String| warnings.push(w);
        match wants(command) {
            Wants::Nothing => (String::new(), None),
            Wants::Project => {
                let project =
                    match super::config::project_for(args, flags, host, &services, &mut warn) {
                        // run takes a job as a one-off service of its own; a name that
                        // is no job keeps the error of the selection.
                        Err(e)
                            if matches!(command, ComposeCommand::Run(_))
                                && e.starts_with("no such service") =>
                        {
                            super::config::job_project(args, flags, host, &services[0], &mut warn)
                                .map_err(anyhow::Error::msg)?
                                .ok_or_else(|| anyhow::Error::msg(e))?
                        }
                        other => other.map_err(anyhow::Error::msg)?,
                    };
                (project.name.clone(), Some(project))
            }
            Wants::ProjectOrName => {
                let given = args.project_name.clone().unwrap_or_default();
                if args.file.is_empty() && !given.is_empty() {
                    (named_as_given(command, given), None)
                } else {
                    match super::config::project_for(args, flags, host, &services, &mut warn) {
                        Ok(project) => (project.name.clone(), Some(project)),
                        Err(e) => match host.env.get("COMPOSE_PROJECT_NAME") {
                            Some(name) if !name.is_empty() => (name.clone(), None),
                            _ => bail!(e),
                        },
                    }
                }
            }
        }
    };
    for w in warnings {
        log.warn(w);
    }
    if args.dry_run {
        if let Some(name) = changes_state(command) {
            bail!("{name} does not run with --dry-run: nspawn compose does not simulate the changes it makes, and nothing was done");
        }
    }
    let client = Client::connect().await?;
    let mut engine = Engine {
        client,
        name,
        project,
        progress: Progress::new(args, host),
        args,
        log,
        cwd: host.cwd.clone(),
    };
    use ComposeCommand as C;
    match command {
        C::Up(c) => up::up(&mut engine, c).await,
        C::Create(c) => up::create(&mut engine, c).await,
        C::Pull(c) => up::pull(&mut engine, c).await,
        C::Build(c) => build::run(&mut engine, c).await,
        C::Scale(c) => up::scale(&mut engine, c).await,
        C::Run(c) => logs::run(&mut engine, c).await,
        C::Down(c) => lifecycle::down(&mut engine, c).await,
        C::Start(c) => lifecycle::start(&mut engine, c).await,
        C::Stop(c) => lifecycle::stop(&mut engine, c).await,
        C::Restart(c) => lifecycle::restart(&mut engine, c).await,
        C::Kill(c) => lifecycle::kill(&mut engine, c).await,
        C::Pause(c) => lifecycle::pause(&mut engine, &c.service, true).await,
        C::Unpause(c) => lifecycle::pause(&mut engine, &c.service, false).await,
        C::Rm(c) => lifecycle::rm(&mut engine, c).await,
        C::Wait(c) => lifecycle::wait(&mut engine, c).await,
        C::Ps(c) => info::ps(&mut engine, c, out).await,
        C::Ls(c) => info::ls(&mut engine, c, out).await,
        C::Images(c) => info::images(&mut engine, c, out).await,
        C::Top(c) => info::top(&mut engine, c, out).await,
        C::Port(c) => info::port(&mut engine, c, out).await,
        C::Volumes(c) => info::volumes(&mut engine, c, out).await,
        C::Stats(c) => info::stats(&mut engine, c, out).await,
        C::Events(c) => info::events(&mut engine, c, out).await,
        C::Logs(c) => logs::logs(&mut engine, c).await,
        C::Exec(c) => logs::exec(&mut engine, c).await,
        C::Cp(c) => logs::cp(&mut engine, c).await,
        C::Config(_) | C::Version(_) => unreachable!("not a command that drives machines"),
    }
}

/// The counts of arguments the reference's commands check before anything is loaded.
fn check_arguments(command: &ComposeCommand) -> Result<()> {
    use ComposeCommand as C;
    match command {
        C::Scale(c) if c.service.is_empty() => {
            bail!("requires at least 1 arg(s), only received 0")
        }
        // docker/cli's RequiresMinArgs and ExactArgs, which wait and cp take.
        C::Wait(c) if c.service.is_empty() => bail!(
            "docker: 'docker compose wait' requires at least 1 argument\n\nUsage:  docker compose wait SERVICE [SERVICE...] [OPTIONS] [flags]\n\nSee 'docker compose wait --help' for more information"
        ),
        C::Cp(c) => {
            let received = usize::from(c.source.is_some())
                + usize::from(c.destination.is_some())
                + c.extra.len();
            if received != 2 {
                bail!("docker: 'docker compose cp' requires 2 arguments\n\nUsage:  docker compose cp [OPTIONS] SERVICE:SRC_PATH DEST_PATH|-\n\tdocker compose cp [OPTIONS] SRC_PATH|- SERVICE:DEST_PATH [flags]\n\nRun 'docker compose cp --help' for more information");
            }
            Ok(())
        }
        C::Stats(c) if c.service.len() > 1 => {
            bail!("accepts at most 1 arg(s), received {}", c.service.len())
        }
        C::Exec(c) => {
            let received = usize::from(c.service.is_some()) + c.command.len();
            if received < 2 {
                bail!("requires at least 2 arg(s), only received {received}");
            }
            Ok(())
        }
        C::Run(c) if c.service.is_none() => {
            bail!("requires at least 1 arg(s), only received 0")
        }
        C::Port(c) => {
            let received = usize::from(c.service.is_some())
                + usize::from(c.private_port.is_some())
                + c.extra.len();
            if !(1..=2).contains(&received) {
                bail!("accepts between 1 and 2 arg(s), received {received}");
            }
            if let Some(port) = &c.private_port {
                info::private_port(port)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// The name of a command that changes machines, images, networks or volumes, which
/// --dry-run refuses: nothing is simulated.
fn changes_state(command: &ComposeCommand) -> Option<&'static str> {
    use ComposeCommand as C;
    Some(match command {
        C::Up(_) => "up",
        C::Create(_) => "create",
        C::Pull(_) => "pull",
        C::Build(_) => "build",
        C::Scale(_) => "scale",
        C::Run(_) => "run",
        C::Down(_) => "down",
        C::Start(_) => "start",
        C::Stop(_) => "stop",
        C::Restart(_) => "restart",
        C::Kill(_) => "kill",
        C::Pause(_) => "pause",
        C::Unpause(_) => "unpause",
        C::Rm(_) => "rm",
        C::Exec(_) => "exec",
        C::Cp(_) => "cp",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_names_the_services_it_scales() {
        let scale = ComposeCommand::Scale(crate::compose::cli::ScaleArgs {
            no_deps: false,
            service: vec!["web=3".to_string(), "worker=0".to_string()],
        });
        assert_eq!(named(&scale), ["web", "worker"]);
    }

    fn compose(argv: &[&str]) -> ComposeCommand {
        use clap::Parser;
        let mut full = vec!["nspawn", "compose"];
        full.extend_from_slice(argv);
        match crate::cli::Cli::try_parse_from(full).unwrap().command {
            crate::cli::Command::Compose(c) => c.command.unwrap(),
            _ => unreachable!(),
        }
    }

    #[test]
    fn counts_are_checked_before_anything_loads() {
        let error = |argv: &[&str]| check_arguments(&compose(argv)).err().map(|e| e.to_string());
        assert_eq!(
            error(&["exec", "web"]).as_deref(),
            Some("requires at least 2 arg(s), only received 1")
        );
        assert_eq!(
            error(&["run"]).as_deref(),
            Some("requires at least 1 arg(s), only received 0")
        );
        assert_eq!(
            error(&["port", "web", "80", "x"]).as_deref(),
            Some("accepts between 1 and 2 arg(s), received 3")
        );
        assert!(error(&["port", "web", "x"])
            .unwrap()
            .starts_with("strconv.ParseUint: parsing"));
        assert!(error(&["cp", "web:/a"])
            .unwrap()
            .starts_with("docker: 'docker compose cp' requires 2 arguments"));
        assert!(error(&["cp", "web:/a", "b", "c"])
            .unwrap()
            .starts_with("docker: 'docker compose cp' requires 2 arguments"));
        assert!(error(&["wait"])
            .unwrap()
            .starts_with("docker: 'docker compose wait' requires at least 1 argument"));
        for fine in [
            &["exec", "web", "sh"][..],
            &["port", "web"],
            &["cp", "web:/a", "b"],
            &["run", "web"],
        ] {
            assert_eq!(error(fine), None, "{fine:?}");
        }
    }

    #[test]
    fn a_project_named_alone_is_looked_for_as_docker_compose_does() {
        for command in [
            "down", "stop", "start", "restart", "rm", "kill", "ps", "top",
        ] {
            assert_eq!(
                named_as_given(&compose(&[command]), "MyApp".to_string()),
                "myapp",
                "{command}"
            );
        }
        // logs, wait and volumes take it as given.
        for command in ["logs", "volumes"] {
            assert_eq!(
                named_as_given(&compose(&[command]), "MyApp".to_string()),
                "MyApp",
                "{command}"
            );
        }
    }

    #[test]
    fn restart_and_wait_load_the_whole_project() {
        use clap::Parser;
        for argv in [["restart", "db"], ["wait", "db"]] {
            let mut full = vec!["nspawn", "compose"];
            full.extend_from_slice(&argv);
            match crate::cli::Cli::try_parse_from(full).unwrap().command {
                crate::cli::Command::Compose(c) => {
                    assert!(named(&c.command.unwrap()).is_empty(), "{argv:?}")
                }
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn argument_counts_are_checked_first() {
        use clap::Parser;
        let check = |argv: &[&str]| {
            let mut full = vec!["nspawn", "compose"];
            full.extend_from_slice(argv);
            match crate::cli::Cli::try_parse_from(full).unwrap().command {
                crate::cli::Command::Compose(c) => check_arguments(&c.command.unwrap()),
                _ => unreachable!(),
            }
        };
        assert!(check(&["scale"]).is_err());
        assert!(check(&["scale", "web=2"]).is_ok());
        assert!(check(&["wait"]).is_err());
        assert_eq!(
            check(&["stats", "a", "b"]).unwrap_err().to_string(),
            "accepts at most 1 arg(s), received 2"
        );
    }

    #[test]
    fn a_dry_run_changes_nothing() {
        use clap::Parser;
        let parse = |argv: &[&str]| -> ComposeCommand {
            let mut full = vec!["nspawn", "compose", "--dry-run"];
            full.extend_from_slice(argv);
            match crate::cli::Cli::try_parse_from(full).unwrap().command {
                crate::cli::Command::Compose(c) => c.command.unwrap(),
                _ => unreachable!(),
            }
        };
        for argv in [
            &["up", "-d"][..],
            &["down", "-v"],
            &["rm", "-f"],
            &["pull"],
            &["exec", "web", "true"],
        ] {
            assert!(changes_state(&parse(argv)).is_some(), "{argv:?}");
        }
        for argv in [
            &["ps"][..],
            &["logs"],
            &["images"],
            &["ls"],
            &["port", "web", "80"],
        ] {
            assert!(changes_state(&parse(argv)).is_none(), "{argv:?}");
        }
    }

    #[test]
    fn machines_are_named_as_docker_compose_names_containers() {
        assert_eq!(machine_name("demo", "web", "", 1), "demo-web-1");
        assert_eq!(machine_name("my_app", "db_main", "", 2), "my-app-db-main-2");
        assert_eq!(
            machine_name("ejabberd", "postgres", "ejabberd_postgres", 1),
            "ejabberd-postgres"
        );
        assert!(valid_machine_name("ejabberd-postgres"));
        assert!(!valid_machine_name("a_b"));
        assert!(!valid_machine_name(&"x".repeat(65)));
        assert!(!valid_machine_name("-x"));
    }

    #[test]
    fn images_come_from_docker_hub_unless_they_name_a_registry() {
        let r = image_reference("postgres:18-alpine").unwrap();
        assert_eq!(r.to_string(), "docker.io/library/postgres:18-alpine");
        assert_eq!(r.local_name(), "postgres-18-alpine");
        let r = image_reference("lucaslorentz/caddy-docker-proxy:2.13-alpine").unwrap();
        assert_eq!(
            r.to_string(),
            "docker.io/lucaslorentz/caddy-docker-proxy:2.13-alpine"
        );
        let r = image_reference("ghcr.io/processone/ejabberd").unwrap();
        assert_eq!(r.to_string(), "ghcr.io/processone/ejabberd:latest");
        let r = image_reference("localhost/x:1").unwrap();
        assert_eq!(r.registry, "localhost");
    }
}
