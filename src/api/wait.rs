//! WaitMachine: the end of a machine's run, or its removal. The end comes from systemd's
//! PropertiesChanged of the machine's unit, which carries how the run ended as it was
//! (a later read can race a restart); a removal from the store's directory of records,
//! through inotify. Nothing is asked again and again, and the journal is read only for
//! a run that ended before the wait, of a unit systemd has unloaded since.

use std::ffi::OsString;
use std::os::fd::{AsFd, AsRawFd, RawFd};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _, Result};
use futures_util::{FutureExt, StreamExt};
use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify, InotifyEvent};
use tokio::io::unix::AsyncFd;
use tokio::sync::broadcast;
use zbus::zvariant::Value;

use crate::api::run::{exit_code, unit_stopped, Ending, CLD_EXITED, REBOOT};
use crate::api::{Context, FailureKind};
use crate::store::{ImageRecord, Store};
use crate::systemd::Systemd;

/// What a wait waits for, as the Docker Engine API names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Condition {
    /// The machine does not run: at once, or at the end of its run.
    NotRunning,
    /// The end of its next run.
    NextExit,
    /// Its record is gone.
    Removed,
}

impl Condition {
    /// not-running (also an empty string), next-exit or removed.
    pub fn parse(text: &str) -> Result<Condition> {
        match text {
            "" | "not-running" => Ok(Condition::NotRunning),
            "next-exit" => Ok(Condition::NextExit),
            "removed" => Ok(Condition::Removed),
            other => Err(FailureKind::Invalid.error(format!(
                "invalid condition {other:?}: not-running, next-exit or removed"
            ))),
        }
    }
}

/// Refuses a name with neither a record nor a running machine behind it, before a wait
/// is set up for it.
pub async fn check(ctx: &Context, name: &str) -> Result<()> {
    crate::reference::validate_entry_name(name)?;
    if ctx.store.load_image(name)?.is_some() {
        return Ok(());
    }
    let sd = ctx.sd().await?;
    crate::api::machines::refuse_foreign(sd, name).await?;
    if sd.machine_exists(name).await? {
        return Ok(());
    }
    Err(FailureKind::NoSuchMachine.error(format!("no machine or image named {name}")))
}

/// What systemd says of the main process of a unit's run: when it started (monotonic
/// microseconds, 0 before any run) and how it ended (code 0 while it runs).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Main {
    start: u64,
    code: i32,
    status: i32,
    result: String,
}

impl Main {
    /// From the properties of the unit's Service interface, by name.
    fn of<'v>(get: impl Fn(&str) -> Option<&'v Value<'v>>) -> Main {
        Main {
            start: get("ExecMainStartTimestampMonotonic")
                .and_then(|v| u64::try_from(v).ok())
                .unwrap_or(0),
            code: get("ExecMainCode")
                .and_then(|v| i32::try_from(v).ok())
                .unwrap_or(0),
            status: get("ExecMainStatus")
                .and_then(|v| i32::try_from(v).ok())
                .unwrap_or(0),
            result: get("Result")
                .and_then(|v| <&str>::try_from(v).ok())
                .unwrap_or_default()
                .to_string(),
        }
    }

    fn ending(&self) -> Ending {
        Ending {
            code: self.code,
            status: self.status,
            result: self.result.clone(),
        }
    }
}

/// Whether the unit runs a machine, or is about to: a start under way, or a main process
/// that has not ended. Between two runs of its restart policy it does not.
fn runs_now(active: &str, sub: &str, main: &Main) -> bool {
    match active {
        "activating" => !sub.starts_with("auto-restart"),
        "active" | "reloading" | "deactivating" => main.start > 0 && main.code == 0,
        _ => false,
    }
}

/// Whether a start of the unit is under way and its main process not started yet: its
/// conditions or its hooks before it.
fn before_main(active: &str, sub: &str) -> bool {
    active == "activating" && matches!(sub, "condition" | "start-pre")
}

/// What a wait knows of the runs of a unit, from its changes: up to which run the ends
/// are said already, a run that ended asking for a reboot, which is its end only when
/// the unit does not start again, and a start whose main process has not started.
#[derive(Debug, Default)]
struct Runs {
    /// The start of the last run whose end counts no more. systemd says a run's end again
    /// with every change until the next run's main process starts.
    done: u64,
    rebooting: Option<Ending>,
    /// A start that fails before its main process (a hook refusing it) ends that run too.
    starting: bool,
}

impl Runs {
    /// As the unit is when the wait begins: the run whose main process ended is over.
    fn new(main: &Main, active: &str, sub: &str) -> Runs {
        Runs {
            done: if main.code != 0 { main.start } else { 0 },
            rebooting: None,
            starting: before_main(active, sub),
        }
    }

    /// A change of the unit's Service interface: the end of a run not said yet.
    fn service(&mut self, main: &Main) -> Option<Ending> {
        if main.start == 0 || main.start <= self.done {
            return None;
        }
        self.starting = false;
        if main.code == 0 {
            // A run of its own after a reboot: that was no end.
            self.rebooting = None;
            return None;
        }
        self.done = main.start;
        let ending = main.ending();
        if ending.code == CLD_EXITED && ending.status == REBOOT {
            self.rebooting = Some(ending);
            return None;
        }
        self.rebooting = None;
        Some(ending)
    }

    /// A change of the unit's Unit interface: a reboot the unit did not follow ends the
    /// run, and so does a start that failed before its main process, as a program that
    /// could not be run (125). systemd sends the Service interface's change of the same
    /// moment first.
    fn unit(&mut self, active: &str, sub: &str) -> Option<Ending> {
        if before_main(active, sub) {
            self.starting = true;
            return None;
        }
        if !unit_stopped(Some(active), Some(sub)) {
            return None;
        }
        if let Some(ending) = self.rebooting.take() {
            self.starting = false;
            return Some(ending);
        }
        std::mem::take(&mut self.starting).then(|| Ending {
            code: 0,
            status: 0,
            result: String::new(),
        })
    }
}

/// An inotify instance as tokio's readiness wants it.
struct Watched(Inotify);

impl AsRawFd for Watched {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_fd().as_raw_fd()
    }
}

/// What happened to a record, as far as the watch can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seen {
    /// Deleted or moved away, or its directory went: the machine's removal.
    Gone,
    /// Written anew, or events were lost: the record says.
    Look,
}

/// What the watch of the records says: of the record of one file, or of all.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Note {
    file: Option<OsString>,
    seen: Seen,
}

/// What one inotify event says.
fn note(event: &InotifyEvent) -> Option<Note> {
    let directory =
        AddWatchFlags::IN_DELETE_SELF | AddWatchFlags::IN_MOVE_SELF | AddWatchFlags::IN_IGNORED;
    if event.mask.intersects(directory) {
        return Some(Note {
            file: None,
            seen: Seen::Gone,
        });
    }
    if event.mask.contains(AddWatchFlags::IN_Q_OVERFLOW) {
        return Some(Note {
            file: None,
            seen: Seen::Look,
        });
    }
    let file = event.name.clone()?;
    let seen = if event
        .mask
        .intersects(AddWatchFlags::IN_DELETE | AddWatchFlags::IN_MOVED_FROM)
    {
        Seen::Gone
    } else {
        Seen::Look
    };
    Some(Note {
        file: Some(file),
        seen,
    })
}

/// The one watch of the directory of records that the waits for removals of a process
/// share: inotify instances count against a limit root shares with the rest of the
/// system. Kept while its task runs, which ends with the directory or an error.
static RECORDS: std::sync::Mutex<Option<(PathBuf, broadcast::WeakSender<Note>)>> =
    std::sync::Mutex::new(None);

/// Starts the watch of `dir`, which reads its events in a task of its own.
fn start_watch(dir: &Path) -> Result<broadcast::Sender<Note>> {
    let inotify = Inotify::init(InitFlags::IN_NONBLOCK | InitFlags::IN_CLOEXEC)
        .context("watching the records")?;
    inotify
        .add_watch(
            dir,
            AddWatchFlags::IN_DELETE
                | AddWatchFlags::IN_MOVED_FROM
                | AddWatchFlags::IN_MOVED_TO
                | AddWatchFlags::IN_DELETE_SELF
                | AddWatchFlags::IN_MOVE_SELF
                | AddWatchFlags::IN_ONLYDIR,
        )
        .with_context(|| format!("watching {}", dir.display()))?;
    let inotify = AsyncFd::new(Watched(inotify)).context("watching the records")?;
    let (tx, _) = broadcast::channel(256);
    let sender = tx.clone();
    tokio::spawn(async move {
        loop {
            let Ok(mut ready) = inotify.readable().await else {
                return;
            };
            let events = match ready
                .try_io(|watched| watched.get_ref().0.read_events().map_err(Into::into))
            {
                Ok(Ok(events)) => events,
                Ok(Err(_)) => return,
                Err(_) => continue,
            };
            let mut over = false;
            for note in events.iter().filter_map(note) {
                over |= note.file.is_none() && note.seen == Seen::Gone;
                // None listening is no reason to stop: the next wait finds it.
                let _ = sender.send(note);
            }
            if over {
                return;
            }
        }
    });
    Ok(tx)
}

/// The directory of records, watched for the one of a name.
struct RecordWatch {
    notes: broadcast::Receiver<Note>,
    file: OsString,
}

impl RecordWatch {
    fn new(dir: &Path, name: &str) -> Result<Self> {
        let mut shared = RECORDS.lock().unwrap_or_else(|e| e.into_inner());
        let running = shared
            .as_ref()
            .filter(|(watched, _)| watched == dir)
            .and_then(|(_, weak)| weak.upgrade());
        let sender = match running {
            Some(sender) => sender,
            None => {
                let sender = start_watch(dir)?;
                *shared = Some((dir.to_path_buf(), sender.downgrade()));
                sender
            }
        };
        Ok(RecordWatch {
            notes: sender.subscribe(),
            file: OsString::from(format!("{name}.json")),
        })
    }

    /// Waits for a note about the record.
    async fn next(&mut self) -> Result<Seen> {
        loop {
            match self.notes.recv().await {
                Ok(note) if note.file.is_none() || note.file.as_ref() == Some(&self.file) => {
                    return Ok(note.seen);
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => return Ok(Seen::Look),
                Err(broadcast::error::RecvError::Closed) => bail!("lost the watch of the records"),
            }
        }
    }
}

/// Whether the record of `name` made at `created` is gone: none of the name, or another
/// machine's.
/// Whether what the watch saw is the removal of the machine made at `created`. A record
/// written anew in place (an update's swap, a rollback) is deleted and written again under
/// the store's lock, as a removal deletes it: once the lock is free, the record says which
/// it was.
async fn removed(store: &Store, name: &str, created: u64, seen: Seen) -> bool {
    if seen == Seen::Gone {
        let _ = store.lock().await;
    }
    record_gone(store, name, created)
}

fn record_gone(store: &Store, name: &str, created: u64) -> bool {
    match store.load_image(name) {
        Ok(record) => record.is_none_or(|r| r.created != created),
        // One that cannot be read now is still there.
        Err(_) => false,
    }
}

/// The exit code of the last run, for a machine that does not run when the wait begins,
/// as GetMachine says it; 0 for one that never ran.
async fn last_code(sd: &Systemd, store: &Store, record: Option<&ImageRecord>, main: &Main) -> i32 {
    match record {
        Some(record) => crate::api::machines::last_exit_code(sd, store, record)
            .await
            .unwrap_or(0),
        None if main.code != 0 => exit_code(&main.ending(), None, false),
        None => 0,
    }
}

/// Waits for `condition` and returns the exit code it ends with: that of the run that
/// ended, the program's or 128 plus the signal it died of. A removal ends any wait.
pub async fn wait(ctx: &Context, name: &str, condition: Condition) -> Result<i32> {
    let sd = ctx.sd().await?;
    let store = &ctx.store;
    let unit = format!("systemd-nspawn@{name}.service");
    // Everything is watched before anything is read: an end or a removal in between is
    // not missed.
    // A removal ends a wait of any condition, with the last exit code.
    let mut records = Some(RecordWatch::new(&store.images_dir(), name)?);
    // systemd only sends a unit's changes while someone subscribed.
    sd.subscribe().await?;
    let properties = zbus::fdo::PropertiesProxy::builder(sd.connection())
        .destination("org.freedesktop.systemd1")?
        .path(sd.unit_path(&unit).await?)?
        .build()
        .await
        .with_context(|| format!("watching {unit}"))?;
    let mut changes = properties
        .receive_properties_changed()
        .await
        .with_context(|| format!("watching {unit}"))?;
    let record = store.load_image(name)?;
    let created = match (&record, condition) {
        (Some(record), _) => record.created,
        // Removed between the call and its job.
        (None, Condition::Removed) => return Ok(0),
        // A machine machined lists without a record of nspawn's has none to watch.
        (None, _) => {
            records = None;
            0
        }
    };
    let service =
        zbus::names::InterfaceName::from_static_str_unchecked("org.freedesktop.systemd1.Service");
    let now = properties
        .get_all(service)
        .await
        .with_context(|| format!("reading how {unit} runs"))?;
    let main = Main::of(|key| now.get(key).map(|v| &**v));
    let state = sd.unit_status(&unit).await?;
    let mut runs = Runs::new(&main, &state.active, &state.sub);
    let mut last = None;
    if !runs_now(&state.active, &state.sub, &main) {
        match condition {
            Condition::NotRunning => {
                return Ok(last_code(sd, store, record.as_ref(), &main).await);
            }
            Condition::Removed => last = Some(last_code(sd, store, record.as_ref(), &main).await),
            Condition::NextExit => {}
        }
    }
    let mut stub = record.as_ref().is_some_and(ImageRecord::under_stub_init);
    // The code of an end, read as it comes: a run --rm removes the machine, and the mark
    // of the signal nspawn sent it, right after.
    let mut code_of = |ending: Ending| {
        if let Some(record) = store.load_image(name).ok().flatten() {
            stub = record.under_stub_init();
        }
        exit_code(&ending, store.last_signal(name), stub)
    };
    let mut ended = |change: zbus::fdo::PropertiesChanged| -> Option<Ending> {
        let args = change.args().ok()?;
        let changed = args.changed_properties();
        match args.interface_name().as_str() {
            "org.freedesktop.systemd1.Service" => runs.service(&Main::of(|key| changed.get(key))),
            "org.freedesktop.systemd1.Unit" => {
                let state = |key: &str| {
                    changed
                        .get(key)
                        .and_then(|v| <&str>::try_from(v).ok())
                        .unwrap_or_default()
                };
                runs.unit(state("ActiveState"), state("SubState"))
            }
            _ => None,
        }
    };
    loop {
        let removal = async {
            match records.as_mut() {
                Some(watch) => watch.next().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            biased;
            change = changes.next() => {
                let Some(change) = change else {
                    bail!("lost the signals of {unit}");
                };
                if let Some(ending) = ended(change) {
                    let code = code_of(ending);
                    if condition != Condition::Removed {
                        return Ok(code);
                    }
                    last = Some(code);
                }
            }
            seen = removal => {
                if !removed(store, name, created, seen?).await {
                    continue;
                }
                // An end said before the removal and not read yet is its last.
                while let Some(Some(change)) = changes.next().now_or_never() {
                    if let Some(ending) = ended(change) {
                        last = Some(code_of(ending));
                    }
                }
                if let Some(code) = last {
                    return Ok(code);
                }
                let code = match sd.exec_main_exit(&unit).await {
                    Ok(Some((code, status))) => exit_code(
                        &Ending { code, status, result: String::new() },
                        None,
                        stub,
                    ),
                    _ => 0,
                };
                return Ok(code);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn main(start: u64, code: i32, status: i32) -> Main {
        Main {
            start,
            code,
            status,
            result: String::new(),
        }
    }

    #[test]
    fn conditions_are_read_as_the_docker_engine_api_names_them() {
        assert_eq!(Condition::parse("").unwrap(), Condition::NotRunning);
        assert_eq!(
            Condition::parse("not-running").unwrap(),
            Condition::NotRunning
        );
        assert_eq!(Condition::parse("next-exit").unwrap(), Condition::NextExit);
        assert_eq!(Condition::parse("removed").unwrap(), Condition::Removed);
        let bad = Condition::parse("stopped").unwrap_err();
        assert_eq!(crate::api::kind_of(&bad), Some(FailureKind::Invalid));
        assert!(bad.to_string().contains("\"stopped\""), "{bad}");
    }

    #[test]
    fn a_machine_runs_from_its_start_to_the_end_of_its_main_process() {
        let alive = main(100, 0, 0);
        let ended = main(100, 1, 3);
        assert!(
            runs_now("activating", "start-pre", &ended),
            "a start under way"
        );
        assert!(
            !runs_now("activating", "auto-restart", &ended),
            "between two runs"
        );
        assert!(runs_now("active", "running", &alive));
        assert!(
            runs_now("deactivating", "stop-sigterm", &alive),
            "a stop under way"
        );
        assert!(
            !runs_now("deactivating", "stop-post", &ended),
            "its hooks after the end"
        );
        assert!(!runs_now("failed", "failed", &ended));
        assert!(!runs_now("inactive", "dead", &main(0, 0, 0)), "never ran");
    }

    #[test]
    fn each_end_is_said_once_and_an_earlier_one_never() {
        // The wait began while the run of 100 went on.
        let mut runs = Runs::new(&main(100, 0, 0), "active", "running");
        assert_eq!(runs.service(&main(100, 0, 0)), None, "it runs");
        let end = runs.service(&main(100, 1, 3)).expect("its end");
        assert_eq!((end.code, end.status), (1, 3));
        assert_eq!(
            runs.service(&main(100, 1, 3)),
            None,
            "said again, not counted"
        );
        // The next run, after a restart.
        assert_eq!(runs.service(&main(200, 0, 0)), None);
        let end = runs.service(&main(200, 2, 9)).expect("the next end");
        assert_eq!((end.code, end.status), (2, 9));

        // The wait began after the run of 100 ended: its end is not the next one.
        let mut runs = Runs::new(&main(100, 1, 0), "failed", "failed");
        assert_eq!(
            runs.service(&main(100, 1, 0)),
            None,
            "the end before the wait"
        );
        assert_eq!(runs.service(&main(300, 0, 0)), None);
        assert!(runs.service(&main(300, 1, 0)).is_some());
        // A unit never started, or forgotten by systemd.
        let mut runs = Runs::new(&main(0, 0, 0), "inactive", "dead");
        assert_eq!(runs.service(&main(0, 0, 0)), None);
        assert!(runs.service(&main(5, 1, 0)).is_some());
    }

    #[test]
    fn a_reboot_ends_the_run_only_when_the_unit_stops() {
        let mut runs = Runs::new(&main(100, 0, 0), "active", "running");
        assert_eq!(runs.service(&main(100, CLD_EXITED, REBOOT)), None);
        // systemd 254 and later pass through an inactive state before the restart.
        assert_eq!(runs.unit("inactive", "dead-before-auto-restart"), None);
        assert_eq!(
            runs.unit("activating", "auto-restart"),
            None,
            "systemd starts it again"
        );
        assert_eq!(runs.unit("activating", "start-pre"), None);
        assert_eq!(runs.service(&main(150, 0, 0)), None, "booted again");
        assert_eq!(runs.unit("activating", "start"), None);
        assert_eq!(runs.unit("active", "running"), None);
        assert_eq!(runs.service(&main(150, CLD_EXITED, REBOOT)), None);
        let end = runs
            .unit("inactive", "dead")
            .expect("the unit stopped instead");
        assert_eq!((end.code, end.status), (CLD_EXITED, REBOOT));
        assert_eq!(runs.unit("inactive", "dead"), None, "said once");
    }

    #[test]
    fn a_start_that_fails_before_its_program_ends_the_wait() {
        // The wait began while a hook before the start ran; the hook refused the start.
        let mut runs = Runs::new(&main(100, CLD_EXITED, 0), "activating", "start-pre");
        assert_eq!(runs.service(&main(100, CLD_EXITED, 0)), None);
        let end = runs.unit("failed", "failed").expect("the start failed");
        assert_eq!(exit_code(&end, None, false), 125);
        assert_eq!(runs.unit("failed", "failed"), None, "said once");

        // One whose program ran ends with the program's code, not 125.
        let mut runs = Runs::new(&main(0, 0, 0), "inactive", "dead");
        assert_eq!(runs.unit("activating", "start-pre"), None);
        assert_eq!(runs.service(&main(200, 0, 0)), None);
        assert_eq!(runs.unit("active", "running"), None);
        let end = runs.service(&main(200, CLD_EXITED, 4)).expect("its end");
        assert_eq!(exit_code(&end, None, false), 4);
        assert_eq!(runs.unit("failed", "failed"), None, "said by the service");
    }

    #[test]
    fn the_main_process_is_read_from_properties_by_name() {
        let props: HashMap<&str, Value<'_>> = HashMap::from([
            ("ExecMainStartTimestampMonotonic", Value::from(42u64)),
            ("ExecMainCode", Value::from(2i32)),
            ("ExecMainStatus", Value::from(9i32)),
            ("Result", Value::from("signal")),
            ("ExecMainPID", Value::from(7u32)),
        ]);
        let read = Main::of(|key| props.get(key));
        assert_eq!(
            read,
            Main {
                start: 42,
                code: 2,
                status: 9,
                result: "signal".to_string()
            }
        );
        assert_eq!(
            exit_code(&read.ending(), None, false),
            128 + 9,
            "killed by SIGKILL"
        );
        assert_eq!(Main::of(|_| None), Main::default());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_record_written_anew_under_the_lock_is_no_removal() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(&tmp.path().join("machines"), &tmp.path().join("state"));
        store.init().unwrap();
        let record: ImageRecord = serde_json::from_value(serde_json::json!({
            "name": "web", "reference": "docker.io/library/busybox:1.37", "manifest_digest": "sha256:m",
            "layers": [], "backend": "overlay", "created": 7,
        }))
        .unwrap();
        store.record_image(&record).unwrap();
        // An update's swap: deleted, then written again, under the lock.
        let lock = store.lock().await.unwrap();
        store.remove_record("web").unwrap();
        let store2 = Store::new(&tmp.path().join("machines"), &tmp.path().join("state"));
        let decided = tokio::spawn(async move { removed(&store2, "web", 7, Seen::Gone).await });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(!decided.is_finished(), "it waits for the lock");
        store.record_image(&record).unwrap();
        drop(lock);
        assert!(!decided.await.unwrap(), "the same machine is back");
        // A removal, and a new machine of the name, are removals.
        store.remove_record("web").unwrap();
        assert!(removed(&store, "web", 7, Seen::Gone).await);
        let newer = ImageRecord {
            created: 8,
            ..record
        };
        store.record_image(&newer).unwrap();
        assert!(removed(&store, "web", 7, Seen::Look).await);
        assert!(!removed(&store, "web", 8, Seen::Look).await);
    }

    #[tokio::test]
    async fn the_record_is_watched_for_its_removal() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let record = dir.join("web.json");
        std::fs::write(&record, "{}").unwrap();
        let mut watch = RecordWatch::new(dir, "web").unwrap();
        // Another wait shares the watch, and hears of its own record only.
        let mut other = RecordWatch::new(dir, "db").unwrap();
        let shared = RECORDS
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|(_, weak)| weak.upgrade())
            .unwrap();
        assert_eq!(shared.receiver_count(), 2, "one inotify for both");
        drop(shared);
        // Another record, and a temporary name, say nothing of this one.
        std::fs::write(dir.join("db.json"), "{}").unwrap();
        std::fs::remove_file(dir.join("db.json")).unwrap();
        let mut db = Vec::new();
        while db.last() != Some(&Seen::Gone) {
            db.push(
                tokio::time::timeout(std::time::Duration::from_secs(5), other.next())
                    .await
                    .unwrap()
                    .unwrap(),
            );
        }
        let tmp = dir.join(".tmp-web.json.1");
        std::fs::write(&tmp, "{}").unwrap();
        std::fs::rename(&tmp, &record).unwrap();
        let seen = tokio::time::timeout(std::time::Duration::from_secs(5), watch.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(seen, Seen::Look, "written anew: the record says");
        std::fs::remove_file(&record).unwrap();
        let seen = tokio::time::timeout(std::time::Duration::from_secs(5), watch.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(seen, Seen::Gone);
    }
}
