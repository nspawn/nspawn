//! Images and containers as tar streams: GET /images/get and /images/{name}/get (save),
//! POST /images/load, POST /commit, GET /containers/{id}/export and POST /images/create
//! with fromSrc (import), on SaveImages, LoadImages, CommitMachine, ExportMachine and
//! ImportImage.

use std::os::fd::{AsFd, OwnedFd};
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::{Response, StatusCode};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::pipe;
use zbus::zvariant::Value as Bus;

use super::containers::{status_of, Answer, Sent};
use super::model::Snapshot;
use super::util::familiar;
use super::{lookup_status, respond, Body, Query};
use crate::api::Context;
use crate::client::{self, Client, Options};

type Served = Result<Response<Body>, Answer>;

const BLOCK: usize = 512;
const CHUNK: usize = 64 * 1024;

fn internal(e: impl std::fmt::Display) -> Answer {
    Answer(StatusCode::INTERNAL_SERVER_ERROR, Some(e.to_string()))
}

/// A failure of the bus as the engine answers it: its status by the error's name.
fn failed(e: &anyhow::Error) -> Answer {
    Answer(status_of(client::error_name(e)), Some(format!("{e:#}")))
}

async fn connect() -> Result<Client, Answer> {
    Client::connect()
        .await
        .map_err(|e| internal(format!("{e:#}")))
}

/// The machine of the container a request names.
async fn machine_of(ctx: &Context, key: &str) -> Result<String, Answer> {
    let snapshot = Snapshot::take(ctx)
        .await
        .map_err(|e| internal(format!("{e:#}")))?;
    let found = snapshot
        .find(key)
        .map_err(|e| Answer(lookup_status(&e), Some(e)))?;
    Ok(found.name().to_string())
}

/// Up to a block of what `reader` has, less when it ends first.
async fn first_block(reader: &mut pipe::Receiver) -> Vec<u8> {
    let mut block = vec![0; BLOCK];
    let mut filled = 0;
    while filled < BLOCK {
        match reader.read(&mut block[filled..]).await {
            Ok(0) | Err(_) => break,
            Ok(n) => filled += n,
        }
    }
    block.truncate(filled);
    block
}

/// A tar on `stream`, `job`'s, as the answer: the status waits for its first block, so
/// that a job that fails before it is answered by its error. One that fails later ends
/// the stream where it stopped, which the client cannot tell from an end; the job's
/// failure stays in the service's journal.
async fn tar_answer(
    watch: client::JobWatch,
    stream: zbus::zvariant::OwnedFd,
    job: zbus::zvariant::OwnedObjectPath,
) -> Served {
    let mut reader = pipe::Receiver::from_owned_fd(OwnedFd::from(stream)).map_err(internal)?;
    let first = first_block(&mut reader).await;
    if first.len() < BLOCK {
        watch.follow(job, |_| {}).await.map_err(|e| failed(&e))?;
    }
    let state = (Some(Bytes::from(first)), reader);
    let stream = futures_util::stream::unfold(state, |(first, mut reader)| async move {
        if let Some(first) = first.filter(|f| !f.is_empty()) {
            return Some((
                Ok::<_, std::convert::Infallible>(Frame::data(first)),
                (None, reader),
            ));
        }
        let mut chunk = vec![0; CHUNK];
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => None,
            Ok(n) => {
                chunk.truncate(n);
                Some((Ok(Frame::data(Bytes::from(chunk))), (None, reader)))
            }
        }
    });
    let mut response = Response::new(StreamBody::new(stream).boxed());
    response
        .headers_mut()
        .insert("Content-Type", "application/x-tar".parse().unwrap());
    Ok(response)
}

/// The `platform` of a save or a load (a JSON OCI platform): the host's, or none.
fn json_platform(query: &Query) -> Result<(), Answer> {
    let given: Vec<&str> = query.all("platform").filter(|p| !p.is_empty()).collect();
    let text = match given.as_slice() {
        [] => return Ok(()),
        [one] => *one,
        _ => {
            return Err(Answer(
                StatusCode::BAD_REQUEST,
                Some("multiple platform parameters not supported".to_string()),
            ))
        }
    };
    let platform: Value = serde_json::from_str(text).map_err(|e| {
        Answer(
            StatusCode::BAD_REQUEST,
            Some(format!("invalid platform parameter: {e}")),
        )
    })?;
    let field = |key: &str| platform.get(key).and_then(Value::as_str).unwrap_or("");
    let os = Some(field("os"))
        .filter(|o| !o.is_empty())
        .unwrap_or("linux");
    let arch = field("architecture");
    let wanted = if arch.is_empty() {
        os.to_string()
    } else {
        format!("{os}/{arch}")
    };
    super::containers::check_platform(&wanted)
}

/// GET /images/get?names=... and GET /images/{name}/get: the images as one tar.
pub async fn save(names: Vec<String>, query: &Query) -> Response<Body> {
    save_images(names, query)
        .await
        .unwrap_or_else(Answer::respond)
}

async fn save_images(keys: Vec<String>, query: &Query) -> Served {
    json_platform(query)?;
    if keys.is_empty() {
        return Err(Answer(
            StatusCode::BAD_REQUEST,
            Some("no image names given to save".to_string()),
        ));
    }
    let mut names = Vec::new();
    for key in &keys {
        match super::images::resolve(key)
            .await
            .map_err(|e| internal(format!("{e:#}")))?
        {
            Some(found) => names.push(found.name),
            None => {
                return Err(Answer(
                    StatusCode::NOT_FOUND,
                    Some(format!("No such image: {key}")),
                ))
            }
        }
    }
    let client = connect().await?;
    let watch = client
        .watch_jobs()
        .await
        .map_err(|e| internal(format!("{e:#}")))?;
    let (stream, job) = client
        .manager
        .save_images(&names, Options::new())
        .await
        .map_err(|e| failed(&client::error(e)))?;
    tar_answer(watch, stream, job).await
}

/// GET /containers/{id}/export: the container's files as a tar.
pub async fn export(ctx: &Arc<Context>, key: &str) -> Response<Body> {
    export_machine(ctx, key)
        .await
        .unwrap_or_else(Answer::respond)
}

async fn export_machine(ctx: &Context, key: &str) -> Served {
    let machine = machine_of(ctx, key).await?;
    let client = connect().await?;
    let watch = client
        .watch_jobs()
        .await
        .map_err(|e| internal(format!("{e:#}")))?;
    let (stream, job) = client
        .manager
        .export_machine(&machine, Options::new())
        .await
        .map_err(|e| failed(&client::error(e)))?;
    tar_answer(watch, stream, job).await
}

/// The request's body into the pipe, until it ends or the client goes away; the pipe
/// closes then. Once the service stops reading, the rest is read and dropped. Whether the
/// body came whole.
async fn pump(mut body: Incoming, pipe: pipe::Sender) -> bool {
    let mut pipe = Some(pipe);
    while let Some(frame) = body.frame().await {
        let Ok(frame) = frame else { return false };
        if let (Ok(data), Some(sender)) = (frame.into_data(), pipe.as_mut()) {
            if sender.write_all(&data).await.is_err() {
                pipe = None;
            }
        }
    }
    true
}

/// Runs `start`, a job that reads the request's body from a pipe, to its end. What is
/// left of the body is read and dropped (for `super::archive::DISCARD` at most), for the
/// client to get the answer rather than an error of its own write.
async fn with_body<F, Fut>(body: Incoming, start: F) -> anyhow::Result<client::Dict>
where
    F: FnOnce(Client, OwnedFd) -> Fut,
    Fut: std::future::Future<
        Output = anyhow::Result<(Client, client::JobWatch, zbus::zvariant::OwnedObjectPath)>,
    >,
{
    let started = async {
        let client = Client::connect().await?;
        let (read, write) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)?;
        let (client, watch, job) = start(client, read).await?;
        Ok::<_, anyhow::Error>((client, watch, job, write))
    }
    .await;
    let (_client, watch, job, write) = match started {
        Ok(started) => started,
        Err(e) => {
            super::archive::discard_body(body).await;
            return Err(e);
        }
    };
    let sender = pipe::Sender::from_owned_fd(write)?;
    let pumped = pump(body, sender);
    let outcome = watch.follow(job, |_| {});
    tokio::pin!(pumped, outcome);
    let (result, whole) = tokio::select! {
        result = &mut outcome => {
            let whole = tokio::time::timeout(super::archive::DISCARD, &mut pumped).await;
            (result, whole.unwrap_or(false))
        }
        whole = &mut pumped => (outcome.await, whole),
    };
    let done = result?;
    if !whole {
        anyhow::bail!("the request's body was cut short");
    }
    Ok(done)
}

/// A line of a JSON stream.
fn line(value: &Value) -> Vec<u8> {
    let mut out = serde_json::to_vec(value).expect("JSON serializes");
    out.push(b'\n');
    out
}

fn error_line(message: &str) -> Vec<u8> {
    line(&json!({ "errorDetail": { "message": message }, "error": message }))
}

/// POST /images/load: the images of the tar in the body. The answer is a JSON stream,
/// 200 whatever happens, as the engine's: a line per image loaded, or the error.
pub async fn load(query: &Query, body: Incoming) -> Response<Body> {
    if let Err(answer) = json_platform(query) {
        super::archive::discard_body(body).await;
        return answer.respond();
    }
    let done = with_body(body, |client, read| async move {
        let watch = client.watch_jobs().await?;
        let job = client
            .manager
            .load_images(zbus::zvariant::Fd::from(read.as_fd()), Options::new())
            .await
            .map_err(client::error)?;
        Ok((client, watch, job))
    })
    .await;
    let body: Vec<u8> = match done {
        Ok(done) => client::strings(&done, "references")
            .iter()
            .flat_map(|r| line(&json!({ "stream": format!("Loaded image: {}\n", familiar(r)) })))
            .collect(),
        Err(e) => error_line(&format!("{e:#}")),
    };
    respond(StatusCode::OK, "application/json", body)
}

/// The changes a query lists (changes=, any number).
fn changes(query: &Query) -> Vec<String> {
    query.all("changes").map(str::to_string).collect()
}

/// The reference repo and tag make, read as the engine's clients name images.
fn reference(repo: &str, tag: &str) -> Result<String, Answer> {
    let bad = |text: String| Answer(StatusCode::BAD_REQUEST, Some(text));
    if repo.is_empty() {
        return Err(bad(
            "nspawn keeps an image under its name: give repo (and tag)".to_string(),
        ));
    }
    if repo.contains('@') {
        return Err(bad("cannot import digest reference".to_string()));
    }
    let text = if tag.is_empty() {
        repo.to_string()
    } else {
        format!("{repo}:{tag}")
    };
    crate::reference::ImageRef::normalized(&text)
        .map(|r| r.to_string())
        .map_err(|e| bad(format!("{e:#}")))
}

/// POST /commit: what the container changed, as a new image. 201 with its ID.
pub async fn commit(ctx: &Arc<Context>, query: &Query, sent: Sent) -> Response<Body> {
    commit_container(ctx, query, sent)
        .await
        .unwrap_or_else(Answer::respond)
}

/// The fields of a container config that are maps, whose keys are what they set (a port,
/// a volume, a label), whatever their values.
const MAP_FIELDS: [&str; 3] = ["ExposedPorts", "Labels", "Volumes"];

/// Whether a JSON value sets nothing: null, false, zero, an empty string, array or
/// object, or an object of such values, as a config of the SDK with no field set is. A
/// map field sets something with any key in it, and an entrypoint given as a list sets
/// one, an empty list none at all. Field names go by their letters whatever their case,
/// as the engine reads them.
fn says_nothing(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.iter().all(|(key, value)| {
            let is = |field: &str| key.eq_ignore_ascii_case(field);
            match value {
                Value::Object(map) if MAP_FIELDS.iter().any(|f| is(f)) => map.is_empty(),
                Value::Array(_) if is("Entrypoint") => false,
                value => says_nothing(value),
            }
        }),
    }
}

async fn commit_container(ctx: &Context, query: &Query, sent: Sent) -> Served {
    let config = super::containers::read_body(sent.body).await?;
    if config.iter().any(|b| !b.is_ascii_whitespace()) {
        let value: Value = serde_json::from_slice(&config).map_err(|e| {
            Answer(
                StatusCode::BAD_REQUEST,
                Some(format!("invalid JSON in the body of a commit: {e}")),
            )
        })?;
        if !says_nothing(&value) {
            return Err(Answer(
                StatusCode::BAD_REQUEST,
                Some(
                    "a config in the body of a commit is not supported by nspawn's Docker API; use changes"
                        .to_string(),
                ),
            ));
        }
    }
    let key = query.get("container").unwrap_or_default();
    let machine = machine_of(ctx, key).await?;
    let reference = reference(
        query.get("repo").unwrap_or_default(),
        query.get("tag").unwrap_or_default(),
    )?;
    let mut options = Options::new();
    for (key, name) in [("author", "author"), ("comment", "message")] {
        if let Some(value) = query.get(key).filter(|v| !v.is_empty()) {
            options.insert(name, Bus::from(value.to_string()));
        }
    }
    let changes = changes(query);
    if !changes.is_empty() {
        options.insert("changes", Bus::from(changes));
    }
    options.insert("pause", Bus::from(query.flag("pause", true)));
    let client = connect().await?;
    let done = client
        .run_job_silent(
            || client.manager.commit_machine(&machine, &reference, options),
            true,
        )
        .await
        .map_err(|e| failed(&e))?;
    Ok(super::json_response(
        StatusCode::CREATED,
        &json!({ "Id": client::string(&done, "id") }),
    ))
}

/// POST /images/create with fromSrc: the tar in the body as an image of one layer. The
/// answer is a JSON stream, 200 once the body is taken: the image's ID, or the error.
pub async fn import(query: &Query, sent: Sent) -> Response<Body> {
    let checked = (|| {
        if query.get("fromSrc").unwrap_or_default() != "-" {
            return Err(Answer(
                StatusCode::NOT_IMPLEMENTED,
                Some("importing from a URL is not supported by nspawn's Docker API".to_string()),
            ));
        }
        super::containers::check_platform(query.get("platform").unwrap_or_default())?;
        reference(
            query.get("repo").unwrap_or_default(),
            query.get("tag").unwrap_or_default(),
        )
    })();
    let reference = match checked {
        Ok(reference) => reference,
        Err(answer) => {
            super::archive::discard_body(sent.body).await;
            return answer.respond();
        }
    };
    let mut options = Options::new();
    if let Some(message) = query.get("message").filter(|m| !m.is_empty()) {
        options.insert("message", Bus::from(message.to_string()));
    }
    let changes = changes(query);
    if !changes.is_empty() {
        options.insert("changes", Bus::from(changes));
    }
    let done = with_body(sent.body, |client, read| async move {
        let watch = client.watch_jobs().await?;
        let job = client
            .manager
            .import_image(zbus::zvariant::Fd::from(read.as_fd()), &reference, options)
            .await
            .map_err(client::error)?;
        Ok((client, watch, job))
    })
    .await;
    let body = match done {
        Ok(done) => line(&json!({ "status": client::string(&done, "id") })),
        Err(e) => error_line(&format!("{e:#}")),
    };
    respond(StatusCode::OK, "application/json", body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_config_of_zero_values_sets_nothing() {
        let zero = serde_json::json!({
            "Hostname": "", "AttachStdin": false, "Cmd": null, "Env": [],
            "ExposedPorts": {}, "Healthcheck": {"Retries": 0},
        });
        assert!(says_nothing(&zero));
        assert!(!says_nothing(&serde_json::json!({"Cmd": ["sh"]})));
        assert!(!says_nothing(&serde_json::json!({"Env": ["A=1"]})));
        for set in [
            serde_json::json!({"ExposedPorts": {"8080/tcp": {}}}),
            serde_json::json!({"Volumes": {"/data": {}}}),
            serde_json::json!({"Labels": {"maintainer": ""}}),
            serde_json::json!({"labels": {"maintainer": ""}}),
            serde_json::json!({"Entrypoint": []}),
        ] {
            assert!(!says_nothing(&set), "{set}");
        }
    }

    #[test]
    fn repo_and_tag_name_an_image_as_docker_names_it() {
        assert_eq!(
            reference("busybox", "").unwrap(),
            "docker.io/library/busybox:latest"
        );
        assert_eq!(reference("me/app", "2").unwrap(), "docker.io/me/app:2");
        assert_eq!(
            reference("hub.nspawn.org/web", "1").unwrap(),
            "hub.nspawn.org/web:1"
        );
        assert_eq!(reference("", "x").unwrap_err().0, StatusCode::BAD_REQUEST);
        assert_eq!(
            reference("a@sha256:00", "").unwrap_err().0,
            StatusCode::BAD_REQUEST
        );
    }
}
