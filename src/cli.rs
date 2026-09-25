use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

pub use crate::backend::BackendChoice;
pub use crate::oci::ModeChoice;
pub use crate::search::SearchSource;

/// Docker-like management of systemd-nspawn machines.
#[derive(Parser, Debug)]
#[command(name = "nspawn", version, about, arg_required_else_help = true)]
pub struct Cli {
    /// Registry (hub) to use for image references without a host part.
    #[arg(long, global = true, env = "NSPAWN_REGISTRY")]
    pub registry: Option<String>,

    /// Extra CA certificate (PEM) to trust when talking to the registry.
    #[arg(long, global = true, env = "NSPAWN_CA_CERT", value_name = "FILE")]
    pub ca_cert: Option<PathBuf>,

    /// Configuration file.
    #[arg(long, global = true, env = "NSPAWN_CONFIG", value_name = "FILE")]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Query the hub (OCI registry).
    Hub(HubArgs),
    /// Find images on the hub and on Docker Hub, like docker search.
    Search(SearchArgs),
    /// Keep credentials for a registry (the hub by default), like docker login.
    Login(LoginArgs),
    /// Forget the credentials of a registry.
    Logout(LogoutArgs),
    /// Download an image from the hub and make it available to machinectl.
    Pull(PullArgs),
    /// Make a machine from an image and start it, like docker run -d: a local image with
    /// that reference is reused, the hub is asked otherwise.
    Run(RunArgs),
    /// Build an image with mkosi and make it available locally, ready to push.
    Build(BuildArgs),
    /// Make another machine from a local image, like docker create (no registry needed).
    Create(CreateArgs),
    /// Upload a local image to the hub.
    Push(PushArgs),
    /// Manage local images.
    Images(ImagesArgs),
    /// Manage running machines.
    Machines(MachinesArgs),
    /// List running machines, like docker ps (same as machines ls).
    Ps(PsArgs),
    /// Everything nspawn knows about machines or images, as JSON, like docker inspect.
    Inspect(InspectArgs),
    /// Boot an image as a machine.
    Start(StartArgs),
    /// Power off a running machine.
    Stop(StopArgs),
    /// Send a signal to running machines, like docker kill (SIGKILL stops them for good).
    Kill(KillArgs),
    /// Change the restart policy and limits of machines, like docker update; a running
    /// machine gets the limits at once.
    Update(UpdateArgs),
    /// What running machines use: CPU, memory, network, disk, processes, like docker stats.
    Stats(StatsArgs),
    /// What happens to machines, networks and volumes, as it happens, like docker events.
    Events(EventsArgs),
    /// Remove machines and what they alone use, like docker rm (same as images rm).
    Rm(RmArgs),
    /// Run a command inside a running machine.
    Exec(ExecArgs),
    /// Open an interactive shell inside a running machine.
    Shell(ShellArgs),
    /// Show what a machine printed, like docker logs.
    Logs(LogsArgs),
    /// Copy files between the host and a machine, like docker cp.
    Cp(CpArgs),
    /// The bridge network shared by the machines.
    Network(NetworkArgs),
    /// Manage named volumes (-v NAME:/path), like docker volume.
    Volume(VolumeArgs),
    /// Serve org.nspawn on the system bus (started by the bus; see --install).
    Daemon(DaemonArgs),
    /// run --rm: removes a machine once its unit is down after that run.
    #[command(hide = true)]
    RemoveAfterExit { name: String, invocation: String },
    /// From a transient unit: runs a machine's health probes until it is gone.
    #[command(hide = true)]
    HealthRun { name: String },
    /// ExecStart of an app machine: runs systemd-nspawn with the terminal or input an
    /// attached run hands over, or as it is.
    #[command(hide = true)]
    AttachExec {
        name: String,
        #[arg(last = true, required = true)]
        argv: Vec<std::ffi::OsString>,
    },
    /// Print the completions for a shell, which the packages install for you.
    Completions(CompletionsArgs),
    /// Print the manual page in roff, as the packages ship it.
    #[command(hide = true)]
    Manpage,
}

#[derive(Args, Debug)]
pub struct CompletionsArgs {
    /// Shell to write the completions for.
    #[arg(value_enum)]
    pub shell: clap_complete::Shell,
}

#[derive(Args, Debug)]
pub struct DaemonArgs {
    /// Write the bus policy, the polkit actions, the activation file and the unit that
    /// make the bus start this binary on demand, then return.
    #[arg(long)]
    pub install: bool,
    /// Exit after this many seconds without a call or a job; 0 keeps serving.
    #[arg(long, default_value_t = 60, value_name = "SECONDS")]
    pub idle_exit: u64,
}

#[derive(Args, Debug)]
pub struct NetworkCreateArgs {
    /// Network name: letters, digits, '_' and '-'.
    pub name: String,
    /// IPv4 subnet in CIDR notation (default: the next free /24 of network_pool in
    /// nspawn.toml, 10.99.0.0/16 unless set).
    #[arg(long, value_name = "CIDR")]
    pub subnet: Option<String>,
    /// Keep the machines from anything beyond the network: no way out, no published
    /// ports; they reach each other and the host.
    #[arg(long)]
    pub internal: bool,
    /// Label the network, KEY=VALUE, like docker network create --label. Repeatable.
    #[arg(long, value_name = "KEY=VALUE")]
    pub label: Vec<String>,
}

#[derive(Args, Debug)]
pub struct NetworkArgs {
    #[command(subcommand)]
    pub command: NetworkCommand,
}

#[derive(Subcommand, Debug)]
pub enum NetworkCommand {
    /// Bring up every network's bridge with its NAT rules (start does it for its
    /// machine's network too; useful at boot).
    Up,
    /// List the networks: the default one (bridge) and those made with network create.
    #[command(alias = "list")]
    Ls(OutputArgs),
    /// Make a network of its own for some machines, like docker network create: they
    /// reach each other by name, and machines on other networks do not reach them.
    Create(NetworkCreateArgs),
    /// The networks with their machines, addresses and published ports, as JSON.
    Inspect {
        /// Network names ("bridge" for the default one).
        #[arg(required = true)]
        names: Vec<String>,
    },
    /// Remove networks no machine uses, with their bridges and rules.
    #[command(alias = "remove")]
    Rm {
        /// Network names.
        #[arg(required = true)]
        names: Vec<String>,
    },
    /// Remove every network no machine uses.
    Prune {
        /// Do not ask.
        #[arg(long, short = 'f')]
        force: bool,
    },
    /// Unit hook (ExecStartPre): prepare a machine's network before it starts.
    #[command(hide = true)]
    Prepare { name: String },
    /// Unit hook (ExecStartPost): publish a machine's ports once it runs.
    #[command(hide = true)]
    Publish { name: String },
    /// Unit hook (ExecStopPost): drop what a machine's network left behind.
    #[command(hide = true)]
    Release { name: String },
}

#[derive(Args, Debug)]
pub struct VolumeArgs {
    #[command(subcommand)]
    pub command: VolumeCommand,
}

#[derive(Subcommand, Debug)]
pub enum VolumeCommand {
    /// List named volumes with the machines that use them.
    #[command(alias = "list")]
    Ls(OutputArgs),
    /// Make a named volume ahead of its first use (start makes it otherwise).
    Create {
        /// Volume name: letters, digits, _ . and -.
        name: String,
    },
    /// Remove named volumes no machine uses.
    Rm {
        /// Volume names.
        #[arg(required = true)]
        names: Vec<String>,
    },
    /// Remove every named volume no machine uses, once a yes comes on standard input.
    Prune {
        /// Do not ask first (without it, nothing is removed when no yes comes).
        #[arg(long, short = 'f')]
        force: bool,
    },
}

#[derive(Args, Debug)]
pub struct HubArgs {
    #[command(subcommand)]
    pub command: HubCommand,
}

#[derive(Subcommand, Debug)]
pub enum HubCommand {
    /// List the repositories of the hub, with their tags.
    #[command(alias = "list")]
    Ls(HubLsArgs),
    /// List the tags of one repository.
    Tags(HubTagsArgs),
}

#[derive(Args, Debug)]
pub struct HubLsArgs {
    /// Only show repositories whose name contains this text.
    pub filter: Option<String>,
    /// Do not query the tags of every repository (faster on big hubs).
    #[arg(long)]
    pub no_tags: bool,
}

#[derive(Args, Debug)]
pub struct HubTagsArgs {
    /// Repository name, for example "fedora".
    pub repository: String,
}

#[derive(Args, Debug)]
pub struct LoginArgs {
    /// Registry host, for example docker.io or hub.nspawn.org (default: the hub).
    pub registry: Option<String>,
    /// User name (asked for when missing).
    #[arg(long, short = 'u')]
    pub username: Option<String>,
    /// Read the password from standard input instead of the terminal.
    #[arg(long)]
    pub password_stdin: bool,
}

#[derive(Args, Debug)]
pub struct LogoutArgs {
    /// Registry host (default: the hub).
    pub registry: Option<String>,
}

#[derive(Args, Debug)]
pub struct SearchArgs {
    /// Text to look for in image names.
    pub term: String,
    /// Only one source instead of both.
    #[arg(long, value_enum)]
    pub source: Option<SearchSource>,
    /// Results per source.
    #[arg(long, short = 'n', default_value_t = 25)]
    pub limit: usize,
}

#[derive(Args, Debug)]
pub struct PullArgs {
    /// Image reference: [registry/]repository[:tag|@digest], for example fedora:44.
    pub reference: String,
    /// Local image name (default: derived from the reference, e.g. fedora-44).
    #[arg(long, short = 'n')]
    pub name: Option<String>,
    /// How to assemble the image on this host.
    #[arg(long, value_enum, default_value_t = BackendChoice::Auto)]
    pub backend: BackendChoice,
    /// Whether the image boots an init system or runs a single program.
    #[arg(long, value_enum, default_value_t = ModeChoice::Auto)]
    pub mode: ModeChoice,
    /// Replace an existing image with the same name.
    #[arg(long, short = 'f')]
    pub force: bool,
}

#[derive(Args, Debug)]
pub struct CreateArgs {
    /// Local image to start from: its name, or the reference it was pulled from.
    pub source: String,
    /// Name of the new machine.
    pub name: String,
    /// How to assemble it (default: like the source).
    #[arg(long, value_enum, default_value_t = BackendChoice::Auto)]
    pub backend: BackendChoice,
    /// Network of the new machine: bridge, veth, host, none or a network made with
    /// network create; repeatable to join several bridge networks, the first one primary
    /// (default: the source's kind; a network made with network create is not
    /// inherited).
    #[arg(long, value_name = "NETWORK")]
    pub network: Vec<String>,
    /// Another name for the machine on a network, like docker --network-alias: NAME on
    /// its primary network, or NETWORK=NAME. Repeatable.
    #[arg(long, value_name = "[NETWORK=]NAME")]
    pub network_alias: Vec<String>,
    /// Ports to publish on the host, like start -p.
    #[arg(long, short = 'p', value_name = "[IP:]HOST:CONTAINER[/udp]")]
    pub publish: Vec<String>,
    /// Replace an existing machine with the same name.
    #[arg(long, short = 'f')]
    pub force: bool,
    /// Replace the image's entrypoint; an empty string runs the arguments alone.
    #[arg(long, value_name = "PROGRAM")]
    pub entrypoint: Option<String>,
    /// Environment for the program, VAR=value or VAR (copied from here), like docker -e.
    #[arg(long, short = 'e', value_name = "VAR[=VALUE]")]
    pub env: Vec<String>,
    /// Mount a host directory or a named volume, SOURCE:TARGET[:ro], like docker -v.
    #[arg(long, short = 'v', value_name = "SOURCE:TARGET[:ro]")]
    pub volume: Vec<String>,
    /// Label the machine, KEY=VALUE, like docker --label; the image's own labels stay
    /// underneath. Repeatable and remembered; "none" forgets them.
    #[arg(long, short = 'l', value_name = "KEY=VALUE")]
    pub label: Vec<String>,
    /// Restart policy, like docker --restart: no, on-failure, always (also starts it at
    /// boot) or unless-stopped (like always, until nspawn stop). Remembered; applied at
    /// the next start.
    #[arg(long, value_enum, value_name = "POLICY")]
    pub restart: Option<crate::policy::Restart>,
    /// Memory limit of the whole machine, like docker -m: 512m, 2g, and as much swap
    /// again; 0 removes it. Remembered; applied at the next start.
    #[arg(long, short = 'm', value_name = "SIZE", value_parser = crate::policy::parse_memory)]
    pub memory: Option<u64>,
    /// CPU limit of the whole machine, like docker --cpus: 0.5, 2; 0 removes it.
    /// Remembered; applied at the next start.
    #[arg(long, value_name = "N", value_parser = crate::policy::parse_cpus)]
    pub cpus: Option<f64>,
    /// Most processes and threads the machine may have; 0 removes the limit.
    /// Remembered; applied at the next start.
    #[arg(long, value_name = "N")]
    pub pids_limit: Option<u64>,
    #[command(flatten)]
    pub health: HealthArgs,
    /// For app images: the arguments after -- replace the image's cmd and follow its
    /// entrypoint, as with docker.
    #[arg(last = true)]
    pub command: Vec<String>,
}

/// docker's --health-* flags, on top of the image's HEALTHCHECK; remembered.
#[derive(Args, Debug, Default, Clone)]
pub struct HealthArgs {
    /// Command that says whether the machine is healthy, run inside it through /bin/sh
    /// -c at every --health-interval, like docker --health-cmd: exit 0 is healthy.
    #[arg(long, value_name = "COMMAND")]
    pub health_cmd: Option<String>,
    /// Time between probes, like docker: 10s, 1m30s, 500ms (default 30s).
    #[arg(long, value_name = "DURATION", value_parser = crate::health::parse_duration)]
    pub health_interval: Option<u64>,
    /// Time a probe may take before it counts as failed (default 30s).
    #[arg(long, value_name = "DURATION", value_parser = crate::health::parse_duration)]
    pub health_timeout: Option<u64>,
    /// Consecutive failed probes that make the machine unhealthy (default 3).
    #[arg(long, value_name = "N")]
    pub health_retries: Option<u32>,
    /// Time after the start during which failed probes do not count (default 0).
    #[arg(long, value_name = "DURATION", value_parser = crate::health::parse_duration)]
    pub health_start_period: Option<u64>,
    /// Time between probes during the start period (default 5s).
    #[arg(long, value_name = "DURATION", value_parser = crate::health::parse_duration)]
    pub health_start_interval: Option<u64>,
    /// No probes, whatever the image says.
    #[arg(long, conflicts_with_all = ["health_cmd", "health_interval", "health_timeout", "health_retries", "health_start_period", "health_start_interval"])]
    pub no_healthcheck: bool,
}

#[derive(Args, Debug)]
pub struct BuildArgs {
    /// Directory with the mkosi configuration (mkosi.conf, mkosi.conf.d, ...).
    #[arg(default_value = ".")]
    pub directory: PathBuf,
    /// Reference for the result, for example myapp:1 or hub.example/team/app:2.
    #[arg(long, short = 't', required = true)]
    pub tag: String,
    /// Local image name (default: derived from the tag).
    #[arg(long, short = 'n')]
    pub name: Option<String>,
    /// Distribution to build (mkosi --distribution).
    #[arg(long, short = 'd')]
    pub distribution: Option<String>,
    /// Release to build (mkosi --release).
    #[arg(long, short = 'r')]
    pub release: Option<String>,
    /// mkosi profile to enable (repeatable).
    #[arg(long)]
    pub profile: Vec<String>,
    /// How to assemble the image on this host.
    #[arg(long, value_enum, default_value_t = BackendChoice::Auto)]
    pub backend: BackendChoice,
    /// Whether the image boots an init system or runs a single program.
    #[arg(long, value_enum, default_value_t = ModeChoice::Auto)]
    pub mode: ModeChoice,
    /// Replace an existing image with the same name.
    #[arg(long, short = 'f')]
    pub force: bool,
    /// Keep the mkosi output directory instead of deleting it after the import.
    #[arg(long)]
    pub keep_output: bool,
    /// Extra arguments passed to mkosi verbatim (after --).
    #[arg(last = true)]
    pub mkosi_args: Vec<String>,
}

#[derive(Args, Debug)]
pub struct PushArgs {
    /// Local image name, or the reference it was pulled from or built as.
    pub image: String,
    /// Push under a different reference than the one recorded for the image.
    #[arg(long)]
    pub to: Option<String>,
}

#[derive(Args, Debug)]
pub struct ImagesArgs {
    #[command(subcommand)]
    pub command: ImagesCommand,
}

#[derive(Subcommand, Debug)]
pub enum ImagesCommand {
    /// List local images.
    #[command(alias = "list")]
    Ls(OutputArgs),
    /// Remove local images (and the layers nobody uses any more).
    Rm(ImagesRmArgs),
}

#[derive(Args, Debug)]
pub struct CpArgs {
    /// What to copy: a local path, or MACHINE:PATH (a local path with a colon is
    /// written ./a:b). DIR/. copies the contents of DIR.
    #[arg(value_name = "SOURCE")]
    pub source: String,
    /// Where to: MACHINE:PATH, or a local path. An existing directory receives the source
    /// under its own name; otherwise the copy takes this name.
    #[arg(value_name = "DESTINATION")]
    pub destination: String,
}

#[derive(Args, Debug)]
pub struct RmArgs {
    /// Machine names.
    #[arg(required = true)]
    pub names: Vec<String>,
    /// Stop a running machine first (SIGKILL, like docker rm -f) instead of refusing.
    #[arg(long, short = 'f')]
    pub force: bool,
}

#[derive(Args, Debug)]
pub struct KillArgs {
    /// Machine names.
    #[arg(required = true)]
    pub names: Vec<String>,
    /// Signal to send: a name (KILL, SIGHUP, RTMIN+3) or a number. SIGKILL stops the
    /// machine like stop --force; other signals go to an app's program or a booted
    /// machine's init, and a machine they end is restarted by its policy, unless the
    /// signal was its stop signal.
    #[arg(long, short = 's', default_value = "KILL")]
    pub signal: String,
}

#[derive(Args, Debug)]
pub struct UpdateArgs {
    /// Machine names.
    #[arg(required = true)]
    pub names: Vec<String>,
    /// Restart policy: no, on-failure, always (also starts it at boot) or unless-stopped.
    #[arg(long, value_enum, value_name = "POLICY")]
    pub restart: Option<crate::policy::Restart>,
    /// Memory limit of the whole machine: 512m, 2g, and as much swap again; 0 removes it.
    #[arg(long, short = 'm', value_name = "SIZE", value_parser = crate::policy::parse_memory)]
    pub memory: Option<u64>,
    /// CPU limit of the whole machine: 0.5, 2; 0 removes it.
    #[arg(long, value_name = "N", value_parser = crate::policy::parse_cpus)]
    pub cpus: Option<f64>,
    /// Most processes and threads the machine may have; 0 removes the limit.
    #[arg(long, value_name = "N")]
    pub pids_limit: Option<u64>,
    #[command(flatten)]
    pub health: HealthArgs,
}

#[derive(Args, Debug)]
pub struct StatsArgs {
    /// Machine names (every running machine when none is given).
    pub names: Vec<String>,
    /// Print one table and return instead of drawing a new one every second.
    #[arg(long)]
    pub no_stream: bool,
    /// One JSON object per machine and sample.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct EventsArgs {
    /// Show events since this time (journalctl's syntax: "2026-09-24 10:00", "-1h",
    /// "today"); without it, only new ones.
    #[arg(long, value_name = "WHEN", allow_hyphen_values = true)]
    pub since: Option<String>,
    /// Stop at this time instead of waiting for new events (with --since).
    #[arg(
        long,
        value_name = "WHEN",
        allow_hyphen_values = true,
        requires = "since"
    )]
    pub until: Option<String>,
    /// Only matching events: name=NAME, type=machine|network|volume, event=ACTION,
    /// label=KEY or label=KEY=VALUE. Repeatable: the same key matches either value,
    /// different keys must all match.
    #[arg(long = "filter", short = 'f', value_name = "KEY=VALUE")]
    pub filters: Vec<String>,
    /// One JSON object per event.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct ImagesRmArgs {
    /// Image names.
    #[arg(required = true)]
    pub names: Vec<String>,
}

#[derive(Args, Debug)]
pub struct MachinesArgs {
    #[command(subcommand)]
    pub command: MachinesCommand,
}

#[derive(Subcommand, Debug)]
pub enum MachinesCommand {
    /// List running machines.
    #[command(alias = "list")]
    Ls(PsArgs),
}

#[derive(Args, Debug, Default)]
pub struct PsArgs {
    /// Also list nspawn images that are not running.
    #[arg(long, short = 'a')]
    pub all: bool,
    #[command(flatten)]
    pub output: OutputArgs,
}

/// How a listing is printed.
#[derive(Args, Debug, Default, Clone, Copy)]
pub struct OutputArgs {
    /// Print the service's answer as JSON instead of a table, for scripts.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct InspectArgs {
    /// Machine or image names.
    #[arg(required = true)]
    pub names: Vec<String>,
}

#[derive(Args, Debug)]
pub struct StartArgs {
    /// Image name.
    pub name: String,
    #[command(flatten)]
    pub options: StartOptions,
    /// Forget the remembered entrypoint and arguments and run the image's own again.
    #[arg(long)]
    pub image_command: bool,
    /// For app images: the arguments after -- replace the image's cmd and follow its
    /// entrypoint, as with docker. Remembered for later starts.
    #[arg(last = true)]
    pub command: Vec<String>,
}

/// When `run` asks the registry.
#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum PullPolicy {
    /// Only when no local image has the reference.
    Missing,
    /// Every time, for the image the tag names now.
    Always,
    /// Never: a local image with the reference, or nothing.
    Never,
}

#[derive(Args, Debug)]
pub struct RunArgs {
    /// Image reference: [registry/]repository[:tag|@digest], for example nginx:1.27.
    pub reference: String,
    /// Name of the machine (default: derived from the reference, e.g. nginx-1.27).
    #[arg(long, short = 'n')]
    pub name: Option<String>,
    /// When to ask the registry: missing (a local image with the reference is reused),
    /// always or never.
    #[arg(long, value_enum, default_value_t = PullPolicy::Missing)]
    pub pull: PullPolicy,
    /// How to assemble the machine on this host.
    #[arg(long, value_enum, default_value_t = BackendChoice::Auto)]
    pub backend: BackendChoice,
    /// Whether the image boots an init system or runs a single program; a mode other
    /// than auto always pulls.
    #[arg(long, value_enum, default_value_t = ModeChoice::Auto)]
    pub mode: ModeChoice,
    /// Make the machine anew when one of that name exists (it must be stopped).
    #[arg(long, short = 'f')]
    pub force: bool,
    /// Start it in the background and return, like docker run -d; without it the
    /// output follows until the machine ends and run exits with its exit code.
    #[arg(long, short = 'd')]
    pub detach: bool,
    /// Remove the machine when it ends (named volumes stay), like docker run --rm.
    #[arg(long)]
    pub rm: bool,
    /// Give the program of an app this standard input, like docker run -i.
    #[arg(long, short = 'i')]
    pub interactive: bool,
    /// Give the program of an app a terminal, like docker run -t; -it on a booted
    /// image opens a shell once it is up, and leaving it powers the machine off.
    #[arg(long, short = 't')]
    pub tty: bool,
    #[command(flatten)]
    pub options: StartOptions,
    /// For app images, as with docker run: what follows the image replaces its cmd
    /// and follows its entrypoint (run -it alpine sh); after -- as well. Remembered for
    /// later starts.
    #[arg(trailing_var_arg = true, value_name = "COMMAND")]
    pub command: Vec<String>,
}

/// What `start` and `run` give a machine; remembered for its next starts.
#[derive(Args, Debug)]
pub struct StartOptions {
    /// Do not wait for a booted machine's init to be up before returning (its
    /// registration is still awaited so that ports and firewall rules can be applied).
    #[arg(long = "no-wait", action = clap::ArgAction::SetFalse)]
    pub wait: bool,
    /// Network of the machine, remembered for the image: bridge (the default network),
    /// veth (systemd-networkd on the host, booted images), host (the host's own
    /// network), none (no network) or the name of a network made with network create;
    /// repeatable to join several bridge networks, the first one primary.
    #[arg(long, value_name = "NETWORK")]
    pub network: Vec<String>,
    /// Another name for the machine on a network, like docker --network-alias: NAME on
    /// its primary network, or NETWORK=NAME. Repeatable and remembered; "none" forgets
    /// them.
    #[arg(long, value_name = "[NETWORK=]NAME")]
    pub network_alias: Vec<String>,
    /// Publish a port on the host, like docker -p: HOST:CONTAINER[/udp], on one address
    /// of the host with IP:HOST:CONTAINER, several with a range on both sides
    /// (8000-8010:8000-8010). Repeatable and remembered for the image; "none" forgets
    /// them all.
    #[arg(long, short = 'p', value_name = "[IP:]HOST:CONTAINER[/udp]")]
    pub publish: Vec<String>,
    /// Replace the image's entrypoint; an empty string runs the arguments alone.
    #[arg(long, value_name = "PROGRAM")]
    pub entrypoint: Option<String>,
    /// Environment for the program, VAR=value or VAR (copied from here), like docker -e.
    /// Repeatable and remembered; "none" forgets them.
    #[arg(long, short = 'e', value_name = "VAR[=VALUE]")]
    pub env: Vec<String>,
    /// Mount a host directory or a named volume, SOURCE:TARGET[:ro], like docker -v.
    /// Repeatable and remembered; "none" forgets them.
    #[arg(long, short = 'v', value_name = "SOURCE:TARGET[:ro]")]
    pub volume: Vec<String>,
    /// Label the machine, KEY=VALUE, like docker --label; the image's own labels stay
    /// underneath. Repeatable and remembered; "none" forgets them.
    #[arg(long, short = 'l', value_name = "KEY=VALUE")]
    pub label: Vec<String>,
    /// Restart policy, like docker --restart: no, on-failure, always (also starts it at
    /// boot) or unless-stopped (like always, until nspawn stop). Remembered; applied at
    /// the next start.
    #[arg(long, value_enum, value_name = "POLICY")]
    pub restart: Option<crate::policy::Restart>,
    /// Memory limit of the whole machine, like docker -m: 512m, 2g, and as much swap
    /// again; 0 removes it. Remembered; applied at the next start.
    #[arg(long, short = 'm', value_name = "SIZE", value_parser = crate::policy::parse_memory)]
    pub memory: Option<u64>,
    /// CPU limit of the whole machine, like docker --cpus: 0.5, 2; 0 removes it.
    /// Remembered; applied at the next start.
    #[arg(long, value_name = "N", value_parser = crate::policy::parse_cpus)]
    pub cpus: Option<f64>,
    /// Most processes and threads the machine may have; 0 removes the limit.
    /// Remembered; applied at the next start.
    #[arg(long, value_name = "N")]
    pub pids_limit: Option<u64>,
    #[command(flatten)]
    pub health: HealthArgs,
}

#[derive(Args, Debug)]
pub struct StopArgs {
    /// Machine name.
    pub name: String,
    /// Kill the machine immediately instead of asking it to power off.
    #[arg(long, short = 'f')]
    pub force: bool,
    /// Return right after the stop request, without waiting for the machine to be gone
    /// (no SIGKILL after --timeout; the unit hooks release its network when it ends).
    #[arg(long = "no-wait", action = clap::ArgAction::SetFalse)]
    pub wait: bool,
    /// App images: seconds to wait after the stop signal before terminating the machine.
    #[arg(long, short = 't', default_value_t = 10)]
    pub timeout: u64,
}

#[derive(Args, Debug)]
pub struct ExecArgs {
    /// Machine name.
    pub machine: String,
    /// User inside the machine.
    #[arg(long, short = 'u', default_value = "root")]
    pub user: String,
    /// Accepted and ignored: exec always enters the machine's namespaces.
    #[arg(long, hide = true)]
    pub nsenter: bool,
    /// Command and arguments.
    #[arg(required = true, trailing_var_arg = true)]
    pub command: Vec<String>,
}

#[derive(Args, Debug)]
pub struct ShellArgs {
    /// Machine name.
    pub machine: String,
    /// User inside the machine.
    #[arg(long, short = 'u', default_value = "root")]
    pub user: String,
}

#[derive(Args, Debug, Default)]
pub struct LogsArgs {
    /// Machine name.
    pub machine: String,
    /// Keep printing new output (starts from the last 10 lines unless --lines says otherwise).
    #[arg(long, short = 'f')]
    pub follow: bool,
    /// Only the last N lines.
    #[arg(long, short = 'n', value_name = "N")]
    pub lines: Option<u32>,
    /// Only output newer than this (journalctl --since syntax, e.g. "10 min ago" or -1h).
    #[arg(long, value_name = "WHEN", allow_hyphen_values = true)]
    pub since: Option<String>,
    /// Prefix every line with its timestamp.
    #[arg(long, short = 't')]
    pub timestamps: bool,
    /// Also show what systemd says about the machine's service (start, stop, failures).
    #[arg(long)]
    pub all: bool,
    /// Boot machines only: read the machine's own journal instead of its console output.
    #[arg(long)]
    pub inside: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_command_line_is_well_formed() {
        Cli::command().debug_assert();
    }

    /// The completions and the manual page are written from this very definition, so a
    /// command or a flag cannot be in one and missing from the other.
    #[test]
    fn what_shells_and_man_are_given_comes_from_here() {
        for shell in [
            clap_complete::Shell::Bash,
            clap_complete::Shell::Zsh,
            clap_complete::Shell::Fish,
        ] {
            let mut out = Vec::new();
            let mut command = Cli::command();
            let name = command.get_name().to_string();
            clap_complete::generate(shell, &mut command, name, &mut out);
            let text = String::from_utf8(out).expect("the generators write text");
            for word in [
                "nspawn", "images", "machines", "network", "volume", "inspect", "rm", "restart",
                "run",
            ] {
                assert!(text.contains(word), "{shell} completions miss {word}");
            }
        }
        let mut man = Vec::new();
        clap_mangen::Man::new(Cli::command())
            .render(&mut man)
            .expect("rendering the manual page");
        let man = String::from_utf8(man).expect("roff is text");
        assert!(man.contains(".TH nspawn 1"), "the header names the tool");
        // roff escapes the hyphens of the description.
        assert!(
            man.contains(r"systemd\-nspawn machines"),
            "and says what it is"
        );
        assert!(man.contains(".SH SYNOPSIS"), "with the usual sections");
    }

    #[test]
    fn restart_and_limits_read_like_docker() {
        let cli = Cli::try_parse_from([
            "nspawn",
            "start",
            "web",
            "--restart",
            "unless-stopped",
            "-m",
            "64m",
            "--cpus",
            "0.5",
            "--pids-limit",
            "100",
            "--label",
            "a=b",
        ])
        .unwrap();
        let Command::Start(start) = cli.command else {
            panic!("not start");
        };
        let start = start.options;
        assert_eq!(start.restart, Some(crate::policy::Restart::UnlessStopped));
        assert_eq!(start.memory, Some(64 << 20));
        assert_eq!(start.cpus, Some(0.5));
        assert_eq!(start.pids_limit, Some(100));
        assert_eq!(start.label, ["a=b"]);
        for bad in [
            &["nspawn", "start", "web", "--restart", "bogus"][..],
            &["nspawn", "start", "web", "-m", "12q"][..],
            &["nspawn", "start", "web", "--cpus", "-1"][..],
            &["nspawn", "create", "img", "web", "-m", "1k"][..],
        ] {
            assert!(Cli::try_parse_from(bad).is_err(), "{bad:?}");
        }
        let cli = Cli::try_parse_from([
            "nspawn",
            "run",
            "nginx:1.27",
            "--name",
            "web",
            "-p",
            "8080:80",
            "--restart",
            "always",
            "--pull",
            "never",
            "--",
            "nginx",
            "-g",
            "daemon off;",
        ])
        .unwrap();
        let Command::Run(run) = cli.command else {
            panic!("not run");
        };
        assert_eq!(run.reference, "nginx:1.27");
        assert_eq!(run.name.as_deref(), Some("web"));
        assert_eq!(run.pull, PullPolicy::Never);
        assert_eq!(run.options.publish, ["8080:80"]);
        assert_eq!(run.options.restart, Some(crate::policy::Restart::Always));
        assert_eq!(run.command, ["nginx", "-g", "daemon off;"]);
        assert!(run.options.wait, "run waits like start unless --no-wait");
        assert!(Cli::try_parse_from(["nspawn", "run", "x", "--pull", "sometimes"]).is_err());
        // docker's order: options, the image, then its command and arguments as they are.
        let cli = Cli::try_parse_from([
            "nspawn",
            "run",
            "-it",
            "--rm",
            "-n",
            "a",
            "alpine:3",
            "sh",
            "-c",
            "echo -n hi",
        ])
        .unwrap();
        let Command::Run(run) = cli.command else {
            panic!("not run");
        };
        assert!(run.tty && run.interactive && run.rm);
        assert_eq!(run.name.as_deref(), Some("a"));
        assert_eq!(run.command, ["sh", "-c", "echo -n hi"]);
        let cli = Cli::try_parse_from(["nspawn", "run", "alpine:3", "-p", "80:80", "-d"]).unwrap();
        let Command::Run(run) = cli.command else {
            panic!("not run");
        };
        assert!(
            run.command.is_empty(),
            "options after the image are still options"
        );
        assert!(
            Cli::try_parse_from(["nspawn", "run", "nginx", "--nmae", "web"]).is_err(),
            "a mistyped option after the image is not taken for the command"
        );
        assert!(
            Cli::try_parse_from(["nspawn", "events", "--until", "now"]).is_err(),
            "until reads back and needs since"
        );
        assert!(
            Cli::try_parse_from(["nspawn", "events", "--since", "-1h", "--until", "now"]).is_ok(),
            "journalctl's relative times start with a hyphen"
        );
        assert!(Cli::try_parse_from(["nspawn", "logs", "web", "--since", "-10min"]).is_ok());
        assert!(run.detach);
        assert_eq!(run.options.publish, ["80:80"]);
        let cli = Cli::try_parse_from(["nspawn", "rm", "-f", "a", "b"]).unwrap();
        let Command::Rm(rm) = cli.command else {
            panic!("not rm");
        };
        assert!(rm.force);
        assert_eq!(rm.names, ["a", "b"]);
    }
}
