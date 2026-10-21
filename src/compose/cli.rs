//! The command line of `nspawn compose`: docker compose's own flags and commands.

use clap::{Args, Subcommand};

#[derive(Args, Debug)]
#[command(
    about = "Run docker compose files on nspawn machines",
    long_about = "Run the files of docker compose as they are, with docker compose's commands \
                  and flags, on nspawn machines, networks, volumes and secrets. \
                  docs/COMPOSE.md has what is supported."
)]
pub struct ComposeArgs {
    /// Compose configuration files.
    #[arg(short = 'f', long = "file", value_name = "FILE")]
    pub file: Vec<String>,
    /// Project name.
    #[arg(short = 'p', long = "project-name", value_name = "NAME")]
    pub project_name: Option<String>,
    /// Specify an alternate working directory (default: the path of the first file).
    #[arg(long = "project-directory", value_name = "DIR")]
    pub project_directory: Option<String>,
    /// Specify an alternate environment file.
    #[arg(long = "env-file", value_name = "FILE")]
    pub env_file: Vec<String>,
    /// Specify a profile to enable.
    #[arg(long = "profile", value_name = "PROFILE")]
    pub profile: Vec<String>,
    /// Control max parallelism, -1 for unlimited.
    #[arg(long, value_name = "N")]
    pub parallel: Option<i64>,
    /// Set type of progress output (auto, tty, plain, json, quiet).
    #[arg(long, value_name = "MODE")]
    pub progress: Option<String>,
    /// Control when to print ANSI control characters ("never"|"always"|"auto").
    #[arg(long, value_name = "WHEN")]
    pub ansi: Option<String>,
    /// Execute command in dry run mode.
    #[arg(long, global = true)]
    pub dry_run: bool,
    /// Run compose in backward compatibility mode; machine names keep `-`, as machined
    /// takes no `_`.
    #[arg(long)]
    pub compatibility: bool,
    /// Include all resources, even those not used by services.
    #[arg(long)]
    pub all_resources: bool,
    /// Do not print ANSI control characters (DEPRECATED).
    #[arg(long, hide = true)]
    pub no_ansi: bool,
    /// Show more output.
    #[arg(long, hide = true)]
    pub verbose: bool,
    /// DEPRECATED! Use --project-directory instead.
    #[arg(long, value_name = "DIR", hide = true)]
    pub workdir: Option<String>,
    /// Use insecure registry to pull Compose OCI artifacts. Doesn't apply to images.
    #[arg(long, value_name = "HOST", hide = true)]
    pub insecure_registry: Vec<String>,
    /// Show the version information.
    #[arg(short = 'v', long = "version", hide = true)]
    pub version: bool,
    #[command(subcommand)]
    pub command: Option<ComposeCommand>,
}

#[derive(Subcommand, Debug)]
pub enum ComposeCommand {
    /// Parse, resolve and render compose file in canonical format.
    Config(ConfigArgs),
    /// Show the version information.
    Version(VersionArgs),
    /// Build or rebuild services.
    Build(BuildArgs),
    /// Copy files/folders between a service container and the local filesystem.
    Cp(CpArgs),
    /// Creates containers for a service.
    Create(CreateArgs),
    /// Stop and remove containers, networks.
    Down(DownArgs),
    /// Receive real time events from containers.
    Events(EventsArgs),
    /// Execute a command in a running container.
    Exec(ExecArgs),
    /// List images used by the created containers.
    Images(ImagesArgs),
    /// Force stop service containers.
    Kill(KillArgs),
    /// View output from containers.
    Logs(LogsArgs),
    /// List running compose projects.
    Ls(LsArgs),
    /// Pause services.
    Pause(PauseArgs),
    /// |.
    Port(PortArgs),
    /// List containers.
    Ps(PsArgs),
    /// Pull service images.
    Pull(PullArgs),
    /// Restart service containers.
    Restart(RestartArgs),
    /// Removes stopped service containers.
    Rm(RmArgs),
    /// Run a one-off command on a service.
    Run(RunArgs),
    /// Scale services.
    Scale(ScaleArgs),
    /// Start services.
    Start(StartArgs),
    /// Display a live stream of container(s) resource usage statistics.
    Stats(StatsArgs),
    /// Stop services.
    Stop(StopArgs),
    /// Display the running processes.
    Top(TopArgs),
    /// Unpause services.
    Unpause(UnpauseArgs),
    /// Create and start containers.
    Up(UpArgs),
    /// List volumes.
    Volumes(VolumesArgs),
    /// Block until containers of all (or specified) services stop..
    Wait(WaitArgs),
}

#[derive(Args, Debug)]
pub struct VersionArgs {
    /// Format the output. Values: [pretty | json]. (Default: pretty)
    #[arg(short, long, value_name = "FORMAT")]
    pub format: Option<String>,
    /// Shows only the version number of docker compose it matches.
    #[arg(long)]
    pub short: bool,
}

/// The options a command line sets, as the support table names them: (command,
/// option), "" for the global ones.
pub fn requested(args: &ComposeArgs) -> Vec<(&'static str, &'static str)> {
    let mut set = Vec::new();
    let mut flag = |command: &'static str, name: &'static str, on: bool| {
        if on {
            set.push((command, name));
        }
    };
    flag("", "file", !args.file.is_empty());
    flag("", "project-name", args.project_name.is_some());
    flag("", "project-directory", args.project_directory.is_some());
    flag("", "env-file", !args.env_file.is_empty());
    flag("", "profile", !args.profile.is_empty());
    flag("", "parallel", args.parallel.is_some());
    flag("", "progress", args.progress.is_some());
    flag("", "ansi", args.ansi.is_some());
    flag("", "dry-run", args.dry_run);
    flag("", "compatibility", args.compatibility);
    flag("", "all-resources", args.all_resources);
    flag("", "no-ansi", args.no_ansi);
    flag("", "verbose", args.verbose);
    flag("", "workdir", args.workdir.is_some());
    flag("", "insecure-registry", !args.insecure_registry.is_empty());
    flag("", "version", args.version);
    match &args.command {
        Some(ComposeCommand::Config(c)) => {
            let command = "config";
            flag(command, "format", c.format.is_some());
            flag(command, "resolve-image-digests", c.resolve_image_digests);
            flag(command, "lock-image-digests", c.lock_image_digests);
            flag(command, "quiet", c.quiet);
            flag(command, "no-interpolate", c.no_interpolate);
            flag(command, "no-normalize", c.no_normalize);
            flag(command, "no-path-resolution", c.no_path_resolution);
            flag(command, "no-consistency", c.no_consistency);
            flag(command, "no-env-resolution", c.no_env_resolution);
            flag(command, "services", c.services);
            flag(command, "volumes", c.volumes);
            flag(command, "networks", c.networks);
            flag(command, "models", c.models);
            flag(command, "profiles", c.profiles);
            flag(command, "images", c.images);
            flag(command, "hash", c.hash.is_some());
            flag(command, "variables", c.variables);
            flag(command, "environment", c.environment);
            flag(command, "output", c.output.is_some());
        }
        Some(ComposeCommand::Version(v)) => {
            flag("version", "format", v.format.is_some());
            flag("version", "short", v.short);
        }
        Some(ComposeCommand::Cp(c)) => {
            let command = "cp";
            flag(command, "all", c.all);
            flag(command, "archive", c.archive);
            flag(command, "follow-link", c.follow_link);
            flag(command, "index", c.index.is_some());
        }
        Some(ComposeCommand::Create(c)) => {
            let command = "create";
            flag(command, "build", c.build);
            flag(command, "force-recreate", c.force_recreate);
            flag(command, "no-build", c.no_build);
            flag(command, "no-recreate", c.no_recreate);
            flag(command, "pull", c.pull.is_some());
            flag(command, "quiet-pull", c.quiet_pull);
            flag(command, "remove-orphans", c.remove_orphans);
            flag(command, "scale", !c.scale.is_empty());
            flag(command, "yes", c.yes);
        }
        Some(ComposeCommand::Down(c)) => {
            let command = "down";
            flag(command, "remove-orphans", c.remove_orphans.is_some());
            flag(command, "rmi", c.rmi.is_some());
            flag(command, "timeout", c.timeout.is_some());
            flag(command, "volumes", c.volumes);
        }
        Some(ComposeCommand::Events(c)) => {
            let command = "events";
            flag(command, "json", c.json);
            flag(command, "since", c.since.is_some());
            flag(command, "until", c.until.is_some());
        }
        Some(ComposeCommand::Exec(c)) => {
            let command = "exec";
            flag(command, "detach", c.detach);
            flag(command, "env", !c.env.is_empty());
            flag(command, "index", c.index.is_some());
            flag(command, "interactive", c.interactive.is_some());
            flag(command, "no-tty", c.no_tty.is_some());
            flag(command, "privileged", c.privileged);
            flag(command, "tty", c.tty.is_some());
            flag(command, "user", c.user.is_some());
            flag(command, "workdir", c.workdir.is_some());
        }
        Some(ComposeCommand::Images(c)) => {
            let command = "images";
            flag(command, "format", c.format.is_some());
            flag(command, "quiet", c.quiet);
        }
        Some(ComposeCommand::Kill(c)) => {
            let command = "kill";
            flag(command, "remove-orphans", c.remove_orphans.is_some());
            flag(command, "signal", c.signal.is_some());
        }
        Some(ComposeCommand::Logs(c)) => {
            let command = "logs";
            flag(command, "follow", c.follow);
            flag(command, "index", c.index.is_some());
            flag(command, "no-color", c.no_color);
            flag(command, "no-log-prefix", c.no_log_prefix);
            flag(command, "since", c.since.is_some());
            flag(command, "tail", c.tail.is_some());
            flag(command, "timestamps", c.timestamps);
            flag(command, "until", c.until.is_some());
        }
        Some(ComposeCommand::Ls(c)) => {
            let command = "ls";
            flag(command, "all", c.all);
            flag(command, "filter", !c.filter.is_empty());
            flag(command, "format", c.format.is_some());
            flag(command, "quiet", c.quiet);
        }
        Some(ComposeCommand::Port(c)) => {
            let command = "port";
            flag(command, "index", c.index.is_some());
            flag(command, "protocol", c.protocol.is_some());
        }
        Some(ComposeCommand::Ps(c)) => {
            let command = "ps";
            flag(command, "all", c.all);
            flag(command, "filter", c.filter.is_some());
            flag(command, "format", c.format.is_some());
            flag(command, "no-trunc", c.no_trunc);
            flag(command, "orphans", c.orphans.is_some());
            flag(command, "quiet", c.quiet);
            flag(command, "services", c.services);
            flag(command, "status", !c.status.is_empty());
        }
        Some(ComposeCommand::Build(c)) => {
            let command = "build";
            flag(command, "build-arg", !c.build_arg.is_empty());
            flag(command, "builder", c.builder.is_some());
            flag(command, "check", c.check);
            flag(command, "print", c.print);
            flag(command, "provenance", c.provenance.is_some());
            flag(command, "sbom", c.sbom.is_some());
            flag(command, "ssh", c.ssh.is_some());
            flag(command, "compress", c.compress.is_some());
            flag(command, "force-rm", c.force_rm.is_some());
            flag(command, "memory", c.memory.is_some());
            flag(command, "no-cache", c.no_cache);
            flag(command, "no-rm", c.no_rm);
            flag(command, "parallel", c.parallel.is_some());
            flag(command, "progress", c.progress.is_some());
            flag(command, "pull", c.pull);
            flag(command, "push", c.push);
            flag(command, "quiet", c.quiet);
            flag(command, "with-dependencies", c.with_dependencies);
        }
        Some(ComposeCommand::Pull(c)) => {
            let command = "pull";
            flag(command, "ignore-buildable", c.ignore_buildable);
            flag(command, "ignore-pull-failures", c.ignore_pull_failures);
            flag(command, "include-deps", c.include_deps);
            flag(command, "no-parallel", c.no_parallel.is_some());
            flag(command, "parallel", c.parallel.is_some());
            flag(command, "policy", c.policy.is_some());
            flag(command, "quiet", c.quiet);
        }
        Some(ComposeCommand::Restart(c)) => {
            let command = "restart";
            flag(command, "no-deps", c.no_deps);
            flag(command, "timeout", c.timeout.is_some());
        }
        Some(ComposeCommand::Rm(c)) => {
            let command = "rm";
            flag(command, "all", c.all);
            flag(command, "force", c.force);
            flag(command, "stop", c.stop);
            flag(command, "volumes", c.volumes);
        }
        Some(ComposeCommand::Run(c)) => {
            let command = "run";
            flag(command, "build", c.build);
            flag(command, "cap-add", !c.cap_add.is_empty());
            flag(command, "cap-drop", !c.cap_drop.is_empty());
            flag(command, "detach", c.detach);
            flag(command, "entrypoint", c.entrypoint.is_some());
            flag(command, "env", !c.env.is_empty());
            flag(command, "env-from-file", !c.env_from_file.is_empty());
            flag(command, "interactive", c.interactive.is_some());
            flag(command, "label", !c.label.is_empty());
            flag(command, "name", c.name.is_some());
            flag(command, "no-deps", c.no_deps);
            flag(command, "no-tty", c.no_tty.is_some());
            flag(command, "publish", !c.publish.is_empty());
            flag(command, "pull", c.pull.is_some());
            flag(command, "quiet", c.quiet);
            flag(command, "quiet-build", c.quiet_build);
            flag(command, "quiet-pull", c.quiet_pull);
            flag(command, "remove-orphans", c.remove_orphans);
            flag(command, "rm", c.rm);
            flag(command, "service-ports", c.service_ports);
            flag(command, "tty", c.tty.is_some());
            flag(command, "use-aliases", c.use_aliases);
            flag(command, "user", c.user.is_some());
            flag(command, "volume", !c.volume.is_empty());
            flag(command, "workdir", c.workdir.is_some());
        }
        Some(ComposeCommand::Scale(c)) => {
            let command = "scale";
            flag(command, "no-deps", c.no_deps);
        }
        Some(ComposeCommand::Start(c)) => {
            let command = "start";
            flag(command, "wait", c.wait);
            flag(command, "wait-timeout", c.wait_timeout.is_some());
        }
        Some(ComposeCommand::Stats(c)) => {
            let command = "stats";
            flag(command, "all", c.all);
            flag(command, "format", c.format.is_some());
            flag(command, "no-stream", c.no_stream);
            flag(command, "no-trunc", c.no_trunc);
        }
        Some(ComposeCommand::Stop(c)) => {
            let command = "stop";
            flag(command, "timeout", c.timeout.is_some());
        }
        Some(ComposeCommand::Up(c)) => {
            let command = "up";
            flag(
                command,
                "abort-on-container-exit",
                c.abort_on_container_exit,
            );
            flag(
                command,
                "abort-on-container-failure",
                c.abort_on_container_failure,
            );
            flag(command, "always-recreate-deps", c.always_recreate_deps);
            flag(command, "attach", !c.attach.is_empty());
            flag(command, "attach-dependencies", c.attach_dependencies);
            flag(command, "build", c.build);
            flag(command, "detach", c.detach);
            flag(command, "exit-code-from", c.exit_code_from.is_some());
            flag(command, "force-recreate", c.force_recreate);
            flag(command, "menu", c.menu);
            flag(command, "no-attach", !c.no_attach.is_empty());
            flag(command, "no-build", c.no_build);
            flag(command, "no-color", c.no_color);
            flag(command, "no-deps", c.no_deps);
            flag(command, "no-log-prefix", c.no_log_prefix);
            flag(command, "no-recreate", c.no_recreate);
            flag(command, "no-start", c.no_start);
            flag(command, "pull", c.pull.is_some());
            flag(command, "quiet-build", c.quiet_build);
            flag(command, "quiet-pull", c.quiet_pull);
            flag(command, "remove-orphans", c.remove_orphans.is_some());
            flag(command, "renew-anon-volumes", c.renew_anon_volumes);
            flag(command, "scale", !c.scale.is_empty());
            flag(command, "timeout", c.timeout.is_some());
            flag(command, "timestamps", c.timestamps);
            flag(command, "wait", c.wait);
            flag(command, "wait-timeout", c.wait_timeout.is_some());
            flag(command, "watch", c.watch);
            flag(command, "yes", c.yes);
        }
        Some(ComposeCommand::Volumes(c)) => {
            let command = "volumes";
            flag(command, "format", c.format.is_some());
            flag(command, "quiet", c.quiet);
        }
        Some(ComposeCommand::Wait(c)) => {
            let command = "wait";
            flag(command, "down-project", c.down_project);
        }
        Some(ComposeCommand::Pause(_) | ComposeCommand::Top(_) | ComposeCommand::Unpause(_))
        | None => {}
    }
    set
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compose::support::{self, Support};
    use clap::{CommandFactory, Parser};

    /// The compose subcommand as clap builds it.
    fn compose() -> clap::Command {
        crate::cli::Cli::command()
            .find_subcommand("compose")
            .cloned()
            .expect("nspawn compose")
    }

    fn longs(command: &clap::Command) -> Vec<String> {
        command
            .get_arguments()
            .filter_map(|a| a.get_long().map(String::from))
            .collect()
    }

    /// What clap takes and what the table decides are the same options, for the
    /// commands nspawn compose has: the global ones and each subcommand's.
    #[test]
    fn the_command_line_is_the_table_s() {
        let compose = compose();
        let mut commands = vec![(String::new(), compose.clone())];
        for sub in compose.get_subcommands() {
            commands.push((sub.get_name().to_string(), sub.clone()));
        }
        let global = longs(&compose);
        for (name, command) in &commands {
            assert!(
                name.is_empty() || support::command(name).is_some(),
                "{name}: a command the table does not decide on"
            );
            for long in longs(command) {
                if long == "help" || (!name.is_empty() && global.contains(&long)) {
                    continue;
                }
                assert!(
                    support::option(name, &long).is_some(),
                    "compose {name} --{long}: not in the support table"
                );
            }
            for option in support::OPTIONS.iter().filter(|o| o.command == name) {
                if matches!(option.support, Support::Refused(_)) {
                    continue;
                }
                assert!(
                    longs(command).iter().any(|l| l == option.name),
                    "compose {name} --{}: decided {:?} but not on the command line",
                    option.name,
                    option.support
                );
            }
        }
    }

    #[test]
    fn refused_build_options_reach_the_table() {
        for flag in [
            "--check",
            "--print",
            "--sbom=true",
            "--provenance=mode=max",
            "--ssh=default",
        ] {
            let cli =
                crate::cli::Cli::try_parse_from(["nspawn", "compose", "build", flag]).unwrap();
            let crate::cli::Command::Compose(args) = cli.command else {
                unreachable!()
            };
            let name = flag.trim_start_matches("--").split('=').next().unwrap();
            assert!(requested(&args).contains(&("build", name)), "{flag}");
            assert!(
                matches!(support::option("build", name), Some(Support::Refused(_))),
                "{flag}"
            );
        }
    }

    #[test]
    fn requested_options_are_named_as_in_the_table() {
        let cli = crate::cli::Cli::try_parse_from([
            "nspawn",
            "compose",
            "--compatibility",
            "-f",
            "x.yml",
            "config",
            "--hash",
            "*",
            "-q",
        ])
        .unwrap();
        let crate::cli::Command::Compose(args) = cli.command else {
            unreachable!()
        };
        let set = requested(&args);
        assert!(set.contains(&("", "compatibility")));
        assert!(set.contains(&("config", "hash")));
        for (command, name) in set {
            assert!(
                support::option(command, name).is_some(),
                "{command} --{name}"
            );
        }
    }
}

#[derive(Args, Clone, Debug, Default)]
pub struct ConfigArgs {
    /// Format the output. Values: [yaml | json].
    #[arg(long, value_name = "FORMAT")]
    pub format: Option<String>,
    /// Pin image tags to digests.
    #[arg(long)]
    pub resolve_image_digests: bool,
    /// Produces an override file with image digests.
    #[arg(long)]
    pub lock_image_digests: bool,
    /// Only validate the configuration, don't print anything.
    #[arg(short, long)]
    pub quiet: bool,
    /// Don't interpolate environment variables.
    #[arg(long)]
    pub no_interpolate: bool,
    /// Don't normalize compose model.
    #[arg(long)]
    pub no_normalize: bool,
    /// Don't resolve file paths.
    #[arg(long)]
    pub no_path_resolution: bool,
    /// Don't check model consistency - warning: may produce invalid Compose output.
    #[arg(long)]
    pub no_consistency: bool,
    /// Don't resolve service env files.
    #[arg(long)]
    pub no_env_resolution: bool,
    /// Print the service names, one per line.
    #[arg(long)]
    pub services: bool,
    /// Print the volume names, one per line.
    #[arg(long)]
    pub volumes: bool,
    /// Print the network names, one per line.
    #[arg(long)]
    pub networks: bool,
    /// Print the model names, one per line.
    #[arg(long)]
    pub models: bool,
    /// Print the profile names, one per line.
    #[arg(long)]
    pub profiles: bool,
    /// Print the image names, one per line.
    #[arg(long)]
    pub images: bool,
    /// Print the service config hash, one per line.
    #[arg(long, value_name = "SERVICES")]
    pub hash: Option<String>,
    /// Print model variables and default values.
    #[arg(long)]
    pub variables: bool,
    /// Print environment used for interpolation.
    #[arg(long)]
    pub environment: bool,
    /// Save to file (default to stdout).
    #[arg(short, long, value_name = "FILE")]
    pub output: Option<String>,
    /// Services to render.
    #[arg(value_name = "SERVICE")]
    pub service: Vec<String>,
}

#[derive(Args, Debug, Default)]
pub struct CpArgs {
    /// Include containers created by the run command.
    #[arg(long)]
    pub all: bool,
    /// Archive mode (copy all uid/gid information).
    #[arg(long, short = 'a')]
    pub archive: bool,
    /// Always follow symbol link in SRC_PATH.
    #[arg(long, short = 'L')]
    pub follow_link: bool,
    /// Index of the container if service has multiple replicas.
    #[arg(long, value_name = "N")]
    pub index: Option<i64>,
    #[arg(value_name = "SRC")]
    pub source: Option<String>,
    #[arg(value_name = "DEST")]
    pub destination: Option<String>,
    /// More than cp takes, for the error to say how many were given.
    #[arg(hide = true)]
    pub extra: Vec<String>,
}

#[derive(Args, Debug, Default)]
pub struct BuildArgs {
    /// Set build-time variables for services.
    #[arg(long, value_name = "BUILD_ARG")]
    pub build_arg: Vec<String>,
    /// Set builder to use.
    #[arg(long, value_name = "BUILDER")]
    pub builder: Option<String>,
    /// Check build configuration.
    #[arg(long, hide = true)]
    pub check: bool,
    /// Compress the build context using gzip. DEPRECATED
    #[arg(long, hide = true, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    pub compress: Option<bool>,
    /// Always remove intermediate containers. DEPRECATED
    #[arg(long, hide = true, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    pub force_rm: Option<bool>,
    /// Set memory limit for the build container. Not supported by BuildKit.
    #[arg(long, short = 'm', value_name = "MEMORY")]
    pub memory: Option<String>,
    /// Do not use cache when building the image.
    #[arg(long)]
    pub no_cache: bool,
    /// Do not remove intermediate containers after a successful build. DEPRECATED
    #[arg(long, hide = true)]
    pub no_rm: bool,
    /// Build images in parallel. DEPRECATED
    #[arg(long, hide = true, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    pub parallel: Option<bool>,
    /// Print equivalent bake file.
    #[arg(long, hide = true)]
    pub print: bool,
    /// Set type of ui output (auto, tty, plain, json, quiet)
    #[arg(long, hide = true, value_name = "PROGRESS")]
    pub progress: Option<String>,
    /// Add a provenance attestation.
    #[arg(long, hide = true, value_name = "PROVENANCE")]
    pub provenance: Option<String>,
    /// Always attempt to pull a newer version of the image.
    #[arg(long)]
    pub pull: bool,
    /// Push service images.
    #[arg(long)]
    pub push: bool,
    /// Suppress the build output.
    #[arg(long, short = 'q')]
    pub quiet: bool,
    /// Add a SBOM attestation.
    #[arg(long, hide = true, value_name = "SBOM")]
    pub sbom: Option<String>,
    /// Set SSH authentications used when building service images.
    #[arg(long, hide = true, value_name = "SSH")]
    pub ssh: Option<String>,
    /// Also build dependencies (transitively).
    #[arg(long)]
    pub with_dependencies: bool,
    #[arg(value_name = "SERVICE")]
    pub service: Vec<String>,
}

#[derive(Args, Debug, Default)]
pub struct CreateArgs {
    /// Build images before starting containers.
    #[arg(long)]
    pub build: bool,
    /// Recreate containers even if their configuration and image haven't changed.
    #[arg(long)]
    pub force_recreate: bool,
    /// Don't build an image, even if it's policy.
    #[arg(long)]
    pub no_build: bool,
    /// If containers already exist, don't recreate them. Incompatible with --force-recreate.
    #[arg(long)]
    pub no_recreate: bool,
    /// Pull image before running ("always"|"missing"|"never"|"build").
    #[arg(long, value_name = "PULL")]
    pub pull: Option<String>,
    /// Pull without printing progress information.
    #[arg(long)]
    pub quiet_pull: bool,
    /// Remove containers for services not defined in the Compose file.
    #[arg(long)]
    pub remove_orphans: bool,
    /// Scale SERVICE to NUM instances. Overrides the `scale` setting in the Compose file if present.
    #[arg(long, value_name = "SCALE")]
    pub scale: Vec<String>,
    /// Assume "yes" as answer to all prompts and run non-interactively.
    #[arg(long, short = 'y')]
    pub yes: bool,
    #[arg(value_name = "SERVICE")]
    pub service: Vec<String>,
}

#[derive(Args, Debug, Default)]
pub struct DownArgs {
    /// Remove containers for services not defined in the Compose file.
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    pub remove_orphans: Option<bool>,
    /// Remove images used by services. "local" remove only images that don't have a custom tag ("local"|"all").
    #[arg(long, value_name = "RMI")]
    pub rmi: Option<String>,
    /// Specify a shutdown timeout in seconds.
    #[arg(long, short = 't', value_name = "N")]
    pub timeout: Option<i64>,
    /// Remove named volumes declared in the "volumes" section of the Compose file and anonymous volumes attached to containers.
    #[arg(long, short = 'v')]
    pub volumes: bool,
    #[arg(value_name = "SERVICE")]
    pub service: Vec<String>,
}

#[derive(Args, Debug, Default)]
pub struct EventsArgs {
    /// Output events as a stream of json objects.
    #[arg(long)]
    pub json: bool,
    /// Show all events created since timestamp.
    #[arg(long, value_name = "SINCE")]
    pub since: Option<String>,
    /// Stream events until this timestamp.
    #[arg(long, value_name = "UNTIL")]
    pub until: Option<String>,
    #[arg(value_name = "SERVICE")]
    pub service: Vec<String>,
}

#[derive(Args, Debug, Default)]
pub struct ExecArgs {
    /// Detached mode: Run command in the background.
    #[arg(long, short = 'd')]
    pub detach: bool,
    /// Set environment variables.
    #[arg(long, short = 'e', value_name = "ENV")]
    pub env: Vec<String>,
    /// Index of the container if service has multiple replicas.
    #[arg(long, value_name = "N")]
    pub index: Option<i64>,
    /// Keep STDIN open even if not attached.
    #[arg(long, short = 'i', hide = true, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    pub interactive: Option<bool>,
    /// Disable pseudo-TTY allocation. By default 'docker compose exec' allocates a TTY.
    #[arg(long, short = 'T', num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    pub no_tty: Option<bool>,
    /// Give extended privileges to the process.
    #[arg(long)]
    pub privileged: bool,
    /// Allocate a pseudo-TTY.
    #[arg(long, short = 't', hide = true, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    pub tty: Option<bool>,
    /// Run the command as this user.
    #[arg(long, short = 'u', value_name = "USER")]
    pub user: Option<String>,
    /// Path to workdir directory for this command.
    #[arg(long, short = 'w', value_name = "WORKDIR")]
    pub workdir: Option<String>,
    #[arg(value_name = "SERVICE")]
    pub service: Option<String>,
    #[arg(
        value_name = "COMMAND",
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    pub command: Vec<String>,
}

#[derive(Args, Debug, Default)]
pub struct ImagesArgs {
    /// Format the output. Values: [table | json].
    #[arg(long, value_name = "FORMAT")]
    pub format: Option<String>,
    /// Only display IDs.
    #[arg(long, short = 'q')]
    pub quiet: bool,
    #[arg(value_name = "SERVICE")]
    pub service: Vec<String>,
}

#[derive(Args, Debug, Default)]
pub struct KillArgs {
    /// Remove containers for services not defined in the Compose file.
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    pub remove_orphans: Option<bool>,
    /// SIGNAL to send to the container.
    #[arg(long, short = 's', value_name = "SIGNAL")]
    pub signal: Option<String>,
    #[arg(value_name = "SERVICE")]
    pub service: Vec<String>,
}

#[derive(Args, Debug, Default)]
pub struct LogsArgs {
    /// Follow log output.
    #[arg(long, short = 'f')]
    pub follow: bool,
    /// index of the container if service has multiple replicas.
    #[arg(long, value_name = "N")]
    pub index: Option<i64>,
    /// Produce monochrome output.
    #[arg(long)]
    pub no_color: bool,
    /// Don't print prefix in logs.
    #[arg(long)]
    pub no_log_prefix: bool,
    /// Show logs since timestamp (e.g. 2013-01-02T13:23:37Z) or relative (e.g. 42m for 42 minutes).
    #[arg(long, value_name = "SINCE")]
    pub since: Option<String>,
    /// Number of lines to show from the end of the logs for each container.
    #[arg(long, short = 'n', value_name = "TAIL")]
    pub tail: Option<String>,
    /// Show timestamps.
    #[arg(long, short = 't')]
    pub timestamps: bool,
    /// Show logs before a timestamp (e.g. 2013-01-02T13:23:37Z) or relative (e.g. 42m for 42 minutes).
    #[arg(long, value_name = "UNTIL")]
    pub until: Option<String>,
    #[arg(value_name = "SERVICE")]
    pub service: Vec<String>,
}

#[derive(Args, Debug, Default)]
pub struct LsArgs {
    /// Show all stopped Compose projects.
    #[arg(long, short = 'a')]
    pub all: bool,
    /// Filter output based on conditions provided.
    #[arg(long, value_name = "FILTER")]
    pub filter: Vec<String>,
    /// Format the output. Values: [table | json].
    #[arg(long, value_name = "FORMAT")]
    pub format: Option<String>,
    /// Only display project names.
    #[arg(long, short = 'q')]
    pub quiet: bool,
    #[arg(value_name = "ARGS", hide = true)]
    pub args: Vec<String>,
}

#[derive(Args, Debug, Default)]
pub struct PauseArgs {
    #[arg(value_name = "SERVICE")]
    pub service: Vec<String>,
}

#[derive(Args, Debug, Default)]
pub struct PortArgs {
    /// Index of the container if service has multiple replicas.
    #[arg(long, value_name = "N")]
    pub index: Option<i64>,
    /// tcp or udp.
    #[arg(long, value_name = "PROTOCOL")]
    pub protocol: Option<String>,
    #[arg(value_name = "SERVICE")]
    pub service: Option<String>,
    #[arg(value_name = "PRIVATE_PORT")]
    pub private_port: Option<String>,
    /// More than port takes, for the error to say how many were given.
    #[arg(hide = true)]
    pub extra: Vec<String>,
}

#[derive(Args, Debug, Default)]
pub struct PsArgs {
    /// Show all stopped containers (including those created by the run command).
    #[arg(long, short = 'a')]
    pub all: bool,
    /// Filter services by a property (supported filters: status).
    #[arg(long, value_name = "FILTER")]
    pub filter: Option<String>,
    /// Format output using a custom template: 'table':            Print output in table format with column headers (default) 'table TEMPLATE':   Print output in table format using the given Go template 'json':             Print in JSON format 'TEMPLATE':         Print output using the given Go template. Refer to https://docs.docker.com/go/formatting/ for more information about formatting output with templates.
    #[arg(long, value_name = "FORMAT")]
    pub format: Option<String>,
    /// Don't truncate output.
    #[arg(long)]
    pub no_trunc: bool,
    /// Include orphaned services (not declared by project).
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    pub orphans: Option<bool>,
    /// Only display IDs.
    #[arg(long, short = 'q')]
    pub quiet: bool,
    /// Display services.
    #[arg(long)]
    pub services: bool,
    /// Filter services by status. Values: [paused | restarting | removing | running | dead | created | exited].
    #[arg(long, value_name = "STATUS")]
    pub status: Vec<String>,
    #[arg(value_name = "SERVICE")]
    pub service: Vec<String>,
}

#[derive(Args, Debug, Default)]
pub struct PullArgs {
    /// Ignore images that can be built.
    #[arg(long)]
    pub ignore_buildable: bool,
    /// Pull what it can and ignores images with pull failures.
    #[arg(long)]
    pub ignore_pull_failures: bool,
    /// Also pull services declared as dependencies.
    #[arg(long)]
    pub include_deps: bool,
    /// DEPRECATED disable parallel pulling.
    #[arg(long, hide = true, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    pub no_parallel: Option<bool>,
    /// DEPRECATED pull multiple images in parallel.
    #[arg(long, hide = true, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    pub parallel: Option<bool>,
    /// Apply pull policy ("missing"|"always").
    #[arg(long, value_name = "POLICY")]
    pub policy: Option<String>,
    /// Pull without printing progress information.
    #[arg(long, short = 'q')]
    pub quiet: bool,
    #[arg(value_name = "SERVICE")]
    pub service: Vec<String>,
}

#[derive(Args, Debug, Default)]
pub struct RestartArgs {
    /// Don't restart dependent services.
    #[arg(long)]
    pub no_deps: bool,
    /// Specify a shutdown timeout in seconds.
    #[arg(long, short = 't', value_name = "N")]
    pub timeout: Option<i64>,
    #[arg(value_name = "SERVICE")]
    pub service: Vec<String>,
}

#[derive(Args, Debug, Default)]
pub struct RmArgs {
    /// Deprecated - no effect.
    #[arg(long, short = 'a', hide = true)]
    pub all: bool,
    /// Don't ask to confirm removal.
    #[arg(long, short = 'f')]
    pub force: bool,
    /// Stop the containers, if required, before removing.
    #[arg(long, short = 's')]
    pub stop: bool,
    /// Remove any anonymous volumes attached to containers.
    #[arg(long, short = 'v')]
    pub volumes: bool,
    #[arg(value_name = "SERVICE")]
    pub service: Vec<String>,
}

#[derive(Args, Debug, Default)]
pub struct RunArgs {
    /// Build image before starting container.
    #[arg(long)]
    pub build: bool,
    /// Add Linux capabilities.
    #[arg(long, value_name = "CAP_ADD")]
    pub cap_add: Vec<String>,
    /// Drop Linux capabilities.
    #[arg(long, value_name = "CAP_DROP")]
    pub cap_drop: Vec<String>,
    /// Run container in background and print container ID.
    #[arg(long, short = 'd')]
    pub detach: bool,
    /// Override the entrypoint of the image.
    #[arg(long, value_name = "ENTRYPOINT")]
    pub entrypoint: Option<String>,
    /// Set environment variables.
    #[arg(long, short = 'e', value_name = "ENV")]
    pub env: Vec<String>,
    /// Set environment variables from file.
    #[arg(long, value_name = "ENV_FROM_FILE")]
    pub env_from_file: Vec<String>,
    /// Keep STDIN open even if not attached.
    #[arg(long, short = 'i', num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    pub interactive: Option<bool>,
    /// Add or override a label.
    #[arg(long, short = 'l', value_name = "LABEL")]
    pub label: Vec<String>,
    /// Assign a name to the container.
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,
    /// Don't start linked services.
    #[arg(long)]
    pub no_deps: bool,
    /// Disable pseudo-TTY allocation (default: auto-detected).
    #[arg(long, short = 'T', num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    pub no_tty: Option<bool>,
    /// Publish a container's port(s) to the host.
    #[arg(long, short = 'p', value_name = "PUBLISH")]
    pub publish: Vec<String>,
    /// Pull image before running ("always"|"missing"|"never").
    #[arg(long, value_name = "PULL")]
    pub pull: Option<String>,
    /// Don't print anything to STDOUT.
    #[arg(long, short = 'q')]
    pub quiet: bool,
    /// Suppress progress output from the build process.
    #[arg(long)]
    pub quiet_build: bool,
    /// Pull without printing progress information.
    #[arg(long)]
    pub quiet_pull: bool,
    /// Remove containers for services not defined in the Compose file.
    #[arg(long)]
    pub remove_orphans: bool,
    /// Automatically remove the container when it exits.
    #[arg(long)]
    pub rm: bool,
    /// Run command with all service's ports enabled and mapped to the host.
    #[arg(long, short = 'P')]
    pub service_ports: bool,
    /// Allocate a pseudo-TTY.
    #[arg(long, short = 't', hide = true, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    pub tty: Option<bool>,
    /// Use the service's network useAliases in the network(s) the container connects to.
    #[arg(long)]
    pub use_aliases: bool,
    /// Run as specified username or uid.
    #[arg(long, short = 'u', value_name = "USER")]
    pub user: Option<String>,
    /// Bind mount a volume.
    #[arg(long, short = 'v', value_name = "VOLUME")]
    pub volume: Vec<String>,
    /// Working directory inside the container.
    #[arg(long, short = 'w', value_name = "WORKDIR")]
    pub workdir: Option<String>,
    #[arg(value_name = "SERVICE")]
    pub service: Option<String>,
    #[arg(
        value_name = "COMMAND",
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    pub command: Vec<String>,
}

#[derive(Args, Debug, Default)]
pub struct ScaleArgs {
    /// Don't start linked services.
    #[arg(long)]
    pub no_deps: bool,
    #[arg(value_name = "SERVICE=REPLICAS")]
    pub service: Vec<String>,
}

#[derive(Args, Debug, Default)]
pub struct StartArgs {
    /// Wait for services to be running|healthy. Implies detached mode.
    #[arg(long)]
    pub wait: bool,
    /// Maximum duration in seconds to wait for the project to be running|healthy.
    #[arg(long, value_name = "N", allow_negative_numbers = true)]
    pub wait_timeout: Option<i64>,
    #[arg(value_name = "SERVICE")]
    pub service: Vec<String>,
}

#[derive(Args, Debug, Default)]
pub struct StatsArgs {
    /// Show all containers (default shows just running).
    #[arg(long, short = 'a')]
    pub all: bool,
    /// Format output using a custom template: 'table':            Print output in table format with column headers (default) 'table TEMPLATE':   Print output in table format using the given Go template 'json':             Print in JSON format 'TEMPLATE':         Print output using the given Go template. Refer to https://docs.docker.com/engine/cli/formatting/ for more information about formatting output with templates.
    #[arg(long, value_name = "FORMAT")]
    pub format: Option<String>,
    /// Disable streaming stats and only pull the first result.
    #[arg(long)]
    pub no_stream: bool,
    /// Do not truncate output.
    #[arg(long)]
    pub no_trunc: bool,
    #[arg(value_name = "SERVICE")]
    pub service: Vec<String>,
}

#[derive(Args, Debug, Default)]
pub struct StopArgs {
    /// Specify a shutdown timeout in seconds.
    #[arg(long, short = 't', value_name = "N")]
    pub timeout: Option<i64>,
    #[arg(value_name = "SERVICE")]
    pub service: Vec<String>,
}

#[derive(Args, Debug, Default)]
pub struct TopArgs {
    #[arg(value_name = "SERVICE")]
    pub service: Vec<String>,
}

#[derive(Args, Debug, Default)]
pub struct UnpauseArgs {
    #[arg(value_name = "SERVICE")]
    pub service: Vec<String>,
}

#[derive(Args, Debug, Default)]
pub struct UpArgs {
    /// Stops all containers if any container was stopped. Incompatible with -d.
    #[arg(long)]
    pub abort_on_container_exit: bool,
    /// Stops all containers if any container exited with failure. Incompatible with -d.
    #[arg(long)]
    pub abort_on_container_failure: bool,
    /// Recreate dependent containers. Incompatible with --no-recreate.
    #[arg(long)]
    pub always_recreate_deps: bool,
    /// Restrict attaching to the specified services. Incompatible with --attach-dependencies.
    #[arg(long, value_name = "ATTACH")]
    pub attach: Vec<String>,
    /// Automatically attach to log output of dependent services.
    #[arg(long)]
    pub attach_dependencies: bool,
    /// Build images before starting containers.
    #[arg(long)]
    pub build: bool,
    /// Detached mode: Run containers in the background.
    #[arg(long, short = 'd')]
    pub detach: bool,
    /// Return the exit code of the selected service container. Implies --abort-on-container-exit.
    #[arg(long, value_name = "EXIT_CODE_FROM")]
    pub exit_code_from: Option<String>,
    /// Recreate containers even if their configuration and image haven't changed.
    #[arg(long)]
    pub force_recreate: bool,
    /// Enable interactive shortcuts when running attached. Incompatible with --detach. Can also be enable/disable by setting COMPOSE_MENU environment var.
    #[arg(long)]
    pub menu: bool,
    /// Do not attach (stream logs) to the specified services.
    #[arg(long, value_name = "NO_ATTACH")]
    pub no_attach: Vec<String>,
    /// Don't build an image, even if it's policy.
    #[arg(long)]
    pub no_build: bool,
    /// Produce monochrome output.
    #[arg(long)]
    pub no_color: bool,
    /// Don't start linked services.
    #[arg(long)]
    pub no_deps: bool,
    /// Don't print prefix in logs.
    #[arg(long)]
    pub no_log_prefix: bool,
    /// If containers already exist, don't recreate them. Incompatible with --force-recreate.
    #[arg(long)]
    pub no_recreate: bool,
    /// Don't start the services after creating them.
    #[arg(long)]
    pub no_start: bool,
    /// Pull image before running ("always"|"missing"|"never").
    #[arg(long, value_name = "PULL")]
    pub pull: Option<String>,
    /// Suppress the build output.
    #[arg(long)]
    pub quiet_build: bool,
    /// Pull without printing progress information.
    #[arg(long)]
    pub quiet_pull: bool,
    /// Remove containers for services not defined in the Compose file.
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    pub remove_orphans: Option<bool>,
    /// Recreate anonymous volumes instead of retrieving data from the previous containers.
    #[arg(long, short = 'V')]
    pub renew_anon_volumes: bool,
    /// Scale SERVICE to NUM instances. Overrides the `scale` setting in the Compose file if present.
    #[arg(long, value_name = "SCALE")]
    pub scale: Vec<String>,
    /// Use this timeout in seconds for container shutdown when attached or when containers are already running.
    #[arg(long, short = 't', value_name = "N")]
    pub timeout: Option<i64>,
    /// Show timestamps.
    #[arg(long)]
    pub timestamps: bool,
    /// Wait for services to be running|healthy. Implies detached mode.
    #[arg(long)]
    pub wait: bool,
    /// Maximum duration in seconds to wait for the project to be running|healthy.
    #[arg(long, value_name = "N", allow_negative_numbers = true)]
    pub wait_timeout: Option<i64>,
    /// Watch source code and rebuild/refresh containers when files are updated.
    #[arg(long, short = 'w')]
    pub watch: bool,
    /// Assume "yes" as answer to all prompts and run non-interactively.
    #[arg(long, short = 'y')]
    pub yes: bool,
    #[arg(value_name = "SERVICE")]
    pub service: Vec<String>,
}

#[derive(Args, Debug, Default)]
pub struct VolumesArgs {
    /// Format output using a custom template: 'table':            Print output in table format with column headers (default) 'table TEMPLATE':   Print output in table format using the given Go template 'json':             Print in JSON format 'TEMPLATE':         Print output using the given Go template. Refer to https://docs.docker.com/go/formatting/ for more information about formatting output with templates.
    #[arg(long, value_name = "FORMAT")]
    pub format: Option<String>,
    /// Only display volume names.
    #[arg(long, short = 'q')]
    pub quiet: bool,
    #[arg(value_name = "SERVICE")]
    pub service: Vec<String>,
}

#[derive(Args, Debug, Default)]
pub struct WaitArgs {
    /// Drops project when the first container stops.
    #[arg(long)]
    pub down_project: bool,
    #[arg(value_name = "SERVICE")]
    pub service: Vec<String>,
}
