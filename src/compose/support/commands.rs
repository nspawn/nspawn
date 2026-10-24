//! The decisions on the commands of docker compose and their options.

use super::Support::{self, Full, Ignored, Partial, Refused};
use super::{Command, Opt};

const fn command(name: &'static str, support: Support) -> Command {
    Command { name, support }
}

const fn option(command: &'static str, name: &'static str, support: Support) -> Opt {
    Opt {
        command,
        name,
        support,
    }
}

const BUILDER: &str = "nspawn builds with its own builder";
const NO_ATTESTATIONS: &str = "nspawn's builder writes no attestations";
const VERSION: &str =
    "--short and json give the docker compose release it matches; the plain form names nspawn's version too";

pub static COMMANDS: &[Command] = &[
    command("alpha", Refused("alpha is not supported by nspawn compose")),
    command(
        "attach",
        Refused(
            "attach is not supported by nspawn compose; logs --follow shows a service's output",
        ),
    ),
    command(
        "bridge",
        Refused("bridge is not supported by nspawn compose"),
    ),
    command(
        "build",
        Partial("with nspawn's builder; see nspawn build for what it does not do"),
    ),
    command(
        "commit",
        Refused("commit is not supported by nspawn compose"),
    ),
    command("config", Full),
    command("cp", Full),
    command("create", Full),
    command("down", Full),
    command("events", Full),
    command("exec", Full),
    command(
        "export",
        Refused("export is not supported by nspawn compose"),
    ),
    command("images", Full),
    command("kill", Full),
    command("logs", Full),
    command("ls", Full),
    command("pause", Full),
    command("port", Full),
    command("ps", Full),
    command(
        "publish",
        Refused("publish is not supported by nspawn compose"),
    ),
    command("pull", Full),
    command(
        "push",
        Refused("push is not supported by nspawn compose; compose build --push or nspawn push sends an image"),
    ),
    command("restart", Full),
    command("rm", Full),
    command("run", Full),
    command("scale", Full),
    command("start", Full),
    command("stats", Full),
    command("stop", Full),
    command(
        "top",
        Partial("the columns are SERVICE, #, PID, USER, TIME and COMMAND: no PPID, C, STIME or TTY"),
    ),
    command("unpause", Full),
    command("up", Full),
    command("version", Partial(VERSION)),
    command("volumes", Full),
    command("wait", Full),
    command("watch", Refused("watch is not supported by nspawn compose")),
];

pub static OPTIONS: &[Opt] = &[
    option("build", "build-arg", Full),
    option("build", "builder", Ignored(BUILDER)),
    option("build", "check", Refused("nspawn's builder has no linter")),
    option("build", "compress", Ignored("the context goes to the service through a pipe")),
    option("build", "force-rm", Ignored("steps leave no intermediate containers")),
    option("build", "memory", Ignored("build steps run without a memory limit")),
    option("build", "no-cache", Full),
    option("build", "no-rm", Ignored("steps leave no intermediate containers")),
    option("build", "parallel", Ignored("services are built one after another")),
    option("build", "print", Refused("there is no bake definition to print")),
    option("build", "progress", Ignored("the steps print plain progress")),
    option("build", "provenance", Refused(NO_ATTESTATIONS)),
    option("build", "pull", Full),
    option("build", "push", Full),
    option("build", "quiet", Full),
    option("build", "sbom", Refused(NO_ATTESTATIONS)),
    option("build", "ssh", Refused("SSH agent forwarding into a build is not supported")),
    option("build", "with-dependencies", Full),
    // The global options, before any command.
    option("", "all-resources", Full),
    option("", "ansi", Full),
    option(
        "",
        "compatibility",
        Partial("config takes it as docker compose does; machine names join their parts with `-`, as machined takes no `_`"),
    ),
    option(
        "",
        "dry-run",
        Partial("commands that only read take it; the ones that change something refuse it, as nothing simulates their changes"),
    ),
    option("", "env-file", Full),
    option("", "file", Partial(crate::compose::paths::REMOTE)),
    option(
        "",
        "insecure-registry",
        Ignored("nspawn.toml says how each registry is reached"),
    ),
    option("", "no-ansi", Full),
    option("", "parallel", Full),
    option("", "profile", Full),
    option("", "progress", Full),
    option("", "project-directory", Full),
    option("", "project-name", Full),
    option("", "verbose", Ignored("nspawn compose has no debug output")),
    option("", "version", Partial(VERSION)),
    option("", "workdir", Full),
    // config.
    option("config", "environment", Full),
    option("config", "format", Full),
    option("config", "hash", Full),
    option("config", "images", Full),
    option("config", "lock-image-digests", Full),
    option("config", "models", Full),
    option("config", "networks", Full),
    option("config", "no-consistency", Full),
    option("config", "no-env-resolution", Full),
    option("config", "no-interpolate", Full),
    option("config", "no-normalize", Full),
    option("config", "no-path-resolution", Full),
    option("config", "output", Full),
    option("config", "profiles", Full),
    option("config", "quiet", Full),
    option("config", "resolve-image-digests", Full),
    option("config", "services", Full),
    option("config", "variables", Full),
    option("config", "volumes", Full),
    // cp
    option("cp", "all", Full),
    option(
        "cp",
        "archive",
        Ignored("what lands in a machine belongs to its root, and on the host to whoever runs cp"),
    ),
    option(
        "cp",
        "follow-link",
        Ignored("a symbolic link is copied as a link"),
    ),
    option("cp", "index", Full),
    // create
    option("create", "build", Full),
    option("create", "force-recreate", Full),
    option("create", "no-build", Full),
    option("create", "no-recreate", Full),
    option("create", "pull", Full),
    option("create", "quiet-pull", Full),
    option("create", "remove-orphans", Full),
    option("create", "scale", Full),
    option("create", "yes", Full),
    // down
    option("down", "remove-orphans", Full),
    option(
        "down",
        "rmi",
        Partial("the images are known from the files: without them none is removed"),
    ),
    option("down", "timeout", Full),
    option(
        "down",
        "volumes",
        Partial("named volumes are known from the files: without them only the anonymous ones go"),
    ),
    // events
    option("events", "json", Full),
    option("events", "since", Full),
    option("events", "until", Full),
    // exec
    option("exec", "detach", Full),
    option("exec", "env", Full),
    option("exec", "index", Full),
    option("exec", "interactive", Full),
    option("exec", "no-tty", Full),
    option(
        "exec",
        "privileged",
        Ignored("a command runs with the capabilities of its machine"),
    ),
    option("exec", "tty", Ignored("as in docker compose, -T decides")),
    option("exec", "user", Full),
    option("exec", "workdir", Full),
    // images
    option("images", "format", Full),
    option("images", "quiet", Full),
    // kill
    option("kill", "remove-orphans", Full),
    option("kill", "signal", Full),
    // logs
    option("logs", "follow", Full),
    option("logs", "index", Full),
    option("logs", "no-color", Full),
    option("logs", "no-log-prefix", Full),
    option("logs", "since", Full),
    option("logs", "tail", Full),
    option("logs", "timestamps", Full),
    option("logs", "until", Full),
    // ls
    option("ls", "all", Full),
    option("ls", "filter", Full),
    option("ls", "format", Full),
    option("ls", "quiet", Full),
    // port
    option("port", "index", Full),
    option("port", "protocol", Full),
    // ps
    option("ps", "all", Full),
    option("ps", "filter", Full),
    option("ps", "format", Full),
    option("ps", "no-trunc", Full),
    option("ps", "orphans", Full),
    option("ps", "quiet", Full),
    option("ps", "services", Full),
    option("ps", "status", Full),
    // pull
    option("pull", "ignore-buildable", Full),
    option("pull", "ignore-pull-failures", Full),
    option("pull", "include-deps", Full),
    option("pull", "no-parallel", Ignored("deprecated; images are pulled one after another")),
    option("pull", "parallel", Ignored("deprecated; images are pulled one after another")),
    option("pull", "policy", Full),
    option("pull", "quiet", Full),
    // restart
    option("restart", "no-deps", Full),
    option("restart", "timeout", Full),
    // rm
    option("rm", "all", Ignored("deprecated, as in docker compose: one-off machines stay")),
    option("rm", "force", Full),
    option("rm", "stop", Full),
    option("rm", "volumes", Full),
    // run
    option("run", "build", Full),
    option("run", "cap-add", Full),
    option("run", "cap-drop", Full),
    option("run", "detach", Full),
    option("run", "entrypoint", Full),
    option("run", "env", Full),
    option("run", "env-from-file", Full),
    option("run", "interactive", Full),
    option("run", "label", Full),
    option("run", "name", Full),
    option("run", "no-deps", Full),
    option("run", "no-tty", Full),
    option("run", "publish", Full),
    option("run", "pull", Full),
    option("run", "quiet", Ignored("pulls print through the service's job")),
    option("run", "quiet-build", Full),
    option("run", "quiet-pull", Full),
    option("run", "remove-orphans", Ignored("orphans are removed by up and down")),
    option("run", "rm", Full),
    option("run", "service-ports", Full),
    option("run", "tty", Full),
    option("run", "use-aliases", Ignored("a one-off machine gets no network aliases")),
    option("run", "user", Full),
    option("run", "volume", Full),
    option("run", "workdir", Full),
    // scale
    option("scale", "no-deps", Full),
    // start
    option("start", "wait", Full),
    option("start", "wait-timeout", Full),
    // stats
    option("stats", "all", Full),
    option("stats", "format", Partial("table and json; a template is refused")),
    option("stats", "no-stream", Full),
    option("stats", "no-trunc", Full),
    // stop
    option("stop", "timeout", Full),
    // up
    option("up", "abort-on-container-exit", Full),
    option("up", "abort-on-container-failure", Full),
    option("up", "always-recreate-deps", Full),
    option("up", "attach", Full),
    option("up", "attach-dependencies", Full),
    option("up", "build", Full),
    option("up", "detach", Full),
    option("up", "exit-code-from", Full),
    option("up", "force-recreate", Full),
    option(
        "up",
        "menu",
        Ignored("nspawn compose has no interactive menu"),
    ),
    option("up", "no-attach", Full),
    option("up", "no-build", Full),
    option("up", "no-color", Full),
    option("up", "no-deps", Full),
    option("up", "no-log-prefix", Full),
    option("up", "no-recreate", Full),
    option("up", "no-start", Full),
    option("up", "pull", Full),
    option("up", "quiet-build", Full),
    option("up", "quiet-pull", Full),
    option("up", "remove-orphans", Full),
    option("up", "renew-anon-volumes", Full),
    option("up", "scale", Full),
    option("up", "timeout", Full),
    option("up", "timestamps", Full),
    option("up", "wait", Full),
    option("up", "wait-timeout", Full),
    option(
        "up",
        "watch",
        Refused("watch is not supported by nspawn compose"),
    ),
    option("up", "yes", Full),
    // version
    option("version", "format", Full),
    option("version", "short", Full),
    // volumes
    option("volumes", "format", Full),
    option("volumes", "quiet", Full),
    // wait
    option("wait", "down-project", Full),
];
