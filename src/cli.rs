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
    /// Boot an image as a machine.
    Start(StartArgs),
    /// Power off a running machine.
    Stop(StopArgs),
    /// Run a command inside a running machine.
    Exec(ExecArgs),
    /// Open an interactive shell inside a running machine.
    Shell(ShellArgs),
    /// Show what a machine printed, like docker logs.
    Logs(LogsArgs),
    /// The bridge network shared by the machines.
    Network(NetworkArgs),
}

#[derive(Args, Debug)]
pub struct NetworkArgs {
    #[command(subcommand)]
    pub command: NetworkCommand,
}

#[derive(Subcommand, Debug)]
pub enum NetworkCommand {
    /// Create the bridge with its NAT rules (start does it too; useful at boot).
    Up,
    /// List the machines on the bridge with their addresses and published ports.
    #[command(alias = "list")]
    Ls,
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
    /// Network of the new machine (default: like the source).
    #[arg(long, value_enum)]
    pub network: Option<crate::settings::Network>,
    /// Ports to publish on the host, like start -p.
    #[arg(long, short = 'p', value_name = "HOST:CONTAINER[/udp]")]
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
    /// For app images: the arguments after -- replace the image's cmd and follow its
    /// entrypoint, as with docker.
    #[arg(last = true)]
    pub command: Vec<String>,
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
    Ls,
    /// Remove local images (and the layers nobody uses any more).
    Rm(ImagesRmArgs),
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
}

#[derive(Args, Debug)]
pub struct StartArgs {
    /// Image name.
    pub name: String,
    /// Do not wait for a booted machine's init to be up before returning (its
    /// registration is still awaited so that ports and firewall rules can be applied).
    #[arg(long = "no-wait", action = clap::ArgAction::SetFalse)]
    pub wait: bool,
    /// Network of the machine, remembered for the image: bridge (default for booted
    /// images), veth (systemd-networkd on the host) or host (the host's own network).
    #[arg(long, value_enum)]
    pub network: Option<crate::settings::Network>,
    /// Publish a port on the host, like docker -p: HOST:CONTAINER[/udp]. Repeatable and
    /// remembered for the image; "none" forgets them all.
    #[arg(long, short = 'p', value_name = "HOST:CONTAINER[/udp]")]
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
    /// Forget the remembered entrypoint and arguments and run the image's own again.
    #[arg(long)]
    pub image_command: bool,
    /// For app images: the arguments after -- replace the image's cmd and follow its
    /// entrypoint, as with docker. Remembered for later starts.
    #[arg(last = true)]
    pub command: Vec<String>,
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
    /// Kept for compatibility: exec always enters the machine's namespaces now.
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
    /// Only output newer than this (journalctl --since syntax, e.g. "10 min ago").
    #[arg(long, value_name = "WHEN")]
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
