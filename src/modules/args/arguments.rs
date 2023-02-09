use {
    super::{images_args::ImagesArgs, machines_args::MachinesArgs},
    clap::Parser,
};

/// Command line utility for importing and managing Nspawn-compatible images.
#[derive(Parser)]
#[command(author, version, about, long_about = None)]
pub struct Arguments {
    #[clap(subcommand)]
    pub cmd: Commands,

    /// Fixed width of the table in characters (default: auto)
    #[arg(short, long, default_value = None)]
    pub width: Option<u16>,
}

#[derive(Parser)]
pub enum Commands {
    /// Manage local and remote images. You can list, remove, import, export, rename, and clone images.
    Images(ImagesArgs),
    /// Manage machines. You can start, stop, kill, terminate, login, get a shell and more.
    Machines(MachinesArgs),
}
