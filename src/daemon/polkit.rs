//! Who may call what. The bus lets everyone in and the service asks polkit about the
//! caller, as machined does: the actions are org.nspawn.inspect for the read-only
//! methods and org.nspawn.manage for the rest, both admin-only by default, so an
//! administrator can hand either of them to a group with a rule of their own.
//! org.nspawn.Names asks for neither: it hands out names alone (see `names.rs`).

use std::collections::HashMap;

use zbus::message::Header;
use zbus::zvariant::Value;
use zbus::Connection;

/// Who called: the uid a job or a command belongs to afterwards, and the bus name its
/// signals go to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    pub uid: u32,
    pub name: Option<String>,
}

/// What a method needs from its caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    /// Reading what is there: the images, the machines, the network.
    Inspect,
    /// Everything else: it changes the host, reaches a registry with the credentials
    /// kept here, or runs a command inside a machine.
    Manage,
}

impl Action {
    pub fn id(self) -> &'static str {
        match self {
            Action::Inspect => "org.nspawn.inspect",
            Action::Manage => "org.nspawn.manage",
        }
    }
}

#[zbus::proxy(
    interface = "org.freedesktop.PolicyKit1.Authority",
    default_service = "org.freedesktop.PolicyKit1",
    default_path = "/org/freedesktop/PolicyKit1/Authority",
    gen_blocking = false
)]
trait Authority {
    /// The subject is ("system-bus-name", {"name": <the caller's unique name>}) or
    /// ("unix-process", {"pid", "start-time", "uid"}), the flags are 1 for
    /// AllowUserInteraction. Returns whether it is authorized, whether
    /// a challenge is pending, and details.
    fn check_authorization(
        &self,
        subject: &(&str, HashMap<&str, Value<'_>>),
        action_id: &str,
        details: HashMap<&str, &str>,
        flags: u32,
        cancellation_id: &str,
    ) -> zbus::Result<(bool, bool, HashMap<String, String>)>;
}

/// Whether `sender` may do `action`. Root is allowed without asking, so the service
/// keeps working on hosts with no polkit at all. polkit asks the user through an agent
/// only when the call allowed it (`interactive`, the call's
/// ALLOW_INTERACTIVE_AUTHORIZATION flag); otherwise a challenge is a refusal at once.
pub async fn allows(
    connection: &Connection,
    sender: &str,
    action: Action,
    interactive: bool,
) -> Result<(), String> {
    if caller_uid(connection, sender).await == Some(0) {
        return Ok(());
    }
    let mut name = HashMap::new();
    name.insert("name", Value::from(sender));
    let (authorized, challenge) =
        check(connection, ("system-bus-name", name), action, interactive).await?;
    if authorized {
        return Ok(());
    }
    Err(if challenge && !interactive {
        format!(
            "{} needs an administrator, and the call did not allow interactive authorization; run the command as root, or let your group through with a polkit rule (see docs/DBUS.md)",
            action.id()
        )
    } else if challenge {
        format!(
            "{} needs an administrator; answer the authentication agent, run the command as root, or let your group through with a polkit rule (see docs/DBUS.md)",
            action.id()
        )
    } else {
        format!(
            "{} is not allowed for you; run the command as root, or let your group through with a polkit rule (see docs/DBUS.md)",
            action.id()
        )
    })
}

/// A process on the other end of a socket, as polkit names it: its pid, the time it
/// started (which tells it from a later process of the same pid) and its uid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Process {
    pub pid: u32,
    pub start_time: u64,
    pub uid: u32,
}

impl Process {
    /// The process `pid` of `uid`, if it is still there to read its start time from.
    pub fn of(pid: u32, uid: u32) -> Option<Process> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        Some(Process {
            pid,
            start_time: start_time(&stat)?,
            uid,
        })
    }
}

/// The start time in /proc/PID/stat: the 22nd field, counted after the command name,
/// which may hold spaces and parentheses itself.
fn start_time(stat: &str) -> Option<u64> {
    let (_, rest) = stat.rsplit_once(')')?;
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// Whether `process` may do `action`, never asking: a client of a socket, which an
/// authentication agent cannot reach. Root is allowed without asking.
pub async fn allows_process(
    connection: &Connection,
    process: Process,
    action: Action,
) -> Result<(), String> {
    if process.uid == 0 {
        return Ok(());
    }
    let mut details = HashMap::new();
    details.insert("pid", Value::from(process.pid));
    details.insert("start-time", Value::from(process.start_time));
    // polkit's unix-process takes the uid signed.
    details.insert("uid", Value::from(process.uid as i32));
    let (authorized, challenge) =
        check(connection, ("unix-process", details), action, false).await?;
    if authorized {
        return Ok(());
    }
    Err(if challenge {
        format!(
            "{} needs an administrator, and a socket cannot ask for a password; use it as root, or let your group through with a polkit rule (see docs/DBUS.md)",
            action.id()
        )
    } else {
        format!(
            "{} is not allowed for you; use the socket as root, or let your group through with a polkit rule (see docs/DBUS.md)",
            action.id()
        )
    })
}

/// polkit's answer about `subject`: whether it is authorized, and whether it would be
/// after a challenge.
async fn check(
    connection: &Connection,
    subject: (&str, HashMap<&str, Value<'_>>),
    action: Action,
    interactive: bool,
) -> Result<(bool, bool), String> {
    let authority = AuthorityProxy::new(connection)
        .await
        .map_err(|e| format!("reaching polkit: {e}"))?;
    let (authorized, challenge, _) = authority
        .check_authorization(
            &subject,
            action.id(),
            HashMap::new(),
            u32::from(interactive),
            "",
        )
        .await
        .map_err(|e| {
            // Building the proxy asks nothing of the bus: polkit's absence shows here.
            if e.to_string().contains("ServiceUnknown") {
                "polkit is not on this bus, so only root can be authorized; run the command as root"
                    .to_string()
            } else {
                format!("asking polkit about {}: {e}", action.id())
            }
        })?;
    Ok((authorized, challenge))
}

/// The uid behind a bus name, as the bus itself reports it.
pub async fn caller_uid(connection: &Connection, sender: &str) -> Option<u32> {
    let bus = zbus::fdo::DBusProxy::new(connection).await.ok()?;
    let name = zbus::names::BusName::try_from(sender).ok()?;
    bus.get_connection_unix_user(name).await.ok()
}

/// Who may look at a job or a command somebody started: whoever started it, and root.
pub fn may_read(owner: u32, caller: Reader) -> bool {
    match caller {
        Reader::Service => true,
        Reader::User(uid) => uid == owner || uid == 0,
        Reader::Unknown => false,
    }
}

/// Who reads a property.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reader {
    /// The service itself, on its way to a signal: no message behind the read.
    Service,
    User(u32),
    /// A message whose sender the bus cannot name (it has just gone, say).
    Unknown,
}

/// Who a message came from.
pub async fn reader(connection: &Connection, header: Option<&Header<'_>>) -> Reader {
    let Some(header) = header else {
        return Reader::Service;
    };
    let Some(sender) = header.sender() else {
        return Reader::Unknown;
    };
    match caller_uid(connection, sender.as_str()).await {
        Some(uid) => Reader::User(uid),
        None => Reader::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_owner_and_root_read_what_was_started() {
        assert!(may_read(1000, Reader::User(1000)), "the one who started it");
        assert!(may_read(1000, Reader::User(0)), "root");
        assert!(!may_read(1000, Reader::User(1001)), "another user");
        assert!(
            !may_read(0, Reader::User(1000)),
            "root's work is not for everyone"
        );
        assert!(
            may_read(1000, Reader::Service),
            "the service reading its own"
        );
        assert!(
            !may_read(1000, Reader::Unknown),
            "nor someone the bus cannot name"
        );
    }

    #[test]
    fn the_start_time_is_read_past_a_command_name_with_spaces() {
        let stat = "4242 (a b) c) S 1 4242 4242 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 987654 1000 10 18446744073709551615";
        assert_eq!(start_time(stat), Some(987654));
        assert_eq!(start_time("4242 (short) S 1"), None);
        assert_eq!(start_time("no parenthesis"), None);
    }

    #[test]
    fn every_action_has_an_id_under_our_prefix() {
        for action in [Action::Inspect, Action::Manage] {
            assert!(action.id().starts_with("org.nspawn."), "{action:?}");
        }
        assert_ne!(Action::Inspect.id(), Action::Manage.id());
    }
}
