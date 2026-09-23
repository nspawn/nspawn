//! Long operations (pull, push, build, create, rm) run as jobs: the method returns the job's
//! object path at once, the job's lines arrive as JobOutput signals and its end as
//! JobRemoved, and the object keeps the outcome for whoever asks later.

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

/// What a job carries; served as org.nspawn.Job.
pub struct JobState {
    pub path: OwnedObjectPath,
    /// The user who asked for it: nobody else reads its output or its result.
    pub owner: u32,
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
}

pub struct Job {
    job: Arc<JobState>,
    state: Arc<State>,
}

impl Job {
    /// Fails for anyone but the user who started the job, and root.
    async fn readable(&self, header: Option<&Header<'_>>) -> zbus::fdo::Result<()> {
        let uid = polkit::header_uid(self.state.connection(), header).await;
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
    owner: u32,
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
        owner,
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
                };
                job.output.lock().unwrap().push(text.clone());
                if let Ok(emitter) = state.emitter() {
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
            let emitter = iface.signal_emitter();
            let _ = iface.get().await.state_changed(emitter).await;
        }
        if let Ok(emitter) = state.emitter() {
            let _ = Manager::job_removed(&emitter, job.path.as_ref(), result).await;
        }
        drop(busy);
    });
    Ok(path)
}

/// Reporter to Report, for the library's signatures.
pub fn report(reporter: &Arc<dyn Fn(Event) + Send + Sync>) -> Report<'_> {
    &**reporter
}
