//! A container's files, through the archive endpoints: HEAD /containers/{id}/archive
//! says what is at a path (the bus's StatPath), GET sends it as a tar stream (CopyFrom)
//! and PUT writes a tar stream beneath a directory (CopyTo's extract), on a running
//! machine or a stopped overlay or flat one, as `nspawn cp` reaches them. What is at the
//! path travels in the X-Docker-Container-Path-Stat header, the engine's JSON in base64;
//! the name of the error the bus answers with decides the status.

use std::os::fd::{AsFd, OwnedFd};
use std::sync::Arc;

use base64::Engine as _;
use bytes::Bytes;
use http_body_util::{BodyExt, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::header::HeaderValue;
use hyper::{Method, Response, StatusCode};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::pipe;
use zbus::zvariant::Value as Bus;

use super::containers::{status_of, Answer, Sent};
use super::model::Snapshot;
use super::{lookup_status, respond, Body, Query};
use crate::api::copy::{base_name, split_source, PathStat};
use crate::api::Context;
use crate::client::{self, Client, Dict, Options};

/// The header that carries what is at the path.
const STAT_HEADER: &str = "X-Docker-Container-Path-Stat";
/// A tar block: the status of a GET waits for the first one, or for the copy's end.
const BLOCK: usize = 512;
/// What one read of the stream takes at most.
const CHUNK: usize = 64 * 1024;

/// An archive request of a container, by its method.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    /// HEAD: what is at the path, in the header alone.
    Stat(String),
    /// GET: the path as a tar stream.
    Get(String),
    /// PUT: a tar stream written beneath the path.
    Put(String),
}

impl Call {
    pub fn of(method: &Method, parts: &[&str]) -> Option<Call> {
        let ["containers", id, "archive"] = parts else {
            return None;
        };
        let id = id.to_string();
        if *method == Method::HEAD {
            Some(Call::Stat(id))
        } else if *method == Method::GET {
            Some(Call::Get(id))
        } else if *method == Method::PUT {
            Some(Call::Put(id))
        } else {
            None
        }
    }
}

type Served = Result<Response<Body>, Answer>;

pub async fn serve(ctx: &Arc<Context>, call: Call, query: &Query, sent: Sent) -> Response<Body> {
    let served = match call {
        Call::Stat(key) => stat(ctx, &key, query).await,
        Call::Get(key) => get(ctx, &key, query).await,
        Call::Put(key) => put(ctx, &key, query, sent.body).await,
    };
    served.unwrap_or_else(Answer::respond)
}

fn internal(e: impl std::fmt::Display) -> Answer {
    Answer(StatusCode::INTERNAL_SERVER_ERROR, Some(e.to_string()))
}

/// The path the query names; an empty one is refused before the container is looked
/// for, in the engine's words.
fn path_of(query: &Query) -> Result<&str, Answer> {
    query.get("path").filter(|p| !p.is_empty()).ok_or_else(|| {
        Answer(
            StatusCode::BAD_REQUEST,
            Some("bad parameter: pathcannot be empty".to_string()),
        )
    })
}

/// The container a request names: its machine, and the user its files get with
/// copyUIDGID (Config.User: its own, else its image's).
struct Target {
    machine: String,
    user: String,
}

async fn target(ctx: &Context, key: &str) -> Result<Target, Answer> {
    let snapshot = Snapshot::take(ctx)
        .await
        .map_err(|e| internal(format!("{e:#}")))?;
    let found = snapshot
        .find(key)
        .map_err(|e| Answer(lookup_status(&e), Some(e)))?;
    Ok(Target {
        machine: found.name().to_string(),
        user: found
            .record
            .effective_user()
            .unwrap_or_default()
            .to_string(),
    })
}

async fn connect() -> Result<Client, Answer> {
    Client::connect()
        .await
        .map_err(|e| internal(format!("{e:#}")))
}

/// How the service names a path of a machine at the head of its messages.
fn label(machine: &str, path: &str) -> String {
    format!("{machine}:{path}")
}

/// A message of the service without the MACHINE:PATH it starts with: the client knows
/// the container and the path it asked for by its own names.
fn unlabelled<'a>(message: &'a str, labels: &[String]) -> &'a str {
    labels
        .iter()
        .find_map(|l| message.strip_prefix(l.as_str())?.strip_prefix(": "))
        .unwrap_or(message)
}

/// What a failure of the bus answers, for `path` of the container `key` as the request
/// named them.
fn failed(e: &anyhow::Error, key: &str, path: &str, labels: &[String]) -> Answer {
    let message = format!("{e:#}");
    match client::error_name(e) {
        Some("NoSuchPath") => Answer(
            StatusCode::NOT_FOUND,
            Some(format!("Could not find the file {path} in container {key}")),
        ),
        Some("NoSuchMachine") => Answer(
            StatusCode::NOT_FOUND,
            Some(format!("No such container: {key}")),
        ),
        kind => Answer(
            status_of(kind),
            Some(unlabelled(&message, labels).to_string()),
        ),
    }
}

/// The path a GET packs, as the engine reads the one asked for: cleaned beneath the
/// root, with the `.` or the slash it ends in kept, since they say what is packed (the
/// contents of a directory, a link followed). Its last element names the stream's top
/// entry.
pub fn engine_path(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    let mut cleaned = format!("/{}", parts.join("/"));
    if base_name(path) == "." {
        if !cleaned.ends_with('/') {
            cleaned.push('/');
        }
        cleaned.push('.');
    }
    if path.ends_with('/') && !cleaned.ends_with('/') {
        cleaned.push('/');
    }
    cleaned
}

/// Go's os.FileMode of an st_mode: the permissions, and the type and the setuid, setgid
/// and sticky bits at the places Go gives them, which clients test (IsDir, ModeSymlink).
pub fn go_mode(st_mode: u32) -> u32 {
    const DIR: u32 = 1 << 31;
    const SYMLINK: u32 = 1 << 27;
    const DEVICE: u32 = 1 << 26;
    const NAMED_PIPE: u32 = 1 << 25;
    const SOCKET: u32 = 1 << 24;
    const SETUID: u32 = 1 << 23;
    const SETGID: u32 = 1 << 22;
    const CHAR_DEVICE: u32 = 1 << 21;
    const STICKY: u32 = 1 << 20;
    let mut mode = st_mode & 0o777;
    mode |= match st_mode & 0o170_000 {
        0o060_000 => DEVICE,
        0o020_000 => DEVICE | CHAR_DEVICE,
        0o040_000 => DIR,
        0o010_000 => NAMED_PIPE,
        0o120_000 => SYMLINK,
        0o140_000 => SOCKET,
        _ => 0,
    };
    for (bit, go) in [(0o4000, SETUID), (0o2000, SETGID), (0o1000, STICKY)] {
        if st_mode & bit != 0 {
            mode |= go;
        }
    }
    mode
}

/// Unix nanoseconds as Go writes a time in JSON: RFC 3339 in UTC, the nanoseconds
/// without their trailing zeros, none when there are none.
pub fn rfc3339_nanos(nanos: i64) -> String {
    let seconds = nanos.div_euclid(1_000_000_000);
    let fraction = nanos.rem_euclid(1_000_000_000);
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days, which takes days before 1970 too.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let mut text = format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    );
    if fraction > 0 {
        text.push('.');
        text.push_str(format!("{fraction:09}").trim_end_matches('0'));
    }
    text.push('Z');
    text
}

/// What StatPath answered.
fn path_stat(dict: &Dict) -> PathStat {
    PathStat {
        name: client::string(dict, "name"),
        size: client::u64(dict, "size"),
        mode: u32::try_from(client::u64(dict, "mode")).unwrap_or(0),
        mtime: dict
            .get("mtime")
            .and_then(|v| i64::try_from(v.clone()).ok())
            .unwrap_or(0),
        link_target: client::string(dict, "link_target"),
    }
}

/// The header's value: the engine's PathStat as JSON, its fields in the engine's order,
/// in base64.
pub fn stat_header(stat: &PathStat) -> String {
    let text = format!(
        r#"{{"name":{},"size":{},"mode":{},"mtime":{},"linkTarget":{}}}"#,
        json!(stat.name),
        stat.size,
        go_mode(stat.mode),
        json!(rfc3339_nanos(stat.mtime)),
        json!(stat.link_target),
    );
    base64::engine::general_purpose::STANDARD.encode(text)
}

/// `response` with the stat header.
fn with_stat(mut response: Response<Body>, stat: &PathStat) -> Served {
    let value = HeaderValue::try_from(stat_header(stat)).map_err(internal)?;
    response.headers_mut().insert(STAT_HEADER, value);
    Ok(response)
}

/// A GET's answer: the stream, a tar never compressed, since it goes over a local socket.
fn tar_response(stat: &PathStat, body: Body) -> Served {
    let mut response = Response::new(body);
    response.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-tar"),
    );
    with_stat(response, stat)
}

async fn stat_of(
    client: &Client,
    machine: &str,
    path: &str,
    fail: impl Fn(anyhow::Error) -> Answer,
) -> Result<PathStat, Answer> {
    let dict = client
        .manager
        .stat_path(machine, path)
        .await
        .map_err(client::error)
        .map_err(fail)?;
    Ok(path_stat(&dict))
}

/// HEAD: what lstat says of the path as it was given; a link is not followed, and its
/// target is where it leads inside the container.
async fn stat(ctx: &Context, key: &str, query: &Query) -> Served {
    let path = path_of(query)?;
    let target = target(ctx, key).await?;
    let client = connect().await?;
    let labels = [label(&target.machine, path)];
    let fail = |e: anyhow::Error| failed(&e, key, path, &labels);
    let stat = stat_of(&client, &target.machine, path, fail).await?;
    with_stat(respond(StatusCode::OK, "", Bytes::new()), &stat)
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

/// The rest of the stream after `first`, until the pipe ends; a client that goes away
/// drops the pipe, and the service's copy stops with it.
fn tar_body(first: Vec<u8>, reader: pipe::Receiver) -> Body {
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
    StreamBody::new(stream).boxed()
}

/// GET: the path as a tar stream, its top entry named as the engine names it (the last
/// element of the cleaned path: `.` for `DIR/.`, `/` for the root). The status waits for
/// the stream's first block, so that a copy that fails before it is answered by its
/// error; one that fails later cuts the stream short.
async fn get(ctx: &Context, key: &str, query: &Query) -> Served {
    let path = path_of(query)?;
    let target = target(ctx, key).await?;
    let packed = engine_path(path);
    let name = base_name(&packed);
    let client = connect().await?;
    let labels = [
        label(&target.machine, &packed),
        label(&target.machine, &split_source(&packed).0),
    ];
    let fail = |e: anyhow::Error| failed(&e, key, path, &labels);
    let stat = stat_of(&client, &target.machine, &packed, fail).await?;
    let watch = client
        .watch_jobs()
        .await
        .map_err(|e| internal(format!("{e:#}")))?;
    let mut options = Options::new();
    options.insert("name", Bus::from(name.as_str()));
    let (stream, job) = client
        .manager
        .copy_from(&target.machine, &packed, options)
        .await
        .map_err(client::error)
        .map_err(fail)?;
    let mut reader = pipe::Receiver::from_owned_fd(OwnedFd::from(stream)).map_err(internal)?;
    let first = first_block(&mut reader).await;
    if first.len() < BLOCK {
        // The stream ended before a whole block: the copy's outcome says why.
        watch.follow(job, |_| {}).await.map_err(fail)?;
    }
    tar_response(&stat, tar_body(first, reader))
}

/// CopyTo's options for a PUT: the stream extracted as it is, with
/// noOverwriteDirNonDir, and the files given to `user` (the container's, with
/// copyUIDGID) when there is one.
fn extract_options(no_overwrite_dir_non_dir: bool, user: Option<&str>) -> Options<'_> {
    let mut options = Options::new();
    options.insert("extract", Bus::from(true));
    options.insert(
        "no_overwrite_dir_non_dir",
        Bus::from(no_overwrite_dir_non_dir),
    );
    if let Some(user) = user.filter(|u| !u.is_empty()) {
        options.insert("owner", Bus::from(user));
    }
    options
}

/// The request's body into the pipe, until it ends or the client goes away; the pipe
/// closes then, which ends the stream the copy reads. Once the copy stops reading, the
/// rest is read and dropped.
async fn pump<B>(mut body: B, pipe: pipe::Sender)
where
    B: hyper::body::Body<Data = Bytes> + Unpin,
{
    let mut pipe = Some(pipe);
    while let Some(frame) = body.frame().await {
        let Ok(frame) = frame else { return };
        if let (Ok(data), Some(sender)) = (frame.into_data(), pipe.as_mut()) {
            if sender.write_all(&data).await.is_err() {
                // The copy stopped reading: what is left is read and dropped, for the
                // client to get the answer.
                pipe = None;
            }
        }
    }
}

/// PUT: the body, a tar stream, plain or compressed, written beneath the path, an
/// existing directory once its links are followed. The service checks the directory and
/// the owner before the body is read, and those refusals answer at once; the copy's own
/// failure answers once it ended.
async fn put(ctx: &Context, key: &str, query: &Query, body: Incoming) -> Served {
    let path = match path_of(query) {
        Ok(path) => path,
        Err(answer) => {
            discard_body(body).await;
            return Err(answer);
        }
    };
    let no_overwrite = query.flag("noOverwriteDirNonDir", false);
    let copy_ids = query.flag("copyUIDGID", false);
    let started = async {
        let target = target(ctx, key).await?;
        let client = connect().await?;
        let labels = [label(&target.machine, path)];
        let watch = client
            .watch_jobs()
            .await
            .map_err(|e| internal(format!("{e:#}")))?;
        let (read, write) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)
            .map_err(|e| internal(format!("creating a pipe: {e}")))?;
        let options = extract_options(no_overwrite, copy_ids.then_some(target.user.as_str()));
        let job = client
            .manager
            .copy_to(
                &target.machine,
                path,
                zbus::zvariant::Fd::from(read.as_fd()),
                options,
            )
            .await
            .map_err(client::error)
            .map_err(|e| failed(&e, key, path, &labels))?;
        // The service's end only: a copy that stops reading makes the writes fail.
        drop(read);
        let sender = pipe::Sender::from_owned_fd(write).map_err(internal)?;
        Ok::<_, Answer>((client, watch, job, sender, labels))
    }
    .await;
    let (_client, watch, job, sender, labels) = match started {
        Ok(started) => started,
        Err(answer) => {
            discard_body(body).await;
            return Err(answer);
        }
    };
    let pumped = pump(body, sender);
    let outcome = watch.follow(job, |_| {});
    tokio::pin!(pumped, outcome);
    let result = tokio::select! {
        result = &mut outcome => {
            if result.is_err() {
                let _ = tokio::time::timeout(DISCARD, &mut pumped).await;
            }
            result
        }
        () = &mut pumped => outcome.await,
    };
    result.map_err(|e| failed(&e, key, path, &labels))?;
    Ok(respond(StatusCode::OK, "", Bytes::new()))
}

/// How long the rest of a refused stream is read: a client still sending it when the
/// answer comes would otherwise get an error of its own write instead of the answer.
const DISCARD: std::time::Duration = std::time::Duration::from_secs(10);

/// Reads what is left of a request's body and drops it, for `DISCARD` at most.
async fn discard_body<B>(mut body: B)
where
    B: hyper::body::Body + Unpin,
{
    let _ = tokio::time::timeout(DISCARD, async {
        while let Some(Ok(_)) = body.frame().await {}
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::super::access::Need;
    use super::super::{classify, query, Route};
    use super::*;
    use crate::client::{JobFailed, ServiceError};

    fn service(name: &str, message: &str) -> anyhow::Error {
        anyhow::Error::new(ServiceError {
            name: format!("org.nspawn.Error.{name}"),
            message: message.to_string(),
        })
    }

    fn answer(a: Answer) -> (u16, String) {
        (a.0.as_u16(), a.1.unwrap_or_default())
    }

    #[test]
    fn modes_carry_go_s_bits() {
        for (st_mode, go) in [
            (0o100_644, 0o644),
            (0o040_755, (1 << 31) | 0o755),
            (0o120_777, (1 << 27) | 0o777),
            (0o020_666, (1 << 26) | (1 << 21) | 0o666),
            (0o060_660, (1 << 26) | 0o660),
            (0o010_644, (1 << 25) | 0o644),
            (0o140_755, (1 << 24) | 0o755),
            (0o104_755, (1 << 23) | 0o755),
            (0o042_755, (1 << 31) | (1 << 22) | 0o755),
            (0o041_777, (1 << 31) | (1 << 20) | 0o777),
        ] {
            assert_eq!(go_mode(st_mode), go, "{st_mode:o}");
        }
        // os.ModeDir|0755, as the engine's JSON has it.
        assert_eq!(go_mode(0o040_755), 2_147_484_141);
    }

    #[test]
    fn times_are_written_as_go_writes_them() {
        assert_eq!(rfc3339_nanos(0), "1970-01-01T00:00:00Z");
        assert_eq!(
            rfc3339_nanos(1_700_000_000_123_456_789),
            "2023-11-14T22:13:20.123456789Z"
        );
        assert_eq!(
            rfc3339_nanos(1_700_000_000_500_000_000),
            "2023-11-14T22:13:20.5Z"
        );
        assert_eq!(
            rfc3339_nanos(951_782_400_000_000_000),
            "2000-02-29T00:00:00Z"
        );
        assert_eq!(rfc3339_nanos(-1), "1969-12-31T23:59:59.999999999Z");
    }

    #[test]
    fn the_stat_header_is_the_engine_s_json_in_base64() {
        let stat = PathStat {
            name: "run".to_string(),
            size: 4,
            mode: 0o120_777,
            mtime: 1_700_000_000_000_000_001,
            link_target: "/run".to_string(),
        };
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(stat_header(&stat))
            .unwrap();
        assert_eq!(
            String::from_utf8(decoded).unwrap(),
            r#"{"name":"run","size":4,"mode":134218239,"mtime":"2023-11-14T22:13:20.000000001Z","linkTarget":"/run"}"#
        );
        let quoted = PathStat {
            name: "a\"b".to_string(),
            size: 0,
            mode: 0o040_700,
            mtime: 0,
            link_target: String::new(),
        };
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(stat_header(&quoted))
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&decoded).unwrap();
        assert_eq!(value["name"], "a\"b");
        assert_eq!(value["mode"], (1u64 << 31) | 0o700);
        assert_eq!(value["linkTarget"], "");
    }

    #[test]
    fn the_path_packed_and_its_top_name_are_the_engine_s() {
        for (asked, packed, name) in [
            ("/", "/", "/"),
            ("/.", "/.", "."),
            (".", "/.", "."),
            ("..", "/", "/"),
            ("etc", "/etc", "etc"),
            ("/etc/", "/etc/", "etc"),
            ("/etc/.", "/etc/.", "."),
            ("etc/./", "/etc/./", "."),
            ("/a/b/..", "/a", "a"),
            ("//etc//passwd", "/etc/passwd", "passwd"),
            ("/../../etc/hosts", "/etc/hosts", "hosts"),
        ] {
            assert_eq!(engine_path(asked), packed, "{asked}");
            assert_eq!(base_name(&engine_path(asked)), name, "{asked}");
        }
    }

    #[test]
    fn failures_answer_by_the_names_of_the_bus_errors() {
        let labels = ["web:/x".to_string()];
        let of = |e: anyhow::Error| answer(failed(&e, "web", "x", &labels));
        assert_eq!(
            of(service("NoSuchPath", "web:/x: No such file or directory")),
            (
                404,
                "Could not find the file x in container web".to_string()
            )
        );
        // A copy's job fails the same way.
        let job = anyhow::Error::new(JobFailed {
            error: ServiceError {
                name: "org.nspawn.Error.NoSuchPath".to_string(),
                message: "web:/x: No such file or directory".to_string(),
            },
            result: Dict::new(),
        });
        assert_eq!(of(job).0, 404);
        assert_eq!(
            of(service("NoSuchMachine", "no machine or image named web-1")),
            (404, "No such container: web".to_string())
        );
        assert_eq!(
            of(service(
                "Invalid",
                "web:/x: extraction point is not a directory"
            )),
            (400, "extraction point is not a directory".to_string())
        );
        assert_eq!(
            of(service("Failed", "web:/x: cannot overwrite a directory")),
            (500, "cannot overwrite a directory".to_string())
        );
        assert_eq!(
            of(anyhow::anyhow!(
                "the bus connection closed while the job ran"
            )),
            (
                500,
                "the bus connection closed while the job ran".to_string()
            )
        );
        // A message about another path keeps it.
        assert_eq!(
            unlabelled("web:/y: Permission denied", &labels),
            "web:/y: Permission denied"
        );
    }

    #[test]
    fn an_empty_path_is_refused_in_the_engine_s_words() {
        for raw in ["", "path=", "x=1"] {
            assert_eq!(
                path_of(&query(raw)).map_err(answer),
                Err((400, "bad parameter: pathcannot be empty".to_string())),
                "{raw}"
            );
        }
        assert_eq!(path_of(&query("path=%2Fetc%2F")), Ok("/etc/"));
    }

    #[test]
    fn a_put_extracts_and_owns_by_the_container_s_user_when_asked() {
        let options = extract_options(true, Some("www-data:1000"));
        assert_eq!(options["extract"], Bus::from(true));
        assert_eq!(options["no_overwrite_dir_non_dir"], Bus::from(true));
        assert_eq!(options["owner"], Bus::from("www-data:1000"));
        let options = extract_options(false, None);
        assert_eq!(options["no_overwrite_dir_non_dir"], Bus::from(false));
        assert!(!options.contains_key("owner"));
        // A container without a user: the headers' owners, as without copyUIDGID.
        assert!(!extract_options(true, Some("")).contains_key("owner"));
    }

    #[test]
    fn a_get_answers_a_tar_with_the_stat() {
        let stat = PathStat {
            name: "etc".to_string(),
            size: 4096,
            mode: 0o040_755,
            mtime: 0,
            link_target: String::new(),
        };
        let body = http_body_util::Full::new(Bytes::new()).boxed();
        let response = tar_response(&stat, body).unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["Content-Type"], "application/x-tar");
        assert_eq!(
            response.headers()[STAT_HEADER].to_str().unwrap(),
            stat_header(&stat)
        );
        let head = with_stat(respond(StatusCode::OK, "", Bytes::new()), &stat).unwrap();
        assert!(head.headers().get("Content-Type").is_none());
        assert!(head.headers().contains_key(STAT_HEADER));
    }

    #[test]
    fn archive_requests_are_routed_and_need_manage() {
        for (method, call) in [
            (Method::HEAD, Call::Stat("web".into())),
            (Method::GET, Call::Get("web".into())),
            (Method::PUT, Call::Put("web".into())),
        ] {
            let route = classify(&method, "/containers/web/archive");
            assert_eq!(route, Route::Archive(call), "{method}");
            // StatPath, CopyFrom and CopyTo all ask org.nspawn.manage: a read-only
            // socket reads no file of a container.
            assert_eq!(route.need(), Need::Manage, "{method}");
        }
        assert_eq!(
            classify(&Method::POST, "/containers/web/archive"),
            Route::Change
        );
        assert_eq!(Call::of(&Method::GET, &["containers", "web", "json"]), None);
    }

    type Framed = Result<Frame<Bytes>, std::io::Error>;

    fn frames_of(
        rx: tokio::sync::mpsc::Receiver<Framed>,
    ) -> impl futures_util::Stream<Item = Framed> + Unpin {
        Box::pin(futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|frame| (frame, rx))
        }))
    }

    #[tokio::test]
    async fn a_stream_the_copy_stopped_reading_is_read_to_its_end() {
        let (frames, rx) = tokio::sync::mpsc::channel::<Framed>(4);
        let body = StreamBody::new(frames_of(rx));
        let (read, write) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC).unwrap();
        // The copy refused the stream: its end of the pipe is gone.
        drop(read);
        let sender = pipe::Sender::from_owned_fd(write).unwrap();
        let pumped = tokio::spawn(pump(body, sender));
        for _ in 0..16 {
            frames
                .send(Ok(Frame::data(Bytes::from(vec![0u8; 64 * 1024]))))
                .await
                .expect("every frame is read");
        }
        drop(frames);
        pumped.await.unwrap();
        // And a body refused before any copy is read the same way.
        let (frames, rx) = tokio::sync::mpsc::channel::<Framed>(1);
        let discarded = tokio::spawn(discard_body(StreamBody::new(frames_of(rx))));
        for _ in 0..8 {
            frames
                .send(Ok(Frame::data(Bytes::from_static(b"x"))))
                .await
                .unwrap();
        }
        drop(frames);
        discarded.await.unwrap();
    }
}
