use clap::Parser;

#[derive(Parser)]
pub struct MachinesArgs {
    #[clap(subcommand)]
    pub commands: MachineCommands,
}

#[derive(Parser)]
pub enum MachineCommands {
    /// List machines
    List(List),
    /// Stop the machine.
    Stop(Stop),
    /// Kill the machine.
    Kill(Kill),
    /// Exec a command in the machine.
    Exec(Exec),
    /// Get a shell in the machine.
    Shell(Shell),
    /// Terminate the machine.
    Terminate(Terminate),
    /// Reboot the machine.
    Reboot(Reboot),
    /// Get the machine's network configuration.
    Network(Network),
}

#[derive(Parser, Clone)]
pub struct List {
    #[arg(short, long)]
    pub pattern: Option<String>,
}

#[derive(Parser, Clone)]
pub struct Stop {
    /// Stop all machines.
    #[arg(short, long, default_value_t = false)]
    pub all: bool,
    /// Stop machines matching the specified pattern.
    #[arg(short, long, default_value = None)]
    pub pattern: Option<String>,
    /// Name of the machine to stop.
    pub machine: Option<String>,
}

#[derive(Parser, Clone)]
pub struct Kill {
    /// Name of the machine to work with.
    pub machine: Option<String>,
    /// Kill all machines.
    #[arg(short, long, default_value_t = false)]
    pub all: bool,
    /// Kill machines matching the specified pattern.
    #[arg(short, long, default_value = None)]
    pub pattern: Option<String>,
    /// Kill the machine with the specified signal.
    #[arg(short, long, default_value = "SIGTERM")]
    pub signal: String,
}

#[derive(Parser, Clone)]
pub struct Exec {
    /// Name of the machine to work with.
    pub machine: Option<String>,
    /// Command to execute.
    #[arg(short, long)]
    pub command: String,
    /// Run the command on all machines.
    #[arg(short, long, default_value_t = false)]
    pub all: bool,
    /// Run the command on machines matching the specified pattern.
    #[arg(short, long, default_value = None)]
    pub pattern: Option<String>,
    /// User to log in as.
    #[arg(short, long, default_value = None)]
    pub user: Option<String>,
}

#[derive(Parser, Clone)]
pub struct Shell {
    /// Name of the machine to work with.
    pub machine: Option<String>,
    /// User to log in as.
    #[arg(short, long, default_value = None)]
    pub user: Option<String>,
}

#[derive(Parser, Clone)]
pub struct Terminate {
    /// Name of the machine to terminate.
    pub machine: Option<String>,
    /// Terminate all machines.
    #[arg(short, long, default_value_t = false)]
    pub all: bool,
    /// Terminate machines matching the specified pattern.
    #[arg(short, long, default_value = None)]
    pub pattern: Option<String>,
}

#[derive(Parser, Clone)]
pub struct Reboot {
    /// Reboot all machines.
    #[arg(short, long, default_value_t = false)]
    pub all: bool,
    /// Reboot machines matching the specified pattern.
    #[arg(short, long, default_value = None)]
    pub pattern: Option<String>,
    /// Name of the machine to reboot.
    pub machine: Option<String>,
}

#[derive(Parser, Clone)]
pub struct Network {
    /// Name of the machine to work with.
    pub machine: Option<String>,
    /// Get the network configuration for all machines.
    #[arg(short, long, default_value_t = false)]
    pub all: bool,
    /// Get the network configuration for machines matching the specified pattern.
    #[arg(short, long, default_value = None)]
    pub pattern: Option<String>,
}
