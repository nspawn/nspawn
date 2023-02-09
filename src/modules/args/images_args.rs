use clap::Parser;

#[derive(Parser)]
pub struct ImagesArgs {
    #[clap(subcommand)]
    pub commands: ImageCommands,
}

#[derive(Parser)]
pub enum ImageCommands {
    /// List local images
    List(List),
    /// Start images
    Start(Start),
    /// Remove images
    Remove(Remove),
    /// Set image properties
    Set(Set),
    /// Clone images
    Clone(Clone),
    /// Rename images
    Rename(Rename),
    /// Operate with remote images
    Remote(Remote),
}

#[derive(Parser, Clone)]
pub struct List {
    /// List only read-only images.
    #[arg(short, long)]
    pub ro: bool,
    /// List images by type.
    #[arg(short, long)]
    pub type_field: Option<String>,
    /// List images by name pattern.
    #[arg(short, long)]
    pub pattern: Option<String>,
}

#[derive(Parser, Clone)]
pub struct Start {
    /// Start all machines.
    #[arg(short, long, default_value_t = false)]
    pub all: bool,
    /// Start machines matching the specified pattern.
    #[arg(short, long, default_value = None)]
    pub pattern: Option<String>,
    /// Start a machine by name.
    pub machine: Option<String>,
}

#[derive(Debug, Parser, Clone)]
pub struct Remove {
    /// Remove all images.
    #[arg(short, long, default_value_t = false)]
    pub all: bool,
    /// Remove images matching the specified pattern.
    #[arg(short, long, default_value = None)]
    pub pattern: Option<String>,
    /// Name of the image to set properties for.
    pub image: Option<String>,
}

#[derive(Parser, Clone)]
pub struct Set {
    /// Set image to read-only.
    #[arg(long = "ro")]
    pub read_only: bool,
    /// Set image to read-write.
    #[arg(short, long = "rw")]
    pub read_write: bool,
    /// Name of the image to set properties for.
    pub image: String,
}

#[derive(Parser, Clone)]
pub struct Clone {
    /// Name of the image to clone.
    pub current_image: String,
    /// Name of the new image.
    pub new_image: String,
}

#[derive(Parser, Clone)]
pub struct Rename {
    /// Name of the image to rename.
    pub current_image: String,
    /// New name of the image.
    pub new_image: String,
}

#[derive(Parser, Clone)]
pub struct Remote {
    #[clap(subcommand)]
    pub remote_ops: RemoteOps,
}

#[derive(Parser, Clone)]
pub enum RemoteOps {
    /// List remote images
    List(RemoteList),
    /// Import remote images
    Import(RemoteImport),
}

#[derive(Parser, Clone)]
pub struct RemoteList {
    /// List images by name pattern.
    #[arg(short, long)]
    pub pattern: Option<String>,
    /// Server to list images from.
    /// If not specified, the default server will be used.
    #[arg(short, long, default_value = Some("https://hub.nspawn.org"))]
    pub server: Option<String>,
}

#[derive(Parser, Clone)]
pub struct RemoteImport {
    /// Remote name of the image to import, the name is given by running `nspawn images remote list`.
    pub remote_name: String,
    /// Local name of the new image. If not specified, the remote name will be used.
    #[arg(short, long, default_value = None)]
    pub local_name: Option<String>,
    /// Server to import image from.
    /// If not specified, the default server will be used.
    #[arg(short, long, default_value = Some("https://hub.nspawn.org"))]
    pub server: Option<String>,
}
