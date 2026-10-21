//! Identifiers, versions and times as the Docker Engine API writes them.

use sha2::{Digest, Sha256};

/// The API version served, and the oldest a client may ask for: those of the docker
/// release the answers were recorded from.
pub const API_VERSION: (u32, u32) = (1, 51);
pub const MIN_API_VERSION: (u32, u32) = (1, 24);

pub fn version_text((major, minor): (u32, u32)) -> String {
    format!("{major}.{minor}")
}

/// A machine's container ID: 64 hex digits that stay the same across its restarts and
/// updates (its creation time is kept) and change for a new machine of the same name.
pub fn container_id(name: &str, created: u64) -> String {
    id("nspawn-container", name, created)
}

/// A network's ID, likewise; the default network and the host's and none have creation
/// time 0.
pub fn network_id(name: &str, created: u64) -> String {
    id("nspawn-network", name, created)
}

/// An endpoint of a machine on a network.
pub fn endpoint_id(machine: &str, network: &str) -> String {
    id("nspawn-endpoint", &format!("{machine}\0{network}"), 0)
}

fn id(kind: &str, name: &str, created: u64) -> String {
    hex::encode(Sha256::digest(format!("{kind}\0{name}\0{created}")))
}

pub fn short(id: &str) -> &str {
    &id[..id.len().min(12)]
}

/// The version a request path names, `/v1.51/...`, and the rest of the path; a path
/// without one is the served version's.
pub fn split_version(path: &str) -> (Option<(u32, u32)>, &str) {
    let Some(rest) = path.strip_prefix("/v") else {
        return (None, path);
    };
    let (version, tail) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let parsed = version
        .split_once('.')
        .and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)));
    match parsed {
        Some(v) => (Some(v), tail),
        None => (None, path),
    }
}

/// HumanDuration of go-units, which the API's statuses are made of.
pub fn human_duration(seconds: u64) -> String {
    let minutes = seconds / 60;
    // Hours are rounded, as the original adds half an hour before truncating.
    let hours = (seconds + 1800) / 3600;
    if seconds < 1 {
        "Less than a second".to_string()
    } else if seconds == 1 {
        "1 second".to_string()
    } else if seconds < 60 {
        format!("{seconds} seconds")
    } else if minutes == 1 {
        "About a minute".to_string()
    } else if minutes < 60 {
        format!("{minutes} minutes")
    } else if hours == 1 {
        "About an hour".to_string()
    } else if hours < 48 {
        format!("{hours} hours")
    } else if hours < 24 * 7 * 2 {
        format!("{} days", hours / 24)
    } else if hours < 24 * 30 * 2 {
        format!("{} weeks", hours / 24 / 7)
    } else if hours < 24 * 365 * 2 {
        format!("{} months", hours / 24 / 30)
    } else {
        format!("{} years", seconds / 3600 / 24 / 365)
    }
}

/// RFC 3339 in UTC with nanoseconds: all nine digits when `fixed` (log lines), without
/// trailing zeros otherwise (Go's RFC3339Nano).
pub fn rfc3339_nano(usec: u64, fixed: bool) -> String {
    let text = crate::api::events::rfc3339(usec);
    let (base, micros) = text
        .trim_end_matches('Z')
        .rsplit_once('.')
        .unwrap_or((&text, ""));
    let nanos = format!("{micros}000");
    if fixed {
        return format!("{base}.{nanos}Z");
    }
    let trimmed = nanos.trim_end_matches('0');
    if trimmed.is_empty() {
        format!("{base}Z")
    } else {
        format!("{base}.{trimmed}Z")
    }
}

/// What the API writes for a time never reached (a container that never finished).
pub const ZERO_TIME: &str = "0001-01-01T00:00:00Z";

/// A container's Command as the API lists it: the program, then its arguments, each one
/// that has a space in single quotes.
pub fn command_line(argv: &[String]) -> String {
    match argv.split_first() {
        None => String::new(),
        Some((program, [])) => program.clone(),
        Some((program, arguments)) => {
            let arguments: Vec<String> = arguments
                .iter()
                .map(|a| {
                    if a.contains(' ') {
                        format!("'{a}'")
                    } else {
                        a.clone()
                    }
                })
                .collect();
            format!("{program} {}", arguments.join(" "))
        }
    }
}

/// A reference as the API shows an image: without Docker Hub's host and library/.
pub fn familiar(reference: &str) -> String {
    let short = reference
        .strip_prefix("docker.io/")
        .or_else(|| reference.strip_prefix("index.docker.io/"))
        .unwrap_or(reference);
    short.strip_prefix("library/").unwrap_or(short).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_read_as_docker_lists_them() {
        let argv = |words: &[&str]| words.iter().map(|w| w.to_string()).collect::<Vec<_>>();
        assert_eq!(
            command_line(&argv(&["sh", "-c", "echo bye; echo oops >&2; exit 3"])),
            "sh -c 'echo bye; echo oops >&2; exit 3'"
        );
        // Only a space quotes, and never the program.
        assert_eq!(
            command_line(&argv(&["/my app", "a\tb", "c\nd"])),
            "/my app a\tb c\nd"
        );
        assert_eq!(command_line(&argv(&["httpd"])), "httpd");
        assert_eq!(command_line(&argv(&["true", ""])), "true ");
        assert_eq!(command_line(&[]), "");
    }

    #[test]
    fn ids_are_stable_and_distinct() {
        let a = container_id("web", 100);
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(a, container_id("web", 100));
        assert_ne!(a, container_id("web", 101));
        assert_ne!(a, network_id("web", 100));
        assert_eq!(short(&a), &a[..12]);
    }

    #[test]
    fn versions_in_paths() {
        assert_eq!(
            split_version("/v1.51/containers/json"),
            (Some((1, 51)), "/containers/json")
        );
        assert_eq!(split_version("/v1.24/_ping"), (Some((1, 24)), "/_ping"));
        assert_eq!(
            split_version("/containers/json"),
            (None, "/containers/json")
        );
        assert_eq!(split_version("/version"), (None, "/version"));
        assert_eq!(split_version("/vx.y/version"), (None, "/vx.y/version"));
    }

    #[test]
    fn durations_as_go_units_writes_them() {
        // The cases of go-units' own test.
        for (seconds, text) in [
            (0, "Less than a second"),
            (1, "1 second"),
            (2, "2 seconds"),
            (60, "About a minute"),
            (3 * 60, "3 minutes"),
            (35 * 60, "35 minutes"),
            (59 * 60 + 59, "59 minutes"),
            (60 * 60, "About an hour"),
            (60 * 60 + 29 * 60, "About an hour"),
            (60 * 60 + 30 * 60, "2 hours"),
            (3 * 3600, "3 hours"),
            (24 * 3600, "24 hours"),
            (48 * 3600, "2 days"),
            (7 * 24 * 3600, "7 days"),
            (13 * 24 * 3600 + 23 * 3600, "13 days"),
            (14 * 24 * 3600, "2 weeks"),
            (3 * 7 * 24 * 3600, "3 weeks"),
            (30 * 24 * 3600, "4 weeks"),
            (59 * 24 * 3600 + 23 * 3600, "8 weeks"),
            (60 * 24 * 3600, "2 months"),
            (3 * 30 * 24 * 3600, "3 months"),
            (365 * 24 * 3600 + 5 * 3600, "12 months"),
            (2 * 365 * 24 * 3600, "2 years"),
            (3 * 365 * 24 * 3600 + 2 * 24 * 3600, "3 years"),
        ] {
            assert_eq!(human_duration(seconds), text, "{seconds}");
        }
    }

    #[test]
    fn times_with_nanoseconds() {
        let usec = 1_792_344_103_549_704;
        assert_eq!(rfc3339_nano(usec, true), "2026-10-18T17:21:43.549704000Z");
        assert_eq!(rfc3339_nano(usec, false), "2026-10-18T17:21:43.549704Z");
        assert_eq!(
            rfc3339_nano(1_792_344_103_000_000, false),
            "2026-10-18T17:21:43Z"
        );
    }

    #[test]
    fn references_as_docker_shows_them() {
        assert_eq!(familiar("docker.io/library/busybox:1.37"), "busybox:1.37");
        assert_eq!(
            familiar("docker.io/lucaslorentz/caddy-docker-proxy:2.13"),
            "lucaslorentz/caddy-docker-proxy:2.13"
        );
        assert_eq!(familiar("ghcr.io/a/b:1"), "ghcr.io/a/b:1");
    }
}
