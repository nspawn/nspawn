//! What kind of failure an error is, for the clients that answer each kind their own way
//! (a D-Bus error name, a status of the Docker API) without reading its message. The
//! kind travels in the error's chain; the message stays what the command line prints.

/// The failures a client tells apart. Anything else is a failure and no more.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FailureKind {
    /// A machine method names no record and no machine.
    NoSuchMachine,
    /// An image that is not here.
    NoSuchImage,
    NoSuchNetwork,
    NoSuchVolume,
    /// A path inside a machine that is not there.
    NoSuchPath,
    /// A name taken already.
    Exists,
    /// A removal refused because something uses what it names.
    InUse,
    /// What needs a running machine, of one that does not run.
    NotRunning,
    /// A start of a machine that runs.
    AlreadyRunning,
    /// A start or a pause of a paused machine.
    Paused,
    /// An unpause of a machine that is not paused.
    NotPaused,
    /// A bad argument or option: a wrong type, an unknown option, a bad value.
    Invalid,
}

impl FailureKind {
    #[cfg(test)]
    pub const ALL: [FailureKind; 12] = [
        FailureKind::NoSuchMachine,
        FailureKind::NoSuchImage,
        FailureKind::NoSuchNetwork,
        FailureKind::NoSuchVolume,
        FailureKind::NoSuchPath,
        FailureKind::Exists,
        FailureKind::InUse,
        FailureKind::NotRunning,
        FailureKind::AlreadyRunning,
        FailureKind::Paused,
        FailureKind::NotPaused,
        FailureKind::Invalid,
    ];

    /// Its name under org.nspawn.Error.
    pub fn name(self) -> &'static str {
        match self {
            FailureKind::NoSuchMachine => "NoSuchMachine",
            FailureKind::NoSuchImage => "NoSuchImage",
            FailureKind::NoSuchNetwork => "NoSuchNetwork",
            FailureKind::NoSuchVolume => "NoSuchVolume",
            FailureKind::NoSuchPath => "NoSuchPath",
            FailureKind::Exists => "Exists",
            FailureKind::InUse => "InUse",
            FailureKind::NotRunning => "NotRunning",
            FailureKind::AlreadyRunning => "AlreadyRunning",
            FailureKind::Paused => "Paused",
            FailureKind::NotPaused => "NotPaused",
            FailureKind::Invalid => "Invalid",
        }
    }

    /// An error of this kind saying `message`.
    pub fn error(self, message: impl Into<String>) -> anyhow::Error {
        anyhow::Error::new(Failure {
            kind: self,
            message: message.into(),
        })
    }
}

/// An error that says its kind. Its text is the message alone, so that a context added
/// around it reads as it would around any other error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub kind: FailureKind,
    pub message: String,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Failure {}

/// The kind of the first failure in `e`'s chain that says one.
pub fn kind_of(e: &anyhow::Error) -> Option<FailureKind> {
    e.chain()
        .find_map(|cause| cause.downcast_ref::<Failure>())
        .map(|f| f.kind)
}

/// `e` as a bad argument, unless it says its kind already; its text as it was.
pub fn invalid(e: anyhow::Error) -> anyhow::Error {
    if kind_of(&e).is_some() {
        return e;
    }
    FailureKind::Invalid.error(format!("{e:#}"))
}

/// The one kind every failure of `kinds` has; none when one has none or two differ.
pub fn common_kind(kinds: &[Option<FailureKind>]) -> Option<FailureKind> {
    let first = (*kinds.first()?)?;
    kinds.iter().all(|k| *k == Some(first)).then_some(first)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context as _;

    #[test]
    fn the_kind_is_found_under_contexts_and_the_text_stays() {
        let e = FailureKind::NotRunning.error("machine web is not running");
        assert_eq!(kind_of(&e), Some(FailureKind::NotRunning));
        assert_eq!(format!("{e:#}"), "machine web is not running");
        let wrapped = Err::<(), _>(e).context("stopping web").unwrap_err();
        assert_eq!(kind_of(&wrapped), Some(FailureKind::NotRunning));
        assert_eq!(
            format!("{wrapped:#}"),
            "stopping web: machine web is not running"
        );
        assert_eq!(kind_of(&anyhow::anyhow!("plain")), None);
    }

    #[test]
    fn a_bad_argument_keeps_its_text_and_a_kind_given_stays() {
        let plain = invalid(anyhow::anyhow!("port 99999 is out of range").context("-p"));
        assert_eq!(kind_of(&plain), Some(FailureKind::Invalid));
        assert_eq!(format!("{plain:#}"), "-p: port 99999 is out of range");
        let known = invalid(FailureKind::NoSuchNetwork.error("network x does not exist"));
        assert_eq!(kind_of(&known), Some(FailureKind::NoSuchNetwork));
    }

    #[test]
    fn several_failures_have_a_kind_only_when_they_share_it() {
        let in_use = Some(FailureKind::InUse);
        assert_eq!(common_kind(&[in_use, in_use]), in_use);
        assert_eq!(common_kind(&[in_use, Some(FailureKind::Invalid)]), None);
        assert_eq!(common_kind(&[in_use, None]), None);
        assert_eq!(common_kind(&[]), None);
        let names: std::collections::HashSet<&str> =
            FailureKind::ALL.iter().map(|k| k.name()).collect();
        assert_eq!(names.len(), FailureKind::ALL.len(), "every name once");
    }
}
