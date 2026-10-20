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

/// The options of docker compose a command line sets, as the support table names
/// them: (command, option), "" for docker compose's own.
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
        None => {}
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
    /// commands nspawn compose has: docker compose's own and each subcommand's.
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

#[derive(Args, Debug, Default)]
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
