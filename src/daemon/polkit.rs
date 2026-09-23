//! Who may call what. The bus lets everyone in and the service asks polkit about the
//! caller, as machined does: the actions are org.nspawn.inspect for the read-only
//! methods and org.nspawn.manage for the rest, both admin-only by default, so an
//! administrator can hand either of them to a group with a rule of their own.

use std::collections::HashMap;

use zbus::message::Header;
use zbus::zvariant::Value;
use zbus::Connection;

/// What a method needs from its caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// The subject is ("system-bus-name", {"name": <the caller's unique name>}), the
    /// flags are 1 for AllowUserInteraction. Returns whether it is authorized, whether
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
/// keeps working on hosts with no polkit at all.
pub async fn allows(connection: &Connection, sender: &str, action: Action) -> Result<(), String> {
    if caller_uid(connection, sender).await == Some(0) {
        return Ok(());
    }
    let authority = AuthorityProxy::new(connection).await.map_err(|_| {
        "polkit is not on this bus, so only root can be authorized; run the command as root"
            .to_string()
    })?;
    let mut name = HashMap::new();
    name.insert("name", Value::from(sender));
    let subject = ("system-bus-name", name);
    let (authorized, challenge, _) = authority
        .check_authorization(&subject, action.id(), HashMap::new(), 1, "")
        .await
        .map_err(|e| format!("asking polkit about {}: {e}", action.id()))?;
    if authorized {
        return Ok(());
    }
    Err(if challenge {
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

/// The uid behind a bus name, as the bus itself reports it.
pub async fn caller_uid(connection: &Connection, sender: &str) -> Option<u32> {
    let bus = zbus::fdo::DBusProxy::new(connection).await.ok()?;
    let name = zbus::names::BusName::try_from(sender).ok()?;
    bus.get_connection_unix_user(name).await.ok()
}

/// Who may look at a job or a command somebody started: whoever started it, and root.
/// A read with no message behind it is the service's own, on its way to a signal.
pub fn may_read(owner: u32, caller: Option<u32>) -> bool {
    match caller {
        None => true,
        Some(uid) => uid == owner || uid == 0,
    }
}

/// The uid of the caller a message came from, None for the service's own reads.
pub async fn header_uid(connection: &Connection, header: Option<&Header<'_>>) -> Option<u32> {
    let sender = header?.sender()?.to_string();
    caller_uid(connection, &sender).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_owner_and_root_read_what_was_started() {
        assert!(may_read(1000, Some(1000)), "the one who started it");
        assert!(may_read(1000, Some(0)), "root");
        assert!(!may_read(1000, Some(1001)), "another user");
        assert!(!may_read(0, Some(1000)), "root's work is not for everyone");
        assert!(may_read(1000, None), "the service reading its own");
    }

    #[test]
    fn every_action_has_an_id_under_our_prefix() {
        for action in [Action::Inspect, Action::Manage] {
            assert!(action.id().starts_with("org.nspawn."), "{action:?}");
        }
        assert_ne!(Action::Inspect.id(), Action::Manage.id());
    }
}
