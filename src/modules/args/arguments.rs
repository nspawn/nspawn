use {
    super::{images_args::ImagesArgs, machines_args::MachinesArgs},
    clap::{value_parser, Parser},
    clap_complete::Shell,
};

/// Command line utility for importing and managing Nspawn-compatible images.
#[derive(Parser)]
#[command(author, version, about, long_about = None, arg_required_else_help = true)]
pub struct Arguments {
    #[clap(subcommand)]
    pub cmd: Option<Commands>,

    /// Fixed width of the table in characters. (default: auto)
    #[arg(short, long, default_value = None)]
    pub width: Option<u16>,

    /// Generate completion file for the specified shell. By default it's printed to stdout, so
    /// remember to redirect the output to a file, e.g. `nspawn --autocomplete bash > nspawn.bash`.
    /// Once you have the file, don't forget to source it for your shell, e.g. `source nspawn.bash`.
    #[arg(long, value_parser = value_parser!(Shell), exclusive = true, verbatim_doc_comment)]
    pub autocomplete: Option<Shell>,
}

#[derive(Parser)]
pub enum Commands {
    /// Manage local and remote images. You can list, remove, import, export, rename, and clone images.
    Images(ImagesArgs),
    /// Manage machines. You can start, stop, kill, terminate, login, get a shell and more.
    Machines(MachinesArgs),
}
