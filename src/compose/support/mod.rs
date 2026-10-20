//! What nspawn compose does with every attribute of the Compose Specification and with
//! every command and option of docker compose, decided one by one against the release
//! whose schema and command reference are under src/compose/upstream. The loader takes
//! its warnings and errors from here; the tests check that the release has nothing the
//! table leaves undecided and the table nothing the release does not have, and
//! docs/COMPOSE.md shows the table as it is.

mod attributes;
mod commands;

pub use attributes::{ATTRIBUTES, VALUES};
pub use commands::{COMMANDS, OPTIONS};

/// The docker compose release the table is decided on.
pub const DOCKER_COMPOSE: &str = "v5.6.0";
/// The compose-go that release is built with, whose schema it validates files with.
pub const COMPOSE_GO: &str = "v2.16.1";

/// What nspawn compose does with an attribute, a value, a command or an option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Support {
    /// As docker compose does.
    Full,
    /// Honored, with the difference the text gives.
    Partial(&'static str),
    /// Read and left without effect, with the text as the warning.
    Ignored(&'static str),
    /// Refused, with the text as the error.
    Refused(&'static str),
}

#[cfg(test)]
impl Support {
    pub fn text(&self) -> Option<&'static str> {
        match self {
            Support::Full => None,
            Support::Partial(text) | Support::Ignored(text) | Support::Refused(text) => Some(text),
        }
    }
}

/// A decision on an attribute, by its path in the schema as compose-go writes it: `*`
/// stands for a name the file chooses (a service, a network, a label) and `[]` for an
/// item of a list, as in `services.*.ports.[].host_ip`.
#[derive(Debug, Clone, Copy)]
pub struct Attribute {
    pub path: &'static str,
    pub support: Support,
    /// The decision holds for every attribute under the path as well.
    pub below: bool,
}

impl Attribute {
    const fn and_below(self) -> Self {
        Attribute {
            below: true,
            ..self
        }
    }
}

const fn full(path: &'static str) -> Attribute {
    Attribute {
        path,
        support: Support::Full,
        below: false,
    }
}

const fn partly(path: &'static str, text: &'static str) -> Attribute {
    Attribute {
        path,
        support: Support::Partial(text),
        below: false,
    }
}

const fn ignored(path: &'static str, text: &'static str) -> Attribute {
    Attribute {
        path,
        support: Support::Ignored(text),
        below: false,
    }
}

const fn refused(path: &'static str, text: &'static str) -> Attribute {
    Attribute {
        path,
        support: Support::Refused(text),
        below: false,
    }
}

/// A decision on some values of an attribute, over the attribute's own.
#[derive(Debug, Clone, Copy)]
pub struct Value {
    pub path: &'static str,
    pub value: Match,
    pub support: Support,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Match {
    Is(&'static str),
    Prefix(&'static str),
}

impl Match {
    pub fn matches(&self, value: &str) -> bool {
        match self {
            Match::Is(is) => value == *is,
            Match::Prefix(prefix) => value.starts_with(prefix),
        }
    }
}

const fn when(path: &'static str, value: Match, support: Support) -> Value {
    Value {
        path,
        value,
        support,
    }
}

/// A decision on a command of docker compose, by its name after `docker compose`:
/// `up`, `alpha watch`.
#[derive(Debug, Clone, Copy)]
pub struct Command {
    pub name: &'static str,
    pub support: Support,
}

/// A decision on an option of a command, by its long name. The command "" is docker
/// compose itself, whose options come before any command.
#[derive(Debug, Clone, Copy)]
pub struct Opt {
    pub command: &'static str,
    pub name: &'static str,
    pub support: Support,
}

/// The decision on an attribute path: its own, or that of the nearest attribute above
/// it whose decision holds below. A job is a service that runs to completion, so an
/// attribute of a job without a decision of its own has that of the service attribute
/// of the same name.
pub fn attribute(path: &str) -> Option<&'static Attribute> {
    decided(path).or_else(|| {
        let rest = path.strip_prefix("jobs.*.")?;
        decided(&format!("services.*.{rest}"))
    })
}

fn decided(path: &str) -> Option<&'static Attribute> {
    if let Some(own) = ATTRIBUTES.iter().find(|a| a.path == path) {
        return Some(own);
    }
    let mut above = path;
    while let Some((parent, _)) = above.rsplit_once('.') {
        if let Some(a) = ATTRIBUTES.iter().find(|a| a.path == parent && a.below) {
            return Some(a);
        }
        above = parent;
    }
    None
}

/// The decision on one value of an attribute: a decision on that value, or else the
/// attribute's.
pub fn value(path: &str, value: &str) -> Option<Support> {
    let a = attribute(path)?;
    Some(
        VALUES
            .iter()
            .find(|v| v.path == a.path && v.value.matches(value))
            .map_or(a.support, |v| v.support),
    )
}

/// The table's decision on a command. A command under a refused one shares its
/// refusal.
pub fn command(name: &str) -> Option<Support> {
    if let Some(own) = COMMANDS.iter().find(|c| c.name == name) {
        return Some(own.support);
    }
    let (parent, _) = name.rsplit_once(' ')?;
    command(parent).filter(|support| matches!(support, Support::Refused(_)))
}

/// What nspawn compose does with an option of a command today; every option of a
/// refused command shares its refusal.
pub fn option(command: &str, name: &str) -> Option<Support> {
    if let Some(refusal @ Support::Refused(_)) = self::command(command) {
        return Some(refusal);
    }
    OPTIONS
        .iter()
        .find(|o| o.command == command && o.name == name)
        .map(|o| o.support)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::{Path, PathBuf};

    fn upstream() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/compose/upstream")
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    /// The attribute paths of the schema, each with the values its `enum` allows (none
    /// when it has no enum): the walk of compose-go's schema.AttributePaths.
    fn schema() -> BTreeMap<String, BTreeSet<String>> {
        let text = read(&upstream().join("compose-go/compose-spec.json"));
        let root: serde_json::Value = serde_json::from_str(&text).unwrap();
        let defs = root["$defs"].as_object().cloned().unwrap_or_default();
        let mut paths = BTreeMap::new();
        walk(
            root.as_object().unwrap(),
            &[],
            &defs,
            &mut BTreeSet::new(),
            &mut paths,
        );
        paths
    }

    fn walk(
        node: &serde_json::Map<String, serde_json::Value>,
        path: &[&str],
        defs: &serde_json::Map<String, serde_json::Value>,
        active: &mut BTreeSet<String>,
        paths: &mut BTreeMap<String, BTreeSet<String>>,
    ) {
        if let Some(reference) = node.get("$ref").and_then(|r| r.as_str()) {
            let name = reference.trim_start_matches("#/$defs/");
            if active.contains(name) {
                return;
            }
            if let Some(def) = defs.get(name).and_then(|d| d.as_object()) {
                active.insert(name.to_string());
                walk(def, path, defs, active, paths);
                active.remove(name);
            }
            return;
        }
        if let (Some(values), Some(own)) = (
            node.get("enum").and_then(|e| e.as_array()),
            paths.get_mut(&path.join(".")),
        ) {
            own.extend(values.iter().map(|v| match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            }));
        }
        for combinator in ["oneOf", "anyOf", "allOf"] {
            for alternative in node
                .get(combinator)
                .and_then(|c| c.as_array())
                .into_iter()
                .flatten()
            {
                if let Some(alternative) = alternative.as_object() {
                    walk(alternative, path, defs, active, paths);
                }
            }
        }
        let mut descend = |key: &str, sub: &serde_json::Value, paths: &mut BTreeMap<_, _>| {
            let next: Vec<&str> = path.iter().copied().chain([key]).collect();
            paths.entry(next.join(".")).or_insert_with(BTreeSet::new);
            if let Some(sub) = sub.as_object() {
                walk(sub, &next, defs, active, paths);
            }
        };
        for (name, sub) in node
            .get("properties")
            .and_then(|p| p.as_object())
            .into_iter()
            .flatten()
        {
            descend(name, sub, paths);
        }
        // Extensions (x-*) are not attributes of the specification.
        for (pattern, sub) in node
            .get("patternProperties")
            .and_then(|p| p.as_object())
            .into_iter()
            .flatten()
        {
            if !pattern.contains("x-") && sub.is_object() {
                descend("*", sub, paths);
            }
        }
        if let Some(sub) = node.get("additionalProperties").filter(|a| a.is_object()) {
            descend("*", sub, paths);
        }
        if let Some(items) = node.get("items").and_then(|i| i.as_object()) {
            let next: Vec<&str> = path.iter().copied().chain(["[]"]).collect();
            walk(items, &next, defs, active, paths);
        }
    }

    /// The shorthand of every option of every command of docker compose, from its
    /// command reference: (command, option) to the letter, for those that have one.
    fn shorthands() -> BTreeMap<(String, String), char> {
        let dir = upstream().join("compose/reference");
        let mut shorts = BTreeMap::new();
        for entry in std::fs::read_dir(&dir).unwrap() {
            let text = read(&entry.unwrap().path());
            let (mut command, mut option, mut section) = (String::new(), None, "");
            for line in text.lines() {
                if !line.starts_with(' ') {
                    let (key, value) = line.split_once(':').unwrap_or((line, ""));
                    section = key;
                    if key == "command" {
                        let name = value.trim().strip_prefix("docker compose").unwrap();
                        command = name.trim().to_string();
                    }
                } else if let Some(name) = line.strip_prefix("    - option: ") {
                    option = (section == "options").then(|| name.trim_matches('"').to_string());
                } else if let (Some(short), Some(name)) =
                    (line.strip_prefix("      shorthand: "), &option)
                {
                    let short = short.trim_matches('"').chars().next().unwrap();
                    shorts.insert((command.clone(), name.clone()), short);
                }
            }
        }
        shorts
    }

    /// What clap takes and the table says nspawn compose does are the same commands, and
    /// their options have docker compose's shorthands.
    #[test]
    fn the_command_line_has_what_the_table_says() {
        use clap::CommandFactory;
        let compose = crate::cli::Cli::command()
            .find_subcommand("compose")
            .cloned()
            .expect("nspawn compose");
        let have: BTreeSet<String> = compose
            .get_subcommands()
            .map(|c| c.get_name().to_string())
            .collect();
        for c in COMMANDS {
            let refused = matches!(command(c.name), Some(Support::Refused(_)));
            assert_eq!(
                have.contains(c.name),
                !refused,
                "{}: on the command line and refused, or taken and not on it",
                c.name
            );
        }
        let shorts = shorthands();
        let mut commands = vec![(String::new(), &compose)];
        commands.extend(
            compose
                .get_subcommands()
                .map(|c| (c.get_name().to_string(), c)),
        );
        for (name, cmd) in commands {
            for arg in cmd.get_arguments() {
                let Some(long) = arg.get_long() else { continue };
                if long == "help" || (arg.is_global_set() && !name.is_empty()) {
                    continue;
                }
                assert_eq!(
                    arg.get_short(),
                    shorts.get(&(name.clone(), long.to_string())).copied(),
                    "compose {name} --{long}: not docker compose's shorthand"
                );
            }
        }
    }

    /// The commands of docker compose with the options each declares, from its command
    /// reference: "" for docker compose itself.
    fn reference() -> BTreeMap<String, BTreeSet<String>> {
        let dir = upstream().join("compose/reference");
        let mut commands = BTreeMap::new();
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let text = read(&path);
            let (mut command, mut options, mut section) = (None, BTreeSet::new(), "");
            for line in text.lines() {
                if !line.starts_with(' ') {
                    let (key, value) = line.split_once(':').unwrap_or((line, ""));
                    section = key;
                    if key == "command" {
                        let name = value.trim().strip_prefix("docker compose").unwrap();
                        command = Some(name.trim().to_string());
                    }
                } else if let Some(name) = line.strip_prefix("    - option: ") {
                    if section == "options" {
                        options.insert(name.trim_matches('"').to_string());
                    }
                }
            }
            let command = command.unwrap_or_else(|| panic!("{}: no command", path.display()));
            commands.insert(command, options);
        }
        commands
    }

    #[test]
    fn upstream_is_the_release_the_table_names() {
        let source = read(&upstream().join("SOURCE"));
        let tags: Vec<(&str, &str)> = source
            .lines()
            .map(|line| {
                let mut fields = line.split(' ');
                (fields.next().unwrap(), fields.next().unwrap())
            })
            .collect();
        assert_eq!(
            tags,
            [
                ("docker/compose", DOCKER_COMPOSE),
                ("compose-spec/compose-go", COMPOSE_GO)
            ]
        );
    }

    #[test]
    fn the_walk_of_the_schema_is_compose_go_s() {
        let theirs = read(&upstream().join("compose-go/attribute-paths.txt"));
        let theirs: BTreeSet<&str> = theirs.lines().collect();
        let schema = schema();
        let ours: BTreeSet<&str> = schema.keys().map(String::as_str).collect();
        assert_eq!(ours, theirs);
    }

    #[test]
    fn every_attribute_of_the_release_is_decided() {
        let undecided: Vec<String> = schema()
            .into_keys()
            .filter(|path| attribute(path).is_none())
            .collect();
        assert!(
            undecided.is_empty(),
            "attributes of the Compose Specification of {COMPOSE_GO} with no decision:\n{}",
            undecided.join("\n")
        );
    }

    #[test]
    fn every_decision_is_on_an_attribute_of_the_release() {
        let schema = schema();
        let mut seen = BTreeSet::new();
        for a in ATTRIBUTES {
            assert!(schema.contains_key(a.path), "{}: not in the schema", a.path);
            assert!(seen.insert(a.path), "{}: decided twice", a.path);
            if let Some((parent, _)) = a.path.rsplit_once('.') {
                let above = decided(parent);
                assert!(
                    !above.is_some_and(|above| above.below),
                    "{}: already decided by {} and below",
                    a.path,
                    above.unwrap().path
                );
            }
        }
        for v in VALUES {
            let a = ATTRIBUTES.iter().find(|a| a.path == v.path);
            assert!(a.is_some(), "{}: a value of an undecided attribute", v.path);
            assert_ne!(v.support, a.unwrap().support, "{}: {:?}", v.path, v.value);
            let allowed = &schema[v.path];
            if let (Match::Is(value), false) = (v.value, allowed.is_empty()) {
                assert!(
                    allowed.contains(value),
                    "{}: {value} not in its enum",
                    v.path
                );
            }
        }
    }

    #[test]
    fn every_command_and_option_of_the_release_is_decided() {
        let mut undecided = Vec::new();
        for (command, options) in reference() {
            if self::command(&command).is_none() && !command.is_empty() {
                undecided.push(format!("docker compose {command}"));
                continue;
            }
            for name in options {
                if option(&command, &name).is_none() {
                    undecided.push(format!("docker compose {command} --{name}"));
                }
            }
        }
        assert!(
            undecided.is_empty(),
            "commands and options of docker compose {DOCKER_COMPOSE} with no decision:\n{}",
            undecided.join("\n")
        );
    }

    #[test]
    fn every_decision_is_on_a_command_or_option_of_the_release() {
        let reference = reference();
        let mut seen = BTreeSet::new();
        for c in COMMANDS {
            assert!(
                reference.contains_key(c.name),
                "{}: no such command",
                c.name
            );
            assert!(
                !c.name.is_empty(),
                "docker compose itself is not decided on"
            );
            assert!(seen.insert(c.name), "{}: decided twice", c.name);
            if let Some((parent, _)) = c.name.rsplit_once(' ') {
                assert!(
                    !matches!(command(parent), Some(Support::Refused(_))),
                    "{}: {parent} is refused already",
                    c.name
                );
            }
        }
        let mut seen = BTreeSet::new();
        for o in OPTIONS {
            let options = reference.get(o.command);
            assert!(
                options.is_some_and(|options| options.contains(o.name)),
                "{} --{}: no such option",
                o.command,
                o.name
            );
            assert!(
                !matches!(command(o.command), Some(Support::Refused(_))),
                "{} --{}: its command is refused already",
                o.command,
                o.name
            );
            assert!(
                seen.insert((o.command, o.name)),
                "{} --{}: decided twice",
                o.command,
                o.name
            );
        }
    }

    #[test]
    fn the_texts_are_plain_lines() {
        let texts = ATTRIBUTES
            .iter()
            .map(|a| (a.path, a.support))
            .chain(VALUES.iter().map(|v| (v.path, v.support)))
            .chain(COMMANDS.iter().map(|c| (c.name, c.support)))
            .chain(OPTIONS.iter().map(|o| (o.name, o.support)));
        for (what, support) in texts {
            let Some(text) = support.text() else { continue };
            assert!(!text.is_empty(), "{what}: an empty text");
            assert!(
                text.chars().all(|c| c.is_ascii() && !c.is_ascii_control()),
                "{what}: {text}"
            );
            assert!(!text.ends_with('.'), "{what}: {text}");
        }
    }

    #[test]
    fn lookups_follow_the_table() {
        // Below a decision that holds below.
        assert_eq!(
            attribute("services.*.build.ssh.default").unwrap().path,
            "services.*.build.ssh"
        );
        // A job's attribute is the service's of the same name.
        assert_eq!(
            attribute("jobs.*.ports.[].host_ip").unwrap().path,
            "services.*.ports.[].host_ip"
        );
        assert_eq!(
            attribute("services.*.restart").unwrap().path,
            "services.*.restart"
        );
        assert!(attribute("services.*.nothing").is_none());
        assert_eq!(
            value("services.*.pull_policy", "build"),
            Some(Support::Full)
        );
        assert!(matches!(
            value("services.*.pull_policy", "weekly"),
            Some(Support::Partial(_))
        ));
        assert_eq!(
            value("services.*.pull_policy", "missing"),
            Some(Support::Full)
        );
        assert!(matches!(
            value("services.*.restart", "on-failure:3"),
            Some(Support::Partial(_))
        ));
        assert!(matches!(
            option("watch", "prune"),
            Some(Support::Refused(_))
        ));
        assert!(matches!(
            command("bridge transformations list"),
            Some(Support::Refused(_))
        ));
        assert!(command("up down").is_none());
        assert_eq!(option("up", "detach"), Some(Support::Full));
        assert_eq!(option("config", "services"), Some(Support::Full));
        assert_eq!(option("", "env-file"), Some(Support::Full));
        assert_eq!(
            option("", "file"),
            Some(Support::Partial(crate::compose::paths::REMOTE))
        );
    }

    fn status(support: Support) -> &'static str {
        match support {
            Support::Full => "yes",
            Support::Partial(_) => "partly",
            Support::Ignored(_) => "ignored",
            Support::Refused(_) => "no",
        }
    }

    fn row(what: String, support: Support) -> String {
        let text = support.text().unwrap_or("").replace('|', "\\|");
        format!("| {what} | {} | {text} |\n", status(support))
    }

    /// The table as docs/COMPOSE.md shows it.
    fn markdown() -> String {
        let mut out = format!(
            "Decided on docker compose {DOCKER_COMPOSE}, whose Compose Specification is \
             that of compose-go {COMPOSE_GO}. An attribute of a job without a row of its \
             own is the service attribute of the same name.\n\n\
             ### Attributes\n\n| Attribute | nspawn | Notes |\n| --- | --- | --- |\n"
        );
        let mut attributes: Vec<&Attribute> = ATTRIBUTES.iter().collect();
        attributes.sort_by_key(|a| a.path);
        for a in attributes {
            let below = if a.below { " and below" } else { "" };
            out += &row(format!("`{}`{below}", a.path), a.support);
            for v in VALUES.iter().filter(|v| v.path == a.path) {
                let value = match v.value {
                    Match::Is(is) => format!("`{is}`"),
                    Match::Prefix(prefix) => format!("`{prefix}...`"),
                };
                out += &row(format!("`{}`: {value}", a.path), v.support);
            }
        }
        out += "\n### Commands\n\n| Command | nspawn | Notes |\n| --- | --- | --- |\n";
        let mut options: Vec<&Opt> = OPTIONS.iter().filter(|o| o.command.is_empty()).collect();
        options.sort_by_key(|o| o.name);
        for o in options {
            out += &row(format!("`docker compose --{}`", o.name), o.support);
        }
        let mut commands: Vec<&Command> = COMMANDS.iter().collect();
        commands.sort_by_key(|c| c.name);
        for c in commands {
            out += &row(format!("`{}`", c.name), c.support);
            let mut options: Vec<&Opt> = OPTIONS.iter().filter(|o| o.command == c.name).collect();
            options.sort_by_key(|o| o.name);
            for o in options {
                out += &row(format!("`{} --{}`", c.name, o.name), o.support);
            }
        }
        out
    }

    const BEGIN: &str = "<!-- The support table: src/compose/support makes it, and \
                         NSPAWN_UPDATE_DOCS=1 cargo test writes it here. -->\n";
    const END: &str = "<!-- End of the support table. -->\n";

    #[test]
    fn the_docs_show_the_table() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/COMPOSE.md");
        let text = read(&path);
        let (head, rest) = text.split_once(BEGIN).expect("the opening marker");
        let (shown, tail) = rest.split_once(END).expect("the closing marker");
        let table = markdown();
        if shown == table {
            return;
        }
        if std::env::var_os("NSPAWN_UPDATE_DOCS").is_some() {
            std::fs::write(&path, format!("{head}{BEGIN}{table}{END}{tail}")).unwrap();
            return;
        }
        panic!(
            "docs/COMPOSE.md does not show the support table as it is; \
             NSPAWN_UPDATE_DOCS=1 cargo test writes it"
        );
    }
}
