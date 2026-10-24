use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::completion::{candidates, Kind};

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
    /// Build an image from a Dockerfile, like docker build, or with mkosi from an mkosi
    /// configuration, and make it available locally, ready to push.
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
    /// Print the nspawn commands that make machines again, on another host or this one:
    /// run -d with every flag a machine keeps, after the networks it joins and the
    /// machines it depends on.
    Generate(GenerateArgs),
    /// Boot an image as a machine.
    Start(StartArgs),
    /// Power off running machines.
    Stop(StopArgs),
    /// Stop machines and start them again, like docker restart.
    Restart(RestartArgs),
    /// Send a signal to running machines, like docker kill (SIGKILL stops them for good).
    Kill(KillArgs),
    /// Freeze running machines, every process in them, like docker pause.
    Pause(NamesArgs),
    /// Let paused machines run again.
    Unpause(NamesArgs),
    /// The processes of a running machine, like docker top.
    Top(TopArgs),
    /// Wait for machines to stop and print each one's exit code, like docker wait.
    Wait(WaitArgs),
    /// The ports a running machine publishes, like docker port.
    Port(PortArgs),
    /// Change the restart policy and limits of machines, like docker update; a running
    /// machine gets the limits at once.
    Update(UpdateArgs),
    /// What running machines use: CPU, memory, network, disk, processes, like docker stats.
    Stats(StatsArgs),
    /// Give the machines labelled org.nspawn.auto-update=registry the image their
    /// reference names now, like podman auto-update: each is made anew with every option
    /// it had, and put back when it does not come up (healthy, with a healthcheck).
    AutoUpdate(AutoUpdateArgs),
    /// What happens to machines, networks, volumes and secrets, as it happens, like docker
    /// events.
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
    /// Manage secrets (--secret NAME), like docker secret: kept encrypted for this host.
    Secret(SecretArgs),
    /// What nspawn keeps on disk, and its prune, like docker system.
    System(SystemArgs),
    /// The cache of Dockerfile builds, like docker builder.
    Builder(BuilderArgs),
    /// Run docker compose files on nspawn machines, with docker compose's commands.
    Compose(crate::compose::cli::ComposeArgs),
    /// Serve org.nspawn on the system bus (started by the bus; see --install).
    Daemon(DaemonArgs),
    /// run --rm: removes a machine once its unit is down after that run.
    #[command(hide = true)]
    RemoveAfterExit { name: String, invocation: String },
    /// From a transient unit: runs a machine's health probes until it is gone.
    #[command(hide = true)]
    HealthRun { name: String },
    /// From nspawn-api@NAME.socket: serves the Docker API on the socket a machine mounts
    /// as docker's.
    #[command(hide = true)]
    ApiServe { name: String },
    /// ExecStartPre of a machine with --depends-on: waits until the other machines are
    /// what it asks of them.
    #[command(hide = true)]
    AwaitDependencies { name: String },
    /// From a transient unit: serves the IPv6 side of a machine's published ports until
    /// its run is over.
    #[command(hide = true)]
    PortProxy { name: String },
    /// ExecStart of an app machine: runs systemd-nspawn with the terminal or input an
    /// attached run hands over, or as it is.
    #[command(hide = true)]
    AttachExec {
        name: String,
        /// The journald namespace the output goes to (--log-driver local).
        #[arg(long)]
        journal_namespace: Option<String>,
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
        #[arg(required = true, add = candidates(Kind::Network))]
        names: Vec<String>,
    },
    /// Remove networks no machine uses, with their bridges and rules.
    #[command(alias = "remove")]
    Rm {
        /// Network names.
        #[arg(required = true, add = candidates(Kind::Network))]
        names: Vec<String>,
    },
    /// Join a machine to a network, like docker network connect: a running one at once,
    /// with an interface of its own on it.
    Connect {
        /// Network name.
        #[arg(add = candidates(Kind::Network))]
        network: String,
        /// Machine name.
        #[arg(add = candidates(Kind::Any))]
        machine: String,
        /// Another name for the machine on this network. Repeatable.
        #[arg(long, value_name = "NAME")]
        alias: Vec<String>,
        /// A fixed IPv4 address on the network (one made with network create).
        #[arg(long, value_name = "ADDRESS")]
        ip: Option<String>,
    },
    /// Take a machine off a network, like docker network disconnect: a running one at
    /// once.
    Disconnect {
        /// Network name.
        #[arg(add = candidates(Kind::Network))]
        network: String,
        /// Machine name.
        #[arg(add = candidates(Kind::Any))]
        machine: String,
        /// No error when the machine or its membership is not there.
        #[arg(long, short = 'f')]
        force: bool,
    },
    /// Remove every network no machine uses.
    Prune {
        /// Do not ask.
        #[arg(long, short = 'f')]
        force: bool,
        /// Only the networks it selects: label=KEY[=VALUE] or until=TIME (a duration
        /// ago, or unix seconds). Repeatable; every one must hold.
        #[arg(long = "filter", value_name = "FILTER")]
        filters: Vec<String>,
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
        #[arg(required = true, add = candidates(Kind::Volume))]
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
pub struct SecretArgs {
    #[command(subcommand)]
    pub command: SecretCommand,
}

#[derive(Subcommand, Debug)]
pub enum SecretCommand {
    /// List the secrets with the machines that take them; never their content.
    #[command(alias = "list")]
    Ls(OutputArgs),
    /// Keep a secret, read from a file or from standard input, encrypted for this host
    /// (systemd-creds: the TPM2 where there is one, the host's key otherwise).
    Create {
        /// Secret name: letters, digits, _ . and -.
        name: String,
        /// Read the content from this file instead of standard input.
        #[arg(long, short = 'f', value_name = "FILE")]
        file: Option<std::path::PathBuf>,
        /// Label the secret, KEY=VALUE, like docker secret create --label. Repeatable.
        #[arg(long, short = 'l', value_name = "KEY=VALUE")]
        label: Vec<String>,
    },
    /// What nspawn keeps about secrets, as JSON: never their content.
    Inspect {
        /// Secret names.
        #[arg(required = true)]
        names: Vec<String>,
    },
    /// Remove secrets no machine takes.
    #[command(alias = "remove")]
    Rm {
        /// Secret names.
        #[arg(required = true)]
        names: Vec<String>,
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
    /// How to assemble the image on this host: auto is overlay, or flat without overlayfs;
    /// mstack (systemd 261 or newer, managed user namespaces) is experimental.
    #[arg(long, value_enum, default_value_t = BackendChoice::Auto)]
    pub backend: BackendChoice,
    /// Whether the image boots an init system or runs a single program.
    #[arg(long, value_enum, default_value_t = ModeChoice::Auto)]
    pub mode: ModeChoice,
    /// Replace an existing image with the same name.
    #[arg(long, short = 'f')]
    pub force: bool,
    /// Skip the signature check: the hub's images are signed and refused without a valid
    /// signature, and nspawn.toml can ask the same of other registries; like docker's
    /// --disable-content-trust.
    #[arg(long)]
    pub no_verify: bool,
}

#[derive(Args, Debug)]
pub struct CreateArgs {
    /// Image to start from: a local one by its name or reference, or a reference to pull
    /// first (kept under its own name, as run does).
    #[arg(add = candidates(Kind::Image))]
    pub source: String,
    /// Name of the new machine.
    pub name: String,
    /// How to assemble it (default: like the source).
    #[arg(long, value_enum, default_value_t = BackendChoice::Auto)]
    pub backend: BackendChoice,
    /// Network of the new machine: bridge, veth, host, none, container:NAME (that
    /// machine's network, as docker's) or a network made with network create;
    /// repeatable to join several bridge networks, the first one primary (default: the
    /// source's kind; a network made with network create is not inherited).
    #[arg(long, value_name = "NETWORK", add = candidates(Kind::NetworkChoice))]
    pub network: Vec<String>,
    /// Another name for the machine on a network, like docker --network-alias: NAME on
    /// its primary network, or NETWORK=NAME. Repeatable.
    #[arg(long, value_name = "[NETWORK=]NAME")]
    pub network_alias: Vec<String>,
    /// A fixed IPv4 address on the primary network, like docker --ip (NETWORK=IP for
    /// another network it joins): one made with network create, inside its subnet, and
    /// nobody else's. Remembered.
    #[arg(long, value_name = "[NETWORK=]IP")]
    pub ip: Vec<String>,
    /// Ports to publish on the host, like start -p.
    #[arg(long, short = 'p', value_name = "[IP:]HOST:CONTAINER[/udp]")]
    pub publish: Vec<String>,
    /// Replace an existing machine with the same name.
    #[arg(long, short = 'f')]
    pub force: bool,
    /// Skip the signature check of an image that has to be pulled (a local source is not
    /// checked); like docker's --disable-content-trust.
    #[arg(long)]
    pub no_verify: bool,
    /// Replace the image's entrypoint; an empty string runs the arguments alone.
    #[arg(long, value_name = "PROGRAM")]
    pub entrypoint: Option<String>,
    /// Environment for the program, VAR=value or VAR (copied from here), like docker -e.
    #[arg(long, short = 'e', value_name = "VAR[=VALUE]")]
    pub env: Vec<String>,
    /// Mount a host directory or a named volume, SOURCE:TARGET[:ro], like docker -v.
    #[arg(long, short = 'v', value_name = "SOURCE:TARGET[:ro]", value_hint = clap::ValueHint::AnyPath)]
    pub volume: Vec<String>,
    /// Read variables from a file of VAR=value lines, like docker --env-file: no quoting
    /// or expansion, a VAR alone copied from here; -e comes after and wins. Repeatable.
    #[arg(long, value_name = "FILE", value_hint = clap::ValueHint::FilePath)]
    pub env_file: Vec<std::path::PathBuf>,
    /// Read labels from a file of KEY=VALUE lines, like docker --label-file; --label comes
    /// after and wins. Repeatable.
    #[arg(long, value_name = "FILE", value_hint = clap::ValueHint::FilePath)]
    pub label_file: Vec<std::path::PathBuf>,
    /// A mount in docker's long form, type=bind|volume|tmpfs,source=...,target=...[,readonly]
    /// (tmpfs-size, tmpfs-mode): -v or --tmpfs written out. Repeatable.
    #[arg(long, value_name = "type=TYPE,source=SRC,target=DST")]
    pub mount: Vec<String>,
    /// Label the machine, KEY=VALUE, like docker --label; the image's own labels stay
    /// underneath. Repeatable and remembered; "none" forgets them.
    #[arg(long, short = 'l', value_name = "KEY=VALUE")]
    pub label: Vec<String>,
    /// Restart policy, like docker --restart: no, on-failure[:N] (the count is not kept),
    /// always (also starts it at
    /// boot) or unless-stopped (like always, until nspawn stop). Remembered; applied at
    /// the next start.
    #[arg(long, value_name = "POLICY", value_parser = crate::policy::RestartParser)]
    pub restart: Option<crate::policy::Restart>,
    /// Memory limit of the whole machine, like docker -m: 512m, 2g, and as much swap
    /// again unless --memory-swap says otherwise; 0 removes it. Remembered; applied at the
    /// next start.
    #[arg(long, short = 'm', value_name = "SIZE", value_parser = crate::policy::parse_memory)]
    pub memory: Option<u64>,
    /// Memory and swap together, like docker --memory-swap: a size of at least --memory
    /// (equal to it, no swap), or -1 for swap without a bound; 0 goes back to as much swap
    /// again as memory. Remembered; applied at the next start.
    #[arg(long, value_name = "SIZE", allow_hyphen_values = true, value_parser = crate::policy::parse_memory_swap)]
    pub memory_swap: Option<i64>,
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
    #[command(flatten)]
    pub tuning: TuningArgs,
    /// For app images: the arguments after -- replace the image's cmd and follow its
    /// entrypoint, as with docker.
    #[arg(last = true)]
    pub command: Vec<String>,
}

/// docker's other per-container flags; remembered, each one given replacing what the
/// machine had.
#[derive(Args, Debug, Default, Clone)]
pub struct TuningArgs {
    /// Hostname inside the machine (default: its name).
    #[arg(long, value_name = "NAME")]
    pub hostname: Option<String>,
    /// User the program runs as, instead of the image's: USER or USER:GROUP, each a
    /// name or a number of the image, as docker takes them.
    #[arg(long, short = 'u', value_name = "USER[:GROUP]")]
    pub user: Option<String>,
    /// Working directory of the program, instead of the image's.
    #[arg(long, short = 'w', value_name = "DIR")]
    pub workdir: Option<String>,
    /// Capability to keep on top of the default set (NET_ADMIN, ALL): the least an app
    /// on a bridge network needs, systemd-nspawn's for a machine in a user namespace.
    /// Repeatable; "none" forgets them.
    #[arg(long, value_name = "CAP")]
    pub cap_add: Vec<String>,
    /// Capability to drop from the default set. Repeatable; "none" forgets them.
    #[arg(long, value_name = "CAP")]
    pub cap_drop: Vec<String>,
    /// Every capability, like docker --privileged (--privileged=false takes it back).
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true", value_name = "BOOL")]
    pub privileged: Option<bool>,
    /// Mount the machine's root read-only (--read-only=false takes it back).
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true", value_name = "BOOL")]
    pub read_only: Option<bool>,
    /// An empty tmpfs at a path inside, PATH[:OPTIONS] (size=64m,mode=1777). Repeatable;
    /// "none" forgets them.
    #[arg(long, value_name = "PATH[:OPTIONS]")]
    pub tmpfs: Vec<String>,
    /// Size of /dev/shm, like docker: 64m, 1g; 0 for the default.
    #[arg(long, value_name = "SIZE", value_parser = crate::policy::parse_size)]
    pub shm_size: Option<u64>,
    /// A device node of the host for the machine, HOST[:CONTAINER[:PERMISSIONS]], like
    /// docker --device, or a CDI device of the host's specs (nvidia.com/gpu=all).
    /// Repeatable; "none" forgets them.
    #[arg(long, value_name = "HOST[:CONTAINER[:rwm]]", value_hint = clap::ValueHint::AnyPath)]
    pub device: Vec<String>,
    /// GPUs from the CDI spec of their vendor, like docker --gpus: all, a count, or
    /// device=0,1 (nvidia-ctk cdi generate writes NVIDIA's). Repeatable; "none" forgets
    /// them.
    #[arg(long, value_name = "REQUEST")]
    pub gpus: Vec<String>,
    /// A network interface of the host, moved into the machine while it runs and given
    /// back when it stops: an ethernet one, or a wifi adapter with its whole phy; the
    /// name is kept inside. Repeatable; "none" forgets them.
    #[arg(long, value_name = "IFACE")]
    pub interface: Vec<String>,
    /// DNS server for the machine, instead of the host's. Repeatable; "none" forgets
    /// them.
    #[arg(long, value_name = "IP")]
    pub dns: Vec<String>,
    /// DNS search domain. Repeatable; "none" forgets them.
    #[arg(long, value_name = "DOMAIN")]
    pub dns_search: Vec<String>,
    /// A line for the machine's /etc/hosts, HOST:IP; host-gateway is the host's address
    /// on the machine's network. Repeatable; "none" forgets them.
    #[arg(long, value_name = "HOST:IP")]
    pub add_host: Vec<String>,
    /// A resource limit of the program, NAME=SOFT[:HARD], like docker --ulimit
    /// (nofile=1024:4096). Repeatable; "none" forgets them.
    #[arg(long, value_name = "NAME=SOFT[:HARD]")]
    pub ulimit: Vec<String>,
    /// OOM score adjustment of the machine, -1000 to 1000.
    #[arg(long, value_name = "N", allow_hyphen_values = true)]
    pub oom_score_adj: Option<i32>,
    /// Signal `stop` sends the program, instead of the image's (SIGTERM).
    #[arg(long, value_name = "SIGNAL")]
    pub stop_signal: Option<String>,
    /// Seconds `stop` waits after the signal before SIGKILL, unless -t says otherwise; a
    /// negative number waits for as long as the program takes, as docker's -1.
    #[arg(long, value_name = "SECONDS", allow_negative_numbers = true)]
    pub stop_timeout: Option<i64>,
    /// How systemd-nspawn sets the machine's /etc/localtime at each start: auto (its
    /// default, the host's zone), off (left alone, so a zone set inside stays), copy,
    /// bind, symlink or delete. Remembered.
    #[arg(long, value_name = "MODE", value_parser = ["auto", "off", "copy", "bind", "symlink", "delete"])]
    pub timezone: Option<String>,
    /// Where the program's output goes, like docker --log-driver: local (the default, a
    /// journal of nspawn's own, apart from the system's, which logs and an attached run
    /// read), journal (the system's journal) or none (dropped; an attached run still shows
    /// it, straight from the program, and logs has nothing). Remembered.
    #[arg(long, value_name = "DRIVER", value_parser = ["local", "journal", "none"])]
    pub log_driver: Option<String>,
    /// Run an app's program under an init that reaps orphans and turns a stop into
    /// SIGTERM (systemd-nspawn's stub init), like docker --init; --init=false makes the
    /// program PID 1 of the machine, as docker runs it. Without it, PID 1 only for images
    /// that ask for it (s6-overlay). Remembered.
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true", value_name = "BOOL")]
    pub init: Option<bool>,
    /// A net.* sysctl for an app machine's network namespace, KEY=VALUE. Repeatable;
    /// "none" forgets them.
    #[arg(long, value_name = "KEY=VALUE")]
    pub sysctl: Vec<String>,
    /// A secret (nspawn secret create) as a read-only file inside the machine, like
    /// docker's --secret: NAME alone is /run/secrets/NAME with mode 0444, root's.
    /// Repeatable; "none" forgets them.
    #[arg(long, value_name = "NAME[:TARGET[:MODE[:UID:GID]]]")]
    pub secret: Vec<String>,
    /// Another machine to start first, like compose's depends_on: started (the default),
    /// healthy (its healthcheck passes) or completed (its program ended with 0); with
    /// :optional a failure of it is a warning. systemd follows it at boot as well.
    /// Repeatable; "none" forgets them.
    #[arg(long, value_name = "NAME[:CONDITION][:optional]", add = candidates(Kind::Any))]
    pub depends_on: Vec<String>,
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
    /// The build context: a directory with a Dockerfile, or an mkosi configuration
    /// (mkosi.conf, mkosi.conf.d); - reads a context tar or a Dockerfile from standard
    /// input.
    #[arg(default_value = ".")]
    pub directory: PathBuf,
    /// Reference for the result, for example myapp:1 or hub.example/team/app:2.
    /// Repeatable for a Dockerfile; an mkosi build takes one.
    #[arg(long, short = 't')]
    pub tag: Vec<String>,
    /// Replace an existing image with the same name.
    #[arg(long)]
    pub force: bool,
    /// How to assemble the image on this host: auto is overlay, or flat without overlayfs;
    /// mstack (systemd 261 or newer, managed user namespaces) is experimental.
    #[arg(long, value_enum, default_value_t = BackendChoice::Auto)]
    pub backend: BackendChoice,
    /// Whether the image boots an init system or runs a single program.
    #[arg(long, value_enum, default_value_t = ModeChoice::Auto)]
    pub mode: ModeChoice,
    #[command(flatten)]
    pub dockerfile: DockerfileArgs,
    #[command(flatten)]
    pub mkosi: MkosiArgs,
}

/// The flags of a Dockerfile build, as docker build takes them.
#[derive(Args, Debug, Default)]
pub struct DockerfileArgs {
    /// The Dockerfile, like docker build -f (default: Dockerfile, dockerfile or
    /// Containerfile in the context); - reads it from standard input.
    #[arg(long, short = 'f', value_name = "PATH", value_hint = clap::ValueHint::FilePath)]
    pub file: Option<String>,
    /// A build argument, KEY=VALUE, or KEY to take it from the environment. Repeatable.
    #[arg(long = "build-arg", value_name = "KEY[=VALUE]")]
    pub build_arg: Vec<String>,
    /// The stage to build (default: the last one).
    #[arg(long, value_name = "STAGE")]
    pub target: Option<String>,
    /// Build every step again instead of taking it from the cache.
    #[arg(long)]
    pub no_cache: bool,
    /// Build these stages again instead of taking their steps from the cache.
    /// Repeatable.
    #[arg(long, value_name = "STAGE")]
    pub no_cache_filter: Vec<String>,
    /// Ask the registry for the base images again even when they are here.
    #[arg(long)]
    pub pull: bool,
    /// A label for the image, KEY=VALUE. Repeatable.
    #[arg(long, value_name = "KEY=VALUE")]
    pub label: Vec<String>,
    /// The network of RUN steps: default (the bridge), none, or host (with --allow
    /// network.host).
    #[arg(long, value_name = "MODE")]
    pub network: Option<String>,
    /// A line for the hosts file of RUN steps, HOST:IP. Repeatable.
    #[arg(long, value_name = "HOST:IP")]
    pub add_host: Vec<String>,
    /// Entitlements: network.host lets a step use --network=host. Repeatable.
    #[arg(long, value_name = "ENTITLEMENT")]
    pub allow: Vec<String>,
    /// A secret for RUN --mount=type=secret, id=ID[,src=PATH | ,env=VAR], read here by
    /// the caller. Repeatable.
    #[arg(long, value_name = "id=ID[,src=PATH]")]
    pub secret: Vec<String>,
    /// Size of /dev/shm in RUN steps, like docker: 64m, 1g.
    #[arg(long, value_name = "SIZE", value_parser = crate::policy::parse_size)]
    pub shm_size: Option<u64>,
    /// A resource limit of RUN steps, NAME=SOFT[:HARD]. Repeatable.
    #[arg(long, value_name = "NAME=SOFT[:HARD]")]
    pub ulimit: Vec<String>,
    /// The platform to build for; this host's is the only one.
    #[arg(long, value_name = "PLATFORM")]
    pub platform: Option<String>,
    /// Print only the image ID.
    #[arg(long, short = 'q')]
    pub quiet: bool,
    /// How to show the steps: auto, plain or quiet; all print every line.
    #[arg(long, value_name = "TYPE")]
    pub progress: Option<String>,
    /// Write the image ID to this file.
    #[arg(long, value_name = "FILE", value_hint = clap::ValueHint::FilePath)]
    pub iidfile: Option<PathBuf>,
    /// Push the tags once built.
    #[arg(long)]
    pub push: bool,
    /// The image is local already; taken for the command lines that ask for it.
    #[arg(long)]
    pub load: bool,
    /// Pull the base images without verifying their signatures.
    #[arg(long)]
    pub no_verify: bool,
    /// Images to take the cache from: ignored, nspawn's cache is this host's.
    #[arg(long, value_name = "SOURCE", hide = true)]
    pub cache_from: Vec<String>,
    #[arg(long, hide = true)]
    pub ssh: Vec<String>,
    #[arg(long, short = 'o', hide = true)]
    pub output: Vec<String>,
    #[arg(long, hide = true)]
    pub cache_to: Vec<String>,
    #[arg(long, hide = true)]
    pub build_context: Vec<String>,
    #[arg(long, num_args = 0..=1, default_missing_value = "", hide = true)]
    pub check: Option<String>,
    #[arg(long, hide = true)]
    pub call: Option<String>,
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true", hide = true)]
    pub sbom: Option<String>,
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true", hide = true)]
    pub provenance: Option<String>,
    #[arg(long, hide = true)]
    pub attest: Vec<String>,
    /// docker build's (buildx's) compatibility flags, which Makefiles still give:
    /// accepted and ignored, as buildx ignores them, three of them with its warning.
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true", hide = true)]
    pub rm: Option<bool>,
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true", hide = true)]
    pub force_rm: Option<bool>,
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true", hide = true)]
    pub compress: Option<bool>,
    #[arg(long, hide = true)]
    pub isolation: Option<String>,
    #[arg(long, hide = true)]
    pub security_opt: Vec<String>,
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true", hide = true)]
    pub squash: Option<bool>,
    #[arg(long, short = 'm', hide = true)]
    pub memory: Option<String>,
    #[arg(long, allow_hyphen_values = true, hide = true)]
    pub memory_swap: Option<String>,
    #[arg(long, short = 'c', allow_hyphen_values = true, hide = true)]
    pub cpu_shares: Option<i64>,
    #[arg(long, allow_hyphen_values = true, hide = true)]
    pub cpu_period: Option<i64>,
    #[arg(long, allow_hyphen_values = true, hide = true)]
    pub cpu_quota: Option<i64>,
    #[arg(long, hide = true)]
    pub cpuset_cpus: Option<String>,
    #[arg(long, hide = true)]
    pub cpuset_mems: Option<String>,
}

impl DockerfileArgs {
    /// Whether a flag only a Dockerfile build takes was given.
    pub fn given(&self) -> bool {
        self.file.is_some()
            || !self.build_arg.is_empty()
            || self.target.is_some()
            || self.no_cache
            || !self.no_cache_filter.is_empty()
            || self.pull
            || !self.label.is_empty()
            || self.network.is_some()
            || !self.add_host.is_empty()
            || !self.allow.is_empty()
            || !self.secret.is_empty()
            || self.shm_size.is_some()
            || !self.ulimit.is_empty()
            || self.platform.is_some()
            || self.quiet
            || self.progress.is_some()
            || self.iidfile.is_some()
            || self.push
            || self.load
            || self.no_verify
    }

    /// The flags of docker build nspawn's builder has no counterpart of.
    pub fn refused(&self) -> Option<&'static str> {
        [
            (
                !self.ssh.is_empty(),
                "--ssh: SSH agent forwarding into a build is not supported",
            ),
            (
                !self.output.is_empty(),
                "--output: a build makes a local image; nspawn push sends it to a registry",
            ),
            (
                !self.cache_to.is_empty(),
                "--cache-to: the build cache stays on this host",
            ),
            (
                !self.build_context.is_empty(),
                "--build-context: a build takes one context",
            ),
            (self.check.is_some(), "--check: the builder has no linter"),
            (self.call.is_some(), "--call: the builder only builds"),
            (
                self.sbom.is_some(),
                "--sbom: the builder writes no attestations",
            ),
            (
                self.provenance.is_some(),
                "--provenance: the builder writes no attestations",
            ),
            (
                !self.attest.is_empty(),
                "--attest: the builder writes no attestations",
            ),
        ]
        .into_iter()
        .find_map(|(given, text)| given.then_some(text))
    }

    /// buildx's warnings for the ignored flags that have one.
    pub fn warnings(&self) -> Vec<&'static str> {
        [
            (self.isolation.is_some(), "isolation flag is deprecated with BuildKit."),
            (
                !self.security_opt.is_empty(),
                "security-opt flag is deprecated. \"RUN --security=insecure\" should be used with BuildKit.",
            ),
            (
                self.squash.is_some(),
                "experimental flag squash is removed with BuildKit. You should squash inside build using a multi-stage Dockerfile for efficiency.",
            ),
        ]
        .into_iter()
        .filter_map(|(given, text)| given.then_some(text))
        .collect()
    }
}

/// The flags of an mkosi build.
#[derive(Args, Debug, Default)]
pub struct MkosiArgs {
    /// Local image name of an mkosi build (default: derived from the tag).
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
    /// Keep the mkosi output directory instead of deleting it after the import.
    #[arg(long)]
    pub keep_output: bool,
    /// Extra arguments passed to mkosi verbatim (after --).
    #[arg(last = true)]
    pub mkosi_args: Vec<String>,
}

impl MkosiArgs {
    /// Whether a flag only an mkosi build takes was given.
    pub fn given(&self) -> bool {
        self.name.is_some()
            || self.distribution.is_some()
            || self.release.is_some()
            || !self.profile.is_empty()
            || self.keep_output
            || !self.mkosi_args.is_empty()
    }
}

#[derive(Args, Debug)]
pub struct PushArgs {
    /// Local image name, or the reference it was pulled from or built as.
    #[arg(add = candidates(Kind::Image))]
    pub image: String,
    /// Push under a different reference than the one recorded for the image.
    #[arg(long, value_name = "REFERENCE")]
    pub to: Option<String>,
}

#[derive(Args, Debug)]
#[command(args_conflicts_with_subcommands = true)]
pub struct ImagesArgs {
    #[command(subcommand)]
    pub command: Option<ImagesCommand>,
    /// The options of the list, which images alone prints.
    #[command(flatten)]
    pub ls: OutputArgs,
}

#[derive(Subcommand, Debug)]
pub enum ImagesCommand {
    /// List local images.
    #[command(alias = "list")]
    Ls(OutputArgs),
    /// Remove local images (and the layers nobody uses any more).
    Rm(ImagesRmArgs),
    /// Remove images no machine uses, like docker image prune: with -a every image no
    /// machine was made from; without, the dangling ones, which nspawn's store has none
    /// of.
    Prune(ImagesPruneArgs),
}

#[derive(Args, Debug)]
pub struct ImagesPruneArgs {
    /// Every image no machine was made from.
    #[arg(long, short = 'a')]
    pub all: bool,
    /// Do not ask.
    #[arg(long, short = 'f')]
    pub force: bool,
    /// label=KEY, label=KEY=VALUE or until=DURATION|UNIX-SECONDS. Repeatable.
    #[arg(long = "filter", value_name = "KEY=VALUE")]
    pub filters: Vec<String>,
}

#[derive(Args, Debug)]
pub struct SystemArgs {
    #[command(subcommand)]
    pub command: SystemCommand,
}

#[derive(Subcommand, Debug)]
pub enum SystemCommand {
    /// What images, machines and volumes take on disk, like docker system df.
    Df(SystemDfArgs),
    /// Remove stopped machines, unused networks and, with -a, unused images, like docker
    /// system prune.
    Prune(SystemPruneArgs),
}

#[derive(Args, Debug)]
pub struct SystemDfArgs {
    /// Each image, machine and volume with its size.
    #[arg(long, short = 'v')]
    pub verbose: bool,
    /// The service's answer as JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct SystemPruneArgs {
    /// Every image no machine was made from, not only the dangling ones.
    #[arg(long, short = 'a')]
    pub all: bool,
    /// Anonymous volumes too, as docker takes it: nspawn has none.
    #[arg(long)]
    pub volumes: bool,
    /// Do not ask.
    #[arg(long, short = 'f')]
    pub force: bool,
    /// label=KEY, label=KEY=VALUE or until=DURATION|UNIX-SECONDS. Repeatable.
    #[arg(long = "filter", value_name = "KEY=VALUE")]
    pub filters: Vec<String>,
}

#[derive(Args, Debug)]
pub struct BuilderArgs {
    #[command(subcommand)]
    pub command: BuilderCommand,
}

#[derive(Subcommand, Debug)]
pub enum BuilderCommand {
    /// Remove build cache, like docker builder prune: without -a what no image takes
    /// too.
    Prune(BuilderPruneArgs),
    /// What the build cache holds, most recently used first.
    Du(BuilderDuArgs),
}

#[derive(Args, Debug)]
pub struct BuilderPruneArgs {
    /// Everything, what images take too.
    #[arg(long, short = 'a')]
    pub all: bool,
    /// Do not ask.
    #[arg(long, short = 'f')]
    pub force: bool,
    /// until=DURATION|UNIX-SECONDS: only what was last used before. Repeatable.
    #[arg(long = "filter", value_name = "KEY=VALUE")]
    pub filters: Vec<String>,
    /// Keep the most recently used cache up to this size (64m, 2g).
    #[arg(long, value_name = "SIZE", value_parser = crate::policy::parse_size)]
    pub keep_storage: Option<u64>,
}

#[derive(Args, Debug)]
pub struct BuilderDuArgs {
    /// Every row, not only the total.
    #[arg(long, short = 'v')]
    pub verbose: bool,
    /// The rows as JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct CpArgs {
    /// What to copy: a local path, or MACHINE:PATH (a local path with a colon is
    /// written ./a:b). DIR/. copies the contents of DIR.
    #[arg(value_name = "SOURCE", value_hint = clap::ValueHint::AnyPath)]
    pub source: String,
    /// Where to: MACHINE:PATH, or a local path. An existing directory receives the source
    /// under its own name; otherwise the copy takes this name.
    #[arg(value_name = "DESTINATION", value_hint = clap::ValueHint::AnyPath)]
    pub destination: String,
}

#[derive(Args, Debug)]
pub struct RmArgs {
    /// Machine names.
    #[arg(required = true, add = candidates(Kind::Any))]
    pub names: Vec<String>,
    /// Stop a running machine first (SIGKILL, like docker rm -f) instead of refusing.
    #[arg(long, short = 'f')]
    pub force: bool,
}

#[derive(Args, Debug)]
pub struct KillArgs {
    /// Machine names.
    #[arg(required = true, add = candidates(Kind::Running))]
    pub names: Vec<String>,
    /// Signal to send: a name (KILL, SIGHUP, RTMIN+3) or a number. SIGKILL stops the
    /// machine like stop --force; other signals go to an app's program or a booted
    /// machine's init. A machine a signal ends is not restarted by its policy when the
    /// signal was SIGKILL, any signal to an app without a stop signal of its own, an
    /// app's stop signal, or a halt or poweroff request to a booted init.
    #[arg(long, short = 's', default_value = "KILL")]
    pub signal: String,
}

#[derive(Args, Debug)]
pub struct UpdateArgs {
    /// Machine names.
    #[arg(required = true, add = candidates(Kind::Any))]
    pub names: Vec<String>,
    /// Restart policy: no, on-failure[:N] (the count is not kept), always (also starts it
    /// at boot) or unless-stopped.
    #[arg(long, value_name = "POLICY", value_parser = crate::policy::RestartParser)]
    pub restart: Option<crate::policy::Restart>,
    /// Memory limit of the whole machine: 512m, 2g, and as much swap again unless
    /// --memory-swap says otherwise; 0 removes it.
    #[arg(long, short = 'm', value_name = "SIZE", value_parser = crate::policy::parse_memory)]
    pub memory: Option<u64>,
    /// Memory and swap together: a size of at least --memory (equal to it, no swap), or -1
    /// for swap without a bound; 0 goes back to as much swap again as memory.
    #[arg(long, value_name = "SIZE", allow_hyphen_values = true, value_parser = crate::policy::parse_memory_swap)]
    pub memory_swap: Option<i64>,
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
    #[arg(add = candidates(Kind::Running))]
    pub names: Vec<String>,
    /// Print one table and return instead of drawing a new one every second.
    #[arg(long)]
    pub no_stream: bool,
    /// One JSON object per machine and sample.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct AutoUpdateArgs {
    /// Machine names (every labelled machine when none is given).
    #[arg(add = candidates(Kind::Any))]
    pub names: Vec<String>,
    /// Only say which machines have a newer image waiting.
    #[arg(long)]
    pub dry_run: bool,
    /// One JSON object per machine.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct EventsArgs {
    /// Show events since this time: a duration ago (42m, 1h30m), unix seconds, an RFC 3339
    /// time or date, or a time as journalctl reads it ("2026-09-24 10:00", -1h, today);
    /// without it, only new ones.
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
    /// Only matching events: name=NAME, type=machine|network|volume|secret, event=ACTION,
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
    #[arg(required = true, add = candidates(Kind::Any))]
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
    /// Only the names, one per line, as docker ps -q gives IDs.
    #[arg(long, short = 'q')]
    pub quiet: bool,
    /// Only matching machines, like docker ps --filter: name, label, status, network,
    /// ancestor, health, exited, volume, publish, expose, id, before, since. Repeatable:
    /// the same key matches either value, different keys must all match.
    #[arg(long = "filter", short = 'f', value_name = "KEY=VALUE")]
    pub filters: Vec<String>,
    /// A Go template over docker's fields (.Names, .Image, .Status, .Ports, .Label "k",
    /// ...) and nspawn's (.Mode, .Address, .OS), "table TEMPLATE" for a table, or json.
    #[arg(long, value_name = "FORMAT")]
    pub format: Option<String>,
    /// Give --format's .Command, .Mounts and .Image whole, which are cut as docker ps
    /// cuts them otherwise.
    #[arg(long)]
    pub no_trunc: bool,
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
    #[arg(required = true, add = candidates(Kind::Any))]
    pub names: Vec<String>,
    /// A Go template over each object, like docker inspect -f: '{{.state}}',
    /// '{{json .labels}}'; the keys are the ones inspect prints.
    #[arg(long, short = 'f', value_name = "FORMAT")]
    pub format: Option<String>,
}

#[derive(Args, Debug)]
pub struct GenerateArgs {
    /// Machine names (every machine of nspawn's when none is given); the machines they
    /// depend on or take their network from come too.
    #[arg(add = candidates(Kind::Any))]
    pub names: Vec<String>,
}

#[derive(Args, Debug)]
pub struct StartArgs {
    /// Image name.
    #[arg(add = candidates(Kind::Startable))]
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
    #[arg(add = candidates(Kind::Reference))]
    pub reference: String,
    /// Name of the machine (default: derived from the reference, e.g. nginx-1.27).
    #[arg(long, short = 'n')]
    pub name: Option<String>,
    /// When to ask the registry: missing (a local image with the reference is reused),
    /// always or never.
    #[arg(long, value_enum, default_value_t = PullPolicy::Missing)]
    pub pull: PullPolicy,
    /// How to assemble the machine on this host: auto is overlay, or flat without
    /// overlayfs; mstack (systemd 261 or newer, managed user namespaces) is experimental.
    #[arg(long, value_enum, default_value_t = BackendChoice::Auto)]
    pub backend: BackendChoice,
    /// Whether the image boots an init system or runs a single program; a mode other
    /// than auto always pulls.
    #[arg(long, value_enum, default_value_t = ModeChoice::Auto)]
    pub mode: ModeChoice,
    /// Make the machine anew when one of that name exists (it must be stopped).
    #[arg(long, short = 'f')]
    pub force: bool,
    /// Skip the signature check: the hub's images are signed and refused without a valid
    /// signature, and nspawn.toml can ask the same of other registries; like docker's
    /// --disable-content-trust.
    #[arg(long)]
    pub no_verify: bool,
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
    /// network), none (no network), container:NAME (the network of that running
    /// machine, as docker's --network container:) or the name of a network made with
    /// network create; repeatable to join several bridge networks, the first one primary.
    #[arg(long, value_name = "NETWORK", add = candidates(Kind::NetworkChoice))]
    pub network: Vec<String>,
    /// Another name for the machine on a network, like docker --network-alias: NAME on
    /// its primary network, or NETWORK=NAME. Repeatable and remembered; "none" forgets
    /// them.
    #[arg(long, value_name = "[NETWORK=]NAME")]
    pub network_alias: Vec<String>,
    /// A fixed IPv4 address on the primary network, like docker --ip (NETWORK=IP for
    /// another network it joins): one made with network create, inside its subnet, and
    /// nobody else's. Remembered.
    #[arg(long, value_name = "[NETWORK=]IP")]
    pub ip: Vec<String>,
    /// Publish a port on the host, like docker -p: HOST:CONTAINER[/udp] on every IPv4 and
    /// IPv6 address of the host, IP:HOST:CONTAINER on one (0.0.0.0 every IPv4 address,
    /// [::] every IPv6 one, an IPv6 address in brackets), several with a range on both
    /// sides (8000-8010:8000-8010). Repeatable and remembered for the image; "none"
    /// forgets them all.
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
    #[arg(long, short = 'v', value_name = "SOURCE:TARGET[:ro]", value_hint = clap::ValueHint::AnyPath)]
    pub volume: Vec<String>,
    /// Read variables from a file of VAR=value lines, like docker --env-file: no quoting
    /// or expansion, a VAR alone copied from here; -e comes after and wins. Repeatable.
    #[arg(long, value_name = "FILE", value_hint = clap::ValueHint::FilePath)]
    pub env_file: Vec<std::path::PathBuf>,
    /// Read labels from a file of KEY=VALUE lines, like docker --label-file; --label comes
    /// after and wins. Repeatable.
    #[arg(long, value_name = "FILE", value_hint = clap::ValueHint::FilePath)]
    pub label_file: Vec<std::path::PathBuf>,
    /// A mount in docker's long form, type=bind|volume|tmpfs,source=...,target=...[,readonly]
    /// (tmpfs-size, tmpfs-mode): -v or --tmpfs written out. Repeatable.
    #[arg(long, value_name = "type=TYPE,source=SRC,target=DST")]
    pub mount: Vec<String>,
    /// Label the machine, KEY=VALUE, like docker --label; the image's own labels stay
    /// underneath. Repeatable and remembered; "none" forgets them.
    #[arg(long, short = 'l', value_name = "KEY=VALUE")]
    pub label: Vec<String>,
    /// Restart policy, like docker --restart: no, on-failure[:N] (the count is not kept),
    /// always (also starts it at
    /// boot) or unless-stopped (like always, until nspawn stop). Remembered; applied at
    /// the next start.
    #[arg(long, value_name = "POLICY", value_parser = crate::policy::RestartParser)]
    pub restart: Option<crate::policy::Restart>,
    /// Memory limit of the whole machine, like docker -m: 512m, 2g, and as much swap
    /// again unless --memory-swap says otherwise; 0 removes it. Remembered; applied at the
    /// next start.
    #[arg(long, short = 'm', value_name = "SIZE", value_parser = crate::policy::parse_memory)]
    pub memory: Option<u64>,
    /// Memory and swap together, like docker --memory-swap: a size of at least --memory
    /// (equal to it, no swap), or -1 for swap without a bound; 0 goes back to as much swap
    /// again as memory. Remembered; applied at the next start.
    #[arg(long, value_name = "SIZE", allow_hyphen_values = true, value_parser = crate::policy::parse_memory_swap)]
    pub memory_swap: Option<i64>,
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
    #[command(flatten)]
    pub tuning: TuningArgs,
}

#[derive(Args, Debug)]
pub struct StopArgs {
    /// Machine names.
    #[arg(required = true, add = candidates(Kind::Running))]
    pub names: Vec<String>,
    /// Kill the machine immediately instead of asking it to power off.
    #[arg(long, short = 'f')]
    pub force: bool,
    /// Return right after the stop request, without waiting for the machine to be gone
    /// (no SIGKILL after --timeout; the unit hooks release its network when it ends).
    #[arg(long = "no-wait", action = clap::ArgAction::SetFalse)]
    pub wait: bool,
    /// App images: seconds to wait after the stop signal before terminating the machine
    /// (default: the machine's --stop-timeout, else 10); a negative number waits for as
    /// long as it takes.
    #[arg(
        long,
        short = 't',
        value_name = "SECONDS",
        allow_negative_numbers = true
    )]
    pub timeout: Option<i64>,
}

#[derive(Args, Debug)]
pub struct RestartArgs {
    /// Machine names.
    #[arg(required = true, add = candidates(Kind::Any))]
    pub names: Vec<String>,
    /// App images: seconds to wait after the stop signal before terminating the machine
    /// (default: the machine's --stop-timeout, else 10); a negative number waits for as
    /// long as it takes.
    #[arg(
        long,
        short = 't',
        value_name = "SECONDS",
        allow_negative_numbers = true
    )]
    pub timeout: Option<i64>,
}

#[derive(Args, Debug)]
pub struct NamesArgs {
    /// Machine names.
    #[arg(required = true, add = candidates(Kind::Running))]
    pub names: Vec<String>,
}

#[derive(Args, Debug)]
pub struct WaitArgs {
    /// Machine names.
    #[arg(required = true, add = candidates(Kind::Any))]
    pub names: Vec<String>,
}

#[derive(Args, Debug)]
pub struct PortArgs {
    /// Machine name.
    #[arg(add = candidates(Kind::Running))]
    pub name: String,
    /// Only this port of the machine: PORT or PORT/PROTOCOL.
    #[arg(value_name = "PRIVATE_PORT[/PROTO]")]
    pub port: Option<String>,
}

#[derive(Args, Debug)]
pub struct TopArgs {
    /// Machine name.
    #[arg(add = candidates(Kind::Running))]
    pub machine: String,
}

#[derive(Args, Debug)]
pub struct ExecArgs {
    /// Machine name.
    #[arg(add = candidates(Kind::Running))]
    pub machine: String,
    /// User inside the machine, USER[:GROUP] by name or number; without it the machine's
    /// own (its --user, else its image's USER), as docker exec runs, root on a booted
    /// machine.
    #[arg(long, short = 'u')]
    pub user: Option<String>,
    /// Environment for the command, VAR=value or VAR (copied from here), like docker
    /// exec -e. Repeatable.
    #[arg(long, short = 'e', value_name = "VAR[=VALUE]")]
    pub env: Vec<String>,
    /// Working directory of the command, instead of the machine's.
    #[arg(long, short = 'w', value_name = "DIR")]
    pub workdir: Option<String>,
    /// No terminal, even from one: pipes, as in a script.
    #[arg(long = "no-tty", short = 'T', conflicts_with = "tty")]
    pub no_tty: bool,
    /// A terminal for the command, even without one here.
    #[arg(long, short = 't')]
    pub tty: bool,
    /// Accepted for docker's sake: the command's input is always this one.
    #[arg(long, short = 'i')]
    pub interactive: bool,
    /// Leave the command running in the background and return at once.
    #[arg(long, short = 'd', conflicts_with_all = ["tty", "no_tty"])]
    pub detach: bool,
    /// Accepted and ignored: exec always enters the machine's namespaces.
    #[arg(long, hide = true)]
    pub nsenter: bool,
    /// docker's: accepted with a note; the command gets the machine's capabilities.
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true", hide = true)]
    pub privileged: Option<bool>,
    /// Command and arguments.
    #[arg(required = true, trailing_var_arg = true)]
    pub command: Vec<String>,
}

#[derive(Args, Debug)]
pub struct ShellArgs {
    /// Machine name.
    #[arg(add = candidates(Kind::Running))]
    pub machine: String,
    /// User inside the machine.
    #[arg(long, short = 'u', default_value = "root")]
    pub user: String,
}

/// --lines and --tail: a count, or all, which is as many as there are.
fn log_lines(text: &str) -> Result<u32, String> {
    if text == "all" {
        return Ok(u32::MAX);
    }
    text.parse()
        .map_err(|_| format!("{text:?} is neither a number of lines nor all"))
}

#[derive(Args, Debug, Default)]
pub struct LogsArgs {
    /// Machine names; with several, every line carries its machine's name.
    #[arg(required = true, add = candidates(Kind::Any))]
    pub machines: Vec<String>,
    /// Keep printing new output (starts from the last 10 lines unless --lines says otherwise).
    #[arg(long, short = 'f')]
    pub follow: bool,
    /// Only the last N lines, or all of them.
    #[arg(long, short = 'n', visible_alias = "tail", value_name = "N", value_parser = log_lines)]
    pub lines: Option<u32>,
    /// Only output newer than this: a duration ago (42m, 1h30m), unix seconds, an RFC 3339
    /// time or date, or a time as journalctl reads it ("10 min ago", -1h).
    #[arg(long, value_name = "WHEN", allow_hyphen_values = true)]
    pub since: Option<String>,
    /// Only output older than this, in the forms --since takes; nothing is followed then.
    #[arg(long, value_name = "WHEN", allow_hyphen_values = true)]
    pub until: Option<String>,
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

    /// The completions and the manual page come from this definition, so a command
    /// or a flag cannot be in one and missing from the other.
    #[test]
    fn what_shells_and_man_are_given_comes_from_here() {
        let complete = |words: &[&str]| -> Vec<String> {
            let words: Vec<std::ffi::OsString> = words.iter().map(Into::into).collect();
            let index = words.len() - 1;
            clap_complete::engine::complete(&mut Cli::command(), words, index, None)
                .expect("completing")
                .iter()
                .map(|c| c.get_value().to_string_lossy().into_owned())
                .collect()
        };
        let commands = complete(&["nspawn", ""]);
        for word in [
            "images", "machines", "network", "volume", "inspect", "rm", "restart", "run",
        ] {
            assert!(
                commands.iter().any(|c| c == word),
                "completions miss {word}"
            );
        }
        assert!(
            !commands.iter().any(|c| c == "attach-exec"),
            "a hidden command completes"
        );
        let flags = complete(&["nspawn", "run", "--"]);
        for flag in ["--log-driver", "--network", "--restart"] {
            assert!(
                flags.iter().any(|f| f == flag),
                "completions miss run {flag}"
            );
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
    fn images_alone_lists_them() {
        let images = |argv: &[&str]| match Cli::try_parse_from(argv).map(|c| c.command) {
            Ok(Command::Images(a)) => Ok(a),
            Ok(other) => panic!("{other:?}"),
            Err(e) => Err(e.kind()),
        };
        let alone = images(&["nspawn", "images"]).unwrap();
        assert!(alone.command.is_none() && !alone.ls.json);
        assert!(images(&["nspawn", "images", "--json"]).unwrap().ls.json);
        let ls = images(&["nspawn", "images", "ls", "--json"]).unwrap();
        assert!(matches!(
            ls.command,
            Some(ImagesCommand::Ls(OutputArgs { json: true }))
        ));
        assert!(matches!(
            images(&["nspawn", "images", "rm", "web"]).unwrap().command,
            Some(ImagesCommand::Rm(_))
        ));
        // A flag of the list belongs before no subcommand.
        assert!(images(&["nspawn", "images", "--json", "rm", "web"]).is_err());
    }

    #[test]
    fn logs_takes_tail_and_all() {
        let lines = |argv: &[&str]| match Cli::try_parse_from(argv).map(|c| c.command) {
            Ok(Command::Logs(a)) => Ok(a.lines),
            Ok(other) => panic!("{other:?}"),
            Err(e) => Err(e.kind()),
        };
        assert_eq!(
            lines(&["nspawn", "logs", "--tail", "50", "web"]),
            Ok(Some(50))
        );
        assert_eq!(
            lines(&["nspawn", "logs", "--tail", "all", "web"]),
            Ok(Some(u32::MAX))
        );
        assert_eq!(lines(&["nspawn", "logs", "-n", "5", "web"]), Ok(Some(5)));
        assert_eq!(lines(&["nspawn", "logs", "--lines=3", "web"]), Ok(Some(3)));
        assert_eq!(lines(&["nspawn", "logs", "web"]), Ok(None));
        assert_eq!(
            lines(&["nspawn", "logs", "--tail", "many", "web"]),
            Err(clap::error::ErrorKind::ValueValidation)
        );
    }

    #[test]
    fn flags_with_an_optional_value_take_no_word_after_them() {
        for argv in [
            &["nspawn", "run", "--privileged", "ubuntu"][..],
            &["nspawn", "run", "-d", "--read-only", "nginx"],
            &["nspawn", "run", "--init", "alpine", "sleep", "10"],
            &["nspawn", "create", "--read-only", "img", "web"],
            &["nspawn", "run", "--privileged=false", "ubuntu"],
        ] {
            assert!(Cli::try_parse_from(argv).is_ok(), "{argv:?}");
        }
    }

    #[test]
    fn exec_names_no_user_unless_told_one() {
        let user = |argv: &[&str]| {
            let Command::Exec(exec) = Cli::try_parse_from(argv).unwrap().command else {
                panic!("not exec");
            };
            exec.user
        };
        // The service then takes the machine's own.
        assert_eq!(user(&["nspawn", "exec", "web", "--", "id"]), None);
        assert_eq!(
            user(&["nspawn", "exec", "-u", "root", "web", "--", "id"]),
            Some("root".to_string())
        );
    }

    #[test]
    fn restart_and_limits_are_parsed() {
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
        // on-failure with a count, the count not kept.
        let counted =
            Cli::try_parse_from(["nspawn", "start", "web", "--restart", "on-failure:3"]).unwrap();
        let Command::Start(counted) = counted.command else {
            panic!("not start");
        };
        assert_eq!(
            counted.options.restart,
            Some(crate::policy::Restart::OnFailure)
        );
        assert!(Cli::try_parse_from(["nspawn", "start", "web", "--restart", "always:3"]).is_err());
        assert!(Cli::try_parse_from(["nspawn", "start", "web", "--restart", "sometimes"]).is_err());
        // -1 for a stop that waits as long as the program takes.
        let Command::Stop(stop) = Cli::try_parse_from(["nspawn", "stop", "-t", "-1", "web"])
            .unwrap()
            .command
        else {
            panic!("not stop");
        };
        assert_eq!(
            (stop.timeout, stop.names),
            (Some(-1), vec!["web".to_string()])
        );
        let Command::Restart(restart) =
            Cli::try_parse_from(["nspawn", "restart", "--timeout", "-1", "web"])
                .unwrap()
                .command
        else {
            panic!("not restart");
        };
        assert_eq!(restart.timeout, Some(-1));
        let Command::Start(forever) =
            Cli::try_parse_from(["nspawn", "start", "web", "--stop-timeout", "-1"])
                .unwrap()
                .command
        else {
            panic!("not start");
        };
        assert_eq!(forever.options.tuning.stop_timeout, Some(-1));
        // What Makefiles and habits give: flags taken and ignored.
        assert!(
            Cli::try_parse_from(["nspawn", "build", "--rm", "--force-rm", "-t", "app", "."])
                .is_ok()
        );
        assert!(Cli::try_parse_from(["nspawn", "build", "--rm=false", "-t", "app", "."]).is_ok());
        assert!(Cli::try_parse_from(["nspawn", "exec", "--privileged", "web", "true"]).is_ok());
        assert!(
            Cli::try_parse_from(["nspawn", "exec", "--privileged=true", "web", "true"]).is_ok()
        );
        let legacy = Cli::try_parse_from([
            "nspawn",
            "build",
            "--force-rm=true",
            "-m",
            "2g",
            "--memory-swap",
            "-1",
            "-c",
            "512",
            "--cpu-period",
            "100000",
            "--cpu-quota",
            "50000",
            "--cpuset-cpus",
            "0-3",
            "--cpuset-mems",
            "0",
            "--compress",
            "--squash",
            "--isolation",
            "default",
            "--security-opt",
            "seccomp=unconfined",
            "-t",
            "app",
            ".",
        ])
        .unwrap();
        let Command::Build(legacy) = legacy.command else {
            panic!("not build");
        };
        assert_eq!(legacy.dockerfile.warnings().len(), 3);
        assert!(!legacy.dockerfile.given(), "none of them picks the builder");
        assert_eq!(legacy.tag, ["app"]);
        assert_eq!(start.memory, Some(64 << 20));
        assert_eq!(start.cpus, Some(0.5));
        assert_eq!(start.pids_limit, Some(100));
        assert_eq!(start.label, ["a=b"]);
        let cli = Cli::try_parse_from([
            "nspawn",
            "start",
            "kali",
            "--interface",
            "wlp11s0f3u2u3",
            "--interface",
            "eth1",
        ])
        .unwrap();
        let Command::Start(start) = cli.command else {
            panic!("not start");
        };
        assert_eq!(start.options.tuning.interface, ["wlp11s0f3u2u3", "eth1"]);
        let cli = Cli::try_parse_from(["nspawn", "start", "kali", "--timezone", "off"]).unwrap();
        let Command::Start(start) = cli.command else {
            panic!("not start");
        };
        assert_eq!(start.options.tuning.timezone.as_deref(), Some("off"));
        let cli =
            Cli::try_parse_from(["nspawn", "run", "-d", "--log-driver", "none", "img"]).unwrap();
        let Command::Run(run) = cli.command else {
            panic!("not run");
        };
        assert_eq!(run.options.tuning.log_driver.as_deref(), Some("none"));
        let cli = Cli::try_parse_from([
            "nspawn",
            "run",
            "-d",
            "--depends-on",
            "db:healthy",
            "--depends-on",
            "migrate:completed",
            "img",
        ])
        .unwrap();
        let Command::Run(run) = cli.command else {
            panic!("not run");
        };
        assert_eq!(
            run.options.tuning.depends_on,
            ["db:healthy", "migrate:completed"]
        );
        let cli =
            Cli::try_parse_from(["nspawn", "create", "img", "web", "--depends-on", "db"]).unwrap();
        let Command::Create(create) = cli.command else {
            panic!("not create");
        };
        assert_eq!(create.tuning.depends_on, ["db"]);
        let cli =
            Cli::try_parse_from(["nspawn", "start", "web", "-m", "1g", "--memory-swap", "-1"])
                .unwrap();
        let Command::Start(start) = cli.command else {
            panic!("not start");
        };
        assert_eq!(start.options.memory_swap, Some(-1));
        let cli = Cli::try_parse_from(["nspawn", "update", "web", "--memory-swap", "2g"]).unwrap();
        let Command::Update(update) = cli.command else {
            panic!("not update");
        };
        assert_eq!(update.memory_swap, Some(2 << 30));
        let cli =
            Cli::try_parse_from(["nspawn", "create", "img", "web", "--memory-swap=512m"]).unwrap();
        let Command::Create(create) = cli.command else {
            panic!("not create");
        };
        assert_eq!(create.memory_swap, Some(512 << 20));
        for bad in [
            &["nspawn", "start", "web", "--restart", "bogus"][..],
            &["nspawn", "start", "web", "-m", "12q"][..],
            &["nspawn", "start", "web", "--cpus", "-1"][..],
            &["nspawn", "create", "img", "web", "-m", "1k"][..],
            &["nspawn", "start", "web", "--memory-swap", "-2"][..],
            &["nspawn", "start", "web", "--timezone", "utc"][..],
            &["nspawn", "start", "web", "--log-driver", "json-file"][..],
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
        assert!(
            !run.no_verify,
            "signatures are checked unless told otherwise"
        );
        assert!(Cli::try_parse_from(["nspawn", "run", "x", "--pull", "sometimes"]).is_err());
        for args in [
            &["nspawn", "pull", "fedora:44", "--no-verify"][..],
            &["nspawn", "run", "-d", "--no-verify", "fedora:44"][..],
            &["nspawn", "create", "--no-verify", "fedora:44", "web"][..],
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            let skipped = match cli.command {
                Command::Pull(a) => a.no_verify,
                Command::Run(a) => a.no_verify,
                Command::Create(a) => a.no_verify,
                _ => panic!("{args:?}"),
            };
            assert!(skipped, "{args:?}");
        }
        // Options, the image, then its command and arguments as they are.
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
        // The env and label files and the long mount flag on the three commands that make
        // or start a machine.
        for args in [
            &[
                "nspawn",
                "run",
                "--env-file",
                "a.env",
                "--label-file",
                "l",
                "--mount",
                "type=tmpfs,dst=/t",
                "img",
            ][..],
            &[
                "nspawn",
                "create",
                "--env-file",
                "a.env",
                "--label-file",
                "l",
                "--mount",
                "type=tmpfs,dst=/t",
                "img",
                "m",
            ][..],
            &[
                "nspawn",
                "start",
                "--env-file",
                "a.env",
                "--label-file",
                "l",
                "--mount",
                "type=tmpfs,dst=/t",
                "m",
            ][..],
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            let (env_file, label_file, mount) = match cli.command {
                Command::Run(a) => (a.options.env_file, a.options.label_file, a.options.mount),
                Command::Create(a) => (a.env_file, a.label_file, a.mount),
                Command::Start(a) => (a.options.env_file, a.options.label_file, a.options.mount),
                _ => panic!("{args:?}"),
            };
            assert_eq!(env_file, [std::path::PathBuf::from("a.env")], "{args:?}");
            assert_eq!(label_file, [std::path::PathBuf::from("l")], "{args:?}");
            assert_eq!(mount, ["type=tmpfs,dst=/t"], "{args:?}");
        }
        let cli = Cli::try_parse_from(["nspawn", "rm", "-f", "a", "b"]).unwrap();
        let Command::Rm(rm) = cli.command else {
            panic!("not rm");
        };
        assert!(rm.force);
        assert_eq!(rm.names, ["a", "b"]);
    }
}
