//! nspawn compose: the files and commands of docker compose, run on nspawn machines.

mod aliases;
mod canonical;
pub mod cli;
mod cobra;
mod config;
#[cfg(test)]
mod conformance;
mod construct;
mod dotenv;
mod emit;
mod emitter;
mod engine;
mod extends;
mod golib;
pub(crate) mod gostr;
mod gotime;
mod include;
mod interpolate;
mod libyaml;
mod loader;
mod merge;
mod model;
mod mount;
mod normalize;
mod options;
mod path;
mod paths;
mod project;
mod reference;
mod schema;
pub mod support;
mod template;
mod types;
mod unicity;
mod unicode;
mod value;
mod variables;
mod yaml;

use std::cell::Cell;
use std::io::{IsTerminal, Write};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use cli::{ComposeArgs, ComposeCommand};
use support::Support;

/// Where the messages go besides the output: warnings through logrus, notices straight
/// to stderr.
pub trait Log {
    fn warn(&self, text: String);
    /// Text written to stderr as it is.
    fn notice(&self, text: &str);
    /// --progress json: logrus writes JSON from then on.
    fn json(&self);
}

/// logrus on this process's stderr: colours and the seconds since the
/// start on a terminal, `time=... level=warning msg=...` otherwise, JSON once asked.
struct Stderr {
    start: Instant,
    terminal: bool,
    json: Cell<bool>,
}

impl Stderr {
    fn write(&self, text: &str) {
        let _ = std::io::stderr().lock().write_all(&gostr::to_bytes(text));
    }
}

/// What logrus's TextFormatter quotes: anything but letters, digits and `-._/@^+`.
fn needs_quoting(text: &str) -> bool {
    !text
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-._/@^+".contains(c))
}

/// The time logrus stamps a line with: now, in RFC 3339, in the local zone.
fn now() -> String {
    let unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    gotime::rfc3339(unix, gotime::local_offset(unix))
}

/// How logrus writes a warning: as JSON, with colours and the seconds since the start
/// on a terminal, or as `key=value` fields.
enum Format {
    Json,
    Terminal { seconds: u64 },
    Text,
}

/// A warning line as logrus formats it, at a time.
fn warning_line(format: Format, time: &str, text: &str) -> String {
    match format {
        Format::Json => {
            let mut msg = String::new();
            emit::json_string(text, true, &mut msg);
            let mut stamp = String::new();
            emit::json_string(time, true, &mut stamp);
            format!("{{\"level\":\"warning\",\"msg\":{msg},\"time\":{stamp}}}\n")
        }
        Format::Terminal { seconds } => {
            // One line break at the end goes; the text is padded to 44 runes.
            let text = text.strip_suffix('\n').unwrap_or(text);
            let pad = 44usize.saturating_sub(text.chars().count());
            format!(
                "\x1b[33mWARN\x1b[0m[{seconds:04}] {text}{} \n",
                " ".repeat(pad)
            )
        }
        Format::Text => {
            let value = |v: &str| {
                if needs_quoting(v) {
                    gostr::quote(v)
                } else {
                    v.to_string()
                }
            };
            format!("time={} level=warning msg={}\n", value(time), value(text))
        }
    }
}

impl Log for Stderr {
    fn warn(&self, text: String) {
        let format = if self.json.get() {
            Format::Json
        } else if self.terminal {
            Format::Terminal {
                seconds: self.start.elapsed().as_secs(),
            }
        } else {
            Format::Text
        };
        self.write(&warning_line(format, &now(), &text));
    }

    fn notice(&self, text: &str) {
        self.write(text);
    }

    fn json(&self) {
        self.json.set(true);
    }
}

/// The help of nspawn compose or of one of its commands.
fn help(path: &[String]) {
    use clap::CommandFactory;
    let mut command = crate::cli::Cli::command()
        .find_subcommand("compose")
        .cloned()
        .expect("nspawn compose");
    let mut name = "nspawn compose".to_string();
    for word in path {
        match command.find_subcommand(word) {
            Some(sub) => {
                command = sub.clone();
                name = format!("{name} {word}");
            }
            None => break,
        }
    }
    let _ = command.bin_name(name).print_help();
}

/// Go's os.Getwd: $PWD when it names the working directory, which keeps the path a
/// symbolic link gave, else the directory resolved.
fn working_directory(env: &dotenv::Env) -> std::io::Result<String> {
    use std::os::unix::fs::MetadataExt;
    let dot = std::fs::metadata(".")?;
    if let Some(pwd) = env.get("PWD").filter(|p| p.starts_with('/')) {
        if let Ok(meta) = std::fs::metadata(pwd) {
            if meta.dev() == dot.dev() && meta.ino() == dot.ino() {
                return Ok(pwd.clone());
            }
        }
    }
    Ok(std::env::current_dir()?.to_string_lossy().into_owned())
}

/// The arguments after `compose` when the command line is `nspawn [OPTION...] compose
/// ...`: nspawn's own options may come first, and the rest is compose's to read.
pub fn arguments(argv: &[std::ffi::OsString]) -> Option<Vec<String>> {
    use std::os::unix::ffi::OsStrExt;
    const GLOBAL: [&str; 3] = ["--registry", "--ca-cert", "--config"];
    let mut i = 1;
    while let Some(arg) = argv.get(i) {
        let arg = arg.to_str()?;
        if GLOBAL.contains(&arg) {
            i += 2;
        } else if GLOBAL
            .iter()
            .any(|g| arg.strip_prefix(g).is_some_and(|r| r.starts_with('=')))
        {
            i += 1;
        } else if arg == "compose" {
            return Some(
                argv[i + 1..]
                    .iter()
                    .map(|a| gostr::from_bytes(a.as_bytes()))
                    .collect(),
            );
        } else {
            return None;
        }
    }
    None
}

/// What the arguments after `nspawn compose` ask for.
pub enum Line {
    Run(Box<ComposeArgs>),
    /// The help of a command, by its path.
    Help(Vec<String>),
    /// An error, after compose's help on standard output when `help`.
    Error {
        message: String,
        help: bool,
    },
}

/// Reads the arguments after `nspawn compose`.
pub fn read(args: &[String]) -> Line {
    use clap::{CommandFactory, Parser};
    let compose = crate::cli::Cli::command()
        .find_subcommand("compose")
        .cloned()
        .expect("nspawn compose");
    let error = |message: String| Line::Error {
        message,
        help: false,
    };
    let line = match cobra::parse(&compose, args) {
        cobra::Outcome::Run(line) => line,
        cobra::Outcome::Help(path) => return Line::Help(path),
        cobra::Outcome::Fail(message) => return error(message),
        cobra::Outcome::Unknown(word) => {
            return match support::command(&word) {
                Some(Support::Refused(text)) => error(text.to_string()),
                _ => Line::Error {
                    message: format!(
                        "unknown docker command: {}",
                        gostr::quote(&format!("compose {word}"))
                    ),
                    help: true,
                },
            };
        }
    };
    let argv = ["nspawn", "compose"]
        .into_iter()
        .map(String::from)
        .chain(line);
    match crate::cli::Cli::try_parse_from(argv) {
        Ok(crate::cli::Cli {
            command: crate::cli::Command::Compose(args),
            ..
        }) => Line::Run(Box::new(args)),
        Ok(_) => unreachable!("the line starts with compose"),
        Err(e) => error(e.to_string().trim_end().to_string()),
    }
}

/// The stack nspawn compose runs on: go-yaml takes documents nested 10000 deep, and
/// each level is a call deep in every pass over the model. The memory is reserved, and
/// only what a document needs is used.
pub const STACK: usize = 1 << 30;

/// `nspawn compose ARGS...`: runs it and gives the exit status, an error printed on
/// stderr.
pub fn main(args: &[String]) -> i32 {
    let args = args.to_vec();
    std::thread::Builder::new()
        .name("compose".to_string())
        .stack_size(STACK)
        .spawn(move || run(&args))
        .map_or(1, |thread| thread.join().unwrap_or(101))
}

fn run(args: &[String]) -> i32 {
    let log = Stderr {
        start: Instant::now(),
        terminal: std::io::stderr().is_terminal(),
        json: Cell::new(false),
    };
    let fail = |text: &str| {
        log.notice(&format!("{text}\n"));
        1
    };
    let args = match read(args) {
        Line::Run(args) => args,
        Line::Help(path) => {
            help(&path);
            return 0;
        }
        Line::Error {
            message,
            help: with_help,
        } => {
            if with_help {
                help(&[]);
            }
            return fail(&message);
        }
    };
    let env: dotenv::Env = std::env::vars_os()
        .map(|(k, v)| {
            use std::os::unix::ffi::OsStrExt;
            (
                gostr::from_bytes(k.as_bytes()),
                gostr::from_bytes(v.as_bytes()),
            )
        })
        .collect();
    let cwd = match working_directory(&env) {
        Ok(cwd) => cwd,
        Err(e) => return fail(&gostr::errno_text(&e)),
    };
    let mut host = config::Host::new(env, cwd, config::Stdin::Process);
    // Not locked for the whole command: what a machine prints reaches stdout from
    // threads of its own.
    let mut out = std::io::stdout();
    let result = execute(&args, &mut host, &mut out, &log);
    let _ = out.flush();
    match result {
        Ok(()) => 0,
        Err(error) => fail(&error),
    }
}

/// Runs a command line nspawn compose has read: the decisions of the table on what it
/// asks, the checks and settings of the command line, then the command.
pub fn execute(
    args: &ComposeArgs,
    host: &mut config::Host,
    out: &mut dyn Write,
    log: &dyn Log,
) -> Result<(), String> {
    let requested = cli::requested(args);
    for &(command, name) in &requested {
        if let Some(Support::Refused(text)) = support::option(command, name) {
            return Err(format!("--{name}: {text}"));
        }
    }
    // The table's notes once the checks have set the format.
    let notes = || {
        for &(command, name) in &requested {
            if let Some(Support::Ignored(text)) = support::option(command, name) {
                log.warn(format!("--{name}: {text}"));
            }
        }
    };
    match &args.command {
        Some(ComposeCommand::Config(config)) => {
            let flags = pre_run(args, host, log)?;
            notes();
            let mut warn = |w: String| log.warn(w);
            let result = config::run(args, &flags, config, host, out, &mut warn);
            // Under --progress json the command's own error is JSON too.
            result.map_err(|e| {
                if log_is_json(args, host) {
                    let mut message = String::new();
                    emit::json_string(&e, true, &mut message);
                    format!("{{\"error\":true,\"message\":{message}}}")
                } else {
                    e
                }
            })
        }
        Some(ComposeCommand::Version(v)) => {
            notes();
            version(v.format.as_deref(), v.short, out).map_err(|e| e.to_string())
        }
        Some(_) => {
            let flags = pre_run(args, host, log)?;
            notes();
            engine::execute(args, host, &flags, log, out)
        }
        None => {
            notes();
            help(&[]);
            Ok(())
        }
    }
}

/// Whether --progress (or COMPOSE_PROGRESS, its default) asked for JSON.
fn log_is_json(args: &ComposeArgs, host: &config::Host) -> bool {
    args.progress
        .as_deref()
        .or(host.progress.as_deref())
        .is_some_and(|p| p == "json")
}

/// PersistentPreRunE for every command but version: the COMPOSE_
/// variables of the env files, the ANSI and progress settings and their checks, the
/// deprecated --workdir, the env files made absolute, COMPOSE_PARALLEL_LIMIT. What comes
/// out are the flags that make the project.
fn pre_run(
    args: &ComposeArgs,
    host: &mut config::Host,
    log: &dyn Log,
) -> Result<options::Flags, String> {
    let mut warn = |w: String| log.warn(w);
    let env_files = if args.env_file.is_empty() {
        host.compose_env_files.clone()
    } else {
        args.env_file.clone()
    };
    let mut flags = options::Flags {
        files: args.file.clone(),
        project_name: args.project_name.clone().unwrap_or_default(),
        project_directory: args.project_directory.clone().unwrap_or_default(),
        env_files,
        profiles: args.profile.clone(),
    };
    options::compose_env_from_files(&flags, &mut host.env, &host.cwd, &mut warn)?;
    let mut ansi = args.ansi.clone().unwrap_or_else(|| "auto".to_string());
    if args.no_ansi {
        if ansi != "auto" {
            return Err(
                "cannot specify DEPRECATED \"--no-ansi\" and \"--ansi\". Please use only \"--ansi\""
                    .to_string(),
            );
        }
        ansi = "never".to_string();
        log.notice("option '--no-ansi' is DEPRECATED ! Please use '--ansi' instead.\n");
    }
    // --ansi is looked for among the command's own flags, where it never is:
    // COMPOSE_ANSI wins over it.
    if let Some(v) = host.env.get("COMPOSE_ANSI") {
        ansi.clone_from(v);
    }
    let progress = args
        .progress
        .clone()
        .or_else(|| host.progress.clone())
        .unwrap_or_default();
    match progress.as_str() {
        "" | "auto" | "quiet" | "none" => {}
        "tty" if ansi == "never" => {
            return Err("can't use --progress tty while ANSI support is disabled".to_string())
        }
        "plain" if ansi == "always" => {
            return Err("can't use --progress plain while ANSI support is forced".to_string())
        }
        "tty" | "plain" => {}
        "json" => log.json(),
        other => {
            return Err(format!(
                "unsupported --progress value {}",
                gostr::quote(other)
            ))
        }
    }
    if let Some(workdir) = args.workdir.as_deref().filter(|w| !w.is_empty()) {
        if !flags.project_directory.is_empty() {
            return Err("cannot specify DEPRECATED \"--workdir\" and \"--project-directory\". Please use only \"--project-directory\" instead".to_string());
        }
        flags.project_directory = workdir.to_string();
        log.notice("\x1b[31moption '--workdir' is DEPRECATED at root level! Please use '--project-directory' instead.\n\x1b[0m");
    }
    for file in &mut flags.env_files {
        let home = host
            .env
            .get("HOME")
            .filter(|h| !h.is_empty())
            .map(String::as_str);
        let expanded = paths::expand_user(file, home, &mut warn);
        *file = paths::abs(&expanded, &host.cwd);
    }
    if args.parallel.is_none() {
        if let Some(v) = host.env.get("COMPOSE_PARALLEL_LIMIT") {
            if gostr::atoi(v).is_err() {
                return Err(format!(
                    "COMPOSE_PARALLEL_LIMIT must be an integer (found: {})",
                    gostr::quote(v)
                ));
            }
        }
    }
    Ok(flags)
}

/// `version`: the docker compose release nspawn compose matches, and nspawn's own.
fn version(format: Option<&str>, short: bool, out: &mut dyn Write) -> anyhow::Result<()> {
    let compose = support::DOCKER_COMPOSE;
    if short {
        writeln!(out, "{}", compose.trim_start_matches('v'))?;
    } else if format == Some("json") {
        writeln!(out, "{{\"version\":{}}}", golib::go_quote(compose))?;
    } else {
        writeln!(
            out,
            "nspawn compose version {}, docker compose {compose} (compose-go {})",
            env!("CARGO_PKG_VERSION"),
            support::COMPOSE_GO
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warnings_are_written_as_logrus_writes_them() {
        let time = "2026-10-15T12:00:00-05:00";
        assert_eq!(
            warning_line(Format::Text, time, "a \"b\""),
            "time=\"2026-10-15T12:00:00-05:00\" level=warning msg=\"a \\\"b\\\"\"\n"
        );
        // Nothing to quote, nothing quoted.
        assert_eq!(
            warning_line(Format::Text, time, "plain.word/x"),
            "time=\"2026-10-15T12:00:00-05:00\" level=warning msg=plain.word/x\n"
        );
        assert_eq!(
            warning_line(Format::Terminal { seconds: 3 }, time, "short\n"),
            format!("\x1b[33mWARN\x1b[0m[0003] short{} \n", " ".repeat(39))
        );
        assert_eq!(
            warning_line(Format::Json, time, "<x>"),
            "{\"level\":\"warning\",\"msg\":\"\\u003cx\\u003e\",\"time\":\"2026-10-15T12:00:00-05:00\"}\n"
        );
    }

    /// A deeply nested document is read on the stack nspawn compose runs on, where the
    /// 2 MiB of a test's thread are far from enough; go-yaml takes 10000 levels, which
    /// the same stack holds, but take long in a debug build.
    #[test]
    fn deep_documents_fit_the_stack() {
        let depth = 3_000;
        let dir = std::env::temp_dir().join(format!("nspawn-deep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let text = format!(
            "services:\n  web:\n    image: x\nx-deep: {}{}\n",
            "[".repeat(depth),
            "]".repeat(depth)
        );
        std::fs::write(dir.join("compose.yaml"), text).unwrap();
        let cwd = dir.to_string_lossy().into_owned();
        let output = std::thread::Builder::new()
            .stack_size(STACK)
            .spawn(move || {
                let Line::Run(args) = read(&["config".to_string(), "-q".to_string()]) else {
                    panic!("a command line");
                };
                let env = dotenv::Env::from([("HOME".to_string(), "/nowhere".to_string())]);
                let mut host = config::Host::new(env, cwd, config::Stdin::Process);
                let record = Silent;
                execute(&args, &mut host, &mut Vec::new(), &record)
            })
            .unwrap()
            .join()
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(output, Ok(()));
    }

    struct Silent;

    impl Log for Silent {
        fn warn(&self, _: String) {}
        fn notice(&self, _: &str) {}
        fn json(&self) {}
    }

    #[derive(Default)]
    struct Warnings(std::cell::RefCell<Vec<String>>);

    impl Log for Warnings {
        fn warn(&self, text: String) {
            self.0.borrow_mut().push(text);
        }
        fn notice(&self, _: &str) {}
        fn json(&self) {}
    }

    /// A project of files written to a directory of its own, and what `config` makes of
    /// it there: the output or the error, and the warnings.
    fn config_of(name: &str, files: &[(&str, &str)]) -> (Result<String, String>, Vec<String>) {
        let dir = std::env::temp_dir().join(format!("nspawn-{name}-{}", std::process::id()));
        for (file, text) in files {
            let path = dir.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let Line::Run(args) = read(&["config".to_string()]) else {
            panic!("a command line");
        };
        let env = dotenv::Env::from([("HOME".to_string(), "/nowhere".to_string())]);
        let mut host = config::Host::new(
            env,
            dir.to_string_lossy().into_owned(),
            config::Stdin::Process,
        );
        let warnings = Warnings::default();
        let mut out = Vec::new();
        let result = execute(&args, &mut host, &mut out, &warnings)
            .map(|()| String::from_utf8(out).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
        (result, warnings.0.into_inner())
    }

    /// compose-go looks for an include cycle on an include's first file only and goes
    /// round one through an override without end; nspawn refuses it.
    #[test]
    fn an_include_cycle_through_an_override_is_refused() {
        let (result, _) = config_of(
            "cycle",
            &[
                (
                    "compose.yaml",
                    "include:\n  - path: [a.yaml, compose.yaml]\nservices:\n  web:\n    image: x\n",
                ),
                ("a.yaml", "services:\n  a:\n    image: y\n"),
            ],
        );
        assert!(result.unwrap_err().starts_with("include cycle detected:"));
    }

    /// Projects included from many places are loaded once, as compose-go caches them:
    /// a diamond twenty levels deep, two million loads without the cache.
    #[test]
    fn included_projects_are_loaded_once() {
        let mut files = vec![(
            "compose.yaml".to_string(),
            "include: [l0a.yaml, l0b.yaml]\nservices:\n  web:\n    image: x\n".to_string(),
        )];
        for level in 0..20 {
            let next = if level == 19 {
                "version: \"3\"\nservices:\n  bottom:\n    image: z\n".to_string()
            } else {
                format!("include: [l{0}a.yaml, l{0}b.yaml]\n", level + 1)
            };
            files.push((format!("l{level}a.yaml"), next.clone()));
            files.push((format!("l{level}b.yaml"), next));
        }
        let files: Vec<(&str, &str)> = files
            .iter()
            .map(|(f, t)| (f.as_str(), t.as_str()))
            .collect();
        let (result, warnings) = config_of("diamond", &files);
        assert!(result.unwrap().contains("bottom:"));
        // The obsolete version of each of the two files at the bottom, once.
        assert_eq!(
            warnings
                .iter()
                .filter(|w| w.contains("`version` is obsolete"))
                .count(),
            2
        );
    }

    /// What reaches the log, and whether it was JSON by then.
    #[derive(Default)]
    struct Lines {
        json: Cell<bool>,
        lines: std::cell::RefCell<Vec<(bool, String)>>,
    }

    impl Log for Lines {
        fn warn(&self, text: String) {
            self.lines.borrow_mut().push((self.json.get(), text));
        }
        fn notice(&self, _: &str) {}
        fn json(&self) {
            self.json.set(true);
        }
    }

    /// The table's notes come once the checks have set the format.
    #[test]
    fn the_table_s_notes_follow_the_progress_format() {
        let dir = std::env::temp_dir().join(format!("nspawn-notes-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("compose.yaml"),
            "services:\n  web:\n    image: x\n",
        )
        .unwrap();
        let line = [
            "--progress",
            "json",
            "--insecure-registry",
            "r",
            "config",
            "-q",
        ];
        let Line::Run(args) = read(&line.map(String::from)) else {
            panic!("a command line");
        };
        let env = dotenv::Env::from([("HOME".to_string(), "/nowhere".to_string())]);
        let mut host = config::Host::new(
            env,
            dir.to_string_lossy().into_owned(),
            config::Stdin::Process,
        );
        let log = Lines::default();
        let result = execute(&args, &mut host, &mut Vec::new(), &log);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(result, Ok(()));
        let lines = log.lines.into_inner();
        assert!(lines
            .iter()
            .any(|(json, text)| *json && text.starts_with("--insecure-registry: ")));
    }

    /// A file the env files cannot be looked for in is an error, with Go's words.
    #[test]
    fn an_env_file_out_of_reach_is_an_error() {
        use std::os::unix::fs::PermissionsExt;
        if nix::unistd::geteuid().is_root() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("nspawn-locked-{}", std::process::id()));
        let locked = dir.join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::write(locked.join("x.env"), "A=1\n").unwrap();
        std::fs::write(
            dir.join("compose.yaml"),
            "services:\n  web:\n    image: x\n    env_file: locked/x.env\n",
        )
        .unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let Line::Run(args) = read(&["config".to_string()]) else {
            panic!("a command line");
        };
        let env = dotenv::Env::from([("HOME".to_string(), "/nowhere".to_string())]);
        let mut host = config::Host::new(
            env,
            dir.to_string_lossy().into_owned(),
            config::Stdin::Process,
        );
        let result = execute(&args, &mut host, &mut Vec::new(), &Silent);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let path = locked.join("x.env");
        assert_eq!(
            result,
            Err(format!("open {}: permission denied", path.display()))
        );
    }

    /// --resolve-image-digests pins each image to the digest its registry names now,
    /// asked once per image, and --lock-image-digests keeps the images alone.
    #[test]
    fn images_are_pinned_to_what_their_registry_names() {
        let hex = |c: char| c.to_string().repeat(64);
        let (a, b, c) = (hex('a'), hex('b'), hex('c'));
        let digests = std::collections::HashMap::from([
            (
                "docker.io/library/busybox:1.36".to_string(),
                format!("sha256:{a}"),
            ),
            (
                "docker.io/library/alpine:latest".to_string(),
                format!("sha256:{b}"),
            ),
            ("ghcr.io/x/db:1".to_string(), format!("sha256:{c}")),
        ]);
        let run = |yaml: &str, line: &[&str]| -> Result<String, String> {
            let line: Vec<String> = ["-f", "-", "-p", "p", "config"]
                .iter()
                .chain(line)
                .map(|s| s.to_string())
                .collect();
            let Line::Run(args) = read(&line) else {
                panic!("a command line");
            };
            let env = dotenv::Env::from([("HOME".to_string(), "/nowhere".to_string())]);
            let mut host = config::Host::new(
                env,
                "/nowhere".to_string(),
                config::Stdin::Given(std::cell::RefCell::new(yaml.as_bytes().to_vec())),
            );
            host.digests = config::Digests::Given(digests.clone());
            let mut out = Vec::new();
            execute(&args, &mut host, &mut out, &Silent).map(|()| String::from_utf8(out).unwrap())
        };
        let yaml = "services:\n  web:\n    image: busybox:1.36\n    volumes:\n      - type: image\n        source: alpine\n        target: /a\n  db:\n    image: ghcr.io/x/db:1\n  again:\n    image: docker.io/library/busybox:1.36\n";
        assert_eq!(
            run(yaml, &["--lock-image-digests"]).unwrap(),
            format!("services:\n  again:\n    image: docker.io/library/busybox:1.36@sha256:{a}\n  db:\n    image: ghcr.io/x/db:1@sha256:{c}\n  web:\n    image: docker.io/library/busybox:1.36@sha256:{a}\n    volumes:\n      - type: image\n        source: docker.io/library/alpine:latest@sha256:{b}\n        target: /a\n")
        );
        let full = run(yaml, &["--resolve-image-digests", "db"]).unwrap();
        assert!(
            full.starts_with(&format!(
                "name: p\nservices:\n  db:\n    image: ghcr.io/x/db:1@sha256:{c}\n"
            )),
            "{full}"
        );
        let raw = run(yaml, &["--no-interpolate", "--lock-image-digests"]).unwrap();
        assert!(
            raw.contains(&format!(
                "source: docker.io/library/alpine:latest@sha256:{b}\n"
            )),
            "{raw}"
        );
        // Without either flag, nothing is asked.
        assert!(run(yaml, &[])
            .unwrap()
            .contains("    image: busybox:1.36\n"));
        // A tag its registry does not have, and a name that is no reference.
        assert_eq!(
            run("services:\n  web:\n    image: nope\n", &["--resolve-image-digests"]),
            Err("failed to resolve digest for docker.io/library/nope:latest: docker.io/library/nope:latest: not found".to_string())
        );
        assert_eq!(
            run(
                "services:\n  web:\n    image: Busybox\n",
                &["--lock-image-digests"]
            ),
            Err(
                "invalid reference format: repository name (library/Busybox) must be lowercase"
                    .to_string()
            )
        );
    }
}
