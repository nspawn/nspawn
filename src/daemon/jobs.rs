//! Long operations (pull, push, build, create, rm) run as jobs: the method returns the job's
//! object path at once, the job's lines arrive as JobOutput signals and its end as
//! JobRemoved, and the object keeps the outcome for whoever asks later, until it is
//! among the oldest ended ones.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
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
    pub output: Mutex<Vec<String>>,
    pub error: Mutex<String>,
    pub result: Mutex<Dict>,
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

    /// Forgets the oldest ended jobs beyond `ENDED_KEPT`; their paths, for the object
    /// server to drop.
    fn retire(&self) -> Vec<OwnedObjectPath> {
        let mut all = self.all.lock().unwrap();
        let ended = |job: &JobState| *job.state.lock().unwrap() != "running";
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
    /// "pull", "push", "build", "create" or "rm".
    #[zbus(property)]
    async fn kind(&self, #[zbus(header)] hdr: Option<Header<'_>>) -> zbus::fdo::Result<String> {
        self.readable(hdr.as_ref()).await?;
        Ok(self.job.kind.clone())
    }

    /// The image or machine the job is about.
    #[zbus(property)]
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

    /// Every line the job said so far.
    #[zbus(property)]
    async fn output(
        &self,
        #[zbus(header)] hdr: Option<Header<'_>>,
    ) -> zbus::fdo::Result<Vec<String>> {
        self.readable(hdr.as_ref()).await?;
        Ok(self.job.output.lock().unwrap().clone())
    }

    /// Why it failed, when it did.
    #[zbus(property)]
    async fn error(&self, #[zbus(header)] hdr: Option<Header<'_>>) -> zbus::fdo::Result<String> {
        self.readable(hdr.as_ref()).await?;
        Ok(self.job.error.lock().unwrap().clone())
    }

    /// What it produced, when it is done.
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
        output: Mutex::new(Vec::new()),
        error: Mutex::new(String::new()),
        result: Mutex::new(Dict::new()),
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

    let (tx, mut rx) = mpsc::unbounded_channel::<Event>();
    let reporter: Arc<dyn Fn(Event) + Send + Sync> = Arc::new(move |event| {
        let _ = tx.send(event);
    });
    // Lines are relayed as they come; the job's own task does the work.
    let forwarder = {
        let state = state.clone();
        let job = job.clone();
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                let (kind, text) = match event {
                    Event::Line(t) => ("line", t),
                    Event::Note(t) => ("note", t),
                    // Only for whoever watches now: not kept in the job's output.
                    Event::Progress { item, done, total } => {
                        if let Ok(emitter) =
                            state.emitter_to(crate::daemon::manager_path(), job.client.as_deref())
                        {
                            let _ = Manager::job_progress(
                                &emitter,
                                job.path.as_ref(),
                                &item,
                                done,
                                total,
                            )
                            .await;
                        }
                        continue;
                    }
                };
                job.output.lock().unwrap().push(text.clone());
                if let Ok(emitter) =
                    state.emitter_to(crate::daemon::manager_path(), job.client.as_deref())
                {
                    let _ = Manager::job_output(&emitter, job.path.as_ref(), kind, &text).await;
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
                *job.state.lock().unwrap() = "done".to_string();
                "done"
            }
            Err(e) => {
                *job.error.lock().unwrap() = format!("{e:#}");
                *job.state.lock().unwrap() = "failed".to_string();
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
                let _ = iface.get().await.state_changed(&emitter).await;
            }
        }
        if let Ok(emitter) = state.emitter_to(crate::daemon::manager_path(), job.client.as_deref())
        {
            let _ = Manager::job_removed(&emitter, job.path.as_ref(), result).await;
        }
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
            output: Mutex::new(Vec::new()),
            error: Mutex::new(String::new()),
            result: Mutex::new(Dict::new()),
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
        assert!(jobs.retire().is_empty(), "nothing more to retire");
    }
}
