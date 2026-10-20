//! Long operations (pull, push, build, create, rm) run as jobs: the method returns the job's
//! object path at once, the job's lines arrive as JobOutput signals and its end as
//! JobRemoved, and the object keeps the outcome for whoever asks later, until it is
//! among the oldest ended ones.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use tokio::sync::mpsc;
use zbus::message::Header;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

use crate::api::{Context, Event, Report};
use crate::daemon::manager::Manager;
use crate::daemon::polkit;
use crate::daemon::State;

pub type Dict = HashMap<String, OwnedValue>;

/// How many ended jobs stay for whoever asks about them later. A client reads the outcome
/// right after JobRemoved; the rest is history, and a service that never goes idle must
/// not keep every job it ever ran.
const ENDED_KEPT: usize = 100;

/// A job that failed after doing part of its work: its Result still says what was done,
/// as a removal's `removed`.
#[derive(Debug)]
pub struct Partly {
    pub result: Dict,
    pub error: String,
}

impl std::fmt::Display for Partly {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.error)
    }
}

impl std::error::Error for Partly {}

/// One event of a job's work: a line or a note kept in its Output and sent as
/// JobOutput, a progress sent alone.
async fn forward(state: &Arc<State>, job: &Arc<JobState>, event: Event) {
    let (kind, text) = match event {
        Event::Line(t) => ("line", t),
        Event::Note(t) => ("note", t),
        // Only for whoever watches now: not kept in the job's output.
        Event::Progress { item, done, total } => {
            if let Ok(emitter) =
                state.emitter_to(crate::daemon::manager_path(), job.client.as_deref())
            {
                let _ =
                    Manager::job_progress(&emitter, job.path.as_ref(), &item, done, total).await;
            }
            return;
        }
    };
    let text = crate::daemon::values::bus_text(&text);
    {
        let mut output = job.output.lock().unwrap();
        // Lines that went out together are kept a line each; a note as it is.
        if kind == "line" {
            keep_lines(&mut output, text.split('\n'));
        } else {
            keep_lines(&mut output, std::iter::once(text.as_str()));
        }
    }
    if let Ok(emitter) = state.emitter_to(crate::daemon::manager_path(), job.client.as_deref()) {
        let _ = Manager::job_output(&emitter, job.path.as_ref(), kind, &text).await;
    }
}

/// Adds lines to what a job keeps of its output: its last lines only, as they went out as
/// JobOutput, and a long build would otherwise stay in memory as long as the job is kept.
fn keep_lines<'a>(output: &mut VecDeque<String>, lines: impl DoubleEndedIterator<Item = &'a str>) {
    let mut last: Vec<&str> = lines.rev().take(OUTPUT_KEPT).collect();
    last.reverse();
    output.extend(last.into_iter().map(str::to_string));
    let mut bytes: usize = output.iter().map(String::len).sum();
    while output.len() > OUTPUT_KEPT || (bytes > OUTPUT_KEPT_BYTES && output.len() > 1) {
        if let Some(old) = output.pop_front() {
            bytes -= old.len();
        }
    }
}

/// What did not fit in a job's queue to the bus: the job's notes, kept to go out later, and
/// how many lines were dropped.
#[derive(Default)]
struct Aside {
    dropped: AtomicU64,
    notes: Mutex<Vec<Event>>,
}

impl Aside {
    fn send(&self, tx: &mpsc::Sender<Event>, event: Event) {
        if let Err(mpsc::error::TrySendError::Full(event)) = tx.try_send(event) {
            match event {
                Event::Note(_) => self.notes.lock().unwrap().push(event),
                Event::Line(_) => {
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                }
                // The next one says as much.
                Event::Progress { .. } => {}
            }
        }
    }

    /// What goes out after `event`, taken from the queue (none once it ended): the notes
    /// kept aside and a word on the lines dropped.
    fn after(&self, event: Option<Event>) -> Vec<Event> {
        let mut events: Vec<Event> = event.into_iter().collect();
        events.append(&mut self.notes.lock().unwrap());
        let lost = self.dropped.swap(0, Ordering::Relaxed);
        if lost > 0 {
            events.push(Event::Note(format!(
                "note: {lost} lines of output were dropped: they came faster than they could be sent"
            )));
        }
        events
    }
}

/// How many lines of its output a job keeps for its Output property.
const OUTPUT_KEPT: usize = 1000;
/// Up to how many bytes of lines queued one behind the other go out in one JobOutput.
const BATCH_BYTES: usize = 64 << 10;

/// `first` with what queued right behind it, when it is a line: the lines that follow go
/// out with it, a newline between each, while they fit in `BATCH_BYTES`. A signal a line
/// cannot keep up with a program that floods its output (a build step), and its lines
/// would be dropped once the queue is full. What else comes first (a note, a progress)
/// waits in `next`.
fn gather(first: Event, rx: &mut mpsc::Receiver<Event>, next: &mut Option<Event>) -> Event {
    let Event::Line(mut text) = first else {
        return first;
    };
    while text.len() < BATCH_BYTES {
        match rx.try_recv() {
            Ok(Event::Line(more)) => {
                text.push('\n');
                text.push_str(&more);
            }
            Ok(other) => {
                *next = Some(other);
                break;
            }
            Err(_) => break,
        }
    }
    Event::Line(text)
}
/// And no more than this many bytes of them: a line of a build step may be long.
const OUTPUT_KEPT_BYTES: usize = 1 << 20;
/// Lines on their way to the bus: a program prints faster than signals go out, and past
/// these the rest is counted and dropped, or the service would fill its memory.
const IN_TRANSIT: usize = 10_000;

/// What a job carries; served as org.nspawn.Job.
pub struct JobState {
    pub path: OwnedObjectPath,
    /// The user who asked for it: nobody else reads its output or its result.
    pub owner: u32,
    /// The bus name of the client that asked for it, which alone gets its signals.
    pub client: Option<String>,
    pub kind: String,
    pub target: String,
    pub state: Mutex<String>,
    pub output: Mutex<VecDeque<String>>,
    pub error: Mutex<String>,
    pub result: Mutex<Dict>,
    /// Its end went out: only then may it be retired, its client still to read it.
    pub announced: AtomicBool,
}

#[derive(Default)]
pub struct Jobs {
    next: AtomicU64,
    all: Mutex<Vec<Arc<JobState>>>,
}

impl Jobs {
    pub fn paths(&self) -> Vec<OwnedObjectPath> {
        self.all
            .lock()
            .unwrap()
            .iter()
            .map(|j| j.path.clone())
            .collect()
    }

    /// Ends a job: its state, and its place after every other one, at once, so that jobs
    /// are retired in the order they ended and never one at its starting place.
    fn end(&self, job: &Arc<JobState>, state: &str) {
        let mut all = self.all.lock().unwrap();
        *job.state.lock().unwrap() = state.to_string();
        if let Some(i) = all.iter().position(|j| Arc::ptr_eq(j, job)) {
            let job = all.remove(i);
            all.push(job);
        }
    }

    /// Forgets the ended jobs that ended first beyond `ENDED_KEPT`; their paths, for the
    /// object server to drop. One whose end has not gone out yet stays.
    fn retire(&self) -> Vec<OwnedObjectPath> {
        let mut all = self.all.lock().unwrap();
        let ended = |job: &JobState| {
            *job.state.lock().unwrap() != "running" && job.announced.load(Ordering::SeqCst)
        };
        let mut excess = all
            .iter()
            .filter(|j| ended(j))
            .count()
            .saturating_sub(ENDED_KEPT);
        let mut retired = Vec::new();
        all.retain(|job| {
            if excess > 0 && ended(job) {
                excess -= 1;
                retired.push(job.path.clone());
                false
            } else {
                true
            }
        });
        retired
    }
}

pub struct Job {
    job: Arc<JobState>,
    state: Arc<State>,
}

impl Job {
    /// Fails for anyone but the user who started the job, and root.
    async fn readable(&self, header: Option<&Header<'_>>) -> zbus::fdo::Result<()> {
        let uid = polkit::reader(self.state.connection(), header).await;
        if polkit::may_read(self.job.owner, uid) {
            return Ok(());
        }
        Err(zbus::fdo::Error::AccessDenied(format!(
            "this job belongs to another user ({})",
            self.job.owner
        )))
    }
}

#[zbus::interface(name = "org.nspawn.Job")]
impl Job {
    /// What it does: pull, create, push, build, rm, cp and the others DBUS.md lists.
    #[zbus(property(emits_changed_signal = "const"))]
    async fn kind(&self, #[zbus(header)] hdr: Option<Header<'_>>) -> zbus::fdo::Result<String> {
        self.readable(hdr.as_ref()).await?;
        Ok(self.job.kind.clone())
    }

    /// The image or machine the job is about.
    #[zbus(property(emits_changed_signal = "const"))]
    async fn target(&self, #[zbus(header)] hdr: Option<Header<'_>>) -> zbus::fdo::Result<String> {
        self.readable(hdr.as_ref()).await?;
        Ok(self.job.target.clone())
    }

    /// "running", "done" or "failed".
    #[zbus(property)]
    async fn state(&self, #[zbus(header)] hdr: Option<Header<'_>>) -> zbus::fdo::Result<String> {
        self.readable(hdr.as_ref()).await?;
        Ok(self.job.state.lock().unwrap().clone())
    }

    /// The last lines the job said: they go out as JobOutput as they come, not as
    /// PropertiesChanged.
    #[zbus(property(emits_changed_signal = "false"))]
    async fn output(
        &self,
        #[zbus(header)] hdr: Option<Header<'_>>,
    ) -> zbus::fdo::Result<Vec<String>> {
        self.readable(hdr.as_ref()).await?;
        Ok(self.job.output.lock().unwrap().iter().cloned().collect())
    }

    /// Why it failed, when it did.
    #[zbus(property)]
    async fn error(&self, #[zbus(header)] hdr: Option<Header<'_>>) -> zbus::fdo::Result<String> {
        self.readable(hdr.as_ref()).await?;
        Ok(self.job.error.lock().unwrap().clone())
    }

    /// What it produced, when it is done; what a removal did before it failed.
    #[zbus(property)]
    async fn result(&self, #[zbus(header)] hdr: Option<Header<'_>>) -> zbus::fdo::Result<Dict> {
        self.readable(hdr.as_ref()).await?;
        Ok(self.job.result.lock().unwrap().clone())
    }
}

/// Starts `work` as a job of `kind` on `target` and returns its object path. `work` gets
/// the library context and a reporter whose events become the job's output.
pub async fn spawn<F, Fut>(
    state: &Arc<State>,
    owner: polkit::Caller,
    ctx: Arc<Context>,
    kind: &str,
    target: &str,
    work: F,
) -> zbus::Result<OwnedObjectPath>
where
    F: FnOnce(Arc<Context>, Arc<dyn Fn(Event) + Send + Sync>) -> Fut + Send + 'static,
    Fut: Future<Output = Result<Dict>> + Send + 'static,
{
    let id = state.jobs.next.fetch_add(1, Ordering::SeqCst) + 1;
    let path = OwnedObjectPath::try_from(format!("/org/nspawn/job/{id}"))?;
    let job = Arc::new(JobState {
        path: path.clone(),
        owner: owner.uid,
        client: owner.name,
        kind: kind.to_string(),
        target: target.to_string(),
        state: Mutex::new("running".to_string()),
        output: Mutex::default(),
        error: Mutex::new(String::new()),
        result: Mutex::new(Dict::new()),
        announced: AtomicBool::new(false),
    });
    state.jobs.all.lock().unwrap().push(job.clone());
    state
        .connection()
        .object_server()
        .at(
            &path,
            Job {
                job: job.clone(),
                state: state.clone(),
            },
        )
        .await?;

    let (tx, mut rx) = mpsc::channel::<Event>(IN_TRANSIT);
    let aside = Arc::new(Aside::default());
    let reporter: Arc<dyn Fn(Event) + Send + Sync> = {
        let aside = aside.clone();
        Arc::new(move |event| aside.send(&tx, event))
    };
    // Lines are relayed as they come; the job's own task does the work.
    let forwarder = {
        let state = state.clone();
        let job = job.clone();
        tokio::spawn(async move {
            let mut next = None;
            loop {
                let event = match next.take() {
                    Some(event) => Some(event),
                    None => rx.recv().await,
                };
                let ended = event.is_none();
                let event = event.map(|event| gather(event, &mut rx, &mut next));
                for event in aside.after(event) {
                    forward(&state, &job, event).await;
                }
                if ended {
                    break;
                }
            }
        })
    };
    let state = state.clone();
    // The service is busy until the job's end has been announced: a client is about to
    // read the outcome, and must not find a fresh service without this object.
    let busy = state.enter();
    tokio::spawn(async move {
        let outcome = work(ctx, reporter).await;
        // The reporter is gone with `work`; the forwarder ends once the channel drains.
        let _ = forwarder.await;
        let result = match outcome {
            Ok(dict) => {
                *job.result.lock().unwrap() = dict;
                state.jobs.end(&job, "done");
                "done"
            }
            Err(e) => {
                let e = match e.downcast::<Partly>() {
                    Ok(partly) => {
                        *job.result.lock().unwrap() = partly.result;
                        anyhow::anyhow!(partly.error)
                    }
                    Err(e) => e,
                };
                *job.error.lock().unwrap() = crate::daemon::values::bus_text(&format!("{e:#}"));
                state.jobs.end(&job, "failed");
                "failed"
            }
        };
        if let Ok(iface) = state
            .connection()
            .object_server()
            .interface::<_, Job>(&job.path)
            .await
        {
            if let Ok(emitter) = state.emitter_to(job.path.as_ref(), job.client.as_deref()) {
                let job = iface.get().await;
                let _ = job.state_changed(&emitter).await;
                let _ = job.error_changed(&emitter).await;
                let _ = job.result_changed(&emitter).await;
            }
        }
        if let Ok(emitter) = state.emitter_to(crate::daemon::manager_path(), job.client.as_deref())
        {
            let _ = Manager::job_removed(&emitter, job.path.as_ref(), result).await;
        }
        job.announced.store(true, Ordering::SeqCst);
        for path in state.jobs.retire() {
            let _ = state
                .connection()
                .object_server()
                .remove::<Job, _>(&path)
                .await;
        }
        drop(busy);
    });
    Ok(path)
}

/// Reporter to Report, for the library's signatures.
pub fn report(reporter: &Arc<dyn Fn(Event) + Send + Sync>) -> Report<'_> {
    &**reporter
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: u64, state: &str) -> Arc<JobState> {
        Arc::new(JobState {
            path: OwnedObjectPath::try_from(format!("/org/nspawn/job/{id}")).unwrap(),
            owner: 0,
            client: None,
            kind: "pull".to_string(),
            target: format!("image-{id}"),
            state: Mutex::new(state.to_string()),
            output: Mutex::default(),
            error: Mutex::new(String::new()),
            result: Mutex::new(Dict::new()),
            announced: AtomicBool::new(state != "running"),
        })
    }

    #[test]
    fn ended_jobs_are_kept_up_to_a_point_and_running_ones_always() {
        let jobs = Jobs::default();
        // Every tenth job still runs; the rest ended, half of them badly.
        let total = ENDED_KEPT as u64 + 30;
        let running = (total / 10) as usize;
        for id in 1..=total {
            let state = if id.is_multiple_of(10) {
                "running"
            } else if id.is_multiple_of(2) {
                "done"
            } else {
                "failed"
            };
            jobs.all.lock().unwrap().push(job(id, state));
        }
        let path = |id: u64| format!("/org/nspawn/job/{id}");
        let retired = jobs.retire();
        let expected: Vec<String> = (1..=total)
            .filter(|id| !id.is_multiple_of(10))
            .take(total as usize - running - ENDED_KEPT)
            .map(path)
            .collect();
        assert_eq!(
            retired.iter().map(|p| p.as_str()).collect::<Vec<_>>(),
            expected,
            "the oldest ended ones, never a running one"
        );
        let kept = jobs.paths();
        assert_eq!(kept.len(), ENDED_KEPT + running);
        assert!(
            kept.iter().any(|p| p.as_str() == path(10)),
            "the oldest running one"
        );
        assert!(kept.iter().any(|p| p.as_str() == path(total)));
        // A job made first that ends last stays: retirement goes by the end, and not
        // before its end went out.
        let first = job(0, "running");
        jobs.all.lock().unwrap().insert(0, first.clone());
        jobs.end(&first, "done");
        jobs.all.lock().unwrap().push(job(total + 1, "done"));
        let retired = jobs.retire();
        assert!(
            !retired.iter().any(|p| p.as_str() == path(0)),
            "{retired:?}"
        );
        first.announced.store(true, Ordering::SeqCst);
        assert!(
            !jobs.retire().iter().any(|p| p.as_str() == path(0)),
            "it ended last"
        );
        assert!(jobs.retire().is_empty(), "nothing more to retire");
    }

    #[test]
    fn a_job_keeps_its_last_lines() {
        let mut output = VecDeque::new();
        let one = |output: &mut VecDeque<String>, text: &str| {
            keep_lines(output, std::iter::once(text));
        };
        for i in 0..OUTPUT_KEPT + 5 {
            one(&mut output, &i.to_string());
        }
        assert_eq!(output.len(), OUTPUT_KEPT);
        assert_eq!(output[0], "5");
        let long = "x".repeat(OUTPUT_KEPT_BYTES / 2 + 1);
        one(&mut output, &long);
        assert_eq!(output.len(), OUTPUT_KEPT);
        one(&mut output, &long);
        assert_eq!(output, std::slice::from_ref(&long));
        // The last line stays, however long.
        let huge = "y".repeat(OUTPUT_KEPT_BYTES * 2);
        one(&mut output, &huge);
        assert_eq!(output, [huge]);
        // Lines that went out together count one by one.
        let batch: String = (0..OUTPUT_KEPT + 3).map(|i| format!("b{i}\n")).collect();
        keep_lines(&mut output, batch.trim_end().split('\n'));
        assert_eq!(output.len(), OUTPUT_KEPT);
        assert_eq!(output[0], "b3");
        assert_eq!(output[OUTPUT_KEPT - 1], format!("b{}", OUTPUT_KEPT + 2));
    }

    #[test]
    fn lines_queued_one_behind_the_other_go_out_together() {
        let text = |event: &Event| match event {
            Event::Line(t) | Event::Note(t) => t.clone(),
            Event::Progress { .. } => "progress".to_string(),
        };
        let (tx, mut rx) = mpsc::channel::<Event>(100);
        for event in [
            Event::Line("line 1".to_string()),
            Event::Line("line 2".to_string()),
            Event::Note("note: one".to_string()),
            Event::Line("line 3".to_string()),
        ] {
            tx.try_send(event).unwrap();
        }
        let mut next = None;
        let batch = gather(Event::Line("line 0".to_string()), &mut rx, &mut next);
        assert_eq!(text(&batch), "line 0\nline 1\nline 2");
        // The note stops the batch and goes next, alone.
        let note = next.take().expect("the note waits");
        assert_eq!(text(&gather(note, &mut rx, &mut next)), "note: one");
        assert!(next.is_none());
        let first = rx.try_recv().unwrap();
        assert_eq!(text(&gather(first, &mut rx, &mut next)), "line 3");
        // A batch grows past BATCH_BYTES by one line at most; the rest waits its turn.
        let kib = "k".repeat(1023);
        for _ in 0..80 {
            tx.try_send(Event::Line(kib.clone())).unwrap();
        }
        let first = rx.try_recv().unwrap();
        let batch = text(&gather(first, &mut rx, &mut next));
        assert!(
            batch.len() >= BATCH_BYTES && batch.len() <= BATCH_BYTES + kib.len(),
            "{}",
            batch.len()
        );
        assert_eq!(batch.split('\n').count(), 65);
        assert!(
            rx.try_recv().is_ok(),
            "the lines past the batch stay queued"
        );
    }

    #[test]
    fn a_full_queue_drops_lines_and_keeps_notes() {
        let text = |event: Event| match event {
            Event::Line(t) | Event::Note(t) => t,
            Event::Progress { .. } => "progress".to_string(),
        };
        let (tx, mut rx) = mpsc::channel::<Event>(2);
        let aside = Aside::default();
        for i in 0..5 {
            aside.send(&tx, Event::Line(format!("line {i}")));
        }
        aside.send(&tx, Event::Note("note: kept".to_string()));
        aside.send(
            &tx,
            Event::Progress {
                item: "1cfa4e2b09e1".to_string(),
                done: 1,
                total: 2,
            },
        );
        let first = rx.try_recv().ok();
        let out: Vec<String> = aside.after(first).into_iter().map(text).collect();
        assert_eq!(
            out,
            [
                "line 0",
                "note: kept",
                "note: 3 lines of output were dropped: they came faster than they could be sent"
            ]
        );
        assert_eq!(rx.try_recv().ok().map(text).as_deref(), Some("line 1"));
        assert!(rx.try_recv().is_err());
        assert!(aside.after(None).is_empty(), "nothing more");
    }
}
