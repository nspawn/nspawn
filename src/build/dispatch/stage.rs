//! One stage dispatched: dispatchStages and dispatch of convert.go with the dispatchX
//! functions of every instruction.

use std::collections::HashMap;

use anyhow::{anyhow, Result};

use super::gofmt::{self, quote};
use super::{
    command_sources, copy_from_source, expose, global_args, parse_trigger, platform, proxy_env,
    resolve_name, stage_name, stage_names, url_path, EnvList, Inherited, Source, DEFAULT_PATH,
    NON_ENV_ARGS,
};
use crate::build::copy::{self, mode};
use crate::build::dockerfile::instructions::{
    Command, Mount, MountType, NetworkMode, RunCommand, Security, ShareMode, SourcesAndDest,
};
use crate::build::dockerfile::parser::{chomp_heredoc_content, must_parse_heredoc};
use crate::build::dockerfile::shell::{envs_from_slice, EnvGetter, Lex};
use crate::build::dockerfile::Error;
use crate::build::image::{Config, Healthcheck, Settings};
use crate::build::plan::{
    Action, BaseRef, Copy, Dispatched, From, Inline, Network, Options, Platform, Prepared, Run,
    RunMount, Step,
};

pub(super) fn dispatch_stage(
    prepared: &Prepared,
    stage: usize,
    base: &Config,
    options: &Options,
    platform: &Platform,
) -> Result<Dispatched> {
    let info = prepared
        .stages
        .get(stage)
        .ok_or_else(|| anyhow!("the Dockerfile has no stage {stage}"))?;
    let parsed = &prepared.parsed[stage];
    let inherited = match info.base {
        BaseRef::Stage(k) => {
            let stored = prepared
                .inherited
                .lock()
                .map_err(|_| anyhow!("the dispatched stages are unavailable"))?
                .get(&k)
                .cloned();
            Some(stored.ok_or_else(|| {
                anyhow!("stage {k} must be dispatched before stage {stage}, which starts from it")
            })?)
        }
        _ => None,
    };

    let mut config = base.clone();
    config.created = None;
    if info.base == BaseRef::Scratch {
        // emptyImage
        config.architecture.clone_from(&platform.architecture);
        config.os.clone_from(&platform.os);
        config.variant.clone_from(&platform.variant);
        config.config = Settings {
            working_dir: "/".to_string(),
            env: vec![format!("PATH={DEFAULT_PATH}")],
            ..Settings::default()
        };
    }
    let triggers = std::mem::take(&mut config.config.on_build);
    if envs_from_slice(&config.config.env).get("PATH").is_none() && config.os != "windows" {
        config.config.env.push(format!("PATH={DEFAULT_PATH}"));
    }
    let Inherited {
        mut env,
        build_args,
        author,
    } = inherited.unwrap_or_default();
    for e in &config.config.env {
        let (k, v) = e.split_once('=').unwrap_or((e, ""));
        env.set(k, v);
    }
    if !config.config.working_dir.is_empty() {
        let wd = config.config.working_dir.clone();
        config.config.working_dir = normalize_workdir(&wd, &wd);
    }

    let names = stage_names(&prepared.parsed);
    let mut d = Dispatcher {
        prepared,
        index: stage,
        name: stage_name(&prepared.parsed, stage),
        options,
        lex: Lex::new(prepared.escape),
        global: global_args(prepared, options, platform),
        names,
        config,
        env,
        build_args,
        cmd_set: false,
        author,
        steps: Vec::new(),
    };

    let mut commands = Vec::with_capacity(triggers.len() + parsed.commands.len());
    for trigger in &triggers {
        let at = |e: Error| e.set_location(parsed.location.clone());
        let cmd = parse_trigger(trigger, &parsed.location).map_err(at)?;
        command_sources(&cmd, &d.lex, &d.names, prepared.parsed.len()).map_err(at)?;
        commands.push((cmd, true));
    }
    commands.extend(parsed.commands.iter().map(|c| (c.clone(), false)));
    for (cmd, onbuild) in commands {
        let location = cmd.location().to_vec();
        d.command(cmd, onbuild)
            .map_err(|e| e.with_location(location))?;
    }

    if stage == prepared.target {
        d.config
            .config
            .labels
            .extend(options.labels.iter().map(|(k, v)| (k.clone(), v.clone())));
        let spec = platform::Spec {
            os: d.config.os.clone(),
            os_version: String::new(),
            architecture: d.config.architecture.clone(),
            variant: d.config.variant.clone().unwrap_or_default(),
        }
        .normalize();
        d.config.os = spec.os;
        d.config.architecture = spec.architecture;
        d.config.variant = (!spec.variant.is_empty()).then_some(spec.variant);
    }

    if let Ok(mut stored) = prepared.inherited.lock() {
        stored.insert(
            stage,
            Inherited {
                env: d.env.clone(),
                build_args: d.build_args.clone(),
                author: d.author.clone(),
            },
        );
    }
    Ok(Dispatched {
        steps: d.steps,
        config: d.config,
        author: d.author,
    })
}

/// NormalizeWorkdir on Linux: a relative path joined to the current one, an absolute
/// one kept as written.
fn normalize_workdir(current: &str, wd: &str) -> String {
    let mut parent = if current.is_empty() { "/" } else { current }.to_string();
    if !parent.starts_with('/') {
        parent = gofmt::join(&["/", &parent]);
    }
    let wd = if wd.is_empty() { parent.as_str() } else { wd };
    if wd.starts_with('/') {
        wd.to_string()
    } else {
        gofmt::join(&[&parent, wd])
    }
}

/// withShell: the image's SHELL, else the default shell of its os, with the command line.
fn with_shell(config: &Config, args: &[String]) -> Vec<String> {
    let mut shell = if config.config.shell.is_empty() {
        if config.os == "windows" {
            vec!["cmd".to_string(), "/S".to_string(), "/C".to_string()]
        } else {
            vec!["/bin/sh".to_string(), "-c".to_string()]
        }
    } else {
        config.config.shell.clone()
    };
    shell.push(args.join(" "));
    shell
}

/// processCmdEnv: the text expanded, or as it is when it does not expand.
fn process_cmd_env(lex: &Lex, cmd: &str, env: &dyn EnvGetter) -> String {
    lex.process_word(cmd, env)
        .map(|(w, _)| w)
        .unwrap_or_else(|_| cmd.to_string())
}

fn summarize_heredoc(doc: &str) -> String {
    let doc = gofmt::trim_space(doc).replace("\r\n", "\n");
    let mut lines = doc.split('\n');
    let first = lines.next().unwrap_or_default().to_string();
    if lines.next().is_some() {
        format!("{first}...")
    } else {
        first
    }
}

/// runCommandString: the build arguments in scope as `|N K=V...` before the command.
fn run_command_string(
    args: &[String],
    build_args: &[(String, Option<String>)],
    env: &EnvList,
) -> String {
    let mut vars: Vec<String> = Vec::new();
    let mut index: HashMap<&str, usize> = HashMap::new();
    for (key, value) in build_args {
        let v = env
            .get(key)
            .map(str::to_string)
            .unwrap_or_else(|| value.clone().unwrap_or_default());
        let line = format!("{key}={v}");
        match index.get(key.as_str()) {
            Some(&i) => vars[i] = line,
            None => {
                index.insert(key, vars.len());
                vars.push(line);
            }
        }
    }
    if !vars.is_empty() {
        vars.insert(0, format!("|{}", vars.len()));
    }
    vars.extend(args.iter().cloned());
    vars.join(" ")
}

/// The environment of RUN's description, the variables of secrets shown as `****`
/// (withSecretEnvMask).
struct SecretMask<'a> {
    base: &'a EnvList,
    masked: Vec<String>,
}

impl EnvGetter for SecretMask<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        if self.masked.iter().any(|k| k == key) {
            return Some("****");
        }
        self.base.get(key)
    }

    fn keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self
            .base
            .keys()
            .into_iter()
            .filter(|k| !self.masked.contains(k))
            .collect();
        keys.extend(self.masked.iter().cloned());
        keys
    }
}

/// moby/sys/signal's ParseSignal on Linux, for its verdict.
fn check_signal(raw: &str) -> Result<(), Error> {
    const NAMES: [&str; 34] = [
        "ABRT", "ALRM", "BUS", "CHLD", "CLD", "CONT", "FPE", "HUP", "ILL", "INT", "IO", "IOT",
        "KILL", "PIPE", "POLL", "PROF", "PWR", "QUIT", "SEGV", "STKFLT", "STOP", "SYS", "TERM",
        "TRAP", "TSTP", "TTIN", "TTOU", "URG", "USR1", "USR2", "VTALRM", "WINCH", "XCPU", "XFSZ",
    ];
    let invalid = || Error::new(format!("invalid signal: {raw}"));
    if let Some(n) = gofmt::atoi(raw) {
        return if n == 0 { Err(invalid()) } else { Ok(()) };
    }
    let upper = raw.to_uppercase();
    let name = upper.strip_prefix("SIG").unwrap_or(&upper);
    let realtime = |prefix: &str, sign: char| {
        name.strip_prefix(prefix).is_some_and(|rest| {
            rest.is_empty()
                || rest.strip_prefix(sign).is_some_and(|n| {
                    // Digits alone: Rust's parse would take a second sign (RTMIN++5).
                    !n.starts_with('0')
                        && n.bytes().all(|b| b.is_ascii_digit())
                        && n.parse::<u32>()
                            .is_ok_and(|n| (1..=if sign == '+' { 15 } else { 14 }).contains(&n))
                })
        })
    };
    if NAMES.contains(&name) || realtime("RTMIN", '+') || realtime("RTMAX", '-') {
        Ok(())
    } else {
        Err(invalid())
    }
}

/// The chmod checks of dispatchCopy: an octal number up to 07777, else a mode that
/// dchapes-mode reads.
fn check_chmod(chmod: &str) -> Result<(), Error> {
    let non_octal = || {
        Error::new(format!(
            "invalid chmod parameter: '{chmod}'. it should be octal string and between 0 and 07777"
        ))
    };
    if let Some(p) = gofmt::parse_octal_u32(chmod) {
        return if p > 0o7777 { Err(non_octal()) } else { Ok(()) };
    }
    match mode::parse(chmod) {
        Ok(_) => Ok(()),
        Err(mode::ModeError::Num { .. }) => Err(non_octal()),
        Err(e) => Err(Error::new(e.to_string())),
    }
}

/// What dfgitutil.ParseGitRef makes of a source: not a git reference, a github.com/...
/// path (a git reference the frontend takes for a local path), or a git reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GitSource {
    No,
    LooksLocal,
    Yes,
}

fn git_source(src: &str) -> GitSource {
    if src.starts_with("./") || src.starts_with("../") {
        return GitSource::No;
    }
    if src.starts_with("github.com/") {
        return GitSource::LooksLocal;
    }
    let scheme_len = src.bytes().take_while(u8::is_ascii_alphanumeric).count();
    if scheme_len > 0 && src[scheme_len..].starts_with("://") {
        return match src[..scheme_len].to_lowercase().as_str() {
            "http" | "https" => match url_path(src) {
                Some(path) if path.ends_with(".git") => GitSource::Yes,
                _ => GitSource::No,
            },
            "ssh" | "git" => GitSource::Yes,
            _ => GitSource::No,
        };
    }
    // `user@host:path`, the form git takes for ssh.
    let Some((user, rest)) = src.split_once('@') else {
        return GitSource::No;
    };
    let Some((host, _)) = rest.split_once(':') else {
        return GitSource::No;
    };
    let user_ok = !user.is_empty()
        && user
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    let host_ok = !host.is_empty()
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.');
    if user_ok && host_ok {
        GitSource::Yes
    } else {
        GitSource::No
    }
}

fn is_http_source(src: &str) -> bool {
    (src.starts_with("http://") || src.starts_with("https://")) && git_source(src) == GitSource::No
}

/// What COPY and ADD hand to dispatchCopy.
struct CopySpec<'c> {
    add: bool,
    sources: &'c SourcesAndDest,
    exclude: &'c [String],
    chown: &'c str,
    chmod: &'c str,
    link: bool,
    keep_git_dir: Option<bool>,
    checksum: &'c str,
    unpack: Option<bool>,
    parents: bool,
    from: From,
}

struct Dispatcher<'a> {
    prepared: &'a Prepared,
    index: usize,
    /// The stage as messages name it.
    name: String,
    options: &'a Options,
    lex: Lex,
    global: EnvList,
    names: HashMap<String, usize>,
    config: Config,
    env: EnvList,
    build_args: Vec<(String, Option<String>)>,
    /// A CMD ran in this stage, which ENTRYPOINT then keeps.
    cmd_set: bool,
    author: Option<String>,
    steps: Vec<Step>,
}

impl Dispatcher<'_> {
    /// The instruction as the progress output shows it.
    fn describe(&self, code: &str) -> String {
        gofmt::uppercase_cmd(&process_cmd_env(&self.lex, code, &self.env))
    }

    fn note(created_by: String, description: String) -> Step {
        Step {
            created_by,
            action: None,
            description,
        }
    }

    fn command(&mut self, mut cmd: Command, onbuild: bool) -> Result<(), Error> {
        // ARG expands its default itself, and only when no build argument replaces it.
        if !matches!(cmd, Command::Arg(_)) {
            if let Some(ex) = cmd.as_single_word_expansion() {
                let (lex, env) = (&self.lex, &self.env);
                ex.expand(&mut |w: &str| lex.process_word(w, env).map(|(s, _)| s))?;
            }
        }
        if let Some(ex) = cmd.as_single_word_expansion_raw() {
            let mut lex = Lex::new('\\');
            lex.skip_process_quotes = true;
            let env = &self.env;
            ex.expand_raw(&mut |w: &str| lex.process_word(w, env).map(|(s, _)| s))?;
        }
        let description = self.describe(cmd.code());
        let step = match &cmd {
            Command::Env(c) => {
                let mut msg = String::from("ENV");
                for e in &c.env {
                    msg.push(' ');
                    msg.push_str(&e.to_string());
                    self.env.set(&e.key, &e.value);
                    self.config.config.set_env(&e.key, &e.value);
                }
                Some(Self::note(msg, description))
            }
            Command::Maintainer(c) => {
                self.author = Some(c.maintainer.clone());
                Some(Self::note(
                    format!("MAINTAINER {}", c.maintainer),
                    description,
                ))
            }
            Command::Label(c) => {
                let mut msg = String::from("LABEL");
                for l in &c.labels {
                    self.config
                        .config
                        .labels
                        .insert(l.key.clone(), l.value.clone());
                    msg.push(' ');
                    msg.push_str(&l.to_string());
                }
                Some(Self::note(msg, description))
            }
            // The reference writes no history for ONBUILD.
            Command::Onbuild(c) => {
                self.config.config.on_build.push(c.expression.clone());
                None
            }
            Command::Cmd(c) => {
                let mut args = c.cmd_line.clone();
                if c.prepend_shell {
                    args = with_shell(&self.config, &args);
                }
                let msg = format!("CMD {}", gofmt::quote_list(&args));
                self.config.config.cmd = (!args.is_empty()).then_some(args);
                self.config.config.args_escaped = true;
                self.cmd_set = true;
                Some(Self::note(msg, description))
            }
            Command::Entrypoint(c) => {
                let mut args = c.cmd_line.clone();
                if c.prepend_shell {
                    args = with_shell(&self.config, &args);
                }
                let msg = format!("ENTRYPOINT {}", gofmt::quote_list(&args));
                self.config.config.entrypoint = (!args.is_empty()).then_some(args);
                if !self.cmd_set {
                    self.config.config.cmd = None;
                }
                Some(Self::note(msg, description))
            }
            Command::Healthcheck(c) => {
                let h = &c.health;
                let nanos =
                    |d: std::time::Duration| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX);
                let check = Healthcheck {
                    test: h.test.clone(),
                    interval: nanos(h.interval),
                    timeout: nanos(h.timeout),
                    start_period: nanos(h.start_period),
                    start_interval: nanos(h.start_interval),
                    retries: i64::from(h.retries),
                };
                let msg = format!(
                    "HEALTHCHECK {{Test:{} Interval:{} Timeout:{} StartPeriod:{} StartInterval:{} Retries:{}}}",
                    gofmt::list(&check.test),
                    gofmt::duration(check.interval),
                    gofmt::duration(check.timeout),
                    gofmt::duration(check.start_period),
                    gofmt::duration(check.start_interval),
                    check.retries
                );
                self.config.config.healthcheck = Some(check);
                Some(Self::note(msg, description))
            }
            Command::Expose(c) => {
                let mut ports = Vec::new();
                for p in &c.ports {
                    ports.extend(self.lex.process_words(p, &self.env)?);
                }
                let parsed = expose::parse_ports(&ports).map_err(Error::new)?;
                for p in &parsed {
                    self.config
                        .config
                        .exposed_ports
                        .insert(p.clone(), serde_json::json!({}));
                }
                Some(Self::note(
                    format!("EXPOSE {}", gofmt::list(&parsed)),
                    description,
                ))
            }
            Command::User(c) => {
                self.config.config.user.clone_from(&c.user);
                Some(Self::note(format!("USER {}", c.user), description))
            }
            Command::Volume(c) => {
                for v in &c.volumes {
                    if v.is_empty() {
                        return Err(Error::new("VOLUME specified can not be an empty string"));
                    }
                    self.config
                        .config
                        .volumes
                        .insert(v.clone(), serde_json::json!({}));
                }
                Some(Self::note(
                    format!("VOLUME {}", gofmt::list(&c.volumes)),
                    description,
                ))
            }
            Command::StopSignal(c) => {
                check_signal(&c.signal)?;
                self.config.config.stop_signal.clone_from(&c.signal);
                Some(Self::note(format!("STOPSIGNAL {}", c.signal), description))
            }
            Command::Shell(c) => {
                self.config.config.shell.clone_from(&c.shell);
                Some(Self::note(
                    format!("SHELL {}", gofmt::list(&c.shell)),
                    description,
                ))
            }
            Command::Arg(c) => {
                let mut parts = Vec::with_capacity(c.args.len());
                for arg in &c.args {
                    let given = self.options.build_args.get(&arg.key);
                    let value = match (given, &arg.value) {
                        (Some(v), _) => Some(v.clone()),
                        (None, Some(default)) => Some(self.lex.process_word(default, &self.env)?.0),
                        (None, None) => self.global.get(&arg.key).map(str::to_string),
                    };
                    if let Some(v) = &value {
                        if !NON_ENV_ARGS.contains(&arg.key.as_str()) {
                            self.env.set(&arg.key, v);
                        }
                    }
                    parts.push(match &value {
                        Some(v) => format!("{}={v}", arg.key),
                        None => arg.key.clone(),
                    });
                    self.build_args.push((arg.key.clone(), value));
                }
                Some(Self::note(format!("ARG {}", parts.join(" ")), description))
            }
            Command::Workdir(c) => Some(self.workdir(&c.path, description)),
            Command::Run(c) => Some(self.run(c)?),
            Command::Add(c) => Some(self.copy(
                CopySpec {
                    add: true,
                    sources: &c.sources_and_dest,
                    exclude: &c.exclude_patterns,
                    chown: &c.chown,
                    chmod: &c.chmod,
                    link: c.link,
                    keep_git_dir: c.keep_git_dir,
                    checksum: &c.checksum,
                    unpack: c.unpack,
                    parents: false,
                    from: From::Context,
                },
                description,
            )?),
            Command::Copy(c) => {
                let from = if c.from.is_empty() {
                    From::Context
                } else {
                    match copy_from_source(&c.from, &self.lex, &self.names, self.prepared.parsed.len())? {
                        Source::Stage(k) if k >= self.index => {
                            return Err(Error::new(format!(
                                "cannot copy from stage {}, it needs to be defined before current stage {}",
                                quote(&c.from),
                                quote(&self.name)
                            )))
                        }
                        source => source_from(source),
                    }
                };
                Some(self.copy(
                    CopySpec {
                        add: false,
                        sources: &c.sources_and_dest,
                        exclude: &c.exclude_patterns,
                        chown: &c.chown,
                        chmod: &c.chmod,
                        link: c.link,
                        keep_git_dir: None,
                        checksum: "",
                        unpack: None,
                        parents: c.parents,
                        from,
                    },
                    description,
                )?)
            }
        };
        if let Some(mut step) = step {
            if onbuild {
                step.description = format!("ONBUILD {}", step.description);
            }
            self.steps.push(step);
        }
        Ok(())
    }

    /// dispatchWorkdir of an instruction: a layer that makes the directory unless it is
    /// the root.
    fn workdir(&mut self, path: &str, description: String) -> Step {
        let wd = normalize_workdir(&self.config.config.working_dir, path);
        self.config.config.working_dir.clone_from(&wd);
        let action = (wd != "/").then(|| Action::Workdir {
            path: wd.clone(),
            user: self.config.config.user.clone(),
        });
        Step {
            created_by: format!("WORKDIR {wd}"),
            action,
            description,
        }
    }

    fn run(&mut self, c: &RunCommand) -> Result<Step, Error> {
        let mut args = c.cmd_line.clone();
        let mut script = None;
        let mut customname = c.code().to_string();
        if !c.files.is_empty() {
            if args.len() != 1 || !c.prepend_shell {
                return Err(Error::new(format!(
                    "parsing produced an invalid run command: {}",
                    gofmt::list(&args)
                )));
            }
            if must_parse_heredoc(&args[0]).is_some() {
                let file = &c.files[0];
                let data = if file.chomp {
                    chomp_heredoc_content(&file.data)
                } else {
                    file.data.clone()
                };
                // The expand option of the heredoc does not apply in either case.
                if self.config.os != "windows" && data.starts_with("#!") {
                    args = vec![gofmt::join(&["/dev/pipes/", &file.name])];
                    script = Some(Inline {
                        name: file.name.clone(),
                        data,
                    });
                } else {
                    args = vec![data];
                }
                customname.push_str(&format!(" ({})", summarize_heredoc(&file.data)));
            } else {
                // The contents are joined unchomped, as the reference joins them.
                let mut line = args[0].clone();
                for f in &c.files {
                    line.push('\n');
                    line.push_str(&f.data);
                    line.push_str(&f.name);
                }
                args = vec![line];
            }
        }
        if c.prepend_shell {
            args = with_shell(&self.config, &args);
        }

        let mounts = self.run_mounts(c)?;
        let insecure = c.security() == Security::Insecure;
        let network = match c.network() {
            NetworkMode::Default => Network::Default,
            NetworkMode::None => Network::None,
            NetworkMode::Host => Network::Host,
        };
        if !c.devices().is_empty() {
            return Err(Error::new(
                "RUN --device is not supported: nspawn's builder does not give devices to RUN",
            ));
        }

        let mut lex = self.lex.clone();
        lex.raw_quotes = true;
        lex.skip_unset_env = true;
        let masked = SecretMask {
            base: &self.env,
            masked: c
                .mounts()
                .iter()
                .filter(|m| m.mount_type == MountType::Secret)
                .filter_map(|m| m.env.clone())
                .collect(),
        };
        let description = gofmt::uppercase_cmd(&process_cmd_env(&lex, &customname, &masked));
        let created_by = format!(
            "RUN {}",
            run_command_string(&args, &self.build_args, &self.env)
        );

        let mut env = self.env.to_array();
        let proxy_env = proxy_env(&self.options.build_args);
        if envs_from_slice(&env).get("PATH").is_none() && self.config.os != "windows" {
            env.push(format!("PATH={DEFAULT_PATH}"));
        }
        let workdir = if self.config.config.working_dir.is_empty() {
            "/".to_string()
        } else {
            self.config.config.working_dir.clone()
        };
        Ok(Step {
            created_by,
            action: Some(Action::Run(Run {
                argv: args,
                script,
                env,
                proxy_env,
                user: self.config.config.user.clone(),
                workdir,
                mounts,
                network,
                insecure,
            })),
            description,
        })
    }

    /// dispatchRunMounts.
    fn run_mounts(&self, c: &RunCommand) -> Result<Vec<RunMount>, Error> {
        let mut out = Vec::with_capacity(c.mounts().len());
        for m in c.mounts() {
            let mut from = m.from.clone();
            if from.is_empty() && m.mount_type == MountType::Cache {
                from = "scratch".to_string();
            }
            let source = (!from.is_empty()).then(|| resolve_name(&from, &self.names));
            if let Some(Source::Stage(k)) = source {
                if k >= self.index {
                    return Err(Error::new(format!(
                        "cannot mount from stage {} to {}, stage needs to be defined before current command",
                        quote(&from),
                        quote(&m.target)
                    )));
                }
            }
            match m.mount_type {
                MountType::Secret => {
                    out.push(secret(m)?);
                    continue;
                }
                MountType::Ssh => {
                    return Err(Error::new(
                        "RUN --mount=type=ssh is not supported: nspawn's builder does not forward an SSH agent",
                    ))
                }
                _ => {}
            }
            let mut target = m.target.clone();
            if !target.starts_with('/') {
                target = gofmt::join(&["/", &self.config.config.working_dir, &m.target]);
            }
            if target == "/" {
                return Err(Error::new(format!(
                    "invalid mount target {}",
                    quote(&target)
                )));
            }
            let src = gofmt::join(&["/", &m.source]);
            out.push(match m.mount_type {
                MountType::Cache => RunMount::Cache {
                    target,
                    id: if m.cache_id.is_empty() {
                        gofmt::clean(&m.target)
                    } else {
                        m.cache_id.clone()
                    },
                    from: match source {
                        Some(Source::Stage(k)) => Some((From::Stage(k), src)),
                        Some(Source::Image(name)) => Some((From::Image(name), src)),
                        _ => None,
                    },
                    sharing: m
                        .cache_sharing
                        .unwrap_or(ShareMode::Shared)
                        .as_str()
                        .to_string(),
                    read_only: m.read_only,
                    mode: m.mode.map_or(0o755, |v| v as u32),
                    uid: m.uid.map_or(0, |v| v as u32),
                    gid: m.gid.map_or(0, |v| v as u32),
                },
                MountType::Tmpfs => RunMount::Tmpfs {
                    target,
                    size: u64::try_from(m.size_limit).ok().filter(|&s| s > 0),
                },
                _ => RunMount::Bind {
                    target,
                    from: source.map_or(From::Context, source_from),
                    source: src,
                    read_write: !m.read_only,
                },
            });
        }
        Ok(out)
    }

    fn copy(&mut self, spec: CopySpec<'_>, description: String) -> Result<Step, Error> {
        let sad = spec.sources;
        let dest = copy::dest_path(&self.config.config.working_dir, &sad.dest_path);
        if !spec.chmod.is_empty() {
            check_chmod(spec.chmod)?;
        }
        if !spec.checksum.is_empty() {
            if !spec.add {
                return Err(Error::new("checksum can't be specified for COPY"));
            }
            if sad.source_paths.len() != 1 {
                return Err(Error::new(
                    "checksum can't be specified for multiple sources",
                ));
            }
            let src = &sad.source_paths[0];
            if !is_http_source(src) && git_source(src) == GitSource::No {
                return Err(Error::new("checksum requires HTTP(S) or Git sources"));
            }
        }

        let mut msg = String::from(if spec.add { "ADD" } else { "COPY" });
        if spec.parents {
            msg.push_str(" --parents");
        }
        if !spec.chown.is_empty() {
            msg.push_str(&format!(" --chown={}", spec.chown));
        }
        if !spec.chmod.is_empty() {
            msg.push_str(&format!(" --chmod={}", spec.chmod));
        }
        for src in &sad.source_paths {
            msg.push(' ');
            msg.push_str(src);
            if git_source(src) == GitSource::Yes {
                if !spec.add {
                    return Err(Error::new("source can't be a git ref for COPY"));
                }
                return Err(Error::new(format!(
                    "ADD of the git repository {} is not supported: nspawn's builder does not fetch git sources",
                    quote(src)
                )));
            }
            if is_http_source(src) {
                if !spec.add {
                    return Err(Error::new("source can't be a URL for COPY"));
                }
                if !spec.checksum.is_empty() {
                    super::reference::parse_digest(spec.checksum).map_err(Error::new)?;
                }
            }
        }
        let mut inline = Vec::with_capacity(sad.source_contents.len());
        for content in &sad.source_contents {
            msg.push_str(" <<");
            msg.push_str(&content.path);
            inline.push(Inline {
                name: content.path.clone(),
                data: content.data.clone(),
            });
        }
        msg.push(' ');
        msg.push_str(&sad.dest_path);

        let some = |s: &str| (!s.is_empty()).then(|| s.to_string());
        Ok(Step {
            created_by: msg,
            action: Some(Action::Copy(Copy {
                add: spec.add,
                from: spec.from,
                sources: sad.source_paths.clone(),
                inline,
                dest,
                chown: some(spec.chown),
                chmod: some(spec.chmod),
                // A --link with --chmod is copied as one without it.
                link: spec.link && spec.chmod.is_empty(),
                parents: spec.parents,
                exclude: spec.exclude.to_vec(),
                checksum: some(spec.checksum),
                keep_git_dir: spec.keep_git_dir.unwrap_or(false),
                unpack: spec.unpack,
                user: self.config.config.user.clone(),
            })),
            description,
        })
    }
}

fn source_from(source: Source) -> From {
    match source {
        Source::Stage(k) => From::Stage(k),
        Source::Scratch => From::Scratch,
        Source::Image(name) => From::Image(name),
    }
}

/// dispatchSecret on Linux.
pub(super) fn secret(m: &Mount) -> Result<RunMount, Error> {
    let target_path = m.target.clone();
    let mut id = m.cache_id.clone();
    if !m.source.is_empty() {
        id.clone_from(&m.source);
    }
    if id.is_empty() {
        if target_path.is_empty() {
            return Err(Error::new("one of source, target required"));
        }
        id = gofmt::base(&target_path);
    }
    let mut target = (!target_path.is_empty()).then(|| target_path.clone());
    if m.env.is_none() {
        target = Some(if target_path.is_empty() {
            format!("/run/secrets/{}", gofmt::base(&id))
        } else {
            target_path
        });
    }
    let file_options = m.uid.is_some() || m.gid.is_some() || m.mode.is_some();
    let mode = if file_options {
        m.mode.unwrap_or(0o400)
    } else {
        0o400
    };
    Ok(RunMount::Secret {
        id,
        target,
        env: m.env.clone(),
        required: m.required,
        mode: mode as u32,
        uid: m.uid.unwrap_or(0) as u32,
        gid: m.gid.unwrap_or(0) as u32,
    })
}
