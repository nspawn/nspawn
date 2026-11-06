//! POST /images/create with fromImage: a pull through the service, as `nspawn pull`
//! makes one, answered with the engine's progress messages. The registry is asked for
//! the tag's digest first, and an image that has it already is up to date and is not
//! pulled again. A pull goes to the record of the reference that never ran, which it
//! replaces, or else to a new record under a name of its own, as compose's pulls do: a
//! container keeps the image it was made from. The credentials are those of
//! `nspawn login`.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use bytes::Bytes;
use hyper::{Response, StatusCode};
use serde_json::{json, Value};
use zbus::zvariant::{OwnedObjectPath, Value as Bus};

use super::containers::{check_platform, status_of, Answer};
use super::images::{by_digest, image_of, pinned, pulled_digest, tagged};
use super::util::familiar;
use super::{Body, Query};
use crate::api::Context;
use crate::client::{self, Client, JobEvent, JobWatch, Options};
use crate::reference::{free_name, ImageRef};
use crate::store::ImageRecord;

fn says(status: StatusCode, text: impl Into<String>) -> Answer {
    Answer(status, Some(text.into()))
}

/// The reference a pull of `from_image` and `tag` names, read as the engine reads them:
/// `tag` is a tag or a digest, in place of what `from_image` gives.
pub fn pull_reference(from_image: &str, tag: &str) -> Result<ImageRef, Answer> {
    let bad = |e: anyhow::Error| {
        says(
            StatusCode::BAD_REQUEST,
            format!("invalid reference format: {e:#}"),
        )
    };
    let last = from_image.rsplit('/').next().unwrap_or_default();
    if tag.is_empty() && !from_image.contains('@') && !last.contains(':') {
        return Err(says(
            StatusCode::NOT_IMPLEMENTED,
            "pulling every tag of a repository is not supported by nspawn's Docker API; name a tag",
        ));
    }
    let base = ImageRef::normalized(from_image).map_err(bad)?;
    if tag.is_empty() {
        // A digest names the image, and the pull keeps it as such.
        return Ok(by_digest(base));
    }
    let text = if tag.contains(':') {
        format!("{}/{}@{tag}", base.registry, base.repository)
    } else {
        let digest = base
            .digest
            .as_ref()
            .map_or(String::new(), |d| format!("@{d}"));
        format!("{}/{}:{tag}{digest}", base.registry, base.repository)
    };
    ImageRef::parse(&text, &base.registry)
        .map(by_digest)
        .map_err(bad)
}

/// The engine's first message of a pull: the repository, and the tag or digest as its
/// ID.
pub fn pulling(wanted: &ImageRef) -> Value {
    let id = wanted
        .digest
        .clone()
        .or_else(|| wanted.tag.clone())
        .unwrap_or_default();
    json!({ "status": format!("Pulling from {}", wanted.repository), "id": id })
}

/// The engine's message for what a pull job says: the steps of a blob under its short
/// digest, in the words the engine has for them, the rest as it is.
pub fn progress_message(event: JobEvent<'_>) -> Value {
    match event {
        JobEvent::Progress { item, done, total } => json!({
            "status": "Downloading",
            "progressDetail": { "current": done, "total": total },
            "id": item,
        }),
        JobEvent::Line { line, .. } => {
            let blob = line
                .strip_prefix("blob ")
                .and_then(|rest| rest.split_once(": "));
            match blob {
                Some((id, what)) => {
                    let status = match what {
                        "downloading" => "Pulling fs layer",
                        "downloaded" => "Download complete",
                        "already present" => "Already exists",
                        other => other,
                    };
                    json!({ "status": status, "progressDetail": {}, "id": id })
                }
                None => json!({ "status": line }),
            }
        }
    }
}

/// How long a pull's stream goes without a word before the last progress is said again.
const QUIET: std::time::Duration = std::time::Duration::from_secs(10);

/// What a pull's stream said last with a progress detail, and when it last said anything.
struct Said {
    last: Bytes,
    at: Instant,
}

impl Said {
    /// Before any progress: a wait under the id of `first`, the pull's first message.
    fn new(first: &Value, now: Instant) -> Said {
        Said {
            last: line(&json!({ "status": "Waiting", "progressDetail": {}, "id": first["id"] })),
            at: now,
        }
    }

    fn note(&mut self, message: &Value, said: &Bytes, now: Instant) {
        self.at = now;
        if message.get("progressDetail").is_some() {
            self.last = said.clone();
        }
    }

    /// The last progress again, once the stream has been quiet for `QUIET`.
    fn again(&mut self, now: Instant) -> Option<Bytes> {
        if now.duration_since(self.at) < QUIET {
            return None;
        }
        self.at = now;
        Some(self.last.clone())
    }
}

/// The engine's message for a pull that failed once its messages began.
pub fn failure_message(message: &str) -> Value {
    json!({ "errorDetail": { "message": message }, "error": message })
}

fn line(value: &Value) -> Bytes {
    // The engine ends each message so.
    Bytes::from(format!("{value}\r\n"))
}

/// Where a pull of `full` goes: the last pulled record of the reference that is an
/// image, which it replaces, or a free name made of the reference.
pub fn destination(
    records: &[ImageRecord],
    full: &str,
    registry: &str,
    is_image: impl Fn(&ImageRecord) -> bool,
    base: &str,
    taken: &BTreeSet<String>,
) -> (String, bool) {
    match image_of(records, full, registry, is_image) {
        Some(record) => (record.name.clone(), true),
        None => (free_name(base, taken), false),
    }
}

fn failed(e: impl std::fmt::Display) -> Answer {
    says(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

/// An error of the bus as an answer, its status by its name.
fn refused(e: zbus::Error) -> Answer {
    let e = client::error(e);
    says(status_of(client::error_name(&e)), format!("{e:#}"))
}

pub async fn create(ctx: &Arc<Context>, query: &Query) -> Response<Body> {
    pull(ctx, query).await.unwrap_or_else(Answer::respond)
}

/// What a pull finds of its reference here: the digest the reference names, and where
/// a new image of it goes.
struct Here {
    current: Option<String>,
    into: String,
    /// `into` is an image of the reference, which the pull replaces.
    replace: bool,
}

/// The records as they are now, and the names machined lists besides them.
async fn here(ctx: &Context, wanted: &ImageRef) -> anyhow::Result<Here> {
    let full = wanted.to_string();
    let registry = &ctx.config.registry;
    let records = ctx.store.list_images()?;
    let is_image = |r: &ImageRecord| crate::api::disk::image_only(&ctx.store, r);
    let current = match &wanted.digest {
        // The digest decides, whatever tag comes with it.
        Some(digest) => {
            let by_digest = ImageRef {
                tag: None,
                ..wanted.clone()
            }
            .to_string();
            pinned(&records, &by_digest, registry).map(|_| digest.clone())
        }
        None => tagged(&records, &full, registry, is_image).map(|r| pulled_digest(r).to_string()),
    };
    let mut taken: BTreeSet<String> = records.iter().map(|r| r.name.clone()).collect();
    taken.extend(
        ctx.sd()
            .await?
            .list_images()
            .await?
            .into_iter()
            .map(|i| i.name),
    );
    let (into, replace) = destination(
        &records,
        &full,
        registry,
        is_image,
        &wanted.local_name(),
        &taken,
    );
    Ok(Here {
        current,
        into,
        replace,
    })
}

/// PullImage of `full` into `into`, replacing it only while it is an image that never
/// ran.
async fn start_pull(
    client: &Client,
    full: &str,
    into: &str,
    replace: bool,
) -> Result<(JobWatch, OwnedObjectPath), Answer> {
    let mut options = Options::new();
    options.insert("name", Bus::from(into.to_string()));
    options.insert("force", Bus::from(replace));
    options.insert("image_only", Bus::from(replace));
    let watch = client
        .watch_jobs_quiet(true)
        .await
        .map_err(|e| failed(format!("{e:#}")))?;
    let job = client
        .manager
        .pull_image(full, options)
        .await
        .map_err(refused)?;
    Ok((watch, job))
}

/// The engine's messages for what a pull job says, one line at a time: the service
/// sends lines that come together as one.
pub fn progress_messages(event: JobEvent<'_>) -> Vec<Value> {
    match event {
        JobEvent::Line { kind, line } => line
            .split('\n')
            .filter(|part| !part.is_empty())
            .map(|part| progress_message(JobEvent::Line { kind, line: part }))
            .collect(),
        progress => vec![progress_message(progress)],
    }
}

async fn pull(ctx: &Arc<Context>, query: &Query) -> Result<Response<Body>, Answer> {
    let from_image = query.get("fromImage").unwrap_or_default();
    if from_image.is_empty() || !query.get("fromSrc").unwrap_or_default().is_empty() {
        return Err(says(
            StatusCode::NOT_IMPLEMENTED,
            "importing an image is not supported by nspawn's Docker API",
        ));
    }
    let wanted = pull_reference(from_image, query.get("tag").unwrap_or_default())?;
    check_platform(query.get("platform").unwrap_or_default())?;
    let full = wanted.to_string();
    let shown = familiar(&full);
    let client = Client::connect().await.map_err(failed)?;
    let found = here(ctx, &wanted)
        .await
        .map_err(|e| failed(format!("{e:#}")))?;
    // What the reference names on its registry now. Asked before anything is sent, so
    // that one the registry does not have answers 404; a digest that is here already is
    // not asked for.
    let remote = match (&wanted.digest, &found.current) {
        (Some(digest), Some(current)) if current == digest => digest.clone(),
        _ => client
            .manager
            .image_digest(&full, Options::new())
            .await
            .map_err(refused)?,
    };
    let remote = wanted.digest.clone().unwrap_or(remote);
    let first = pulling(&wanted);
    if found.current.as_deref() == Some(remote.as_str()) {
        let messages = [
            first,
            json!({ "status": format!("Digest: {remote}") }),
            json!({ "status": format!("Status: Image is up to date for {shown}") }),
        ];
        let body: Vec<u8> = messages.iter().flat_map(|m| line(m).to_vec()).collect();
        return Ok(super::respond(StatusCode::OK, "application/json", body));
    }
    let (watch, job) = start_pull(&client, &full, &found.into, found.replace).await?;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Bytes>();
    let _ = tx.send(line(&first));
    let ctx = ctx.clone();
    // The last word with a progress detail, said again while the job is quiet: a pull
    // waits for the store and extracts its layers without one, and clients take a long
    // silence for a pull that hangs.
    let progress = Arc::new(Mutex::new(Said::new(&first, Instant::now())));
    let keeping = {
        let (tx, progress) = (tx.clone(), progress.clone());
        super::attach::Aborted(tokio::spawn(async move {
            loop {
                tokio::time::sleep(QUIET).await;
                let again = progress.lock().unwrap().again(Instant::now());
                if let Some(again) = again {
                    if tx.send(again).is_err() {
                        return;
                    }
                }
            }
        }))
    };
    let gone = tx.clone();
    let work = async move {
        let _keeping = keeping;
        let say = |event: JobEvent<'_>| {
            for message in progress_messages(event) {
                let said = line(&message);
                progress
                    .lock()
                    .unwrap()
                    .note(&message, &said, Instant::now());
                let _ = tx.send(said);
            }
        };
        let mut into = found.into.clone();
        let mut outcome = watch.follow(job, say).await;
        // Another pull, or a start of the image it was to replace, took the name
        // meanwhile: the records say what to do now, once.
        let taken = matches!(&outcome, Err(e) if client::error_name(e) == Some("Exists"));
        if taken {
            if let Ok(again) = here(&ctx, &wanted).await {
                if again.current.as_deref() == Some(remote.as_str()) {
                    outcome = Ok(Default::default());
                    into = String::new();
                } else if again.into != into {
                    into = again.into.clone();
                    outcome = match start_pull(&client, &full, &again.into, again.replace).await {
                        Ok((watch, job)) => watch.follow(job, say).await,
                        Err(Answer(_, text)) => Err(anyhow::anyhow!(text.unwrap_or_default())),
                    };
                }
            }
        }
        let messages = match outcome {
            Ok(_) => {
                let digest = ctx
                    .store
                    .load_image(&into)
                    .ok()
                    .flatten()
                    .map_or(remote, |r| pulled_digest(&r).to_string());
                let word = if found.current.as_deref() == Some(digest.as_str()) {
                    "Image is up to date for"
                } else {
                    "Downloaded newer image for"
                };
                vec![
                    json!({ "status": format!("Digest: {digest}") }),
                    json!({ "status": format!("Status: {word} {shown}") }),
                ]
            }
            Err(e) => vec![failure_message(&format!("{e:#}"))],
        };
        for message in messages {
            let _ = tx.send(line(&message));
        }
    };
    // A client that went takes nothing of this server with it: the pull goes on in the
    // service without a follower here.
    tokio::spawn(async move {
        tokio::select! {
            () = work => {}
            () = gone.closed() => {}
        }
    });
    Ok(super::streaming_from(rx, "application/json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quiet_pull_says_its_last_progress_again() {
        let start = Instant::now();
        let first = json!({ "status": "Pulling from library/busybox", "id": "1.37" });
        let mut said = Said::new(&first, start);
        assert_eq!(said.again(start + QUIET / 2), None);
        let waiting = said.again(start + QUIET).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&waiting).unwrap(),
            json!({ "status": "Waiting", "progressDetail": {}, "id": "1.37" })
        );
        let blob = progress_message(JobEvent::Line {
            kind: "line",
            line: "blob 0123456789ab: downloaded",
        });
        let later = start + QUIET * 2;
        said.note(&blob, &line(&blob), later);
        // A word without a progress detail counts as a word, and is not said again.
        let plain = json!({ "status": "Digest: sha256:00" });
        said.note(&plain, &line(&plain), later + QUIET / 2);
        assert_eq!(said.again(later + QUIET), None);
        assert_eq!(said.again(later + QUIET * 2).unwrap(), line(&blob));
    }

    fn record(name: &str, reference: &str, created: u64) -> ImageRecord {
        serde_json::from_value(json!({
            "name": name,
            "reference": reference,
            "manifest_digest": format!("sha256:{}", "a".repeat(64)),
            "layers": [],
            "backend": "overlay",
            "created": created,
        }))
        .unwrap()
    }

    #[test]
    fn a_pull_names_its_image_as_the_engine_reads_from_image_and_tag() {
        let full = |from: &str, tag: &str| pull_reference(from, tag).map(|r| r.to_string());
        assert_eq!(
            full("alpine", "3.20").unwrap(),
            "docker.io/library/alpine:3.20"
        );
        assert_eq!(
            full(
                "registry.gitlab.com/garuda-linux/tools/chaotic-manager/builder",
                "latest"
            )
            .unwrap(),
            "registry.gitlab.com/garuda-linux/tools/chaotic-manager/builder:latest"
        );
        // The tag given apart wins over the one in the name.
        assert_eq!(
            full("alpine:3.19", "3.20").unwrap(),
            "docker.io/library/alpine:3.20"
        );
        assert_eq!(
            full("alpine:3.20", "").unwrap(),
            "docker.io/library/alpine:3.20"
        );
        assert_eq!(
            full("localhost:5000/app:1", "").unwrap(),
            "localhost:5000/app:1"
        );
        let digest = format!("sha256:{}", "b".repeat(64));
        assert_eq!(
            full("alpine", &digest).unwrap(),
            format!("docker.io/library/alpine@{digest}")
        );
        assert_eq!(
            full(&format!("alpine@{digest}"), "").unwrap(),
            format!("docker.io/library/alpine@{digest}")
        );
        // A tag beside a digest is left out, as the engine keeps the digest alone.
        assert_eq!(
            full(&format!("postgres:16@{digest}"), "").unwrap(),
            format!("docker.io/library/postgres@{digest}")
        );
        // A repository without a tag is all of its tags.
        for every in ["alpine", "localhost:5000/app"] {
            let Answer(status, text) = pull_reference(every, "").unwrap_err();
            assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{every}");
            assert!(text.unwrap().contains("not supported"), "{every}");
        }
        let Answer(status, _) = pull_reference("Alpine Linux", "1").unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn a_pull_says_what_it_does_in_the_engine_s_words() {
        let wanted = pull_reference("alpine", "3.20").unwrap();
        assert_eq!(
            pulling(&wanted),
            json!({ "status": "Pulling from library/alpine", "id": "3.20" })
        );
        let said = |line: &str| progress_message(JobEvent::Line { kind: "line", line });
        assert_eq!(
            said("blob 1cfa4e2b09e1: downloading"),
            json!({ "status": "Pulling fs layer", "progressDetail": {}, "id": "1cfa4e2b09e1" })
        );
        assert_eq!(
            said("blob 1cfa4e2b09e1: downloaded")["status"],
            "Download complete"
        );
        assert_eq!(
            said("blob 1cfa4e2b09e1: already present")["status"],
            "Already exists"
        );
        assert_eq!(
            said("docker.io/library/alpine:3.20: manifest 1a2b with 1 layer(s)"),
            json!({ "status": "docker.io/library/alpine:3.20: manifest 1a2b with 1 layer(s)" })
        );
        assert_eq!(
            progress_message(JobEvent::Progress {
                item: "1cfa4e2b09e1",
                done: 5,
                total: 10
            }),
            json!({
                "status": "Downloading",
                "progressDetail": { "current": 5, "total": 10 },
                "id": "1cfa4e2b09e1",
            })
        );
        assert_eq!(
            failure_message("no such manifest"),
            json!({ "errorDetail": { "message": "no such manifest" }, "error": "no such manifest" })
        );
        assert_eq!(
            line(&json!({ "status": "x" })),
            Bytes::from("{\"status\":\"x\"}\r\n")
        );
    }

    #[test]
    fn lines_the_service_sends_together_are_said_one_by_one() {
        let lines = "docker.io/library/alpine:3.20: manifest 1a2b with 2 layer(s)\nblob aaa: already present\nblob bbb: downloading";
        let said = progress_messages(JobEvent::Line {
            kind: "line",
            line: lines,
        });
        assert_eq!(said.len(), 3, "{said:?}");
        assert_eq!(
            said[1],
            json!({ "status": "Already exists", "progressDetail": {}, "id": "aaa" })
        );
        assert_eq!(said[2]["status"], "Pulling fs layer");
        assert_eq!(said[2]["id"], "bbb");
        let progress = progress_messages(JobEvent::Progress {
            item: "aaa",
            done: 1,
            total: 2,
        });
        assert_eq!(progress.len(), 1);
    }

    #[test]
    fn a_pull_replaces_the_image_of_its_reference_and_leaves_containers_alone() {
        let full = "docker.io/library/alpine:3.20";
        let records = [
            record("alpine-3.20", full, 10),
            record("alpine-3.20-2", full, 20),
            record("web", full, 30),
            record("other", "docker.io/library/alpine:3.19", 40),
        ];
        let is_image = |r: &ImageRecord| r.name != "web";
        let taken: BTreeSet<String> = records.iter().map(|r| r.name.clone()).collect();
        assert_eq!(
            destination(
                &records,
                full,
                "hub.nspawn.org",
                is_image,
                "alpine-3.20",
                &taken
            ),
            ("alpine-3.20-2".to_string(), true),
            "the last pulled image of the reference"
        );
        // Only a container of it: the image comes under a name of its own.
        let containers = |r: &ImageRecord| r.name == "other";
        assert_eq!(
            destination(
                &records,
                full,
                "hub.nspawn.org",
                containers,
                "alpine-3.20",
                &taken
            ),
            ("alpine-3.20-3".to_string(), false)
        );
        assert_eq!(
            destination(
                &[],
                full,
                "hub.nspawn.org",
                is_image,
                "alpine-3.20",
                &BTreeSet::new()
            ),
            ("alpine-3.20".to_string(), false)
        );
    }

    #[test]
    fn a_pull_is_up_to_date_against_the_record_its_reference_names() {
        let full = "docker.io/library/alpine:3.20";
        let mut old = record("alpine-3.20", full, 10);
        old.manifest_digest = format!("sha256:{}", "1".repeat(64));
        let mut new = record("alpine-3.20-2", full, 20);
        new.index_digest = Some(format!("sha256:{}", "2".repeat(64)));
        let container = record("web", full, 30);
        let records = [old, new, container];
        let is_image = |r: &ImageRecord| r.name != "web";
        let named = tagged(&records, full, "hub.nspawn.org", is_image).unwrap();
        assert_eq!(named.name, "alpine-3.20-2", "an image before a container");
        assert_eq!(pulled_digest(named), format!("sha256:{}", "2".repeat(64)));
        let none = |_: &ImageRecord| false;
        assert_eq!(
            tagged(&records, full, "hub.nspawn.org", none).unwrap().name,
            "web",
            "the last made of the containers when no image has it"
        );
        let digest = format!("sha256:{}", "1".repeat(64));
        let wanted = format!("docker.io/library/alpine@{digest}");
        assert_eq!(
            pinned(&records, &wanted, "hub.nspawn.org").unwrap().name,
            "alpine-3.20"
        );
        assert!(pinned(
            &records,
            "docker.io/library/alpine@sha256:9",
            "hub.nspawn.org"
        )
        .is_none());
        // The platform's manifest of a record pulled by an index is not what it was
        // pulled by: a pull of it makes a record of its own, which a create then finds.
        let manifest = format!("docker.io/library/alpine@sha256:{}", "a".repeat(64));
        assert!(pinned(&records[1..2], &manifest, "hub.nspawn.org").is_none());
    }
}
