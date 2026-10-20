//! The progress of a command on stderr: a line per event (` Container demo-web-1
//! Started `), on a terminal too, JSON objects under --progress json, nothing under
//! quiet.

use std::io::Write;

use crate::compose::cli::ComposeArgs;
use crate::compose::config::Host;

/// What an event says of its task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Working,
    Done,
    Warning,
    Error,
}

impl Status {
    fn text(self) -> &'static str {
        match self {
            Status::Working => "Working",
            Status::Done => "Done",
            Status::Warning => "Warning",
            Status::Error => "Error",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Lines,
    Json,
    Quiet,
}

pub struct Progress {
    mode: Mode,
    dry_run: bool,
}

/// display.DRYRUN_PREFIX.
const DRY_RUN: &str = " DRY-RUN MODE - ";

impl Progress {
    /// --progress, or COMPOSE_PROGRESS, as the checks of the pre-run left them.
    pub fn new(args: &ComposeArgs, host: &Host) -> Progress {
        let mode = match args
            .progress
            .as_deref()
            .or(host.progress.as_deref())
            .unwrap_or("")
        {
            "json" => Mode::Json,
            "quiet" | "none" => Mode::Quiet,
            _ => Mode::Lines,
        };
        Progress {
            mode,
            dry_run: args.dry_run,
        }
    }

    /// One event of a resource: `id` names it with its kind ("Container demo-web-1").
    pub fn event(&self, id: &str, text: &str, details: &str, status: Status) {
        let line = match self.mode {
            Mode::Quiet => return,
            Mode::Lines => line(self.dry_run, id, text, details),
            Mode::Json => json(self.dry_run, id, text, details, status),
        };
        let _ = std::io::stderr().write_all(line.as_bytes());
    }

    pub fn container(&self, name: &str, text: &str, status: Status) {
        self.event(&format!("Container {name}"), text, "", status);
    }

    pub fn network(&self, name: &str, text: &str, status: Status) {
        self.event(&format!("Network {name}"), text, "", status);
    }

    pub fn volume(&self, name: &str, text: &str, status: Status) {
        self.event(&format!("Volume {name}"), text, "", status);
    }
}

/// The plain writer's line: fmt.Fprintln of the prefix, the id, the text and the
/// details, spaces between.
fn line(dry_run: bool, id: &str, text: &str, details: &str) -> String {
    let prefix = if dry_run { DRY_RUN } else { "" };
    format!("{prefix} {id} {text} {details}\n")
}

/// jsonMessage marshalled: its fields in their order, the empty ones left out.
fn json(dry_run: bool, id: &str, text: &str, details: &str, status: Status) -> String {
    let mut fields = Vec::new();
    if dry_run {
        fields.push("\"dry-run\":true".to_string());
    }
    for (key, value) in [
        ("id", id),
        ("status", status.text()),
        ("text", text),
        ("details", details),
    ] {
        if !value.is_empty() {
            let mut quoted = String::new();
            crate::compose::emit::json_string(value, true, &mut quoted);
            fields.push(format!("\"{key}\":{quoted}"));
        }
    }
    format!("{{{}}}\n", fields.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_are_written_as_docker_compose_writes_them() {
        assert_eq!(
            line(false, "Container demo-web-1", "Started", ""),
            " Container demo-web-1 Started \n"
        );
        assert_eq!(
            line(true, "Network demo_default", "Created", ""),
            " DRY-RUN MODE -  Network demo_default Created \n"
        );
        assert_eq!(
            json(false, "Container demo-db-1", "Healthy", "", Status::Done),
            "{\"id\":\"Container demo-db-1\",\"status\":\"Done\",\"text\":\"Healthy\"}\n"
        );
    }
}
