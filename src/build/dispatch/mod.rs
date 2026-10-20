//! The instructions of a Dockerfile worked out into steps: a port of the dispatch of the
//! Dockerfile frontend of moby/buildkit v0.33.1 (dockerfile2llb's convert.go,
//! convert_copy.go, convert_runmount.go, convert_runnetwork.go, convert_runsecurity.go,
//! convert_secrets.go, convert_expose.go, image.go, platform.go and defaultshell.go),
//! which makes the [`plan`](crate::build::plan) types where the reference makes LLB.
//!
//! [`prepare`] reads the Dockerfile once: its stages, the meta ARGs, the stages the
//! target needs and the order they are built in. [`dispatch`] then works out one stage
//! from the config it starts with: the steps, each with the history line the reference
//! writes for it, and the config the stage leaves.
//!
//! The history lines of RUN, COPY and ADD lack the ` # buildkit` the reference appends
//! to them, which names the builder. Errors keep the reference's messages, the location
//! of the instruction attached as a [`dockerfile::Error`](crate::build::dockerfile::Error)
//! carries it; the refusals of what nspawn does not do are worded here.
// The port keeps the reference's API whole; its tests use what the builder does not.
#![allow(dead_code)]

mod expose;
mod gofmt;
mod platform;
mod reference;
mod stage;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashMap};

use anyhow::Result;

use crate::build::dockerfile::instructions::{
    self, ArgCommand, Command, Stage, SupportsSingleWordExpansion as _,
};
use crate::build::dockerfile::parser;
use crate::build::dockerfile::shell::{EnvGetter, Lex};
use crate::build::dockerfile::{Error, Range};
use crate::build::image::Config;
use crate::build::plan::{BaseRef, Dispatched, Options, Platform, Prepared, StageInfo};

use gofmt::quote;

const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// ARGs that never reach the environment of RUN.
const NON_ENV_ARGS: [&str; 2] = ["BUILDKIT_SBOM_SCAN_CONTEXT", "BUILDKIT_SBOM_SCAN_STAGE"];

/// Reads a Dockerfile: its syntax directive, its stages and their bases with the meta
/// ARGs expanded, the target and the stages it needs, dependencies first.
pub fn prepare(dockerfile: &[u8], options: &Options, platform: &Platform) -> Result<Prepared> {
    check_syntax(dockerfile, options)?;
    if dockerfile.is_empty() {
        return Err(Error::new("the Dockerfile cannot be empty").into());
    }
    let parsed = parser::parse(dockerfile)?;
    let (stages, meta_cmds) = instructions::parse(&parsed.ast)?;
    let Some(last) = stages.last() else {
        return Err(Error::new("dockerfile contains no stages to build").into());
    };
    let lex = Lex::new(parsed.escape_token);
    let target_option = options.target.as_deref().filter(|t| !t.is_empty());
    let target_name = target_option.unwrap_or(&last.name).to_string();
    let mut global = default_args(platform, &options.build_args, &target_name);
    let meta_args = build_meta_args(&mut global, &lex, &meta_cmds, &options.build_args)?;

    let mut infos = Vec::with_capacity(stages.len());
    let mut platforms = Vec::with_capacity(stages.len());
    let mut names: HashMap<String, usize> = HashMap::new();
    for (i, st) in stages.iter().enumerate() {
        let at = |e: Error| e.with_location(st.location.clone());
        let name = lex
            .process_word_with_matches(&st.base_name, &global)
            .map_err(at)?;
        if name.result.is_empty() {
            return Err(at(Error::new(format!(
                "base name ({}) should not be blank",
                st.base_name
            )))
            .into());
        }
        let spec = if st.platform.is_empty() {
            None
        } else {
            Some(from_platform(st, &lex, &global, platform)?)
        };
        let base = match names.get(&name.result) {
            Some(&k) => BaseRef::Stage(k),
            None if name.result == "scratch" => BaseRef::Scratch,
            None => BaseRef::Image(name.result),
        };
        if !st.name.is_empty() {
            names.insert(st.name.clone(), i);
        }
        infos.push(StageInfo {
            index: i,
            name: st.name.clone(),
            base,
        });
        platforms.push(spec);
    }

    let target = match target_option {
        None => stages.len() - 1,
        Some(t) => names.get(&t.to_lowercase()).copied().ok_or_else(|| {
            let known: Vec<String> = (0..stages.len()).map(|i| stage_name(&stages, i)).collect();
            Error::new(gofmt::with_suggestion(
                format!("target stage {} could not be found", quote(t)),
                t,
                &known,
                true,
            ))
        })?,
    };

    let count = stages.len();
    let mut deps: Vec<Vec<usize>> = vec![Vec::new(); count];
    let mut images: Vec<(String, Vec<Range>)> = Vec::new();
    for (i, st) in stages.iter().enumerate() {
        for cmd in &st.commands {
            for source in command_sources(cmd, &lex, &names, count)? {
                match source {
                    Source::Stage(k) => deps[i].push(k),
                    Source::Image(n) => images.push((n, cmd.location().to_vec())),
                    Source::Scratch => {}
                }
            }
        }
    }
    if let Some(i) = first_cycle(&deps) {
        return Err(Error::new(format!(
            "circular dependency detected on stage: {}",
            stage_name(&stages, i)
        ))
        .into());
    }
    for (info, st) in infos.iter().zip(&stages) {
        if let BaseRef::Image(name) = &info.base {
            check_image(name, &st.location)?;
        }
    }
    for (name, location) in &images {
        check_image(name, location)?;
    }

    // The ONBUILD triggers of a stage FROM an earlier one are known now and may need
    // more stages; those of an image are known once its config is.
    let mut trigger_deps: Vec<Vec<usize>> = vec![Vec::new(); count];
    let mut triggers: Vec<Option<Vec<Command>>> = vec![None; count];
    let reachable = loop {
        let reachable = reachable_stages(target, &infos, &deps, &trigger_deps);
        let mut new_deps = false;
        let mut new_images = Vec::new();
        for &s in &reachable {
            let BaseRef::Stage(k) = infos[s].base else {
                continue;
            };
            if triggers[s].is_some() {
                continue;
            }
            let location = &stages[s].location;
            let mut parsed_triggers = Vec::new();
            for expression in onbuild_expressions(&stages[k]) {
                let cmd = parse_trigger(&expression, location)
                    .map_err(|e| e.set_location(location.clone()))?;
                let sources = command_sources(&cmd, &lex, &names, count)
                    .map_err(|e| e.set_location(location.clone()))?;
                new_deps |= !sources.is_empty();
                for source in sources {
                    match source {
                        Source::Stage(d) => trigger_deps[s].push(d),
                        Source::Image(n) => new_images.push((n, location.clone())),
                        Source::Scratch => {}
                    }
                }
                parsed_triggers.push(cmd);
            }
            triggers[s] = Some(parsed_triggers);
        }
        for (name, location) in &new_images {
            check_image(name, location)?;
        }
        if !new_deps {
            break reachable;
        }
    };

    // The reference pulls an image for the platform asked for; a stage FROM scratch or
    // FROM another stage gets the target's or that stage's platform whatever it asks.
    let host = platform::Spec::of(platform);
    for &s in &reachable {
        if let (Some((expression, spec)), BaseRef::Image(_)) = (&platforms[s], &infos[s].base) {
            if !platform::same(spec, &host) {
                return Err(Error::new(format!(
                    "FROM --platform={expression} asks for {}: nspawn builds only for the platform of this host, {}",
                    spec.normalize().format(),
                    host.normalize().format()
                ))
                .with_location(stages[s].location.clone())
                .into());
            }
        }
    }
    for &s in &reachable {
        let own = triggers[s].iter().flatten().chain(&stages[s].commands);
        for cmd in own {
            check_later_sources(cmd, s, &stages, &lex, &names)?;
        }
    }

    Ok(Prepared {
        stages: infos,
        order: reachable,
        target,
        parsed: stages,
        meta_args,
        escape: parsed.escape_token,
        inherited: Default::default(),
    })
}

/// Works out stage `stage` of a prepared Dockerfile from the config it starts with: the
/// image's for a FROM of an image, the dispatched config of the stage it names for a
/// FROM of an earlier stage (dispatched with the same `prepared`), or
/// [`Config::scratch`]. The ONBUILD triggers of that config run first.
pub fn dispatch(
    prepared: &Prepared,
    stage: usize,
    base: &Config,
    options: &Options,
    platform: &Platform,
) -> Result<Dispatched> {
    stage::dispatch_stage(prepared, stage, base, options, platform)
}

/// The file an ADD of a URL fetches: the last element of the URL's path, or
/// `__unnamed__` when it has none.
pub fn url_file_name(url: &str) -> String {
    let unnamed = || "__unnamed__".to_string();
    let Some(path) = url_path(url) else {
        return unnamed();
    };
    let base = gofmt::base(&path);
    if base == "." || base == "/" {
        return unnamed();
    }
    base
}

/// The decoded path of an absolute URL, as url.Parse gives it; None when it does not
/// parse.
fn url_path(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("://")?;
    let rest = rest.split(['#', '?']).next().unwrap_or_default();
    let path = rest.find('/').map_or("", |i| &rest[i..]);
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = |c: Option<&u8>| c.and_then(|&c| (c as char).to_digit(16));
            let (h, l) = (hex(bytes.get(i + 1))?, hex(bytes.get(i + 2))?);
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// A stage's environment as the reference's llb.EnvList keeps it: a variable set again
/// moves to the end.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct EnvList {
    vars: Vec<(String, String)>,
}

impl EnvList {
    fn set(&mut self, key: &str, value: &str) {
        self.vars.retain(|(k, _)| k != key);
        self.vars.push((key.to_string(), value.to_string()));
    }

    fn to_array(&self) -> Vec<String> {
        self.vars.iter().map(|(k, v)| format!("{k}={v}")).collect()
    }
}

impl EnvGetter for EnvList {
    fn get(&self, key: &str) -> Option<&str> {
        self.vars
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    fn keys(&self) -> Vec<String> {
        self.vars.iter().map(|(k, _)| k.clone()).collect()
    }
}

/// What a dispatched stage leaves to a stage FROM it besides its config: the reference
/// starts such a stage from the earlier one's state, its ARGs included.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Inherited {
    env: EnvList,
    build_args: Vec<(String, Option<String>)>,
    author: Option<String>,
}

/// Where a COPY --from or a RUN mount takes its files, the name resolved as toCommand
/// and detectRunMount resolve it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Source {
    Stage(usize),
    Scratch,
    Image(String),
}

/// A stage as the reference names it in messages.
fn stage_name(stages: &[Stage], i: usize) -> String {
    if stages[i].name.is_empty() {
        format!("stage-{i}")
    } else {
        stages[i].name.clone()
    }
}

/// Every stage name to the last stage so named (the reference's statesByName once all
/// stages are read).
fn stage_names(stages: &[Stage]) -> HashMap<String, usize> {
    let mut names = HashMap::new();
    for (i, s) in stages.iter().enumerate() {
        if !s.name.is_empty() {
            names.insert(s.name.clone(), i);
        }
    }
    names
}

/// The stage of that name (the last one so named), else scratch or an image.
fn resolve_name(name: &str, names: &HashMap<String, usize>) -> Source {
    match names.get(&name.to_lowercase()) {
        Some(&k) => Source::Stage(k),
        None if name == "scratch" => Source::Scratch,
        None => Source::Image(name.to_string()),
    }
}

/// COPY --from: a stage by name or index, else an image.
fn copy_from_source(
    from: &str,
    lex: &Lex,
    names: &HashMap<String, usize>,
    count: usize,
) -> Result<Source, Error> {
    let expanded = lex.process_word_with_matches(from, &EnvList::default())?;
    if expanded.result != from {
        return Err(Error::new(
            "variable expansion is not supported for --from, define a new stage with FROM using ARG from global scope as a workaround",
        ));
    }
    match gofmt::atoi(from) {
        Some(index) => match usize::try_from(index) {
            Ok(i) if i < count => Ok(Source::Stage(i)),
            _ => Err(Error::new(format!("invalid stage index {index}"))),
        },
        None => Ok(resolve_name(from, names)),
    }
}

/// The sources of a command: its --from for COPY, one per mount for RUN (a mount
/// without from= being looked up as scratch, as the reference does).
fn command_sources(
    cmd: &Command,
    lex: &Lex,
    names: &HashMap<String, usize>,
    count: usize,
) -> Result<Vec<Source>, Error> {
    match cmd {
        Command::Copy(c) if !c.from.is_empty() => {
            Ok(vec![copy_from_source(&c.from, lex, names, count)?])
        }
        Command::Run(r) => Ok(r
            .mounts()
            .iter()
            .map(|m| {
                let from = if m.from.is_empty() {
                    "scratch"
                } else {
                    &m.from
                };
                resolve_name(from, names)
            })
            .collect()),
        _ => Ok(Vec::new()),
    }
}

/// The reference's errors for a COPY or a mount from a stage that is not built before
/// the one that takes from it.
fn check_later_sources(
    cmd: &Command,
    s: usize,
    stages: &[Stage],
    lex: &Lex,
    names: &HashMap<String, usize>,
) -> Result<(), Error> {
    let at = |message: String| Error::new(message).with_location(cmd.location().to_vec());
    match cmd {
        Command::Copy(c) if !c.from.is_empty() => {
            if let Ok(Source::Stage(k)) = copy_from_source(&c.from, lex, names, stages.len()) {
                if k >= s {
                    return Err(at(format!(
                        "cannot copy from stage {}, it needs to be defined before current stage {}",
                        quote(&c.from),
                        quote(&stage_name(stages, s))
                    )));
                }
            }
        }
        Command::Run(r) => {
            let mut literal = r.clone();
            // The targets as written: the reference names the expanded one, which is
            // only known at dispatch.
            let targets: Vec<String> = match literal.expand(&mut |w: &str| Ok(w.to_string())) {
                Ok(()) => literal.mounts().iter().map(|m| m.target.clone()).collect(),
                Err(_) => Vec::new(),
            };
            for (i, m) in r.mounts().iter().enumerate() {
                if m.from.is_empty() {
                    continue;
                }
                if let Source::Stage(k) = resolve_name(&m.from, names) {
                    if k >= s {
                        return Err(at(format!(
                            "cannot mount from stage {} to {}, stage needs to be defined before current command",
                            quote(&m.from),
                            quote(targets.get(i).map_or("", String::as_str))
                        )));
                    }
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// The first stage, in order, from which the COPY and mount dependencies lead to a cycle
/// (validateCircularDependency).
fn first_cycle(deps: &[Vec<usize>]) -> Option<usize> {
    fn visit(s: usize, deps: &[Vec<usize>], visited: &mut [bool], path: &mut [bool]) -> bool {
        if visited[s] {
            return false;
        }
        visited[s] = true;
        path[s] = true;
        for &d in &deps[s] {
            if path[d] || visit(d, deps, visited, path) {
                return true;
            }
        }
        path[s] = false;
        false
    }
    let mut visited = vec![false; deps.len()];
    let mut path = vec![false; deps.len()];
    (0..deps.len()).find(|&s| visit(s, deps, &mut visited, &mut path))
}

/// The stages `target` needs, itself included, in Dockerfile order.
fn reachable_stages(
    target: usize,
    infos: &[StageInfo],
    deps: &[Vec<usize>],
    trigger_deps: &[Vec<usize>],
) -> Vec<usize> {
    let mut seen = vec![false; infos.len()];
    let mut todo = vec![target];
    while let Some(s) = todo.pop() {
        if std::mem::replace(&mut seen[s], true) {
            continue;
        }
        if let BaseRef::Stage(k) = infos[s].base {
            todo.push(k);
        }
        todo.extend(&deps[s]);
        todo.extend(&trigger_deps[s]);
    }
    (0..infos.len()).filter(|&s| seen[s]).collect()
}

/// The ONBUILD instructions of a stage, the triggers of the stages FROM it.
fn onbuild_expressions(stage: &Stage) -> Vec<String> {
    stage
        .commands
        .iter()
        .filter_map(|c| match c {
            Command::Onbuild(o) => Some(o.expression.clone()),
            _ => None,
        })
        .collect()
}

/// An ONBUILD trigger read as an instruction, placed at the FROM of the stage it runs in.
fn parse_trigger(trigger: &str, location: &[Range]) -> Result<Command, Error> {
    let mut parsed = parser::parse(trigger.as_bytes())?;
    if parsed.ast.children.len() != 1 {
        return Err(Error::new("onbuild trigger should be a single expression"));
    }
    let mut node = parsed.ast.children.remove(0);
    let (start, end) = range_start_end(location);
    node.start_line = start;
    node.end_line = end;
    instructions::parse_command(&node)
}

fn range_start_end(ranges: &[Range]) -> (usize, usize) {
    if ranges.is_empty() {
        return (0, 0);
    }
    let start = ranges.iter().map(|r| r.start.line).min().unwrap_or(0);
    let end = ranges.iter().map(|r| r.end.line).max().unwrap_or(0);
    (start, end)
}

fn check_image(name: &str, location: &[Range]) -> Result<(), Error> {
    reference::check_normalized_named(name).map_err(|e| {
        Error::new(format!("failed to parse stage name {}: {e}", quote(name)))
            .with_location(location.to_vec())
    })
}

/// FROM --platform expanded with the meta ARGs and read.
fn from_platform(
    st: &Stage,
    lex: &Lex,
    global: &EnvList,
    host: &Platform,
) -> Result<(String, platform::Spec), Error> {
    let v = &st.platform;
    let at = |e: Error| e.with_location(st.location.clone());
    let matched = lex
        .process_word_with_matches(v, global)
        .map_err(|e| at(e.wrap("failed to process arguments for platform ")))?;
    let suggest = |message: String| {
        let keys = global.keys();
        matched
            .unmatched
            .iter()
            .find_map(|k| gofmt::suggest(k, &keys, true))
            .map_or_else(
                || message.clone(),
                |m| format!("{message} (did you mean {m}?)"),
            )
    };
    if matched.result.is_empty() {
        return Err(at(Error::new(suggest(format!(
            "empty platform value from expression {v}"
        )))));
    }
    let spec = platform::parse(&matched.result, host).map_err(|e| {
        at(Error::new(format!(
            "failed to parse platform {v}: {}",
            suggest(e)
        )))
    })?;
    Ok((v.clone(), spec))
}

/// The predefined ARGs of the platform and the target stage (defaultArgs), each
/// overridden by a build argument of its name.
fn default_args(
    platform: &Platform,
    build_args: &BTreeMap<String, String>,
    target: &str,
) -> EnvList {
    let target = if target.is_empty() { "default" } else { target };
    let formatted = platform::Spec::of(platform).format();
    let variant = platform.variant.clone().unwrap_or_default();
    let pairs = [
        ("BUILDPLATFORM", formatted.as_str()),
        ("BUILDOS", platform.os.as_str()),
        ("BUILDOSVERSION", ""),
        ("BUILDARCH", platform.architecture.as_str()),
        ("BUILDVARIANT", variant.as_str()),
        ("TARGETPLATFORM", formatted.as_str()),
        ("TARGETOS", platform.os.as_str()),
        ("TARGETOSVERSION", ""),
        ("TARGETARCH", platform.architecture.as_str()),
        ("TARGETVARIANT", variant.as_str()),
        ("TARGETSTAGE", target),
    ];
    let mut env = EnvList::default();
    for (k, v) in pairs {
        env.set(k, build_args.get(k).map_or(v, String::as_str));
    }
    env
}

/// The meta ARGs (buildMetaArgs): a build argument's value, else the default expanded
/// with what is defined so far; those with a value join `global`.
fn build_meta_args(
    global: &mut EnvList,
    lex: &Lex,
    cmds: &[ArgCommand],
    build_args: &BTreeMap<String, String>,
) -> Result<Vec<(String, Option<String>)>, Error> {
    let mut out = Vec::new();
    for cmd in cmds {
        for kp in &cmd.args {
            let value = match (build_args.get(&kp.key), &kp.value) {
                (Some(v), _) => Some(v.clone()),
                (None, Some(default)) => Some(
                    lex.process_word_with_matches(default, &*global)
                        .map_err(|e| e.with_location(cmd.location().to_vec()))?
                        .result,
                ),
                (None, None) => None,
            };
            if let Some(v) = &value {
                global.set(&kp.key, v);
            }
            out.push((kp.key.clone(), value));
        }
    }
    Ok(out)
}

/// The predefined and meta ARGs as `prepare` left them, for the ARGs of the stages.
fn global_args(prepared: &Prepared, options: &Options, platform: &Platform) -> EnvList {
    let target = options
        .target
        .as_deref()
        .filter(|t| !t.is_empty())
        .or_else(|| prepared.parsed.last().map(|s| s.name.as_str()))
        .unwrap_or_default();
    let mut global = default_args(platform, &options.build_args, target);
    for (k, v) in &prepared.meta_args {
        if let Some(v) = v {
            global.set(k, v);
        }
    }
    global
}

/// The variables a RUN gets from the proxy build arguments, which need no ARG
/// (proxyEnvFromBuildArgs, then the executor's proxyEnvList).
fn proxy_env(build_args: &BTreeMap<String, String>) -> Vec<String> {
    let names = [
        "http_proxy",
        "https_proxy",
        "ftp_proxy",
        "no_proxy",
        "all_proxy",
    ];
    let mut values = [""; 5];
    for (k, v) in build_args {
        if let Some(i) = names.iter().position(|n| k.eq_ignore_ascii_case(n)) {
            values[i] = v;
        }
    }
    let mut out = Vec::new();
    for (name, value) in names.iter().zip(values) {
        if !value.is_empty() {
            out.push(format!("{}={value}", name.to_uppercase()));
            out.push(format!("{name}={value}"));
        }
    }
    out
}

/// The syntax directive, or BUILDKIT_SYNTAX before it, may only ask for the frontend
/// this is a port of: the reference would hand the build to the image it names.
fn check_syntax(dockerfile: &[u8], options: &Options) -> Result<()> {
    let (image, what) = match options.build_args.get("BUILDKIT_SYNTAX") {
        Some(cmdline) => (
            gofmt::trim_space(cmdline)
                .split(' ')
                .next()
                .unwrap_or_default()
                .to_string(),
            "the build argument BUILDKIT_SYNTAX",
        ),
        None => match parser::detect_syntax(dockerfile) {
            Some(d) => (d.value, "the syntax directive"),
            None => return Ok(()),
        },
    };
    if is_reference_frontend(&image) {
        return Ok(());
    }
    Err(anyhow::anyhow!(
        "{what} asks for the frontend image {}: nspawn builds Dockerfiles with its own port of the reference frontend and accepts only docker/dockerfile:1, :1.N or :1.N.M, their -labs variants and :labs, optionally with an @sha256 digest",
        quote(&image)
    ))
}

/// The reference frontend's image, with or without its registry, at 1, 1.N or 1.N.M,
/// each also -labs, or at labs, with or without a sha256 digest. References are
/// case-sensitive.
fn is_reference_frontend(image: &str) -> bool {
    let (named, digest) = match image.split_once('@') {
        Some((n, d)) => (n, Some(d)),
        None => (image, None),
    };
    if let Some(d) = digest {
        let ok = d.strip_prefix("sha256:").is_some_and(|hex| {
            hex.len() == 64
                && hex
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        });
        if !ok {
            return false;
        }
    }
    let Some((name, tag)) = named.rsplit_once(':') else {
        return false;
    };
    if !matches!(
        name,
        "docker/dockerfile" | "docker.io/docker/dockerfile" | "index.docker.io/docker/dockerfile"
    ) {
        return false;
    }
    if tag == "labs" {
        return true;
    }
    let version = tag.strip_suffix("-labs").unwrap_or(tag);
    let parts: Vec<&str> = version.split('.').collect();
    parts.len() <= 3
        && parts[0] == "1"
        && parts[1..]
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}
