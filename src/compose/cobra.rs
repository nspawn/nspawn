//! The command line read the way cobra and pflag read it: the global flags before the
//! command and only there (but --dry-run), a bool
//! flag that takes `=false`, the last of a repeated flag winning, flags after the
//! command's arguments, and their errors word for word. The flags themselves are clap's
//! definitions in src/compose/cli.rs; what this reads is handed to clap again in a form
//! it cannot read otherwise than intended.

use clap::{ArgAction, Command};

use super::gostr;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Bool,
    Str,
    Strings,
    Int,
    /// opts.MemBytes: a size as units.RAMInBytes reads one.
    Bytes,
    /// opts.FilterOpt: NAME=VALUE, an empty one passed over.
    Filters,
}

#[derive(Debug)]
struct Flag {
    name: String,
    short: Option<char>,
    kind: Kind,
    /// A bool whose default is decided later (a default of true, or what the terminal
    /// says): clap gets its value either way.
    tristate: bool,
}

/// The flags of a command as pflag knows them: its own, and with `persistent` those it
/// inherits; cobra adds -h/--help to every command.
fn flags_of(command: &Command, inherited: &[Flag]) -> Vec<Flag> {
    let mut flags = vec![Flag {
        name: "help".into(),
        short: Some('h'),
        kind: Kind::Bool,
        tristate: false,
    }];
    for arg in command.get_arguments() {
        let Some(name) = arg.get_long() else {
            continue;
        };
        if name == "help" || arg.is_global_set() && !inherited.is_empty() {
            continue;
        }
        let parser = arg.get_value_parser().type_id();
        let tristate = !matches!(arg.get_action(), ArgAction::SetTrue)
            && parser == std::any::TypeId::of::<bool>();
        let filters = arg
            .get_value_names()
            .is_some_and(|n| n.iter().any(|n| n == "FILTER"));
        let kind = match arg.get_action() {
            ArgAction::SetTrue => Kind::Bool,
            _ if tristate => Kind::Bool,
            ArgAction::Append if filters => Kind::Filters,
            ArgAction::Append => Kind::Strings,
            _ if parser == std::any::TypeId::of::<i64>() => Kind::Int,
            _ if arg
                .get_value_names()
                .is_some_and(|n| n.iter().any(|n| n == "MEMORY")) =>
            {
                Kind::Bytes
            }

            _ => Kind::Str,
        };
        flags.push(Flag {
            name: name.to_string(),
            short: arg.get_short(),
            kind,
            tristate,
        });
    }
    for flag in inherited {
        flags.push(Flag {
            name: flag.name.clone(),
            short: flag.short,
            kind: flag.kind,
            tristate: flag.tristate,
        });
    }
    flags
}

/// The flags a command hands down to its subcommands: cobra's persistent flags, which
/// are clap's global ones.
fn persistent(command: &Command) -> Vec<Flag> {
    flags_of(command, &[])
        .into_iter()
        .filter(|f| {
            command
                .get_arguments()
                .any(|a| a.get_long() == Some(f.name.as_str()) && a.is_global_set())
        })
        .collect()
}

/// The values a command line gave, as pflag holds them.
#[derive(Default, Debug)]
struct Values {
    set: Vec<(String, Vec<String>)>,
    args: Vec<String>,
}

impl Values {
    fn put(&mut self, flag: &Flag, value: String) {
        match self.set.iter_mut().find(|(n, _)| *n == flag.name) {
            Some((_, values)) if matches!(flag.kind, Kind::Strings | Kind::Filters) => {
                values.push(value)
            }
            Some((_, values)) => *values = vec![value],
            None => self.set.push((flag.name.clone(), vec![value])),
        }
    }

    fn get(&self, name: &str) -> Option<&[String]> {
        self.set
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_slice())
    }

    fn help(&self) -> bool {
        self.get("help").is_some_and(|v| v[0] == "true")
    }
}

/// strconv.ParseBool.
fn parse_bool(text: &str) -> Result<bool, String> {
    match text {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Ok(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Ok(false),
        _ => Err(format!(
            "strconv.ParseBool: parsing {}: invalid syntax",
            gostr::quote(text)
        )),
    }
}

/// Go's %q of a rune.
fn quote_rune(c: char) -> String {
    let quoted = gostr::quote(&c.to_string());
    let inner = &quoted[1..quoted.len() - 1];
    let inner = match inner {
        "\\\"" => "\"".to_string(),
        "'" => "\\'".to_string(),
        other => other.to_string(),
    };
    format!("'{inner}'")
}

/// pflag's Set: the value checked against the flag's type, kept in canonical form.
fn set(values: &mut Values, flag: &Flag, value: &str) -> Result<(), String> {
    let canonical = match flag.kind {
        Kind::Bool => parse_bool(value).map(|b| b.to_string()),
        Kind::Int => gostr::parse_int(value, 0, 64)
            .map(|n| n.to_string())
            .map_err(|e| gostr::num_error("ParseInt", value, e)),
        Kind::Bytes => {
            crate::build::dockerfile::compat::ram_in_bytes(value).map(|_| value.to_string())
        }
        Kind::Filters if value.is_empty() => return Ok(()),
        Kind::Filters if !value.contains('=') => {
            Err("bad format of filter (expected name=value)".to_string())
        }
        Kind::Filters => {
            let (name, value) = value.split_once('=').unwrap_or_default();
            Ok(format!("{}={}", name.trim().to_lowercase(), value.trim()))
        }
        Kind::Str | Kind::Strings => Ok(value.to_string()),
    };
    match canonical {
        Ok(v) => {
            values.put(flag, v);
            Ok(())
        }
        Err(cause) => {
            // The help flag has its -h marked deprecated: pflag names it alone.
            let name = match flag.short {
                Some(s) if flag.name != "help" => format!("-{s}, --{}", flag.name),
                _ => format!("--{}", flag.name),
            };
            Err(format!(
                "invalid argument {} for {} flag: {cause}",
                gostr::quote(value),
                gostr::quote(&name)
            ))
        }
    }
}

/// pflag's parseArgs over a command's flags.
/// The flag names a command's SetNormalizeFunc maps to its own, and the deprecation
/// warning some of them give.
fn normalize<'a>(command: &str, name: &'a str, notes: &mut Vec<String>) -> &'a str {
    match (command, name) {
        ("down", "volume") => {
            notes.push("--volume is deprecated, please use --volumes".to_string());
            "volumes"
        }
        ("up" | "create", "y") => {
            notes.push("--y is deprecated, please use --yes instead".to_string());
            "yes"
        }
        ("exec" | "run", "no-TTY") => "no-tty",
        ("run", "volumes") => "volume",
        ("run", "labels") => "label",
        _ => name,
    }
}

fn parse_flags(
    flags: &[Flag],
    args: &[String],
    interspersed: bool,
    command: &str,
    notes: &mut Vec<String>,
) -> Result<Values, String> {
    let mut values = Values::default();
    let mut rest = args;
    while let Some((s, tail)) = rest.split_first() {
        rest = tail;
        let b = s.as_bytes();
        if b.len() < 2 || b[0] != b'-' {
            values.args.push(s.clone());
            if !interspersed {
                values.args.extend(rest.iter().cloned());
                return Ok(values);
            }
            continue;
        }
        if b[1] == b'-' {
            if b.len() == 2 {
                values.args.extend(rest.iter().cloned());
                break;
            }
            let name = &s[2..];
            if name.starts_with(['-', '=']) {
                return Err(format!("bad flag syntax: {s}"));
            }
            let (name, inline) = match name.split_once('=') {
                Some((n, v)) => (n, Some(v)),
                None => (name, None),
            };
            let name = normalize(command, name, notes);
            let Some(flag) = flags.iter().find(|f| f.name == name) else {
                return Err(format!("unknown flag: --{name}"));
            };
            let value = match inline {
                Some(v) => v.to_string(),
                None if flag.kind == Kind::Bool => "true".to_string(),
                None => match rest.split_first() {
                    Some((v, tail)) => {
                        rest = tail;
                        v.clone()
                    }
                    None => return Err(format!("flag needs an argument: --{name}")),
                },
            };
            set(&mut values, flag, &value)?;
        } else {
            // A run of shorthands, byte by byte, each looked up in the whole run's
            // original arguments, as pflag does.
            let args_at_start = rest;
            let run = gostr::to_bytes(&s[1..]);
            let mut at = 0;
            while at < run.len() {
                let c = run[at];
                let shorthands = &run[at..];
                let after = &run[at + 1..];
                let text = |bytes: &[u8]| gostr::from_bytes(bytes);
                let Some(flag) = flags.iter().find(|f| f.short == Some(char::from(c))) else {
                    return Err(format!(
                        "unknown shorthand flag: {} in -{}",
                        quote_rune(char::from(c)),
                        text(shorthands)
                    ));
                };
                let value = if after.len() > 1 && after[0] == b'=' {
                    at = run.len();
                    text(&after[1..])
                } else if flag.kind == Kind::Bool {
                    at += 1;
                    "true".to_string()
                } else if !after.is_empty() {
                    at = run.len();
                    text(after)
                } else {
                    match args_at_start.split_first() {
                        Some((v, tail)) => {
                            rest = tail;
                            at = run.len();
                            v.clone()
                        }
                        None => {
                            return Err(format!(
                                "flag needs an argument: {} in -{}",
                                quote_rune(char::from(c)),
                                text(shorthands)
                            ))
                        }
                    }
                };
                set(&mut values, flag, &value)?;
            }
        }
    }
    Ok(values)
}

/// What a command line asks of nspawn compose.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Run it: the command line as clap reads it, after `nspawn compose`.
    Run(Vec<String>),
    /// The help of a command: its path below compose.
    Help(Vec<String>),
    /// compose itself with words that name no command: the first of them.
    Unknown(String),
    /// A refusal, printed as it is.
    Fail(String),
}

/// The flags of the docker client that docker-compose takes before the command and
/// hands to it: those without a value, and those with one.
const CLIENT_BOOLS: [&str; 4] = ["--debug", "-D", "--tls", "--tlsverify"];
const CLIENT_VALUES: [&str; 7] = [
    "--tlscacert",
    "--tlscert",
    "--tlskey",
    "--host",
    "-H",
    "--context",
    "--log-level",
];

/// The standalone binary's conversion of its arguments before cobra sees them: up to
/// the first word, `-h` is `--help`, `-v` and `--version` are the version command, and
/// the flags of the docker client go to that client, which here is nowhere.
fn convert(args: &[String]) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut i = 0;
    'args: while i < args.len() {
        let arg = &args[i];
        if !arg.is_empty() && !arg.starts_with('-') {
            out.extend(args[i..].iter().cloned());
            break;
        }
        let arg = match arg.as_str() {
            "--verbose" => "--debug",
            "-h" => "--help",
            "--version" | "-v" => "version",
            other => other,
        };
        i += 1;
        if CLIENT_BOOLS.contains(&arg) {
            continue;
        }
        for flag in CLIENT_VALUES {
            if arg == flag {
                if i >= args.len() {
                    return Err(format!("flag needs an argument: '{arg}'"));
                }
                i += 1;
                continue 'args;
            }
            // `--hostx=v` is read as `--host v`.
            if arg.starts_with(flag) && arg.contains('=') {
                continue 'args;
            }
        }
        out.push(arg.to_string());
    }
    Ok(out)
}

/// cobra's Traverse over one command: the flags before the first word, and the
/// subcommand that word names, if it names one.
fn traverse<'a>(
    command: &'a Command,
    flags: &[Flag],
    args: &[String],
) -> Option<(usize, &'a Command)> {
    let no_value = |name: &str| flags.iter().any(|f| f.name == name && f.kind == Kind::Bool);
    let mut in_flag = false;
    for (i, arg) in args.iter().enumerate() {
        let b = arg.as_bytes();
        if arg.starts_with("--") && !arg.contains('=') {
            in_flag = !no_value(&arg[2..]);
            continue;
        }
        if b.len() == 2 && b[0] == b'-' && !arg.contains('=') {
            let c = arg[1..].chars().next();
            in_flag = !flags.iter().any(|f| f.short == c && f.kind == Kind::Bool);
            continue;
        }
        if in_flag {
            in_flag = false;
            continue;
        }
        if (b.len() >= 3 && arg.starts_with("--")) || (b.len() >= 2 && b[0] == b'-' && b[1] != b'-')
        {
            continue;
        }
        return command
            .get_subcommands()
            .find(|s| s.get_name() == arg || s.get_all_aliases().any(|a| a == arg))
            .map(|s| (i, s));
    }
    None
}

/// Canonical arguments for clap from what pflag read.
fn canonical(flags: &[Flag], values: &Values, out: &mut Vec<String>) {
    for flag in flags {
        if flag.name == "help" {
            continue;
        }
        let Some(set) = values.get(&flag.name) else {
            continue;
        };
        match flag.kind {
            Kind::Bool if flag.tristate => out.push(format!("--{}={}", flag.name, set[0])),
            Kind::Bool => {
                if set[0] == "true" {
                    out.push(format!("--{}", flag.name));
                }
            }
            _ => out.extend(set.iter().map(|v| format!("--{}={v}", flag.name))),
        }
    }
}

/// --version with words after it: the version command runs on its own, on the
/// arguments as they were given, where the flag that got there is not one of the
/// command's; cobra prints the error and the usage, then the error comes again.
fn version_again(compose: &Command, given: &[String]) -> Outcome {
    let version = compose
        .find_subcommand("version")
        .expect("compose has a version command");
    let flags = flags_of(version, &[]);
    match parse_flags(&flags, given, true, "version", &mut Vec::new()) {
        Err(e) => Outcome::Fail(format!("Error: {e}\n{}\n{e}", version_usage(version))),
        Ok(values) if values.help() => Outcome::Help(vec!["version".to_string()]),
        Ok(values) => {
            let mut out = vec!["version".to_string()];
            canonical(&flags, &values, &mut out);
            Outcome::Run(out)
        }
    }
}

/// cobra's usage of the version command as a command of its own: its use line and
/// pflag's FlagUsages, sorted by name, the texts aligned.
fn version_usage(version: &Command) -> String {
    let mut flags = flags_of(version, &[]);
    flags.sort_by(|a, b| a.name.cmp(&b.name));
    let lines: Vec<(String, String)> = flags
        .iter()
        .map(|flag| {
            let mut line = match flag.short {
                Some(s) => format!("  -{s}, --{}", flag.name),
                None => format!("      --{}", flag.name),
            };
            line.push_str(match flag.kind {
                Kind::Bool => "",
                Kind::Str => " string",
                Kind::Strings => " stringArray",
                Kind::Int => " int",
                Kind::Bytes => " bytes",
                Kind::Filters => " filter",
            });
            let usage = if flag.name == "help" {
                "help for version".to_string()
            } else {
                version
                    .get_arguments()
                    .find(|a| a.get_long() == Some(flag.name.as_str()))
                    .and_then(|a| a.get_help())
                    .map(ToString::to_string)
                    .unwrap_or_default()
            };
            (line, usage)
        })
        .collect();
    let width = lines.iter().map(|(line, _)| line.len()).max().unwrap_or(0);
    let mut usage = "Usage:\n  version [OPTIONS] [flags]\n\nFlags:\n".to_string();
    for (line, text) in lines {
        usage.push_str(&format!("{line:width$}   {text}\n"));
    }
    usage
}

/// Reads the arguments after `nspawn compose`, the warnings the flag names give into
/// `notes`.
pub fn parse_noting(compose: &Command, given: &[String], notes: &mut Vec<String>) -> Outcome {
    let args = match convert(given) {
        Ok(args) => args,
        Err(e) => return Outcome::Fail(e),
    };
    let root_flags = flags_of(compose, &[]);
    let inherited = persistent(compose);
    let Some((at, sub)) = traverse(compose, &root_flags, &args) else {
        // compose itself: its flags, then what is left are words that are not commands.
        let values = match parse_flags(&root_flags, &args, false, "", &mut Vec::new()) {
            Ok(v) => v,
            Err(e) => return Outcome::Fail(e),
        };
        return match values.args.first() {
            _ if values.help() => Outcome::Help(Vec::new()),
            None => Outcome::Help(Vec::new()),
            Some(_) if values.get("version").is_some_and(|v| v[0] == "true") => {
                version_again(compose, given)
            }
            Some(word) => Outcome::Unknown(word.clone()),
        };
    };
    let mut root = match parse_flags(&root_flags, &args[..at], false, "", notes) {
        Ok(v) => v,
        Err(e) => return Outcome::Fail(e),
    };
    let sub_flags = flags_of(sub, &inherited);
    // exec and run stop at their first argument: what follows is the command's.
    let interspersed = !matches!(sub.get_name(), "exec" | "run");
    let values = match parse_flags(
        &sub_flags,
        &args[at + 1..],
        interspersed,
        sub.get_name(),
        notes,
    ) {
        Ok(v) => v,
        Err(e) => return Outcome::Fail(e),
    };
    // The help flag and the persistent ones are one flag each in cobra, whichever
    // command they are given to: the last value given is the one.
    let help = values.get("help").or_else(|| root.get("help"));
    if help.is_some_and(|v| v[0] == "true") {
        return Outcome::Help(vec![sub.get_name().to_string()]);
    }
    for flag in &inherited {
        if values.get(&flag.name).is_some() {
            root.set.retain(|(n, _)| *n != flag.name);
        }
    }
    if sub.get_name() == "version" {
        if let Some(word) = values.args.first() {
            return Outcome::Fail(format!(
                "unknown command {} for \"docker compose version\"",
                gostr::quote(word)
            ));
        }
    }
    let mut out = Vec::new();
    canonical(&root_flags, &root, &mut out);
    out.push(sub.get_name().to_string());
    canonical(&sub_flags, &values, &mut out);
    if !values.args.is_empty() {
        out.push("--".to_string());
        out.extend(values.args);
    }
    Outcome::Run(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn compose() -> Command {
        crate::cli::Cli::command()
            .find_subcommand("compose")
            .cloned()
            .expect("nspawn compose")
    }

    fn parse(line: &str) -> Outcome {
        let args: Vec<String> = line.split_whitespace().map(String::from).collect();
        super::parse_noting(&compose(), &args, &mut Vec::new())
    }

    fn run(line: &str) -> Vec<String> {
        match parse(line) {
            Outcome::Run(args) => args,
            other => panic!("{line}: {other:?}"),
        }
    }

    fn fail(line: &str) -> String {
        match parse(line) {
            Outcome::Fail(e) => e,
            other => panic!("{line}: {other:?}"),
        }
    }

    #[test]
    fn legacy_flag_names_are_taken_and_some_say_so() {
        let noting = |line: &str| {
            let args: Vec<String> = line.split_whitespace().map(String::from).collect();
            let mut notes = Vec::new();
            let outcome = super::parse_noting(&compose(), &args, &mut notes);
            (outcome, notes)
        };
        let (down, notes) = noting("down --volume");
        assert!(
            matches!(&down, Outcome::Run(l) if l.iter().any(|a| a == "--volumes")),
            "{down:?}"
        );
        assert_eq!(notes, ["--volume is deprecated, please use --volumes"]);
        let (up, notes) = noting("up --y -d");
        assert!(
            matches!(&up, Outcome::Run(l) if l.iter().any(|a| a == "--yes")),
            "{up:?}"
        );
        assert_eq!(notes, ["--y is deprecated, please use --yes instead"]);
        let (run, notes) = noting("run --no-TTY --labels a=b --volumes /x:/y web");
        assert!(notes.is_empty());
        let Outcome::Run(line) = run else {
            panic!("{run:?}")
        };
        for want in ["--no-tty=true", "--label=a=b", "--volume=/x:/y"] {
            assert!(line.iter().any(|a| a == want), "{want}: {line:?}");
        }
        assert_eq!(fail("ps --volume"), "unknown flag: --volume");
        assert!(
            fail("build -m bogus").contains("invalid size: 'bogus'"),
            "{}",
            fail("build -m bogus")
        );
    }

    #[test]
    fn flags_read_as_pflag_reads_them() {
        assert_eq!(
            run("-f a.yml -f=b.yml config"),
            ["--file=a.yml", "--file=b.yml", "config"]
        );
        assert_eq!(run("-p a -p b config"), ["--project-name=b", "config"]);
        assert_eq!(run("config --services=false"), ["config"]);
        assert_eq!(run("config --quiet=true -q"), ["config", "--quiet"]);
        assert_eq!(
            run("config --format json --format yaml"),
            ["config", "--format=yaml"]
        );
        assert_eq!(
            run("config web --hash= -ofile db"),
            ["config", "--hash=", "--output=file", "--", "web", "db"]
        );
        assert_eq!(run("--parallel 0x10 config"), ["--parallel=16", "config"]);
        assert_eq!(run("config --dry-run"), ["config", "--dry-run"]);
        assert_eq!(run("--dry-run config"), ["--dry-run", "config"]);
        assert_eq!(run("version -f json"), ["version", "--format=json"]);
        assert_eq!(run("-v"), ["version"]);
        assert_eq!(run("config -- -x"), ["config", "--", "-x"]);
        // The docker client's flags go to it, which is nowhere here.
        assert_eq!(
            run("--debug -H unix:///x --context=c --verbose config"),
            ["config"]
        );
        assert_eq!(run("--dry-run config --dry-run=false"), ["config"]);
    }

    #[test]
    fn errors_are_pflag_s_and_cobra_s() {
        assert_eq!(
            fail("config -f compose.yaml"),
            "unknown shorthand flag: 'f' in -f"
        );
        assert_eq!(
            fail("config --compatibility"),
            "unknown flag: --compatibility"
        );
        assert_eq!(fail("config -qx"), "unknown shorthand flag: 'x' in -x");
        assert_eq!(fail("config --hash"), "flag needs an argument: --hash");
        assert_eq!(fail("config -o"), "flag needs an argument: 'o' in -o");
        assert_eq!(
            fail("config --quiet=maybe"),
            "invalid argument \"maybe\" for \"-q, --quiet\" flag: strconv.ParseBool: parsing \"maybe\": invalid syntax"
        );
        assert_eq!(
            fail("--parallel x config"),
            "invalid argument \"x\" for \"--parallel\" flag: strconv.ParseInt: parsing \"x\": invalid syntax"
        );
        assert_eq!(fail("config ---x"), "bad flag syntax: ---x");
        assert_eq!(fail("--host"), "flag needs an argument: '--host'");
        assert_eq!(
            fail("config -\u{e9}"),
            "unknown shorthand flag: '\u{c3}' in -\u{e9}"
        );
        assert_eq!(
            fail("config --help=maybe"),
            "invalid argument \"maybe\" for \"--help\" flag: strconv.ParseBool: parsing \"maybe\": invalid syntax"
        );
        assert_eq!(
            fail("version x"),
            "unknown command \"x\" for \"docker compose version\""
        );
    }

    #[test]
    fn words_that_are_no_command() {
        assert_eq!(parse("convert"), Outcome::Unknown("convert".into()));
        // --parallel takes "-1" and then "config" as its value, as cobra's Traverse does.
        assert_eq!(
            parse("--parallel -1 config"),
            Outcome::Unknown("config".into())
        );
        assert_eq!(parse(""), Outcome::Help(vec![]));
        assert_eq!(parse("config --help"), Outcome::Help(vec!["config".into()]));
        assert_eq!(parse("--help config"), Outcome::Help(vec!["config".into()]));
        assert_eq!(parse("help config"), Outcome::Unknown("help".into()));
        assert_eq!(
            parse("exec -T web sh -c ls"),
            Outcome::Run(
                ["exec", "--no-tty=true", "--", "web", "sh", "-c", "ls"]
                    .map(String::from)
                    .to_vec()
            )
        );
        let Outcome::Fail(text) = parse("--version=true x") else {
            panic!("--version with a word runs the version command");
        };
        assert_eq!(
            text.lines().take(6).collect::<Vec<_>>(),
            [
                "Error: unknown flag: --version",
                "Usage:",
                "  version [OPTIONS] [flags]",
                "",
                "Flags:",
                "  -f, --format string   Format the output. Values: [pretty | json]. (Default: pretty)",
            ]
        );
        assert!(text.ends_with("\n\nunknown flag: --version"));
        assert!(matches!(parse("-p a --version=1 x"),
            Outcome::Fail(t) if t.starts_with("Error: unknown shorthand flag: 'p' in -p\n")));
        assert_eq!(parse("--version=true"), Outcome::Help(vec![]));
    }
}
