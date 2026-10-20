//! What a build does, worked out from the Dockerfile and the build's options before a
//! step runs: the stages it needs, the base each starts from, and each stage's steps
//! with the image settings they leave. The executor runs the steps; nothing here
//! touches the filesystem or the network.

use std::collections::BTreeMap;

use crate::build::image::Config;

/// The platform builds are for: this host's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Platform {
    pub os: String,
    pub architecture: String,
    pub variant: Option<String>,
}

impl Platform {
    pub fn host() -> Platform {
        Platform {
            os: "linux".to_string(),
            architecture: crate::oci::host_architecture().to_string(),
            variant: None,
        }
    }
}

/// What the build was asked for, besides the Dockerfile.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    /// --build-arg KEY=VALUE, the client having taken values given as KEY alone from its
    /// environment.
    pub build_args: BTreeMap<String, String>,
    /// --target: the stage to build; the last one otherwise.
    pub target: Option<String>,
    /// --label: on top of what the Dockerfile sets.
    pub labels: BTreeMap<String, String>,
}

/// Where a stage starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BaseRef {
    Scratch,
    /// An earlier stage, by its index among the Dockerfile's stages.
    Stage(usize),
    /// An image, its reference with the meta ARGs expanded, as written (not normalized).
    Image(String),
}

/// A stage of the Dockerfile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageInfo {
    /// Its position among the Dockerfile's stages.
    pub index: usize,
    /// The name of FROM ... AS NAME, lower case; empty without one.
    pub name: String,
    pub base: BaseRef,
}

/// The Dockerfile read: its stages, which of them the target needs, and the meta ARGs.
#[derive(Debug, Clone)]
pub struct Prepared {
    pub stages: Vec<StageInfo>,
    /// The stages to build, every one after those it takes from (its base, COPY --from,
    /// RUN --mount from=), the target last. A stage that only the ONBUILD triggers of a
    /// base image take from is not among them: the steps `dispatch` makes of those
    /// triggers name it (always an earlier stage), to be built before they run.
    pub order: Vec<usize>,
    pub target: usize,
    /// The parsed stages, for `dispatch`.
    pub(crate) parsed: Vec<crate::build::dockerfile::instructions::Stage>,
    /// The meta ARGs (before the first FROM) in order, with their values once the build
    /// arguments are applied.
    pub(crate) meta_args: Vec<(String, Option<String>)>,
    /// The escape token of the Dockerfile.
    pub(crate) escape: char,
    /// What each stage `dispatch` worked out leaves to the stages FROM it besides its
    /// config (its ARGs in scope, its environment with them, its author), filled by
    /// `dispatch`: a stage is dispatched with the same `Prepared` as the stage it starts
    /// from.
    pub(crate) inherited:
        std::sync::Arc<std::sync::Mutex<BTreeMap<usize, crate::build::dispatch::Inherited>>>,
}

/// Where the files of a COPY or ADD source or a RUN mount come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum From {
    /// The build context.
    Context,
    /// The result of an earlier stage, by index.
    Stage(usize),
    /// An image of a registry (COPY --from=IMAGE), its reference as written.
    Image(String),
    /// An empty filesystem: `scratch` named where a stage or an image is expected.
    Scratch,
}

/// A file of a heredoc given as a COPY or ADD source, or the script of a RUN.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inline {
    pub name: String,
    pub data: String,
}

/// What a step does to the filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Run(Run),
    Copy(Copy),
    /// WORKDIR: the directory made, owned by `user` when it is not root.
    Workdir {
        path: String,
        user: String,
    },
}

/// A mount of RUN --mount, its options checked and expanded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunMount {
    Bind {
        target: String,
        from: From,
        source: String,
        read_write: bool,
    },
    Cache {
        target: String,
        /// The cache's id: its target unless given.
        id: String,
        /// From an earlier stage or the context, the content a new cache starts with.
        from: Option<(From, String)>,
        /// shared, private or locked.
        sharing: String,
        read_only: bool,
        mode: u32,
        uid: u32,
        gid: u32,
    },
    Tmpfs {
        target: String,
        size: Option<u64>,
    },
    Secret {
        id: String,
        /// The file's path, /run/secrets/ID unless given; None with env= alone.
        target: Option<String>,
        /// The variable it is also given as.
        env: Option<String>,
        required: bool,
        mode: u32,
        uid: u32,
        gid: u32,
    },
}

/// RUN's network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    Default,
    None,
    Host,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    /// The program and its arguments, the shell prepended for the shell form.
    pub argv: Vec<String>,
    /// A heredoc script that starts with #!: written to a file the program then is,
    /// /dev/pipes/NAME, mode 0755, on a read-only mount of its own.
    pub script: Option<Inline>,
    /// The process's whole environment: the image's, then the build arguments in scope,
    /// then the proxy variables of the build arguments (HTTP_PROXY and http_proxy, ...).
    pub env: Vec<String>,
    /// USER as the image has it at that point; empty for root.
    pub user: String,
    /// The working directory, never empty.
    pub workdir: String,
    pub mounts: Vec<RunMount>,
    pub network: Network,
    /// --security=insecure was asked for.
    pub insecure: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Copy {
    /// ADD rather than COPY: local archives unpacked, URLs fetched.
    pub add: bool,
    pub from: From,
    /// Source paths, expanded, relative to the source's root; wildcards kept. For ADD,
    /// http:// and https:// URLs among them, fetched to the file `dispatch::url_file_name`
    /// names.
    pub sources: Vec<String>,
    pub inline: Vec<Inline>,
    /// The destination, made absolute against WORKDIR, a trailing slash kept.
    pub dest: String,
    /// --chown as written after expansion (USER[:GROUP], names or numbers), resolved
    /// against the destination's /etc/passwd and /etc/group by the executor.
    pub chown: Option<String>,
    /// --chmod: octal or symbolic.
    pub chmod: Option<String>,
    /// --link, false with --chmod as the reference copies such a COPY.
    pub link: bool,
    pub parents: bool,
    pub exclude: Vec<String>,
    /// ADD --checksum.
    pub checksum: Option<String>,
    /// ADD --keep-git-dir.
    pub keep_git_dir: bool,
    /// ADD --unpack: None leaves it to whether the source is a local archive.
    pub unpack: Option<bool>,
    /// The user the files belong to without --chown: root unless the stage runs as
    /// another user (COPY keeps root; this is for what the executor makes on the way).
    pub user: String,
}

/// One instruction's mark on the image: a layer when it has an action, its history line
/// either way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// The history's created_by, as the reference writes it.
    pub created_by: String,
    pub action: Option<Action>,
    /// What the progress output names the step by: the instruction as written, with
    /// build arguments expanded.
    pub description: String,
}

/// A stage worked out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dispatched {
    pub steps: Vec<Step>,
    /// The image settings once every step ran (history and rootfs are the executor's).
    pub config: Config,
    /// MAINTAINER's name, here or in a stage this one starts from: the image's author,
    /// which the config does not carry.
    pub author: Option<String>,
}
