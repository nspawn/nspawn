//! Who may do what through the Docker API, by the socket a client came in through. A
//! machine's socket serves the machine whatever it asks, as the engine's socket serves
//! whoever reaches it, unless the machine binds it read-only: then it reads, follows
//! logs and kills. The host's socket asks polkit about each client, with the actions
//! the bus service asks about the methods behind each request.

use std::collections::HashSet;

use hyper::StatusCode;
use tokio::sync::Mutex;

use crate::api::Context;
use crate::daemon::polkit::{self, Action, Process};
use crate::store::Store;

/// The socket the server was started for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// The host's socket, nspawn-api.socket.
    Host,
    /// A machine's socket, nspawn-api@NAME.socket; `write` unless the machine binds it
    /// read-only.
    Machine { write: bool },
}

impl Profile {
    /// The profile of `name`'s socket: read-only when a bind of the socket is, or when
    /// the record cannot be read or binds none.
    pub fn of_machine(store: &Store, name: &str) -> Profile {
        let write = match store.load_image(name) {
            Ok(Some(record)) => {
                let mut binds = record.volumes.iter().filter(|v| v.is_api_socket());
                let first = binds.next();
                first.is_some_and(|v| !v.read_only) && binds.all(|v| !v.read_only)
            }
            _ => false,
        };
        Profile::Machine { write }
    }
}

/// What a request asks of its client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    /// Nothing: _ping and version, which clients ask first to learn what answers.
    Nothing,
    /// A look at what is there (org.nspawn.inspect).
    Inspect,
    /// What the bus asks org.nspawn.manage for and a read-only socket serves all the
    /// same: logs and a kill.
    Observe,
    /// A change (org.nspawn.manage).
    Manage,
}

impl Need {
    fn action(self) -> Option<Action> {
        match self {
            Need::Nothing => None,
            Need::Inspect => Some(Action::Inspect),
            Need::Observe | Need::Manage => Some(Action::Manage),
        }
    }
}

/// One connection's client, and the actions polkit allowed it so far.
pub struct Client {
    profile: Profile,
    /// On the host's socket: the process that connected, if its credentials could be
    /// read.
    process: Option<Process>,
    allowed: Mutex<HashSet<Action>>,
}

impl Client {
    /// The client of a connection on a socket of `profile`, from the credentials the
    /// kernel gives of its other end.
    pub fn new(profile: Profile, peer: Option<(u32, Option<i32>)>) -> Client {
        let process = match profile {
            Profile::Host => peer.and_then(|(uid, pid)| {
                let pid = u32::try_from(pid?).ok().filter(|pid| *pid > 0)?;
                Process::of(pid, uid)
            }),
            Profile::Machine { .. } => None,
        };
        Client {
            profile,
            process,
            allowed: Mutex::new(HashSet::new()),
        }
    }

    /// Whether the client may do what `need` names, or the status and the words of the
    /// refusal.
    pub async fn allows(&self, ctx: &Context, need: Need) -> Result<(), (StatusCode, String)> {
        let Some(action) = need.action() else {
            return Ok(());
        };
        match self.profile {
            Profile::Machine { write } => {
                if write || need != Need::Manage {
                    Ok(())
                } else {
                    Err((
                        StatusCode::FORBIDDEN,
                        "this machine binds nspawn's Docker API read-only".to_string(),
                    ))
                }
            }
            Profile::Host => {
                let Some(process) = self.process else {
                    return Err((
                        StatusCode::FORBIDDEN,
                        "the credentials of this socket's client could not be read".to_string(),
                    ));
                };
                // Held across the question, so that the requests of one connection
                // are allowed once. A refusal is asked again: it may have been polkit
                // out of reach.
                let mut allowed = self.allowed.lock().await;
                if allowed.contains(&action) {
                    return Ok(());
                }
                let answer = match ctx.sd().await {
                    Ok(sd) => polkit::allows_process(sd.connection(), process, action).await,
                    Err(e) => Err(format!("reaching the system bus: {e:#}")),
                };
                if answer.is_ok() {
                    allowed.insert(action);
                }
                answer.map_err(|e| (StatusCode::FORBIDDEN, e))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, FileConfig};
    use crate::store::ImageRecord;

    fn context() -> (tempfile::TempDir, Context) {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = Config::merge(FileConfig::default(), None, None).unwrap();
        config.machines_dir = tmp.path().join("machines");
        config.state_dir = tmp.path().join("state");
        let ctx = Context::new(config);
        ctx.store.init().unwrap();
        (tmp, ctx)
    }

    fn record(ctx: &Context, name: &str, volumes: &[&str]) {
        let mut record: ImageRecord = serde_json::from_str(&format!(
            r#"{{"name": "{name}", "reference": "busybox:1.37", "manifest_digest": "d", "layers": [], "backend": "overlay", "created": 0}}"#
        ))
        .unwrap();
        record.volumes = volumes.iter().map(|v| v.parse().unwrap()).collect();
        ctx.store.record_image(&record).unwrap();
    }

    #[tokio::test]
    async fn a_machine_socket_serves_writes_unless_bound_read_only() {
        let (_tmp, ctx) = context();
        let read_only = Client::new(Profile::Machine { write: false }, None);
        for need in [Need::Nothing, Need::Inspect, Need::Observe] {
            assert!(read_only.allows(&ctx, need).await.is_ok(), "{need:?}");
        }
        let (status, text) = read_only.allows(&ctx, Need::Manage).await.unwrap_err();
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(text.contains("read-only"), "{text}");
        let full = Client::new(Profile::Machine { write: true }, None);
        assert!(full.allows(&ctx, Need::Manage).await.is_ok());
    }

    #[tokio::test]
    async fn the_host_socket_answers_root_and_refuses_an_unknown_client() {
        let (_tmp, ctx) = context();
        let me = std::process::id() as i32;
        let root = Client::new(Profile::Host, Some((0, Some(me))));
        assert!(root.allows(&ctx, Need::Manage).await.is_ok());
        let unknown = Client::new(Profile::Host, Some((1000, None)));
        assert!(unknown.allows(&ctx, Need::Nothing).await.is_ok());
        let (status, text) = unknown.allows(&ctx, Need::Inspect).await.unwrap_err();
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(text.contains("credentials"), "{text}");
    }

    #[test]
    fn a_machine_profile_follows_its_bind_of_the_socket() {
        let (_tmp, ctx) = context();
        let socket = "/var/run/docker.sock:/var/run/docker.sock";
        record(&ctx, "full", &[socket, "/srv:/srv:ro"]);
        record(
            &ctx,
            "ro",
            &["/var/run/docker.sock:/var/run/docker.sock:ro"],
        );
        record(
            &ctx,
            "mixed",
            &[socket, "/run/docker.sock:/run/docker.sock:ro"],
        );
        record(&ctx, "none", &["/srv:/srv"]);
        let of = |name| Profile::of_machine(&ctx.store, name);
        assert_eq!(of("full"), Profile::Machine { write: true });
        assert_eq!(of("ro"), Profile::Machine { write: false });
        assert_eq!(of("mixed"), Profile::Machine { write: false });
        assert_eq!(of("none"), Profile::Machine { write: false });
        assert_eq!(of("missing"), Profile::Machine { write: false });
    }
}
