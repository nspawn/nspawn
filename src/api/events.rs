//! docker events: what happens to machines, read from the journal. systemd logs every
//! start, end, restart and stop of a machine's unit with a message ID of its own, so
//! machines started by machinectl, at boot or by a restart policy are seen too; what
//! nspawn itself does (pull, create, remove...) it logs the same way under its own
//! message ID. Nothing is kept in the service: the journal is the history.

use std::collections::{BTreeMap, VecDeque};

use anyhow::{bail, Result};
use serde_json::Value;

use crate::journal;

/// The message ID of nspawn's own entries.
pub const MESSAGE_ID: &str = "b0b60147942247cab22cc49510006a0b";

/// systemd's message IDs for a unit's life, and the action each one is.
const UNIT_STARTED: &str = "39f53479d3a045ac8e11786248231fbf";
const UNIT_PROCESS_EXIT: &str = "98e322203f7a4ed290d09fe03c09fe15";
const UNIT_SUCCESS: &str = "7ad2d189f7e94e70a38c781354912448";
const UNIT_STOPPED: &str = "9d1aaa27d60140bd96365438aad20286";
const UNIT_RESTART_SCHEDULED: &str = "5eb03494b6584870a536b337290809b3";
const UNIT_OUT_OF_MEMORY: &str = "fe6faa94e7774663a0da52717891d8ef";
const UNIT_FAILURE_RESULT: &str = "d9b373ed55a64feb8242e02dbe79a49c";
const SYSTEMD_IDS: [&str; 7] = [
    UNIT_STARTED,
    UNIT_PROCESS_EXIT,
    UNIT_SUCCESS,
    UNIT_STOPPED,
    UNIT_RESTART_SCHEDULED,
    UNIT_OUT_OF_MEMORY,
    UNIT_FAILURE_RESULT,
];

/// Logs one of nspawn's own events. Best effort: an operation that worked is not undone
/// because the journal could not take the entry.
pub fn emit(kind: &str, action: &str, name: &str, attributes: &[(&str, &str)]) {
    let message = format!("{kind} {action} {name}");
    let mut fields: Vec<(String, String)> = vec![
        ("MESSAGE".into(), message),
        ("MESSAGE_ID".into(), MESSAGE_ID.into()),
        ("PRIORITY".into(), "6".into()),
        ("SYSLOG_IDENTIFIER".into(), "nspawn".into()),
        ("NSPAWN_TYPE".into(), kind.into()),
        ("NSPAWN_ACTION".into(), action.into()),
        ("NSPAWN_NAME".into(), name.into()),
    ];
    for (key, value) in attributes {
        fields.push((attribute_field(key), value.to_string()));
    }
    let fields: Vec<(&str, &str)> = fields
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let _ = journal::send(&fields);
}

/// The journal field of an attribute: letters, digits and underscores in capitals.
fn attribute_field(key: &str) -> String {
    let key: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("NSPAWN_ATTR_{key}")
}

/// journalctl's arguments: JSON entries of PID 1 about units, and nspawn's own, both
/// from root alone (journald sets _UID and _PID itself, so nobody else can pass off an
/// entry as one of these). Pure field matches: a unit glob would be expanded once, when
/// journalctl starts, and fail when no machine ran yet. Without `until` it follows,
/// from `since` or from now on.
pub fn journalctl_arguments(since: Option<&str>, until: Option<&str>) -> Vec<String> {
    let mut argv = vec![
        "--no-pager".to_string(),
        "--quiet".to_string(),
        "--output=json".to_string(),
    ];
    match since {
        Some(since) => argv.push(format!("--since={since}")),
        None => argv.push("--lines=0".to_string()),
    }
    match until {
        Some(until) => argv.push(format!("--until={until}")),
        None => argv.push("--follow".to_string()),
    }
    argv.push("_PID=1".to_string());
    argv.push("_UID=0".to_string());
    argv.extend(SYSTEMD_IDS.iter().map(|id| format!("MESSAGE_ID={id}")));
    argv.push("+".to_string());
    argv.push("_UID=0".to_string());
    argv.push(format!("MESSAGE_ID={MESSAGE_ID}"));
    argv
}

/// One event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// Microseconds since the epoch.
    pub time_usec: u64,
    /// machine, network or volume.
    pub kind: String,
    pub action: String,
    pub name: String,
    pub attributes: BTreeMap<String, String>,
    /// The machine's labels, when it still has a record.
    pub labels: BTreeMap<String, String>,
}

impl Event {
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "time": rfc3339(self.time_usec),
            "time_usec": self.time_usec,
            "type": self.kind,
            "action": self.action,
            "name": self.name,
            "attributes": self.attributes,
            "labels": self.labels,
        })
    }
}

/// Turns journal entries into events. A clean exit is not logged as such (only as the
/// unit's success), so it remembers which runs had their exit logged.
#[derive(Default)]
pub struct Mapper {
    exited: VecDeque<String>,
}

impl Mapper {
    pub fn map(&mut self, entry: &Value) -> Option<Event> {
        let field = |key: &str| entry.get(key).and_then(Value::as_str);
        let time_usec = field("__REALTIME_TIMESTAMP")?.parse().ok()?;
        let id = field("MESSAGE_ID")?;
        if id == MESSAGE_ID {
            let mut attributes = BTreeMap::new();
            if let Some(object) = entry.as_object() {
                for (key, value) in object {
                    if let (Some(attr), Some(value)) =
                        (key.strip_prefix("NSPAWN_ATTR_"), value.as_str())
                    {
                        attributes.insert(attr.to_ascii_lowercase(), value.to_string());
                    }
                }
            }
            return Some(Event {
                time_usec,
                kind: field("NSPAWN_TYPE")?.to_string(),
                action: field("NSPAWN_ACTION")?.to_string(),
                name: field("NSPAWN_NAME")?.to_string(),
                attributes,
                labels: BTreeMap::new(),
            });
        }
        let name = field("UNIT")?
            .strip_prefix("systemd-nspawn@")?
            .strip_suffix(".service")?
            .to_string();
        let invocation = field("INVOCATION_ID").unwrap_or_default().to_string();
        let mut attributes = BTreeMap::new();
        let action = match id {
            UNIT_STARTED => "start",
            UNIT_PROCESS_EXIT => {
                // The machine's own end; hooks and control processes are the unit's.
                if field("COMMAND") != Some("ExecStart") {
                    return None;
                }
                let code = field("EXIT_CODE").unwrap_or("exited");
                let status: i32 = field("EXIT_STATUS").and_then(|s| s.parse().ok())?;
                attributes.insert("code".into(), code.to_string());
                if code == "exited" {
                    attributes.insert("exit_code".into(), status.to_string());
                } else {
                    attributes.insert("signal".into(), status.to_string());
                    attributes.insert("exit_code".into(), (128 + status).to_string());
                }
                self.remember(invocation);
                "die"
            }
            UNIT_SUCCESS => {
                if self.exited.contains(&invocation) {
                    return None;
                }
                attributes.insert("code".into(), "exited".into());
                attributes.insert("exit_code".into(), "0".into());
                self.remember(invocation);
                "die"
            }
            UNIT_STOPPED => "stop",
            UNIT_RESTART_SCHEDULED => {
                if let Some(n) = field("N_RESTARTS") {
                    attributes.insert("restarts".into(), n.to_string());
                }
                "restart"
            }
            UNIT_OUT_OF_MEMORY => "oom",
            UNIT_FAILURE_RESULT => {
                if let Some(result) = field("UNIT_RESULT") {
                    attributes.insert("result".into(), result.to_string());
                }
                "fail"
            }
            _ => return None,
        };
        Some(Event {
            time_usec,
            kind: "machine".into(),
            action: action.into(),
            name,
            attributes,
            labels: BTreeMap::new(),
        })
    }

    fn remember(&mut self, invocation: String) {
        if invocation.is_empty() {
            return;
        }
        if self.exited.len() >= 1024 {
            self.exited.pop_front();
        }
        self.exited.push_back(invocation);
    }
}

/// docker's --filter: the same key given twice matches either value, different keys
/// must all match.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filters {
    names: Vec<String>,
    kinds: Vec<String>,
    actions: Vec<String>,
    /// KEY, or KEY and VALUE.
    labels: Vec<(String, Option<String>)>,
}

impl Filters {
    pub fn parse(filters: &[String]) -> Result<Self> {
        let mut out = Filters::default();
        for filter in filters {
            let Some((key, value)) = filter.split_once('=') else {
                bail!("filter {filter}: expected KEY=VALUE");
            };
            if value.is_empty() {
                bail!("filter {filter}: no value");
            }
            match key {
                "name" | "machine" => out.names.push(value.to_string()),
                "type" => match value {
                    "machine" | "network" | "volume" => out.kinds.push(value.to_string()),
                    other => {
                        bail!("filter {filter}: unknown type {other} (machine, network or volume)")
                    }
                },
                "event" | "action" => out.actions.push(value.to_string()),
                "label" => out.labels.push(match value.split_once('=') {
                    Some((k, v)) => (k.to_string(), Some(v.to_string())),
                    None => (value.to_string(), None),
                }),
                other => bail!("filter {filter}: unknown key {other} (name, type, event or label)"),
            }
        }
        Ok(out)
    }

    pub fn matches(&self, event: &Event) -> bool {
        let any =
            |wanted: &[String], value: &str| wanted.is_empty() || wanted.iter().any(|w| w == value);
        any(&self.names, &event.name)
            && any(&self.kinds, &event.kind)
            && any(&self.actions, &event.action)
            && self.labels.iter().all(|(key, value)| match value {
                Some(value) => event.labels.get(key) == Some(value),
                None => event.labels.contains_key(key),
            })
    }
}

/// A time as RFC 3339 in UTC with microseconds: 2026-09-24T10:00:00.123456Z.
pub fn rfc3339(usec: u64) -> String {
    let secs = usec / 1_000_000;
    let days = (secs / 86_400) as i64;
    let rest = secs % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:06}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60,
        usec % 1_000_000
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Entries as the journals of the test hosts have them (fields trimmed).
    fn entry(json: &str) -> Value {
        serde_json::from_str(json).unwrap()
    }

    const STARTED: &str = r#"{"__REALTIME_TIMESTAMP": "1790229291916165", "MESSAGE_ID": "39f53479d3a045ac8e11786248231fbf", "UNIT": "systemd-nspawn@e2e-twin-b.service", "_PID": "1", "INVOCATION_ID": "d88544d7e0d14f5395b056d60667192b", "MESSAGE": "Started systemd-nspawn@e2e-twin-b.service - Container e2e-twin-b.", "JOB_RESULT": "done", "JOB_TYPE": "start", "_UID": "0"}"#;
    const EXITED: &str = r#"{"UNIT": "systemd-nspawn@e2e-busybox.service", "EXIT_CODE": "exited", "_UID": "0", "COMMAND": "ExecStart", "EXIT_STATUS": "1", "INVOCATION_ID": "b94ee618ff994385b17142a77fe6cdf8", "__REALTIME_TIMESTAMP": "1790229272497465", "_PID": "1", "MESSAGE_ID": "98e322203f7a4ed290d09fe03c09fe15", "MESSAGE": "systemd-nspawn@e2e-busybox.service: Main process exited, code=exited, status=1/FAILURE"}"#;
    const FAILED: &str = r#"{"UNIT": "systemd-nspawn@e2e-busybox.service", "MESSAGE": "systemd-nspawn@e2e-busybox.service: Failed with result 'exit-code'.", "MESSAGE_ID": "d9b373ed55a64feb8242e02dbe79a49c", "UNIT_RESULT": "exit-code", "_PID": "1", "_UID": "0", "INVOCATION_ID": "b94ee618ff994385b17142a77fe6cdf8", "__REALTIME_TIMESTAMP": "1790229272524518"}"#;
    const SUCCESS: &str = r#"{"INVOCATION_ID": "d88544d7e0d14f5395b056d60667192b", "MESSAGE_ID": "7ad2d189f7e94e70a38c781354912448", "_PID": "1", "UNIT": "systemd-nspawn@e2e-twin-b.service", "__REALTIME_TIMESTAMP": "1790229292488891", "MESSAGE": "systemd-nspawn@e2e-twin-b.service: Deactivated successfully.", "_UID": "0"}"#;
    const STOPPED: &str = r#"{"JOB_TYPE": "stop", "UNIT": "systemd-nspawn@e2e-restart.service", "INVOCATION_ID": "0200e79359724cb7913dfe1a0bafa0f0", "_UID": "0", "MESSAGE_ID": "9d1aaa27d60140bd96365438aad20286", "_PID": "1", "__REALTIME_TIMESTAMP": "1790229255971774", "JOB_RESULT": "done", "MESSAGE": "Stopped systemd-nspawn@e2e-restart.service - Container e2e-restart."}"#;
    const RESTART: &str = r#"{"INVOCATION_ID": "a1691492352f4863bccfc3a4be9177c3", "N_RESTARTS": "1", "_UID": "0", "__REALTIME_TIMESTAMP": "1790229255058748", "_PID": "1", "MESSAGE_ID": "5eb03494b6584870a536b337290809b3", "UNIT": "systemd-nspawn@e2e-restart.service", "MESSAGE": "systemd-nspawn@e2e-restart.service: Scheduled restart job, restart counter is at 1."}"#;
    const OOM: &str = r#"{"MESSAGE": "systemd-nspawn@e2e-busybox.service: The kernel OOM killer killed some processes in this unit.", "UNIT": "systemd-nspawn@e2e-busybox.service", "__REALTIME_TIMESTAMP": "1790229269318480", "_UID": "0", "_PID": "1", "MESSAGE_ID": "fe6faa94e7774663a0da52717891d8ef", "INVOCATION_ID": "1d1124bd88ba456ba2b0953e45f21d2a"}"#;

    fn action(
        mapper: &mut Mapper,
        json: &str,
    ) -> Option<(String, String, BTreeMap<String, String>)> {
        mapper
            .map(&entry(json))
            .map(|e| (e.action, e.name, e.attributes))
    }

    #[test]
    fn systemd_entries_are_machine_events() {
        let mut m = Mapper::default();
        let (a, n, _) = action(&mut m, STARTED).unwrap();
        assert_eq!((a.as_str(), n.as_str()), ("start", "e2e-twin-b"));
        let (a, n, attrs) = action(&mut m, EXITED).unwrap();
        assert_eq!((a.as_str(), n.as_str()), ("die", "e2e-busybox"));
        assert_eq!(attrs["exit_code"], "1");
        assert_eq!(attrs["code"], "exited");
        let (a, _, attrs) = action(&mut m, FAILED).unwrap();
        assert_eq!(a, "fail");
        assert_eq!(attrs["result"], "exit-code");
        assert_eq!(action(&mut m, STOPPED).unwrap().0, "stop");
        let (a, _, attrs) = action(&mut m, RESTART).unwrap();
        assert_eq!(a, "restart");
        assert_eq!(attrs["restarts"], "1");
        assert_eq!(action(&mut m, OOM).unwrap().0, "oom");
    }

    #[test]
    fn a_clean_end_is_a_die_with_code_0_once_per_run() {
        let mut m = Mapper::default();
        let (a, _, attrs) = action(&mut m, SUCCESS).unwrap();
        assert_eq!(a, "die");
        assert_eq!(attrs["exit_code"], "0");
        // A run whose exit was logged already ends with that one alone.
        let exited = EXITED.replace("b94ee618ff994385b17142a77fe6cdf8", "run2");
        let success = SUCCESS.replace("d88544d7e0d14f5395b056d60667192b", "run2");
        assert!(action(&mut m, &exited).is_some());
        assert!(action(&mut m, &success).is_none());
    }

    #[test]
    fn signals_and_other_units_and_hooks() {
        let mut m = Mapper::default();
        let killed = EXITED
            .replace(r#""EXIT_CODE": "exited""#, r#""EXIT_CODE": "killed""#)
            .replace(r#""EXIT_STATUS": "1""#, r#""EXIT_STATUS": "9""#);
        let (_, _, attrs) = action(&mut m, &killed).unwrap();
        assert_eq!(attrs["signal"], "9");
        assert_eq!(attrs["exit_code"], "137");
        let hook = EXITED.replace(r#""COMMAND": "ExecStart""#, r#""COMMAND": "ExecStartPre""#);
        assert!(action(&mut m, &hook).is_none(), "a hook is not the machine");
        let other = STARTED.replace("systemd-nspawn@e2e-twin-b.service", "sshd.service");
        assert!(action(&mut m, &other).is_none());
    }

    #[test]
    fn nspawn_entries_carry_their_attributes() {
        let json = format!(
            r#"{{"__REALTIME_TIMESTAMP": "1790229291000000", "MESSAGE_ID": "{MESSAGE_ID}", "NSPAWN_TYPE": "machine", "NSPAWN_ACTION": "pull", "NSPAWN_NAME": "web", "NSPAWN_ATTR_REFERENCE": "hub.nspawn.org/nginx:1.27", "_UID": "0"}}"#
        );
        let e = Mapper::default().map(&entry(&json)).unwrap();
        assert_eq!(
            (e.kind.as_str(), e.action.as_str(), e.name.as_str()),
            ("machine", "pull", "web")
        );
        assert_eq!(e.attributes["reference"], "hub.nspawn.org/nginx:1.27");
        assert_eq!(attribute_field("image-ref"), "NSPAWN_ATTR_IMAGE_REF");
    }

    #[test]
    fn filters_like_docker() {
        let mut e = Mapper::default().map(&entry(STARTED)).unwrap();
        e.labels.insert("caddy".into(), "web.example".into());
        let f = |list: &[&str]| {
            Filters::parse(&list.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()
        };
        assert!(f(&[]).matches(&e));
        assert!(f(&["name=e2e-twin-b", "name=other"]).matches(&e));
        assert!(!f(&["name=other"]).matches(&e));
        assert!(f(&["type=machine", "event=start"]).matches(&e));
        assert!(!f(&["type=machine", "event=die"]).matches(&e));
        assert!(f(&["label=caddy"]).matches(&e));
        assert!(f(&["label=caddy=web.example"]).matches(&e));
        assert!(!f(&["label=caddy=other"]).matches(&e));
        assert!(!f(&["label=none"]).matches(&e));
        for bad in ["name", "name=", "type=container", "colour=red"] {
            assert!(Filters::parse(&[bad.to_string()]).is_err(), "{bad}");
        }
    }

    #[test]
    fn journalctl_follows_from_now_or_reads_a_window() {
        let follow = journalctl_arguments(None, None);
        assert!(follow.contains(&"--lines=0".to_string()));
        assert!(follow.contains(&"--follow".to_string()));
        assert!(!follow
            .iter()
            .any(|a| a.starts_with("-u") || a.starts_with("--unit")));
        let plus = follow.iter().position(|a| a == "+").unwrap();
        assert_eq!(
            follow[plus + 1..],
            ["_UID=0".to_string(), format!("MESSAGE_ID={MESSAGE_ID}")]
        );
        let window = journalctl_arguments(Some("-1h"), Some("now"));
        assert!(window.contains(&"--since=-1h".to_string()));
        assert!(window.contains(&"--until=now".to_string()));
        assert!(!window.contains(&"--follow".to_string()));
    }

    #[test]
    fn times_in_utc() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00.000000Z");
        assert_eq!(
            rfc3339(1_790_229_291_916_165),
            "2026-09-24T05:54:51.916165Z"
        );
        assert_eq!(rfc3339(951_782_400_000_001), "2000-02-29T00:00:00.000001Z");
    }
}
