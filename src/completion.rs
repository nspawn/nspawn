//! What a shell completes after `nspawn`. The commands and flags come from the command
//! line's own definition; the names of machines, images, networks and volumes from the
//! service's org.nspawn.Names, asked at each TAB. The script a shell sources only calls
//! `nspawn` back (`NSPAWN_COMPLETE=bash nspawn -- WORDS`), so it never falls behind the
//! binary.
//!
//! A completion prints nothing but candidates: when the service cannot be reached, a
//! name argument simply has none.

use std::collections::HashMap;
use std::time::Duration;

use clap::CommandFactory;
use clap_complete::engine::{ArgValueCandidates, CompletionCandidate};

use crate::client::NamesProxy;

/// The variable that asks the binary for completions, as the scripts set it: clap's
/// COMPLETE is shared by every program built with it, and one set for another would
/// turn nspawn into a completer.
pub const VARIABLE: &str = "NSPAWN_COMPLETE";

/// Long enough for the bus to start the service, short enough not to hang a TAB.
const TIMEOUT: Duration = Duration::from_secs(3);

/// The network kinds `--network` takes besides the networks' names.
const NETWORK_KINDS: [&str; 3] = ["host", "none", "veth"];

/// What an argument takes a name of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A machine that runs: stop, exec, kill.
    Running,
    /// An image that does not run: start.
    Startable,
    /// Any machine or image: rm, inspect, logs.
    Any,
    /// A local image: create, push.
    Image,
    /// The reference of a local image: run.
    Reference,
    /// A network.
    Network,
    /// What `--network` takes: a network or a kind of network.
    NetworkChoice,
    /// A named volume.
    Volume,
}

/// One question to the service.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Query {
    Running,
    /// The running machines and those nspawn keeps a record of.
    Recorded,
    Images,
    References,
    Networks,
    Volumes,
}

type Answers = HashMap<Query, Vec<String>>;

impl Kind {
    fn queries(self) -> &'static [Query] {
        match self {
            Kind::Running => &[Query::Running],
            Kind::Startable => &[Query::Images, Query::Running],
            Kind::Any => &[Query::Images, Query::Recorded],
            Kind::Image => &[Query::Images],
            Kind::Reference => &[Query::References],
            Kind::Network | Kind::NetworkChoice => &[Query::Networks],
            Kind::Volume => &[Query::Volumes],
        }
    }

    /// The names out of the service's answers, sorted, once each.
    fn names(self, answers: &Answers) -> Vec<String> {
        let answer = |query| answers.get(&query).cloned().unwrap_or_default();
        let mut names = match self {
            Kind::Startable => {
                let running = answer(Query::Running);
                answer(Query::Images)
                    .into_iter()
                    .filter(|n| !running.contains(n))
                    .collect()
            }
            Kind::Any => [answer(Query::Images), answer(Query::Recorded)].concat(),
            Kind::NetworkChoice => [
                answer(Query::Networks),
                NETWORK_KINDS.map(String::from).to_vec(),
            ]
            .concat(),
            _ => answer(self.queries()[0]),
        };
        names.sort();
        names.dedup();
        names
    }
}

/// The candidates of an argument that takes a name of `kind`, asked when the shell
/// completes it.
pub fn candidates(kind: Kind) -> ArgValueCandidates {
    ArgValueCandidates::new(move || {
        kind.names(&ask(kind.queries()))
            .into_iter()
            .map(CompletionCandidate::new)
            .collect()
    })
}

/// The command line as completion sees it: every flag hidden. clap's engine leaves the
/// hidden candidates out when a visible one fits the word, so TAB on an empty word
/// offers names and subcommands alone, and the flags come once the word starts with
/// `-`, or when nothing else fits.
pub fn command() -> clap::Command {
    let mut command = crate::cli::Cli::command();
    // Built first, so that the help and version flags clap adds are hidden too.
    command.build();
    hide_flags(command)
}

fn hide_flags(command: clap::Command) -> clap::Command {
    command
        .mut_args(|arg| {
            if arg.is_positional() {
                arg
            } else {
                arg.hide(true)
            }
        })
        .mut_subcommands(hide_flags)
}

/// The script that hooks `nspawn` into `shell`: a function that calls the binary found
/// on PATH, never the one that wrote it, since the packages write it at build time.
pub fn registration(shell: clap_complete::Shell) -> std::io::Result<Vec<u8>> {
    let name = shell.to_string();
    let shells = clap_complete::env::Shells::builtins();
    let completer = shells
        .completer(&name)
        .ok_or_else(|| std::io::Error::other(format!("no completions for {name}")))?;
    let mut script = Vec::new();
    completer.write_registration(VARIABLE, "nspawn", "nspawn", "nspawn", &mut script)?;
    Ok(script)
}

#[cfg(test)]
thread_local! {
    /// What the service would answer, for the tests.
    static ANSWERS: std::cell::RefCell<Option<Answers>> = const { std::cell::RefCell::new(None) };
}

fn ask(queries: &[Query]) -> Answers {
    #[cfg(test)]
    if let Some(answers) = ANSWERS.with(|a| a.borrow().clone()) {
        return answers;
    }
    from_service(queries).unwrap_or_default()
}

/// The service's answers, over a connection of their own: a completion runs before
/// anything else of the program, outside its runtime.
fn from_service(queries: &[Query]) -> anyhow::Result<Answers> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        tokio::time::timeout(TIMEOUT, async {
            let connection = zbus::connection::Builder::system()?
                .method_timeout(TIMEOUT)
                .build()
                .await?;
            let names = NamesProxy::new(&connection).await?;
            let mut answers = Answers::new();
            for &query in queries {
                let list = match query {
                    Query::Running => names.machines(false).await?,
                    Query::Recorded => names.machines(true).await?,
                    Query::Images => names.images().await?,
                    Query::References => names.references().await?,
                    Query::Networks => names.networks().await?,
                    Query::Volumes => names.volumes().await?,
                };
                answers.insert(query, list);
            }
            anyhow::Ok(answers)
        })
        .await?
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answering(f: impl FnOnce()) {
        let answers = Answers::from([
            (Query::Running, vec!["db".into(), "web".into()]),
            (
                Query::Recorded,
                vec!["db".into(), "old".into(), "web".into()],
            ),
            (
                Query::Images,
                vec!["db".into(), "fedora-44".into(), "old".into(), "web".into()],
            ),
            (
                Query::References,
                vec!["docker.io/library/nginx:1.27".into()],
            ),
            (Query::Networks, vec!["backend".into(), "bridge".into()]),
            (Query::Volumes, vec!["pgdata".into()]),
        ]);
        ANSWERS.with(|a| *a.borrow_mut() = Some(answers));
        f();
        ANSWERS.with(|a| *a.borrow_mut() = None);
    }

    /// What TAB offers at the end of `line`; a trailing space starts a new word.
    fn complete(line: &str) -> Vec<String> {
        let words: Vec<std::ffi::OsString> = line.split(' ').map(Into::into).collect();
        let index = words.len() - 1;
        clap_complete::engine::complete(&mut command(), words, index, None)
            .unwrap()
            .iter()
            .map(|c| c.get_value().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn each_command_offers_the_names_it_takes() {
        answering(|| {
            for (line, expected) in [
                ("nspawn stop ", &["db", "web"][..]),
                ("nspawn stop db ", &["db", "web"]),
                ("nspawn exec ", &["db", "web"]),
                ("nspawn kill ", &["db", "web"]),
                ("nspawn pause ", &["db", "web"]),
                ("nspawn shell ", &["db", "web"]),
                ("nspawn top ", &["db", "web"]),
                ("nspawn stats ", &["db", "web"]),
                ("nspawn start ", &["fedora-44", "old"]),
                ("nspawn rm ", &["db", "fedora-44", "old", "web"]),
                ("nspawn inspect ", &["db", "fedora-44", "old", "web"]),
                ("nspawn logs ", &["db", "fedora-44", "old", "web"]),
                ("nspawn restart ", &["db", "fedora-44", "old", "web"]),
                ("nspawn update ", &["db", "fedora-44", "old", "web"]),
                ("nspawn images rm ", &["db", "fedora-44", "old", "web"]),
                ("nspawn create ", &["db", "fedora-44", "old", "web"]),
                ("nspawn push ", &["db", "fedora-44", "old", "web"]),
                ("nspawn run ", &["docker.io/library/nginx:1.27"]),
                ("nspawn network rm ", &["backend", "bridge"]),
                ("nspawn network inspect ", &["backend", "bridge"]),
                ("nspawn volume rm ", &["pgdata"]),
                (
                    "nspawn start old --network ",
                    &["backend", "bridge", "host", "none", "veth"],
                ),
                (
                    "nspawn run docker.io/library/nginx:1.27 --network ",
                    &["backend", "bridge", "host", "none", "veth"],
                ),
            ] {
                assert_eq!(complete(line), expected, "{line:?}");
            }
        });
    }

    #[test]
    fn what_was_typed_narrows_the_names() {
        answering(|| {
            assert_eq!(complete("nspawn stop w"), ["web"]);
            assert_eq!(complete("nspawn start f"), ["fedora-44"]);
            assert_eq!(complete("nspawn network rm b"), ["backend", "bridge"]);
        });
    }

    #[test]
    fn flags_come_with_a_dash() {
        answering(|| {
            // One dash brings the short form of a flag that has one, two the long ones.
            for (line, expected) in [
                ("nspawn stop -", ["-f", "-t", "--no-wait", "-h"]),
                (
                    "nspawn stop --",
                    ["--force", "--timeout", "--no-wait", "--help"],
                ),
            ] {
                let flags = complete(line);
                for flag in expected {
                    assert!(
                        flags.iter().any(|f| f == flag),
                        "{line:?}: {flag} missing: {flags:?}"
                    );
                }
                assert!(
                    flags.iter().all(|f| f.starts_with('-')),
                    "{line:?}: {flags:?}"
                );
            }
            assert_eq!(complete("nspawn stop --f"), ["--force"]);
            assert_eq!(
                complete("nspawn run nginx --net"),
                ["--network", "--network-alias"]
            );
        });
    }

    #[test]
    fn subcommands_come_without_the_global_flags() {
        let commands = complete("nspawn ");
        assert!(commands.iter().any(|c| c == "stop"), "{commands:?}");
        assert!(!commands.iter().any(|c| c.starts_with('-')), "{commands:?}");
        assert!(complete("nspawn --reg").contains(&"--registry".to_string()));
    }

    #[test]
    fn without_names_the_flags_come() {
        answering(|| {
            for line in [
                "nspawn secret rm ",
                "nspawn network create ",
                "nspawn create fedora-44 ",
            ] {
                let offered = complete(line);
                assert!(
                    !offered.is_empty() && offered.iter().all(|o| o.starts_with('-')),
                    "{line:?}: {offered:?}"
                );
            }
        });
    }

    #[test]
    fn nothing_from_the_service_is_no_names() {
        assert_eq!(Kind::Running.names(&Answers::new()), Vec::<String>::new());
        assert_eq!(
            Kind::NetworkChoice.names(&Answers::new()),
            ["host", "none", "veth"]
        );
    }

    #[test]
    fn the_scripts_call_nspawn_back_from_path() {
        for (shell, call) in [
            (
                clap_complete::Shell::Bash,
                "NSPAWN_COMPLETE=\"bash\" \\\n        \"nspawn\" -- ",
            ),
            (
                clap_complete::Shell::Zsh,
                "NSPAWN_COMPLETE=\"zsh\" \\\n        nspawn -- ",
            ),
            (
                clap_complete::Shell::Fish,
                "(NSPAWN_COMPLETE=fish nspawn -- ",
            ),
        ] {
            let script = String::from_utf8(registration(shell).unwrap()).unwrap();
            assert!(script.contains(call), "{shell}: {script}");
        }
    }
}
