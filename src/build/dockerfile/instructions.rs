//! Typed instructions and build stages from the AST (instructions/parse.go,
//! commands.go, support.go; the flags in bflag.go and the RUN flag files).

use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

use super::compat::{
    equal_fold, parse_duration, parse_int, quote, to_lower, to_upper, with_suggestion,
};
use super::parser::{chomp_heredoc_content, must_parse_heredoc, Heredoc, Node};
use super::run_flags::parse_mount;
use super::{Error, Range};

// Part of the interface of the module; nothing in the crate names them yet.
#[cfg_attr(not(test), allow(unused_imports))]
pub use super::bflag::{BFlags, Flag, FlagId, FlagType};
#[cfg_attr(not(test), allow(unused_imports))]
pub use super::run_flags::{
    parse_device, Device, Mount, MountType, NetworkMode, Security, ShareMode,
};

/// Expands one word, as the dispatcher does with the shell lexer and the
/// current environment.
pub type SingleWordExpander<'a> = dyn FnMut(&str) -> Result<String, Error> + 'a;

/// A command whose words are expanded before it runs.
pub trait SupportsSingleWordExpansion {
    fn expand(&mut self, expander: &mut SingleWordExpander<'_>) -> Result<(), Error>;
}

/// A command with content expanded with as few transformations as possible
/// (quotes kept), the inline files of ADD and COPY.
pub trait SupportsSingleWordExpansionRaw {
    fn expand_raw(&mut self, expander: &mut SingleWordExpander<'_>) -> Result<(), Error>;
}

/// A command that only some platforms support.
pub trait PlatformSpecific {
    fn check_platform(&self, platform: &str) -> Result<(), Error>;
}

/// A key and a value in the order written; `no_delim` marks the `KEY value`
/// form, written without `=`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyValuePair {
    pub key: String,
    pub value: String,
    pub no_delim: bool,
}

impl fmt::Display for KeyValuePair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}={}", self.key, self.value)
    }
}

/// A key with an optional value, as ARG takes them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyValuePairOptional {
    pub key: String,
    pub value: Option<String>,
    /// The text after `KEY ` in a comment line right above the instruction.
    pub doc_comment: String,
}

impl KeyValuePairOptional {
    pub fn value_string(&self) -> &str {
        self.value.as_deref().unwrap_or_default()
    }
}

impl fmt::Display for KeyValuePairOptional {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}={}", self.key, self.value_string())
    }
}

/// What every command carries: its source, its name as written, where it
/// is and the comments above it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandBase {
    code: String,
    name: String,
    location: Vec<Range>,
    comments: Vec<String>,
}

impl CommandBase {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn location(&self) -> &[Range] {
        &self.location
    }

    pub fn comments(&self) -> &[String] {
        &self.comments
    }

    /// The instruction as written, trimmed.
    pub fn code(&self) -> &str {
        &self.code
    }
}

macro_rules! command_base {
    ($($t:ty),* $(,)?) => {
        $(
            impl $t {
                pub fn base(&self) -> &CommandBase {
                    &self.base
                }

                pub fn name(&self) -> &str {
                    self.base.name()
                }

                pub fn location(&self) -> &[Range] {
                    self.base.location()
                }

                pub fn comments(&self) -> &[String] {
                    self.base.comments()
                }

                pub fn code(&self) -> &str {
                    self.base.code()
                }
            }
        )*
    };
}

command_base!(
    EnvCommand,
    MaintainerCommand,
    LabelCommand,
    AddCommand,
    CopyCommand,
    OnbuildCommand,
    WorkdirCommand,
    RunCommand,
    CmdCommand,
    HealthCheckCommand,
    EntrypointCommand,
    ExposeCommand,
    UserCommand,
    VolumeCommand,
    StopSignalCommand,
    ArgCommand,
    ShellCommand,
);

fn expand_kvps_in_place(
    kvps: &mut [KeyValuePair],
    expander: &mut SingleWordExpander<'_>,
) -> Result<(), Error> {
    for kvp in kvps {
        let key = expander(&kvp.key)?;
        let value = expander(&kvp.value)?;
        kvp.key = key;
        kvp.value = value;
    }
    Ok(())
}

fn expand_slice_in_place(
    values: &mut [String],
    expander: &mut SingleWordExpander<'_>,
) -> Result<(), Error> {
    for v in values {
        *v = expander(v)?;
    }
    Ok(())
}

/// `ENV key=value ...` or `ENV key value`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvCommand {
    base: CommandBase,
    pub env: Vec<KeyValuePair>,
}

impl SupportsSingleWordExpansion for EnvCommand {
    fn expand(&mut self, expander: &mut SingleWordExpander<'_>) -> Result<(), Error> {
        expand_kvps_in_place(&mut self.env, expander)
    }
}

/// `MAINTAINER name` (deprecated).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaintainerCommand {
    base: CommandBase,
    pub maintainer: String,
}

/// `LABEL key=value ...`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelCommand {
    base: CommandBase,
    pub labels: Vec<KeyValuePair>,
    no_expand: bool,
}

impl LabelCommand {
    /// A LABEL that is not written in the Dockerfile (NewLabelCommand);
    /// `no_expand` leaves its words alone during expansion.
    pub fn new(key: &str, value: &str, no_expand: bool) -> LabelCommand {
        let kvp = KeyValuePair {
            key: key.to_string(),
            value: value.to_string(),
            no_delim: false,
        };
        LabelCommand {
            base: CommandBase {
                code: format!("LABEL {kvp}"),
                name: "label".to_string(),
                location: Vec::new(),
                comments: Vec::new(),
            },
            labels: vec![kvp],
            no_expand,
        }
    }
}

impl SupportsSingleWordExpansion for LabelCommand {
    fn expand(&mut self, expander: &mut SingleWordExpander<'_>) -> Result<(), Error> {
        if self.no_expand {
            return Ok(());
        }
        expand_kvps_in_place(&mut self.labels, expander)
    }
}

/// An inline file of ADD or COPY, from a heredoc.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceContent {
    pub path: String,
    pub data: String,
    /// Expand variables in `data` (the heredoc delimiter is unquoted).
    pub expand: bool,
}

/// The sources and the destination of ADD and COPY.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourcesAndDest {
    pub dest_path: String,
    pub source_paths: Vec<String>,
    pub source_contents: Vec<SourceContent>,
}

impl SourcesAndDest {
    pub fn expand(&mut self, expander: &mut SingleWordExpander<'_>) -> Result<(), Error> {
        expand_slice_in_place(&mut self.source_paths, expander)?;
        self.dest_path = expander(&self.dest_path)?;
        Ok(())
    }

    pub fn expand_raw(&mut self, expander: &mut SingleWordExpander<'_>) -> Result<(), Error> {
        for content in &mut self.source_contents {
            if content.expand {
                content.data = expander(&content.data)?;
            }
        }
        Ok(())
    }
}

/// `ADD [flags] src... dest`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddCommand {
    base: CommandBase,
    pub sources_and_dest: SourcesAndDest,
    pub chown: String,
    pub chmod: String,
    pub link: bool,
    pub exclude_patterns: Vec<String>,
    /// `--keep-git-dir`, `None` when not given.
    pub keep_git_dir: Option<bool>,
    pub checksum: String,
    /// `--unpack`, `None` when not given.
    pub unpack: Option<bool>,
}

impl SupportsSingleWordExpansion for AddCommand {
    fn expand(&mut self, expander: &mut SingleWordExpander<'_>) -> Result<(), Error> {
        self.chown = expander(&self.chown)?;
        self.chmod = expander(&self.chmod)?;
        self.checksum = expander(&self.checksum)?;
        self.sources_and_dest.expand(expander)
    }
}

impl SupportsSingleWordExpansionRaw for AddCommand {
    fn expand_raw(&mut self, expander: &mut SingleWordExpander<'_>) -> Result<(), Error> {
        self.sources_and_dest.expand_raw(expander)
    }
}

/// `COPY [flags] src... dest`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyCommand {
    base: CommandBase,
    pub sources_and_dest: SourcesAndDest,
    pub from: String,
    pub chown: String,
    pub chmod: String,
    pub link: bool,
    pub exclude_patterns: Vec<String>,
    pub parents: bool,
}

impl SupportsSingleWordExpansion for CopyCommand {
    fn expand(&mut self, expander: &mut SingleWordExpander<'_>) -> Result<(), Error> {
        self.chown = expander(&self.chown)?;
        self.chmod = expander(&self.chmod)?;
        self.sources_and_dest.expand(expander)
    }
}

impl SupportsSingleWordExpansionRaw for CopyCommand {
    fn expand_raw(&mut self, expander: &mut SingleWordExpander<'_>) -> Result<(), Error> {
        self.sources_and_dest.expand_raw(expander)
    }
}

/// `ONBUILD instruction`: the instruction is kept as text, with its
/// heredocs appended, to be parsed by the builds that use the image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnbuildCommand {
    base: CommandBase,
    pub expression: String,
}

/// `WORKDIR path`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkdirCommand {
    base: CommandBase,
    pub path: String,
}

impl SupportsSingleWordExpansion for WorkdirCommand {
    fn expand(&mut self, expander: &mut SingleWordExpander<'_>) -> Result<(), Error> {
        self.path = expander(&self.path)?;
        Ok(())
    }
}

/// An inline file of RUN, from a heredoc.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShellInlineFile {
    pub name: String,
    pub data: String,
    pub chomp: bool,
}

struct ShellDependantCmdLine {
    cmd_line: Vec<String>,
    files: Vec<ShellInlineFile>,
    prepend_shell: bool,
}

/// `RUN command` (shell form, run by the shell) or `RUN ["exec", "form"]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunCommand {
    base: CommandBase,
    /// The shell form as one element, or the exec form.
    pub cmd_line: Vec<String>,
    pub files: Vec<ShellInlineFile>,
    pub prepend_shell: bool,
    /// The names of the flags given.
    pub flags_used: Vec<String>,
    mount_specs: Vec<String>,
    mounts: Vec<Mount>,
    network: NetworkMode,
    security: Security,
    devices: Vec<Device>,
}

impl RunCommand {
    /// The `--mount` flags (GetMounts). Before [`expand`] only their syntax
    /// is checked and their keys are not applied.
    ///
    /// [`expand`]: SupportsSingleWordExpansion::expand
    pub fn mounts(&self) -> &[Mount] {
        &self.mounts
    }

    pub fn network(&self) -> NetworkMode {
        self.network
    }

    pub fn security(&self) -> Security {
        self.security
    }

    pub fn devices(&self) -> &[Device] {
        &self.devices
    }
}

impl SupportsSingleWordExpansion for RunCommand {
    /// Parses the `--mount` flags again with their values expanded.
    fn expand(&mut self, expander: &mut SingleWordExpander<'_>) -> Result<(), Error> {
        let mut mounts = Vec::with_capacity(self.mount_specs.len());
        for spec in &self.mount_specs {
            mounts.push(parse_mount(spec, Some(&mut *expander))?);
        }
        self.mounts = mounts;
        Ok(())
    }
}

/// `CMD command` or `CMD ["exec", "form"]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmdCommand {
    base: CommandBase,
    pub cmd_line: Vec<String>,
    pub files: Vec<ShellInlineFile>,
    pub prepend_shell: bool,
}

/// `HEALTHCHECK [flags] CMD command` or `HEALTHCHECK NONE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthCheckCommand {
    base: CommandBase,
    pub health: HealthcheckConfig,
}

/// The health check of an image, as in the image configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HealthcheckConfig {
    /// `["NONE"]`, `["CMD", args...]` or `["CMD-SHELL", command]`.
    pub test: Vec<String>,
    pub interval: Duration,
    pub timeout: Duration,
    pub start_period: Duration,
    pub start_interval: Duration,
    pub retries: u32,
}

/// `ENTRYPOINT command` or `ENTRYPOINT ["exec", "form"]`. An empty
/// `cmd_line` (`ENTRYPOINT []`) is the reference's nil command line, which
/// resets the entrypoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntrypointCommand {
    base: CommandBase,
    pub cmd_line: Vec<String>,
    pub files: Vec<ShellInlineFile>,
    pub prepend_shell: bool,
}

/// `EXPOSE port[/proto]...`, sorted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExposeCommand {
    base: CommandBase,
    pub ports: Vec<String>,
}

/// `USER user[:group]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserCommand {
    base: CommandBase,
    pub user: String,
}

impl SupportsSingleWordExpansion for UserCommand {
    fn expand(&mut self, expander: &mut SingleWordExpander<'_>) -> Result<(), Error> {
        self.user = expander(&self.user)?;
        Ok(())
    }
}

/// `VOLUME path...`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeCommand {
    base: CommandBase,
    pub volumes: Vec<String>,
}

impl SupportsSingleWordExpansion for VolumeCommand {
    fn expand(&mut self, expander: &mut SingleWordExpander<'_>) -> Result<(), Error> {
        expand_slice_in_place(&mut self.volumes, expander)
    }
}

/// `STOPSIGNAL signal`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopSignalCommand {
    base: CommandBase,
    pub signal: String,
}

impl SupportsSingleWordExpansion for StopSignalCommand {
    fn expand(&mut self, expander: &mut SingleWordExpander<'_>) -> Result<(), Error> {
        self.signal = expander(&self.signal)?;
        Ok(())
    }
}

impl PlatformSpecific for StopSignalCommand {
    fn check_platform(&self, platform: &str) -> Result<(), Error> {
        if platform == "windows" {
            return Err(Error::new(
                "The daemon on this platform does not support the command stopsignal",
            ));
        }
        Ok(())
    }
}

/// `ARG name[=default] ...`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgCommand {
    base: CommandBase,
    pub args: Vec<KeyValuePairOptional>,
}

impl SupportsSingleWordExpansion for ArgCommand {
    fn expand(&mut self, expander: &mut SingleWordExpander<'_>) -> Result<(), Error> {
        for arg in &mut self.args {
            let key = expander(&arg.key)?;
            let value = match &arg.value {
                Some(v) => Some(expander(v)?),
                None => None,
            };
            arg.key = key;
            arg.value = value;
        }
        Ok(())
    }
}

/// `SHELL ["executable", "params"]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellCommand {
    base: CommandBase,
    pub shell: Vec<String>,
}

/// Any instruction but FROM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Env(EnvCommand),
    Maintainer(MaintainerCommand),
    Label(LabelCommand),
    Add(AddCommand),
    Copy(CopyCommand),
    Onbuild(OnbuildCommand),
    Workdir(WorkdirCommand),
    Run(RunCommand),
    Cmd(CmdCommand),
    Healthcheck(HealthCheckCommand),
    Entrypoint(EntrypointCommand),
    Expose(ExposeCommand),
    User(UserCommand),
    Volume(VolumeCommand),
    StopSignal(StopSignalCommand),
    Arg(ArgCommand),
    Shell(ShellCommand),
}

impl Command {
    pub fn base(&self) -> &CommandBase {
        match self {
            Command::Env(c) => &c.base,
            Command::Maintainer(c) => &c.base,
            Command::Label(c) => &c.base,
            Command::Add(c) => &c.base,
            Command::Copy(c) => &c.base,
            Command::Onbuild(c) => &c.base,
            Command::Workdir(c) => &c.base,
            Command::Run(c) => &c.base,
            Command::Cmd(c) => &c.base,
            Command::Healthcheck(c) => &c.base,
            Command::Entrypoint(c) => &c.base,
            Command::Expose(c) => &c.base,
            Command::User(c) => &c.base,
            Command::Volume(c) => &c.base,
            Command::StopSignal(c) => &c.base,
            Command::Arg(c) => &c.base,
            Command::Shell(c) => &c.base,
        }
    }

    /// The instruction name as written, `RUN` or `run`.
    pub fn name(&self) -> &str {
        self.base().name()
    }

    pub fn location(&self) -> &[Range] {
        self.base().location()
    }

    pub fn comments(&self) -> &[String] {
        self.base().comments()
    }

    pub fn code(&self) -> &str {
        self.base().code()
    }

    /// The command as [`SupportsSingleWordExpansion`] when it is one.
    pub fn as_single_word_expansion(&mut self) -> Option<&mut dyn SupportsSingleWordExpansion> {
        match self {
            Command::Env(c) => Some(c),
            Command::Label(c) => Some(c),
            Command::Add(c) => Some(c),
            Command::Copy(c) => Some(c),
            Command::Workdir(c) => Some(c),
            Command::Run(c) => Some(c),
            Command::User(c) => Some(c),
            Command::Volume(c) => Some(c),
            Command::StopSignal(c) => Some(c),
            Command::Arg(c) => Some(c),
            _ => None,
        }
    }

    /// The command as [`SupportsSingleWordExpansionRaw`] when it is one.
    pub fn as_single_word_expansion_raw(
        &mut self,
    ) -> Option<&mut dyn SupportsSingleWordExpansionRaw> {
        match self {
            Command::Add(c) => Some(c),
            Command::Copy(c) => Some(c),
            _ => None,
        }
    }

    /// The command as [`PlatformSpecific`] when it is one.
    pub fn as_platform_specific(&self) -> Option<&dyn PlatformSpecific> {
        match self {
            Command::StopSignal(c) => Some(c),
            _ => None,
        }
    }
}

/// A build stage: the FROM that starts it and the commands up to the next
/// FROM.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stage {
    /// The name given with `AS`, lowercased; empty when none.
    pub name: String,
    pub commands: Vec<Command>,
    /// FROM as written, for its casing.
    pub orig_cmd: String,
    /// The image or stage the stage starts from.
    pub base_name: String,
    /// `--platform`, empty when not given.
    pub platform: String,
    /// The comment right above FROM that starts with the stage name.
    pub doc_comment: String,
    /// The FROM instruction as written, trimmed.
    pub source_code: String,
    pub location: Vec<Range>,
    pub comments: Vec<String>,
}

impl Stage {
    pub fn add_command(&mut self, cmd: Command) {
        self.commands.push(cmd);
    }
}

/// Whether `name` is the name of the last stage.
pub fn is_current_stage(stages: &[Stage], name: &str) -> bool {
    stages.last().is_some_and(|s| s.name == name)
}

/// The last stage.
pub fn current_stage(stages: &mut [Stage]) -> Result<&mut Stage, Error> {
    stages
        .last_mut()
        .ok_or_else(|| Error::new("no build stage in current context"))
}

/// The index of the stage called `name`, compared without case.
pub fn has_stage(stages: &[Stage], name: &str) -> Option<usize> {
    stages.iter().position(|s| equal_fold(&s.name, name))
}

/// What an instruction becomes: a FROM starts a stage, anything else is a
/// command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Instruction {
    Stage(Stage),
    Command(Command),
}

/// The instruction names, uppercase, offered as suggestions for an unknown
/// one. The reference takes them from a map in random order; the order only
/// matters between equally close names.
const ALL_INSTRUCTION_NAMES: [&str; 18] = [
    "ADD",
    "ARG",
    "CMD",
    "COPY",
    "ENTRYPOINT",
    "ENV",
    "EXPOSE",
    "FROM",
    "HEALTHCHECK",
    "LABEL",
    "MAINTAINER",
    "ONBUILD",
    "RUN",
    "SHELL",
    "STOPSIGNAL",
    "USER",
    "VOLUME",
    "WORKDIR",
];

struct ParseRequest {
    command: String,
    args: Vec<String>,
    heredocs: Vec<Heredoc>,
    json: bool,
    flags: BFlags,
    original: String,
    location: Vec<Range>,
    comments: Vec<String>,
}

impl ParseRequest {
    fn from_node(node: &Node) -> ParseRequest {
        ParseRequest {
            command: node.value.clone(),
            args: node_args(node),
            heredocs: node.heredocs.clone(),
            json: node.json,
            flags: BFlags::with_args(node.flags.clone()),
            original: node.original.clone(),
            location: node.location(),
            comments: node.prev_comment.clone(),
        }
    }

    fn base(&self) -> CommandBase {
        CommandBase {
            code: self.original.trim().to_string(),
            name: self.command.clone(),
            location: self.location.clone(),
            comments: self.comments.clone(),
        }
    }
}

/// The arguments of an instruction node; the sub-instruction of ONBUILD
/// contributes its name and its own arguments.
fn node_args(node: &Node) -> Vec<String> {
    let mut result = Vec::new();
    let mut stack = vec![node.next.as_deref()];
    while let Some(cursor) = stack.pop() {
        let Some(arg) = cursor else { continue };
        stack.push(arg.next.as_deref());
        match arg.children.as_slice() {
            [] => result.push(arg.value.clone()),
            [child] => {
                result.push(child.value.clone());
                stack.push(child.next.as_deref());
            }
            _ => {}
        }
    }
    result
}

/// Parses the instruction of one AST node.
pub fn parse_instruction(node: &Node) -> Result<Instruction, Error> {
    parse_instruction_node(node).map_err(|e| e.with_location(node.location()))
}

fn parse_instruction_node(node: &Node) -> Result<Instruction, Error> {
    let mut req = ParseRequest::from_node(node);
    let req = &mut req;
    let cmd = match to_lower(&node.value).as_str() {
        "env" => Command::Env(parse_env(req)?),
        "maintainer" => Command::Maintainer(parse_maintainer(req)?),
        "label" => Command::Label(parse_label(req)?),
        "add" => Command::Add(parse_add(req)?),
        "copy" => Command::Copy(parse_copy(req)?),
        "from" => return Ok(Instruction::Stage(parse_from(req)?)),
        "onbuild" => Command::Onbuild(parse_on_build(req)?),
        "workdir" => Command::Workdir(parse_workdir(req)?),
        "run" => Command::Run(parse_run(req)?),
        "cmd" => Command::Cmd(parse_cmd(req)?),
        "healthcheck" => Command::Healthcheck(parse_healthcheck(req)?),
        "entrypoint" => Command::Entrypoint(parse_entrypoint(req)?),
        "expose" => Command::Expose(parse_expose(req)?),
        "user" => Command::User(parse_user(req)?),
        "volume" => Command::Volume(parse_volume(req)?),
        "stopsignal" => Command::StopSignal(parse_stop_signal(req)?),
        "arg" => Command::Arg(parse_arg(req)?),
        "shell" => Command::Shell(parse_shell(req)?),
        _ => {
            return Err(Error::new(with_suggestion(
                format!("unknown instruction: {}", node.value),
                &node.value,
                &ALL_INSTRUCTION_NAMES,
                false,
            )))
        }
    };
    Ok(Instruction::Command(cmd))
}

/// Parses the command of one AST node; FROM is not a command.
pub fn parse_command(node: &Node) -> Result<Command, Error> {
    match parse_instruction(node)? {
        Instruction::Command(c) => Ok(c),
        Instruction::Stage(_) => {
            Err(Error::new("*instructions.Stage is not a command type")
                .with_location(node.location()))
        }
    }
}

/// Parses a whole AST into its stages and the ARGs before the first FROM.
pub fn parse(ast: &Node) -> Result<(Vec<Stage>, Vec<ArgCommand>), Error> {
    let mut stages: Vec<Stage> = Vec::new();
    let mut meta_args = Vec::new();
    for n in &ast.children {
        let cmd = parse_instruction(n)
            .map_err(|e| e.wrap(&format!("dockerfile parse error on line {}", n.start_line)))?;
        match cmd {
            Instruction::Command(Command::Arg(a)) if stages.is_empty() => meta_args.push(a),
            Instruction::Stage(s) => stages.push(s),
            Instruction::Command(c) => {
                let stage =
                    current_stage(&mut stages).map_err(|e| e.with_location(n.location()))?;
                stage.add_command(c);
            }
        }
    }
    Ok((stages, meta_args))
}

fn err_at_least_one_argument(command: &str) -> Error {
    Error::new(format!("{command} requires at least one argument"))
}

fn err_exactly_one_argument(command: &str) -> Error {
    Error::new(format!("{command} requires exactly one argument"))
}

fn err_no_destination_argument(command: &str) -> Error {
    Error::new(format!(
        "{command} requires at least two arguments, but only one was provided. Destination could not be determined"
    ))
}

fn err_bad_heredoc(command: &str, option: &str) -> Error {
    Error::new(format!("{command} cannot accept a heredoc as {option}"))
}

fn err_blank_command_names(command: &str) -> Error {
    Error::new(format!("{command} names can not be blank"))
}

fn err_too_many_arguments(command: &str) -> Error {
    Error::new(format!("Bad input to {command}, too many arguments"))
}

fn err_not_json(command: &str) -> Error {
    Error::new(format!(
        "{command} requires the arguments to be in JSON form"
    ))
}

fn parse_kvps(args: &[String], cmd_name: &str) -> Result<Vec<KeyValuePair>, Error> {
    if args.is_empty() {
        return Err(err_at_least_one_argument(cmd_name));
    }
    if !args.len().is_multiple_of(3) {
        return Err(err_too_many_arguments(cmd_name));
    }
    let mut res = Vec::with_capacity(args.len() / 3);
    for triple in args.chunks(3) {
        if triple[0].is_empty() {
            return Err(err_blank_command_names(cmd_name));
        }
        res.push(KeyValuePair {
            key: triple[0].clone(),
            value: triple[1].clone(),
            no_delim: triple[2].is_empty(),
        });
    }
    Ok(res)
}

fn parse_env(req: &mut ParseRequest) -> Result<EnvCommand, Error> {
    req.flags.parse()?;
    let env = parse_kvps(&req.args, "ENV")?;
    Ok(EnvCommand {
        base: req.base(),
        env,
    })
}

fn parse_maintainer(req: &mut ParseRequest) -> Result<MaintainerCommand, Error> {
    if req.args.len() != 1 {
        return Err(err_exactly_one_argument("MAINTAINER"));
    }
    req.flags.parse()?;
    Ok(MaintainerCommand {
        base: req.base(),
        maintainer: req.args[0].clone(),
    })
}

fn parse_label(req: &mut ParseRequest) -> Result<LabelCommand, Error> {
    req.flags.parse()?;
    let labels = parse_kvps(&req.args, "LABEL")?;
    Ok(LabelCommand {
        base: req.base(),
        labels,
        no_expand: false,
    })
}

fn parse_sources_and_dest(req: &ParseRequest, command: &str) -> Result<SourcesAndDest, Error> {
    let Some((dest, srcs)) = req.args.split_last() else {
        return Err(err_no_destination_argument(command));
    };
    if must_parse_heredoc(dest).is_some() {
        return Err(err_bad_heredoc(command, "a destination"));
    }

    let heredoc_lookup: HashMap<&str, &Heredoc> =
        req.heredocs.iter().map(|h| (h.name.as_str(), h)).collect();

    let mut source_paths = Vec::new();
    let mut source_contents = Vec::new();
    for src in srcs {
        match must_parse_heredoc(src) {
            Some(heredoc) => {
                let mut content = heredoc_lookup
                    .get(heredoc.name.as_str())
                    .map(|h| h.content.clone())
                    .unwrap_or_default();
                if heredoc.chomp {
                    content = chomp_heredoc_content(&content);
                }
                source_contents.push(SourceContent {
                    data: content,
                    path: heredoc.name,
                    expand: heredoc.expand,
                });
            }
            None => source_paths.push(src.clone()),
        }
    }
    Ok(SourcesAndDest {
        dest_path: dest.clone(),
        source_paths,
        source_contents,
    })
}

fn parse_add(req: &mut ParseRequest) -> Result<AddCommand, Error> {
    if req.args.len() < 2 {
        return Err(err_no_destination_argument("ADD"));
    }
    let fl_chown = req.flags.add_string("chown", "");
    let fl_chmod = req.flags.add_string("chmod", "");
    let fl_link = req.flags.add_bool("link", false);
    let fl_keep_git_dir = req.flags.add_bool("keep-git-dir", false);
    let fl_checksum = req.flags.add_string("checksum", "");
    let fl_unpack = req.flags.add_bool("unpack", false);
    let fl_excludes = req.flags.add_strings("exclude");
    req.flags.parse()?;

    let sources_and_dest = parse_sources_and_dest(req, "ADD")?;
    let f = &req.flags;
    let given_bool = |id: FlagId| f.is_used(id).then(|| f.flag(id).value == "true");
    Ok(AddCommand {
        base: req.base(),
        sources_and_dest,
        chown: f.flag(fl_chown).value.clone(),
        chmod: f.flag(fl_chmod).value.clone(),
        link: f.flag(fl_link).value == "true",
        exclude_patterns: f.flag(fl_excludes).string_values.clone(),
        keep_git_dir: given_bool(fl_keep_git_dir),
        checksum: f.flag(fl_checksum).value.clone(),
        unpack: given_bool(fl_unpack),
    })
}

fn parse_copy(req: &mut ParseRequest) -> Result<CopyCommand, Error> {
    if req.args.len() < 2 {
        return Err(err_no_destination_argument("COPY"));
    }
    let fl_chown = req.flags.add_string("chown", "");
    let fl_from = req.flags.add_string("from", "");
    let fl_chmod = req.flags.add_string("chmod", "");
    let fl_link = req.flags.add_bool("link", false);
    let fl_excludes = req.flags.add_strings("exclude");
    let fl_parents = req.flags.add_bool("parents", false);
    req.flags.parse()?;

    let sources_and_dest = parse_sources_and_dest(req, "COPY")?;
    let f = &req.flags;
    Ok(CopyCommand {
        base: req.base(),
        sources_and_dest,
        from: f.flag(fl_from).value.clone(),
        chown: f.flag(fl_chown).value.clone(),
        chmod: f.flag(fl_chmod).value.clone(),
        link: f.flag(fl_link).value == "true",
        exclude_patterns: f.flag(fl_excludes).string_values.clone(),
        parents: f.flag(fl_parents).value == "true",
    })
}

fn parse_from(req: &mut ParseRequest) -> Result<Stage, Error> {
    let stage_name = parse_build_stage_name(&req.args)?;
    let fl_platform = req.flags.add_string("platform", "");
    req.flags.parse()?;
    let doc_comment = get_doc_comment(&req.comments, &stage_name);
    Ok(Stage {
        base_name: req.args[0].clone(),
        orig_cmd: req.command.clone(),
        name: stage_name,
        source_code: req.original.trim().to_string(),
        commands: Vec::new(),
        platform: req.flags.flag(fl_platform).value.clone(),
        location: req.location.clone(),
        comments: req.comments.clone(),
        doc_comment,
    })
}

/// `^[a-z][a-z0-9-_.]*$`
fn is_valid_stage_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b[0].is_ascii_lowercase()
        && b[1..].iter().all(|&c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'-' | b'_' | b'.')
        })
}

fn parse_build_stage_name(args: &[String]) -> Result<String, Error> {
    if args.len() == 3 && equal_fold(&args[1], "as") {
        let stage_name = to_lower(&args[2]);
        if !is_valid_stage_name(&stage_name) {
            return Err(Error::new(format!(
                "invalid name for build stage: {}, name can't start with a number or contain symbols",
                quote(&args[2])
            )));
        }
        return Ok(stage_name);
    }
    if args.len() != 1 {
        return Err(Error::new("FROM requires either one or three arguments"));
    }
    Ok(String::new())
}

/// The original line without `ONBUILD` and the blanks around it
/// (`(?i)^\s*ONBUILD\s*`).
fn strip_onbuild_prefix(s: &str) -> &str {
    let is_space = |c: char| matches!(c, '\t' | '\n' | '\x0c' | '\r' | ' ');
    let t = s.trim_start_matches(is_space);
    match t.get(..7) {
        Some(word) if word.eq_ignore_ascii_case("onbuild") => t[7..].trim_start_matches(is_space),
        _ => s,
    }
}

fn parse_on_build(req: &mut ParseRequest) -> Result<OnbuildCommand, Error> {
    if req.args.is_empty() {
        return Err(err_at_least_one_argument("ONBUILD"));
    }
    req.flags.parse()?;

    let trigger = to_upper(req.args[0].trim());
    match trigger.as_str() {
        "ONBUILD" => {
            return Err(Error::new(
                "Chaining ONBUILD via `ONBUILD ONBUILD` isn't allowed",
            ))
        }
        "MAINTAINER" | "FROM" => {
            return Err(Error::new(format!(
                "{trigger} isn't allowed as an ONBUILD trigger"
            )))
        }
        _ => {}
    }

    let mut expression = strip_onbuild_prefix(&req.original).to_string();
    for heredoc in &req.heredocs {
        expression.push('\n');
        expression.push_str(&heredoc.content);
        expression.push_str(&heredoc.name);
    }
    Ok(OnbuildCommand {
        base: req.base(),
        expression,
    })
}

fn parse_workdir(req: &mut ParseRequest) -> Result<WorkdirCommand, Error> {
    if req.args.len() != 1 {
        return Err(err_exactly_one_argument("WORKDIR"));
    }
    req.flags.parse()?;
    Ok(WorkdirCommand {
        base: req.base(),
        path: req.args[0].clone(),
    })
}

/// The exec form as it is, or the shell form joined into one string.
fn handle_json_args(args: &[String], json: bool) -> Vec<String> {
    if args.is_empty() {
        return Vec::new();
    }
    if json {
        return args.to_vec();
    }
    vec![args.join(" ")]
}

fn parse_shell_dependent_command(req: &ParseRequest) -> ShellDependantCmdLine {
    let files = req
        .heredocs
        .iter()
        .map(|h| ShellInlineFile {
            name: h.name.clone(),
            data: h.content.clone(),
            chomp: h.chomp,
        })
        .collect();
    ShellDependantCmdLine {
        cmd_line: handle_json_args(&req.args, req.json),
        files,
        prepend_shell: !req.json,
    }
}

fn parse_run(req: &mut ParseRequest) -> Result<RunCommand, Error> {
    // The flags of the RUN hooks, in the order the reference registers them.
    let fl_device = req.flags.add_strings("device");
    let fl_mount = req.flags.add_strings("mount");
    let fl_network = req
        .flags
        .add_string("network", NetworkMode::Default.as_str());
    let fl_security = req.flags.add_string("security", Security::Sandbox.as_str());

    req.flags.parse()?;
    let flags_used = req.flags.used();
    let line = parse_shell_dependent_command(req);
    let base = req.base();

    let f = &req.flags;
    let devices = f
        .flag(fl_device)
        .string_values
        .iter()
        .map(|d| parse_device(d))
        .collect::<Result<Vec<_>, _>>()?;
    let mount_specs = f.flag(fl_mount).string_values.clone();
    let mounts = mount_specs
        .iter()
        .map(|m| parse_mount(m, None))
        .collect::<Result<Vec<_>, _>>()?;
    let network = NetworkMode::parse(&f.flag(fl_network).value)?;
    let security = Security::parse(&f.flag(fl_security).value)?;

    Ok(RunCommand {
        base,
        cmd_line: line.cmd_line,
        files: line.files,
        prepend_shell: line.prepend_shell,
        flags_used,
        mount_specs,
        mounts,
        network,
        security,
        devices,
    })
}

fn parse_cmd(req: &mut ParseRequest) -> Result<CmdCommand, Error> {
    req.flags.parse()?;
    let line = parse_shell_dependent_command(req);
    Ok(CmdCommand {
        base: req.base(),
        cmd_line: line.cmd_line,
        files: line.files,
        prepend_shell: line.prepend_shell,
    })
}

fn parse_entrypoint(req: &mut ParseRequest) -> Result<EntrypointCommand, Error> {
    req.flags.parse()?;
    let line = parse_shell_dependent_command(req);
    Ok(EntrypointCommand {
        base: req.base(),
        cmd_line: line.cmd_line,
        files: line.files,
        prepend_shell: line.prepend_shell,
    })
}

/// The duration of a flag, zero when empty; a duration below one
/// millisecond other than zero is refused.
fn parse_opt_interval(flag: &Flag) -> Result<Duration, Error> {
    let s = &flag.value;
    if s.is_empty() {
        return Ok(Duration::ZERO);
    }
    let d = parse_duration(s).map_err(Error::new)?;
    if d == 0 {
        return Ok(Duration::ZERO);
    }
    const MINIMUM_NANOS: i64 = 1_000_000;
    if d < MINIMUM_NANOS {
        return Err(Error::new(format!(
            "Interval {} cannot be less than 1ms",
            quote(flag.name())
        )));
    }
    Ok(Duration::from_nanos(d.unsigned_abs()))
}

fn parse_healthcheck(req: &mut ParseRequest) -> Result<HealthCheckCommand, Error> {
    if req.args.is_empty() {
        return Err(err_at_least_one_argument("HEALTHCHECK"));
    }
    let base = req.base();
    let mut typ = to_upper(&req.args[0]);
    let args = &req.args[1..];

    // The flags are not even checked for NONE.
    if typ == "NONE" {
        if !args.is_empty() {
            return Err(Error::new("HEALTHCHECK NONE takes no arguments"));
        }
        return Ok(HealthCheckCommand {
            base,
            health: HealthcheckConfig {
                test: vec![typ],
                ..HealthcheckConfig::default()
            },
        });
    }

    let fl_interval = req.flags.add_string("interval", "");
    let fl_timeout = req.flags.add_string("timeout", "");
    let fl_start_period = req.flags.add_string("start-period", "");
    let fl_start_interval = req.flags.add_string("start-interval", "");
    let fl_retries = req.flags.add_string("retries", "");
    req.flags.parse()?;

    if typ != "CMD" {
        return Err(Error::new(format!(
            "Unknown type {} in HEALTHCHECK (try CMD)",
            quote(&typ)
        )));
    }
    let cmd_slice = handle_json_args(args, req.json);
    if cmd_slice.is_empty() {
        return Err(Error::new("Missing command after HEALTHCHECK CMD"));
    }
    if !req.json {
        typ = "CMD-SHELL".to_string();
    }
    let mut test = vec![typ];
    test.extend(cmd_slice);

    let f = &req.flags;
    let interval = parse_opt_interval(f.flag(fl_interval))?;
    let timeout = parse_opt_interval(f.flag(fl_timeout))?;
    let start_period = parse_opt_interval(f.flag(fl_start_period))?;
    let start_interval = parse_opt_interval(f.flag(fl_start_interval))?;
    let retries_value = &f.flag(fl_retries).value;
    let retries = if retries_value.is_empty() {
        0
    } else {
        let retries = parse_int(retries_value, 32).map_err(Error::new)?;
        if retries < 0 {
            return Err(Error::new(format!(
                "--retries cannot be negative ({retries})"
            )));
        }
        retries as u32
    };

    Ok(HealthCheckCommand {
        base,
        health: HealthcheckConfig {
            test,
            interval,
            timeout,
            start_period,
            start_interval,
            retries,
        },
    })
}

fn parse_expose(req: &mut ParseRequest) -> Result<ExposeCommand, Error> {
    if req.args.is_empty() {
        return Err(err_at_least_one_argument("EXPOSE"));
    }
    req.flags.parse()?;
    let mut ports = req.args.clone();
    ports.sort();
    Ok(ExposeCommand {
        base: req.base(),
        ports,
    })
}

fn parse_user(req: &mut ParseRequest) -> Result<UserCommand, Error> {
    if req.args.len() != 1 {
        return Err(err_exactly_one_argument("USER"));
    }
    req.flags.parse()?;
    Ok(UserCommand {
        base: req.base(),
        user: req.args[0].clone(),
    })
}

fn parse_volume(req: &mut ParseRequest) -> Result<VolumeCommand, Error> {
    if req.args.is_empty() {
        return Err(err_at_least_one_argument("VOLUME"));
    }
    req.flags.parse()?;
    let mut volumes = Vec::with_capacity(req.args.len());
    for v in &req.args {
        let v = v.trim();
        if v.is_empty() {
            return Err(Error::new("VOLUME specified can not be an empty string"));
        }
        volumes.push(v.to_string());
    }
    Ok(VolumeCommand {
        base: req.base(),
        volumes,
    })
}

// STOPSIGNAL and ARG never check their flags in the reference.

fn parse_stop_signal(req: &mut ParseRequest) -> Result<StopSignalCommand, Error> {
    if req.args.len() != 1 {
        return Err(err_exactly_one_argument("STOPSIGNAL"));
    }
    Ok(StopSignalCommand {
        base: req.base(),
        signal: req.args[0].clone(),
    })
}

fn parse_arg(req: &mut ParseRequest) -> Result<ArgCommand, Error> {
    if req.args.is_empty() {
        return Err(err_at_least_one_argument("ARG"));
    }
    let mut args = Vec::with_capacity(req.args.len());
    for arg in &req.args {
        let (key, value) = match arg.split_once('=') {
            Some((k, v)) => {
                if k.is_empty() {
                    return Err(err_blank_command_names("ARG"));
                }
                (k.to_string(), Some(v.to_string()))
            }
            None => (arg.clone(), None),
        };
        let doc_comment = get_doc_comment(&req.comments, &key);
        args.push(KeyValuePairOptional {
            key,
            value,
            doc_comment,
        });
    }
    Ok(ArgCommand {
        base: req.base(),
        args,
    })
}

fn parse_shell(req: &mut ParseRequest) -> Result<ShellCommand, Error> {
    req.flags.parse()?;
    let shell = handle_json_args(&req.args, req.json);
    if shell.is_empty() {
        return Err(err_at_least_one_argument("SHELL"));
    }
    if !req.json {
        return Err(err_not_json("SHELL"));
    }
    Ok(ShellCommand {
        base: req.base(),
        shell,
    })
}

fn get_doc_comment(comments: &[String], name: &str) -> String {
    if name.is_empty() {
        return String::new();
    }
    let prefix = format!("{name} ");
    comments
        .iter()
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::dockerfile::parser;
    use crate::build::dockerfile::to_ranges;

    fn first_node(dockerfile: &str) -> Node {
        let mut ast = parser::parse(dockerfile.as_bytes()).unwrap().ast;
        ast.children.remove(0)
    }

    fn instruction(dockerfile: &str) -> Result<Instruction, Error> {
        parse_instruction(&first_node(dockerfile))
    }

    fn command(dockerfile: &str) -> Command {
        match instruction(dockerfile) {
            Ok(Instruction::Command(c)) => c,
            other => panic!("{dockerfile}: {other:?}"),
        }
    }

    fn error(dockerfile: &str) -> String {
        instruction(dockerfile).unwrap_err().to_string()
    }

    /// An instruction node with these argument values.
    fn node(cmd: &str, args: &[&str]) -> Node {
        let mut next = None;
        for v in args.iter().rev() {
            let mut n = Node::default();
            n.value = (*v).to_string();
            n.next = next;
            next = Some(Box::new(n));
        }
        let mut n = Node::default();
        n.value = cmd.to_string();
        n.next = next;
        n
    }

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn commands_exactly_one_argument() {
        for cmd in ["MAINTAINER", "WORKDIR", "USER", "STOPSIGNAL"] {
            assert_eq!(error(cmd), err_exactly_one_argument(cmd).to_string());
        }
    }

    #[test]
    fn commands_at_least_one_argument() {
        for cmd in ["ENV", "LABEL", "ONBUILD", "HEALTHCHECK", "EXPOSE", "VOLUME"] {
            assert_eq!(error(cmd), err_at_least_one_argument(cmd).to_string());
        }
    }

    #[test]
    fn commands_no_destination_argument() {
        for cmd in ["ADD", "COPY"] {
            assert_eq!(
                error(&format!("{cmd} arg1")),
                err_no_destination_argument(cmd).to_string()
            );
        }
    }

    #[test]
    fn commands_too_many_arguments() {
        for cmd in ["ENV", "LABEL"] {
            let mut n = node(&to_lower(cmd), &["arg1", "arg2", "arg3", ""]);
            n.original = format!("{cmd}arg1 arg2 arg3");
            let err = parse_instruction(&n).unwrap_err();
            assert_eq!(err.to_string(), err_too_many_arguments(cmd).to_string());
        }
    }

    #[test]
    fn commands_blank_names() {
        for cmd in ["ENV", "LABEL"] {
            let mut n = node(&to_lower(cmd), &["", "arg2", "="]);
            n.original = format!("{cmd} =arg2");
            let err = parse_instruction(&n).unwrap_err();
            assert_eq!(err.to_string(), err_blank_command_names(cmd).to_string());
        }
    }

    #[test]
    fn health_check_cmd() {
        let n = node("healthcheck", &["CMD", "hello", "world"]);
        let Ok(Instruction::Command(Command::Healthcheck(hc))) = parse_instruction(&n) else {
            panic!("not a HEALTHCHECK");
        };
        assert_eq!(hc.health.test, strings(&["CMD-SHELL", "hello world"]));
    }

    #[test]
    fn parse_opt_interval_cases() {
        let mut bf = BFlags::new();
        let id = bf.add_string("interval", "");
        let mut flag = bf.flag(id).clone();
        flag.value = "50ns".to_string();
        let err = parse_opt_interval(&flag).unwrap_err();
        assert!(err.to_string().contains("cannot be less than 1ms"), "{err}");
        assert_eq!(
            err.to_string(),
            "Interval \"interval\" cannot be less than 1ms"
        );
        flag.value = "0ms".to_string();
        assert_eq!(parse_opt_interval(&flag).unwrap(), Duration::ZERO);
        flag.value = "1ms".to_string();
        assert_eq!(parse_opt_interval(&flag).unwrap(), Duration::from_millis(1));
        flag.value = "-5s".to_string();
        assert!(parse_opt_interval(&flag).is_err());
        flag.value = "5".to_string();
        assert_eq!(
            parse_opt_interval(&flag).unwrap_err().to_string(),
            "time: missing unit in duration \"5\""
        );
    }

    // The reference checks with a nil linter that no instruction panics.
    #[test]
    fn no_instruction_panics() {
        for cmd in ALL_INSTRUCTION_NAMES {
            let cmd = to_lower(cmd);
            for tc in [
                format!("{cmd} foo=bar"),
                format!("{cmd} a"),
                format!("{cmd} a b"),
                format!("{cmd} a b c"),
                format!("{cmd} 0 0"),
            ] {
                if let Ok(parsed) = parser::parse(format!("FROM busybox\n{tc}").as_bytes()) {
                    let _ = parse(&parsed.ast);
                }
            }
        }
    }

    #[test]
    fn comments_detection() {
        let dt = "# foo sets foo
ARG foo=bar

# base defines first stage
FROM busybox AS base
# this is irrelevant
ARG foo
# bar defines bar
# baz is something else
ARG bar baz=123
";
        let ast = parser::parse(dt.as_bytes()).unwrap().ast;
        let (stages, meta) = parse(&ast).unwrap();
        assert_eq!(stages[0].doc_comment, "defines first stage");
        assert_eq!(meta[0].args[0].key, "foo");
        assert_eq!(meta[0].args[0].doc_comment, "sets foo");

        let st = &stages[0];
        let Command::Arg(a0) = &st.commands[0] else {
            panic!()
        };
        assert_eq!(a0.args[0].key, "foo");
        assert_eq!(a0.args[0].doc_comment, "");
        let Command::Arg(a1) = &st.commands[1] else {
            panic!()
        };
        assert_eq!(a1.args[0].key, "bar");
        assert_eq!(a1.args[0].doc_comment, "defines bar");
        assert_eq!(a1.args[1].key, "baz");
        assert_eq!(a1.args[1].doc_comment, "is something else");
        assert_eq!(a1.args[1].value.as_deref(), Some("123"));
    }

    #[test]
    fn error_cases() {
        let cases = [
            (
                "COPY\t\n\t\tquux \\\n      bar",
                "COPY requires at least two arguments",
            ),
            (
                "ONBUILD FROM scratch",
                "FROM isn't allowed as an ONBUILD trigger",
            ),
            ("MAINTAINER --boo joe@example.com", "unknown flag: --boo"),
            (
                "ONBUILD ONBUILD RUN touch foobar",
                "Chaining ONBUILD via `ONBUILD ONBUILD` isn't allowed",
            ),
            ("FOO bar", "unknown instruction: FOO"),
            ("foo bar", "unknown instruction: foo"),
        ];
        for (dockerfile, expected) in cases {
            let err = error(dockerfile);
            assert!(err.contains(expected), "{dockerfile:?}: {err}");
        }
    }

    #[test]
    fn run_cmd_flags_used() {
        let Command::Run(run) = command("RUN --mount=type=tmpfs,target=/foo/ echo hello") else {
            panic!("not a RUN");
        };
        assert_eq!(run.flags_used, strings(&["mount"]));
    }

    #[test]
    fn error_cases_heredoc() {
        let err = error("COPY /foo <<EOF\nEOF");
        assert!(
            err.contains("COPY cannot accept a heredoc as a destination"),
            "{err}"
        );
    }

    #[test]
    fn copy_heredoc() {
        let content = |path: &str, data: &str, expand: bool| SourceContent {
            path: path.into(),
            data: data.into(),
            expand,
        };
        let cases = [
            (
                "COPY /foo /bar",
                SourcesAndDest {
                    dest_path: "/bar".into(),
                    source_paths: strings(&["/foo"]),
                    source_contents: vec![],
                },
            ),
            (
                "COPY <<EOF /bar\nEOF",
                SourcesAndDest {
                    dest_path: "/bar".into(),
                    source_paths: vec![],
                    source_contents: vec![content("EOF", "", true)],
                },
            ),
            (
                "COPY <<EOF /bar\nTESTING\nEOF",
                SourcesAndDest {
                    dest_path: "/bar".into(),
                    source_paths: vec![],
                    source_contents: vec![content("EOF", "TESTING\n", true)],
                },
            ),
            (
                "COPY <<-EOF /bar\n\tTESTING\nEOF",
                SourcesAndDest {
                    dest_path: "/bar".into(),
                    source_paths: vec![],
                    source_contents: vec![content("EOF", "TESTING\n", true)],
                },
            ),
            (
                "COPY <<'EOF' /bar\nTESTING\nEOF",
                SourcesAndDest {
                    dest_path: "/bar".into(),
                    source_paths: vec![],
                    source_contents: vec![content("EOF", "TESTING\n", false)],
                },
            ),
            (
                "COPY <<EOF1 <<EOF2 /bar\nthis is the first file\nEOF1\nthis is the second file\nEOF2",
                SourcesAndDest {
                    dest_path: "/bar".into(),
                    source_paths: vec![],
                    source_contents: vec![
                        content("EOF1", "this is the first file\n", true),
                        content("EOF2", "this is the second file\n", true),
                    ],
                },
            ),
            (
                "COPY <<EOF foo.txt /bar\nthis is inline\nEOF",
                SourcesAndDest {
                    dest_path: "/bar".into(),
                    source_paths: strings(&["foo.txt"]),
                    source_contents: vec![content("EOF", "this is inline\n", true)],
                },
            ),
            (
                "COPY <<EOF /quotes\n\"quotes\"\nEOF",
                SourcesAndDest {
                    dest_path: "/quotes".into(),
                    source_paths: vec![],
                    source_contents: vec![content("EOF", "\"quotes\"\n", true)],
                },
            ),
        ];
        for (dockerfile, expected) in cases {
            let Command::Copy(copy) = command(dockerfile) else {
                panic!("not a COPY");
            };
            assert_eq!(copy.sources_and_dest, expected, "{dockerfile:?}");
        }
    }

    #[test]
    fn run_heredoc() {
        let file = |name: &str, data: &str, chomp: bool| ShellInlineFile {
            name: name.into(),
            data: data.into(),
            chomp,
        };
        let cases = [
            ("RUN [\"ls\", \"/\"]", false, strings(&["ls", "/"]), vec![]),
            ("RUN [\"<<EOF\"]", false, strings(&["<<EOF"]), vec![]),
            ("RUN ls /", true, strings(&["ls /"]), vec![]),
            (
                "RUN <<EOF\nls /\nwhoami\nEOF",
                true,
                strings(&["<<EOF"]),
                vec![file("EOF", "ls /\nwhoami\n", false)],
            ),
            (
                "RUN <<'EOF' | python\nprint(\"hello\")\nprint(\"world\")\nEOF",
                true,
                strings(&["<<'EOF' | python"]),
                vec![file("EOF", "print(\"hello\")\nprint(\"world\")\n", false)],
            ),
            (
                "RUN <<-EOF\n\techo test\nEOF",
                true,
                strings(&["<<-EOF"]),
                vec![file("EOF", "\techo test\n", true)],
            ),
        ];
        for (dockerfile, shell, cmd_line, files) in cases {
            let Command::Run(run) = command(dockerfile) else {
                panic!("not a RUN");
            };
            assert_eq!(run.prepend_shell, shell, "{dockerfile:?}");
            assert_eq!(run.cmd_line, cmd_line, "{dockerfile:?}");
            assert_eq!(run.files, files, "{dockerfile:?}");
        }
    }

    #[test]
    fn handle_json_args_cases() {
        assert!(handle_json_args(&[], false).is_empty());
        assert_eq!(
            handle_json_args(&strings(&["foo"]), true),
            strings(&["foo"])
        );
        assert_eq!(
            handle_json_args(&strings(&["foo", "bar"]), true),
            strings(&["foo", "bar"])
        );
        assert_eq!(
            handle_json_args(&strings(&["foo", "bar"]), false),
            strings(&["foo bar"])
        );
    }

    #[test]
    fn stages_and_meta_args() {
        let dt = "ARG BASE=alpine
ARG V
FROM --platform=$BUILDPLATFORM ${BASE} AS Build-1
RUN make
FROM scratch
COPY --from=build-1 /out /
";
        let ast = parser::parse(dt.as_bytes()).unwrap().ast;
        let (stages, meta) = parse(&ast).unwrap();
        assert_eq!(meta.len(), 2);
        assert_eq!(meta[0].args[0].to_string(), "BASE=alpine");
        assert_eq!(meta[1].args[0].value, None);
        assert_eq!(stages.len(), 2);
        assert_eq!(stages[0].name, "build-1");
        assert_eq!(stages[0].base_name, "${BASE}");
        assert_eq!(stages[0].platform, "$BUILDPLATFORM");
        assert_eq!(stages[0].orig_cmd, "FROM");
        assert_eq!(
            stages[0].source_code,
            "FROM --platform=$BUILDPLATFORM ${BASE} AS Build-1"
        );
        assert_eq!(stages[0].location, to_ranges(3, 3));
        assert_eq!(stages[0].commands.len(), 1);
        assert_eq!(stages[0].commands[0].name(), "RUN");
        assert_eq!(stages[1].name, "");
        let Command::Copy(copy) = &stages[1].commands[0] else {
            panic!()
        };
        assert_eq!(copy.from, "build-1");
        assert_eq!(copy.code(), "COPY --from=build-1 /out /");
        assert_eq!(copy.location(), to_ranges(6, 6).as_slice());
        assert_eq!(has_stage(&stages, "BUILD-1"), Some(0));
        assert_eq!(has_stage(&stages, "nope"), None);
        assert!(is_current_stage(&stages, ""));
        assert!(!is_current_stage(&[], ""));
    }

    #[test]
    fn parse_errors_carry_line_and_location() {
        let ast = parser::parse(b"FROM a\n\nRUN --foo x\n").unwrap().ast;
        let err = parse(&ast).unwrap_err();
        assert_eq!(
            err.to_string(),
            "dockerfile parse error on line 3: unknown flag: --foo"
        );
        assert_eq!(err.locations(), &[to_ranges(3, 3)]);

        let ast = parser::parse(b"RUN x\n").unwrap().ast;
        let err = parse(&ast).unwrap_err();
        assert_eq!(err.to_string(), "no build stage in current context");
        assert_eq!(err.locations(), &[to_ranges(1, 1)]);

        let err = parse_command(&first_node("FROM a")).unwrap_err();
        assert_eq!(err.to_string(), "*instructions.Stage is not a command type");
        assert_eq!(err.locations(), &[to_ranges(1, 1)]);
    }

    #[test]
    fn from_errors() {
        assert_eq!(
            error("FROM a b"),
            "FROM requires either one or three arguments"
        );
        assert_eq!(
            error("FROM a AS b c"),
            "FROM requires either one or three arguments"
        );
        assert_eq!(
            error("FROM a AS 1st"),
            "invalid name for build stage: \"1st\", name can't start with a number or contain symbols"
        );
        assert_eq!(
            error("FROM a as b$c"),
            "invalid name for build stage: \"b$c\", name can't start with a number or contain symbols"
        );
        assert_eq!(
            error("FROM --platfrm=x a"),
            "unknown flag: --platfrm (did you mean platform?)"
        );
        let Ok(Instruction::Stage(s)) = instruction("FROM a aS my_stage.1-x") else {
            panic!()
        };
        assert_eq!(s.name, "my_stage.1-x");
    }

    #[test]
    fn unknown_instruction_suggestions() {
        assert_eq!(
            error("FORM a"),
            "unknown instruction: FORM (did you mean FROM?)"
        );
        assert_eq!(
            error("form a"),
            "unknown instruction: form (did you mean from?)"
        );
        assert_eq!(
            error("Workdr a"),
            "unknown instruction: Workdr (did you mean workdir?)"
        );
        assert_eq!(error("XYZZY a"), "unknown instruction: XYZZY");
    }

    #[test]
    fn add_and_copy_flags() {
        let Command::Add(add) = command(
            "ADD --chown=1:2 --chmod=644 --link --keep-git-dir=false --checksum=sha256:abc --unpack --exclude=*.md --exclude=x https://e/x.tar /dst/",
        ) else {
            panic!()
        };
        assert_eq!((add.chown.as_str(), add.chmod.as_str()), ("1:2", "644"));
        assert!(add.link);
        assert_eq!(add.keep_git_dir, Some(false));
        assert_eq!(add.unpack, Some(true));
        assert_eq!(add.checksum, "sha256:abc");
        assert_eq!(add.exclude_patterns, strings(&["*.md", "x"]));
        assert_eq!(
            add.sources_and_dest.source_paths,
            strings(&["https://e/x.tar"])
        );
        assert_eq!(add.sources_and_dest.dest_path, "/dst/");

        let Command::Add(add) = command("ADD a b") else {
            panic!()
        };
        assert_eq!(
            (add.keep_git_dir, add.unpack, add.link),
            (None, None, false)
        );

        let Command::Copy(copy) =
            command("COPY --from=st --parents --link=false [\"a b\", \"c\", \"/d\"]")
        else {
            panic!()
        };
        assert_eq!(copy.from, "st");
        assert!(copy.parents && !copy.link);
        assert_eq!(copy.sources_and_dest.source_paths, strings(&["a b", "c"]));
        assert_eq!(error("COPY --checksum=x a b"), "unknown flag: --checksum");
        assert_eq!(error("ADD --parents a b"), "unknown flag: --parents");
    }

    #[test]
    fn run_flags() {
        let Command::Run(run) = command(
            "RUN --network=none --security=insecure --device=vendor.com/gpu=all,required --mount=type=cache,target=/c [\"make\"]",
        ) else {
            panic!()
        };
        assert_eq!(run.network(), NetworkMode::None);
        assert_eq!(run.security(), Security::Insecure);
        assert_eq!(
            run.devices(),
            &[Device {
                name: "vendor.com/gpu=all".into(),
                required: true
            }]
        );
        assert!(!run.prepend_shell);
        assert_eq!(run.cmd_line, strings(&["make"]));
        let mut used = run.flags_used.clone();
        used.sort();
        assert_eq!(used, strings(&["device", "mount", "network", "security"]));
        // Before expansion the keys of a mount are not applied.
        assert_eq!(run.mounts()[0].mount_type, MountType::Bind);

        let Command::Run(run) = command("RUN echo") else {
            panic!()
        };
        assert_eq!(
            (run.network(), run.security()),
            (NetworkMode::Default, Security::Sandbox)
        );
        assert!(run.mounts().is_empty() && run.devices().is_empty());

        assert_eq!(
            error("RUN --network=bridge x"),
            "invalid network mode \"bridge\""
        );
        assert_eq!(
            error("RUN --security=open x"),
            "security \"open\" is not valid"
        );
        assert_eq!(
            error("RUN --netwrk=host x"),
            "unknown flag: --netwrk (did you mean network?)"
        );
        assert_eq!(
            error("RUN --mount=from=$X x"),
            "'from' doesn't support variable expansion, define alias stage instead"
        );
        assert_eq!(
            error("RUN --device=a,b x"),
            "invalid field 'b' must be a key=value pair"
        );
    }

    #[test]
    fn run_mounts_after_expansion() {
        let mut cmd = command("RUN --mount=type=$T,target=/cache,id=${ID} make");
        let mut expander = |w: &str| -> Result<String, Error> {
            Ok(w.replace("$T", "cache").replace("${ID}", "go"))
        };
        cmd.as_single_word_expansion()
            .unwrap()
            .expand(&mut expander)
            .unwrap();
        let Command::Run(run) = &cmd else { panic!() };
        let m = &run.mounts()[0];
        assert_eq!(m.mount_type, MountType::Cache);
        assert_eq!(m.target, "/cache");
        assert_eq!(m.cache_id, "go");
        assert!(!m.read_only);

        let mut cmd = command("RUN --mount=type=bind,size=1 make");
        let mut identity = |w: &str| -> Result<String, Error> { Ok(w.to_string()) };
        let err = cmd
            .as_single_word_expansion()
            .unwrap()
            .expand(&mut identity)
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "unexpected key 'size' for mount type 'bind'"
        );
    }

    #[test]
    fn healthcheck_options() {
        let Command::Healthcheck(hc) = command(
            "HEALTHCHECK --interval=5s --timeout=3s --start-period=1m --start-interval=500ms --retries=3 CMD [\"curl\", \"-f\", \"http://x\"]",
        ) else {
            panic!()
        };
        assert_eq!(
            hc.health,
            HealthcheckConfig {
                test: strings(&["CMD", "curl", "-f", "http://x"]),
                interval: Duration::from_secs(5),
                timeout: Duration::from_secs(3),
                start_period: Duration::from_secs(60),
                start_interval: Duration::from_millis(500),
                retries: 3,
            }
        );
        let Command::Healthcheck(hc) = command("HEALTHCHECK --bogus NONE") else {
            panic!()
        };
        assert_eq!(hc.health.test, strings(&["NONE"]));
        assert_eq!(
            error("HEALTHCHECK NONE x"),
            "HEALTHCHECK NONE takes no arguments"
        );
        assert_eq!(
            error("HEALTHCHECK CMD"),
            "Missing command after HEALTHCHECK CMD"
        );
        assert_eq!(
            error("HEALTHCHECK CONNECT TCP 7000"),
            "Unknown type \"CONNECT\" in HEALTHCHECK (try CMD)"
        );
        assert_eq!(
            error("HEALTHCHECK --retries=-1 CMD x"),
            "--retries cannot be negative (-1)"
        );
        assert_eq!(
            error("HEALTHCHECK --retries=x CMD x"),
            "strconv.ParseInt: parsing \"x\": invalid syntax"
        );
        assert_eq!(
            error("HEALTHCHECK --timeout=1 CMD x"),
            "time: missing unit in duration \"1\""
        );
        assert_eq!(
            error("HEALTHCHECK --interval=1us CMD x"),
            "Interval \"interval\" cannot be less than 1ms"
        );
    }

    #[test]
    fn simple_commands() {
        let Command::Expose(e) = command("EXPOSE 80/tcp 443 1000") else {
            panic!()
        };
        assert_eq!(e.ports, strings(&["1000", "443", "80/tcp"]));
        let Command::Volume(v) = command("VOLUME [\"/a\", \" /b \"]") else {
            panic!()
        };
        assert_eq!(v.volumes, strings(&["/a", "/b"]));
        assert_eq!(
            error("VOLUME [\" \"]"),
            "VOLUME specified can not be an empty string"
        );
        let Command::Shell(s) = command("SHELL [\"/bin/bash\", \"-c\"]") else {
            panic!()
        };
        assert_eq!(s.shell, strings(&["/bin/bash", "-c"]));
        assert_eq!(
            error("SHELL /bin/bash -c"),
            "SHELL requires the arguments to be in JSON form"
        );
        assert_eq!(error("SHELL []"), "SHELL requires at least one argument");
        let Command::StopSignal(s) = command("STOPSIGNAL --ignored SIGTERM") else {
            panic!()
        };
        assert_eq!(s.signal, "SIGTERM");
        let Command::Arg(a) = command("ARG --ignored a=1 b") else {
            panic!()
        };
        assert_eq!(a.args.len(), 2);
        assert_eq!(error("ARG =x"), "ARG names can not be blank");
        let Command::Entrypoint(e) = command("ENTRYPOINT []") else {
            panic!()
        };
        assert!(e.cmd_line.is_empty() && !e.prepend_shell);
        let Command::Cmd(c) = command("CMD echo $HOME") else {
            panic!()
        };
        assert_eq!(c.cmd_line, strings(&["echo $HOME"]));
        assert!(c.prepend_shell);
        let Command::Env(env) = command("ENV a=1 b=\"2 3\"") else {
            panic!()
        };
        assert_eq!(env.env[1].value, "\"2 3\"");
        assert!(!env.env[1].no_delim);
        let Command::Env(env) = command("ENV a 1 2") else {
            panic!()
        };
        assert_eq!(env.env[0].to_string(), "a=1 2");
        assert!(env.env[0].no_delim);
        let Command::Maintainer(m) = command("MAINTAINER me <me@x>") else {
            panic!()
        };
        assert_eq!(m.maintainer, "me <me@x>");
        let Command::Workdir(w) = command("workdir /x") else {
            panic!()
        };
        assert_eq!((w.name(), w.path.as_str()), ("workdir", "/x"));
        let Command::User(u) = command("USER 1:2") else {
            panic!()
        };
        assert_eq!(u.user, "1:2");
    }

    #[test]
    fn onbuild_expression() {
        let Command::Onbuild(o) = command("  onbuild   RUN echo hi") else {
            panic!()
        };
        assert_eq!(o.expression, "RUN echo hi");
        let Command::Onbuild(o) = command("ONBUILD COPY <<EOF /x\nhello\nEOF") else {
            panic!()
        };
        assert_eq!(o.expression, "COPY <<EOF /x\nhello\nEOF");
        assert_eq!(
            error("ONBUILD maintainer x"),
            "MAINTAINER isn't allowed as an ONBUILD trigger"
        );
    }

    #[test]
    fn expansion() {
        let mut upper = |w: &str| -> Result<String, Error> { Ok(w.to_uppercase()) };
        let dt = "FROM a
ENV a=b
LABEL c=d
ADD --chown=u --chmod=m --checksum=c s <<EOF /d
x
EOF
COPY --chown=u --from=f s /d
WORKDIR w
USER u
VOLUME v
STOPSIGNAL s
ARG a=b c
CMD x
";
        let ast = parser::parse(dt.as_bytes()).unwrap().ast;
        let (mut stages, _) = parse(&ast).unwrap();
        let mut expandable = 0;
        let mut raw = 0;
        for cmd in &mut stages[0].commands {
            if let Some(e) = cmd.as_single_word_expansion() {
                e.expand(&mut upper).unwrap();
                expandable += 1;
            }
            if let Some(e) = cmd.as_single_word_expansion_raw() {
                e.expand_raw(&mut upper).unwrap();
                raw += 1;
            }
        }
        assert_eq!((expandable, raw), (9, 2));
        let c = &stages[0].commands;
        let Command::Env(env) = &c[0] else { panic!() };
        assert_eq!(env.env[0].to_string(), "A=B");
        let Command::Add(add) = &c[2] else { panic!() };
        assert_eq!(
            (
                add.chown.as_str(),
                add.chmod.as_str(),
                add.checksum.as_str()
            ),
            ("U", "M", "C")
        );
        assert_eq!(add.sources_and_dest.source_paths, strings(&["S"]));
        assert_eq!(add.sources_and_dest.dest_path, "/D");
        assert_eq!(add.sources_and_dest.source_contents[0].data, "X\n");
        let Command::Copy(copy) = &c[3] else { panic!() };
        assert_eq!((copy.chown.as_str(), copy.from.as_str()), ("U", "f"));
        let Command::Arg(arg) = &c[8] else { panic!() };
        assert_eq!(arg.args[0].to_string(), "A=B");
        assert_eq!(arg.args[1].value, None);
        assert!(c[7]
            .as_platform_specific()
            .unwrap()
            .check_platform("linux")
            .is_ok());
        assert_eq!(
            c[7].as_platform_specific()
                .unwrap()
                .check_platform("windows")
                .unwrap_err()
                .to_string(),
            "The daemon on this platform does not support the command stopsignal"
        );

        let mut label = LabelCommand::new("k", "$v", true);
        label.expand(&mut upper).unwrap();
        assert_eq!(label.labels[0].value, "$v");
        assert_eq!((label.name(), label.code()), ("label", "LABEL k=$v"));

        // A failing expansion of an ARG value leaves the pair as it was.
        let Command::Arg(mut arg) = command("ARG a=b") else {
            panic!()
        };
        let mut calls = 0;
        let mut fail_second = |w: &str| -> Result<String, Error> {
            calls += 1;
            if calls == 2 {
                return Err(Error::new("boom"));
            }
            Ok(w.to_uppercase())
        };
        assert!(arg.expand(&mut fail_second).is_err());
        assert_eq!(arg.args[0].key, "a");
    }

    #[test]
    fn accessors_of_every_command() {
        let dt = "FROM a
ENV a=b
MAINTAINER m
LABEL l=v
ADD a b
COPY a b
ONBUILD RUN x
WORKDIR /w
# runs x
RUN  x
CMD x
HEALTHCHECK NONE
ENTRYPOINT x
EXPOSE 80
USER u
VOLUME /v
STOPSIGNAL s
ARG a
SHELL [\"sh\"]
";
        let ast = parser::parse(dt.as_bytes()).unwrap().ast;
        let (stages, _) = parse(&ast).unwrap();
        let commands = &stages[0].commands;
        assert_eq!(commands.len(), 17);
        for (i, cmd) in commands.iter().enumerate() {
            macro_rules! check {
                ($c:expr) => {{
                    assert_eq!($c.base(), cmd.base());
                    assert_eq!($c.name(), cmd.name());
                    assert_eq!($c.location(), cmd.location());
                    assert_eq!($c.comments(), cmd.comments());
                    assert_eq!($c.code(), cmd.code());
                }};
            }
            match cmd {
                Command::Env(c) => check!(c),
                Command::Maintainer(c) => check!(c),
                Command::Label(c) => check!(c),
                Command::Add(c) => check!(c),
                Command::Copy(c) => check!(c),
                Command::Onbuild(c) => check!(c),
                Command::Workdir(c) => check!(c),
                Command::Run(c) => check!(c),
                Command::Cmd(c) => check!(c),
                Command::Healthcheck(c) => check!(c),
                Command::Entrypoint(c) => check!(c),
                Command::Expose(c) => check!(c),
                Command::User(c) => check!(c),
                Command::Volume(c) => check!(c),
                Command::StopSignal(c) => check!(c),
                Command::Arg(c) => check!(c),
                Command::Shell(c) => check!(c),
            }
            let line = i + 2 + usize::from(i >= 7);
            assert_eq!(
                cmd.location(),
                to_ranges(line, line).as_slice(),
                "{}",
                cmd.code()
            );
        }
        let run = &commands[7];
        assert_eq!(run.code(), "RUN  x");
        assert_eq!(run.comments(), ["runs x".to_string()]);
        assert_eq!(run.base().comments(), run.comments());
        let Command::Onbuild(o) = &commands[5] else {
            panic!()
        };
        assert_eq!(o.expression, "RUN x");

        let Command::Run(run) = command("RUN --mount=type=cache,target=/c,sharing=locked x") else {
            panic!()
        };
        let mut identity = |w: &str| -> Result<String, Error> { Ok(w.to_string()) };
        let mut run = run;
        run.expand(&mut identity).unwrap();
        assert_eq!(run.mounts()[0].cache_sharing, Some(ShareMode::Locked));
        let mut bf = BFlags::new();
        let id = bf.add_strings("x");
        assert_eq!(bf.flag(id).flag_type(), FlagType::Strings);
    }

    #[test]
    fn expansion_with_the_shell_lexer() {
        use crate::build::dockerfile::shell::{envs_from_slice, Lex};
        let lex = Lex::new('\\');
        let env = envs_from_slice(&["DIR=/srv", "NAME=app"]);
        let mut expander = |w: &str| -> Result<String, Error> { Ok(lex.process_word(w, &env)?.0) };
        let mut cmd = command("WORKDIR ${DIR}/${NAME:-x}");
        cmd.as_single_word_expansion()
            .unwrap()
            .expand(&mut expander)
            .unwrap();
        let Command::Workdir(w) = cmd else { panic!() };
        assert_eq!(w.path, "/srv/app");
        let mut cmd = command("USER ${MISSING?no user}");
        let err = cmd
            .as_single_word_expansion()
            .unwrap()
            .expand(&mut expander)
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "failed to process \"${MISSING?no user}\": MISSING: no user"
        );
    }
}
