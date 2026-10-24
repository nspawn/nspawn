//! The Docker Engine API, read-only, for the machines that mount its socket:
//! reverse proxies that follow container labels, dashboards and log viewers find nspawn's
//! machines through it. It is served by `nspawn api-serve NAME`, which systemd starts
//! from the machine's own socket (`nspawn-api@NAME.socket`) when something connects, and
//! which leaves once nothing has for a minute. Nothing here changes a machine but a kill,
//! which goes to the service as `nspawn kill` does.

pub mod events;
pub mod filters;
pub mod model;
pub(crate) mod streams;
pub mod util;

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::{Method, Request, Response, StatusCode};
use serde_json::{json, Value};

use self::filters::Filters;
use self::model::{ImageInfo, Snapshot};
use self::util::{split_version, version_text, API_VERSION, MIN_API_VERSION};
use crate::api::Context;
use crate::config::Config;

type Body = BoxBody<Bytes, Infallible>;

/// The machine's end of its Docker API, which nspawn-api@NAME.socket listens on.
pub fn socket_path(machine: &str) -> std::path::PathBuf {
    std::path::Path::new("/run/nspawn/api").join(format!("{machine}.sock"))
}

/// How long the server stays without a connection before it leaves.
const IDLE: Duration = Duration::from_secs(60);

/// What a machine may hold of the host at once: connections, and streams that run a
/// journalctl (logs, events). A stats stream reads the cgroup once a second and is
/// bounded by the connections alone: a dashboard keeps one per running machine.
static CONNECTIONS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(64);
static STREAMS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(16);

/// How long a client has to send a request's headers.
const HEADER_TIMEOUT: Duration = Duration::from_secs(30);

/// `response`, its body written by `work`, or the error that there are too many
/// streams.
fn spawn_stream<F>(work: F, response: Response<Body>) -> Response<Body>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let Ok(permit) = STREAMS.try_acquire() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "too many streams are open on this socket",
        );
    };
    tokio::spawn(async move {
        let _permit = permit;
        work.await;
    });
    response
}

pub async fn serve(machine: &str, config: Config) -> Result<()> {
    let fd = crate::nsenter::listen_fd().with_context(|| {
        format!("no socket was handed over; nspawn api-serve runs from nspawn-api@{machine}.socket")
    })?;
    let listener = std::os::unix::net::UnixListener::from(fd);
    listener.set_nonblocking(true)?;
    let listener = tokio::net::UnixListener::from_std(listener)?;
    let ctx = Arc::new(Context::new(config));
    let open = Arc::new(AtomicUsize::new(0));
    let last = Arc::new(Mutex::new(Instant::now()));
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = match accepted {
                    Ok(accepted) => accepted,
                    // Out of descriptors, say: tried again in a moment rather than at once.
                    Err(_) => {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };
                // Over the limit the connection is closed right away.
                let Ok(permit) = CONNECTIONS.try_acquire() else { continue };
                open.fetch_add(1, Ordering::SeqCst);
                let (ctx, open, last) = (ctx.clone(), open.clone(), last.clone());
                tokio::spawn(async move {
                    let _permit = permit;
                    let service = hyper::service::service_fn(move |request| {
                        let ctx = ctx.clone();
                        async move { Ok::<_, Infallible>(handle(ctx, request).await) }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .timer(hyper_util::rt::TokioTimer::new())
                        .header_read_timeout(HEADER_TIMEOUT)
                        .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                        .await;
                    *last.lock().unwrap() = Instant::now();
                    open.fetch_sub(1, Ordering::SeqCst);
                });
            }
            _ = tokio::time::sleep(Duration::from_secs(5)) => {
                if open.load(Ordering::SeqCst) == 0 && last.lock().unwrap().elapsed() > IDLE {
                    return Ok(());
                }
            }
        }
    }
}

fn respond(status: StatusCode, content_type: &str, body: impl Into<Bytes>) -> Response<Body> {
    let mut response = Response::new(Full::new(body.into()).boxed());
    *response.status_mut() = status;
    if !content_type.is_empty() {
        response
            .headers_mut()
            .insert("Content-Type", content_type.parse().unwrap());
    }
    response
}

fn json_response(status: StatusCode, value: &Value) -> Response<Body> {
    respond(status, "application/json", format!("{value}\n"))
}

fn error(status: StatusCode, message: impl AsRef<str>) -> Response<Body> {
    json_response(status, &json!({ "message": message.as_ref() }))
}

/// A body written as it goes, by a task that gets the channel's other end.
fn streaming(content_type: &str) -> (Response<Body>, tokio::sync::mpsc::Sender<Bytes>) {
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(64);
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|bytes| (Ok::<_, Infallible>(Frame::data(bytes)), rx))
    });
    let mut response = Response::new(StreamBody::new(stream).boxed());
    response
        .headers_mut()
        .insert("Content-Type", content_type.parse().unwrap());
    (response, tx)
}

/// The query string's parameters, percent-decoded as Go's ParseQuery decodes them: a
/// pair that does not decode is left out, and the first error kept for the requests
/// the API answers it for. A key given twice keeps its first value, except where all are
/// read.
fn query(raw: &str) -> Query {
    let mut pairs = Vec::new();
    let mut bad = None;
    for pair in raw.split('&') {
        if pair.contains(';') {
            bad.get_or_insert_with(|| "invalid semicolon separator in query".to_string());
            continue;
        }
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        match (decode(k), decode(v)) {
            (Ok(k), Ok(v)) => pairs.push((k, v)),
            (Err(e), _) | (_, Err(e)) => {
                bad.get_or_insert(e);
            }
        }
    }
    Query { pairs, bad }
}

/// Go's QueryUnescape: `+` a space, `%` and two hex digits a byte, any other `%` an
/// error.
fn decode(text: &str) -> Result<String, String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' => {
                let hex = bytes
                    .get(i + 1..i + 3)
                    .filter(|h| h.iter().all(u8::is_ascii_hexdigit));
                let Some(hex) = hex else {
                    let bad = String::from_utf8_lossy(&bytes[i..(i + 3).min(bytes.len())]);
                    return Err(format!("invalid URL escape {bad:?}"));
                };
                let digit = |b: u8| (b as char).to_digit(16).unwrap_or(0) as u8;
                out.push(digit(hex[0]) << 4 | digit(hex[1]));
                i += 2;
            }
            b => out.push(b),
        }
        i += 1;
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

struct Query {
    pairs: Vec<(String, String)>,
    /// What made a pair undecodable.
    bad: Option<String>,
}

impl Query {
    fn get(&self, key: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// BoolValue: anything but nothing, 0, no, false and none is true; absent is
    /// `default`.
    fn flag(&self, key: &str, default: bool) -> bool {
        match self.get(key) {
            None => default,
            // httputils.BoolValue: trimmed, then anything but these is true.
            Some(v) => !matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "" | "0" | "no" | "false" | "none"
            ),
        }
    }
}

async fn handle(ctx: Arc<Context>, request: Request<Incoming>) -> Response<Body> {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let query = query(request.uri().query().unwrap_or_default());
    let (version, route) = split_version(&path);
    let mut response = match version {
        Some(v) if v < MIN_API_VERSION => respond(
            StatusCode::BAD_REQUEST,
            "text/plain; charset=utf-8",
            format!(
                "client version {} is too old. Minimum supported API version is {}, please upgrade your client to a newer version",
                version_text(v),
                version_text(MIN_API_VERSION)
            ),
        ),
        Some(v) if v > API_VERSION => error(
            StatusCode::BAD_REQUEST,
            format!(
                "client version {} is too new. Maximum supported API version is {}",
                version_text(v),
                version_text(API_VERSION)
            ),
        ),
        _ => route_request(&ctx, &method, route, &query).await,
    };
    let headers = response.headers_mut();
    headers.insert("Api-Version", version_text(API_VERSION).parse().unwrap());
    headers.insert("Docker-Experimental", "false".parse().unwrap());
    headers.insert("Ostype", "linux".parse().unwrap());
    headers.insert(
        "Server",
        format!("nspawn/{} (linux)", env!("CARGO_PKG_VERSION"))
            .parse()
            .unwrap(),
    );
    if method == Method::HEAD {
        *response.body_mut() = Full::new(Bytes::new()).boxed();
    }
    response
}

/// The reads docker serves that this API has no answer for.
const UNSERVED: &[&str] = &[
    "/images/json",
    "/volumes",
    "/plugins",
    "/swarm",
    "/services",
    "/tasks",
    "/secrets",
    "/configs",
    "/nodes",
];

/// What a request asks for, from its method and its path without the version.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Route {
    Ping,
    Version,
    Info,
    DiskUsage,
    Containers,
    /// A container's json, top, logs, stats...
    Container(String, String),
    Kill(String),
    Networks,
    Network(String),
    Events,
    /// A read docker serves and this API does not.
    Unserved,
    NotFound,
    /// Anything that would change something, but a kill.
    ReadOnly,
}

fn classify(method: &Method, path: &str) -> Route {
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    let read = *method == Method::GET || *method == Method::HEAD;
    match (read, parts.as_slice()) {
        (true, ["_ping"]) => Route::Ping,
        (true, ["version"]) => Route::Version,
        (true, ["info"]) => Route::Info,
        (true, ["system", "df"]) => Route::DiskUsage,
        (true, ["containers", "json"]) => Route::Containers,
        (true, ["containers", id, what]) => Route::Container(id.to_string(), what.to_string()),
        (false, ["containers", id, "kill"]) if *method == Method::POST => {
            Route::Kill(id.to_string())
        }
        (true, ["networks"]) => Route::Networks,
        (true, ["networks", id]) => Route::Network(id.to_string()),
        (true, ["events"]) => Route::Events,
        (true, _)
            if UNSERVED.iter().any(|u| path.starts_with(u)) || parts.first() == Some(&"images") =>
        {
            Route::Unserved
        }
        (true, _) => Route::NotFound,
        (false, _) => Route::ReadOnly,
    }
}

async fn route_request(
    ctx: &Arc<Context>,
    method: &Method,
    path: &str,
    query: &Query,
) -> Response<Body> {
    let route = classify(method, path);
    // The requests whose handlers docker starts with ParseForm, which answers a query
    // that does not decode.
    let parses_form = match &route {
        Route::Containers
        | Route::Kill(_)
        | Route::Networks
        | Route::Network(_)
        | Route::Events
        | Route::DiskUsage => true,
        Route::Container(_, what) => what != "json",
        _ => false,
    };
    if let (true, Some(bad)) = (parses_form, &query.bad) {
        return error(StatusCode::BAD_REQUEST, bad.clone());
    }
    match route {
        Route::Ping => {
            let mut response = respond(StatusCode::OK, "text/plain; charset=utf-8", "OK");
            let headers = response.headers_mut();
            headers.insert(
                "Cache-Control",
                "no-cache, no-store, must-revalidate".parse().unwrap(),
            );
            headers.insert("Pragma", "no-cache".parse().unwrap());
            headers.insert("Swarm", "inactive".parse().unwrap());
            response
        }
        Route::Version => json_response(StatusCode::OK, &version()),
        Route::Info => match Snapshot::take(ctx).await {
            Ok(snapshot) => json_response(StatusCode::OK, &info(ctx, &snapshot)),
            Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
        },
        Route::DiskUsage => json_response(
            StatusCode::OK,
            &json!({ "LayersSize": 0, "Images": [], "Containers": [], "Volumes": [], "BuildCache": [] }),
        ),
        Route::Containers => list_containers(ctx, query).await,
        Route::Container(id, what) => container_read(ctx, &id, &what, query).await,
        Route::Kill(id) => kill(ctx, &id, query).await,
        Route::Networks => list_networks(ctx, query).await,
        Route::Network(id) => match Snapshot::take(ctx).await {
            Ok(snapshot) => match snapshot.network(&id) {
                Some(view) => json_response(StatusCode::OK, &model::network_json(&snapshot, &view)),
                None => error(StatusCode::NOT_FOUND, format!("network {id} not found")),
            },
            Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
        },
        Route::Events => events(ctx, query),
        Route::Unserved => error(
            StatusCode::NOT_IMPLEMENTED,
            format!("{path} is not served by nspawn's Docker API"),
        ),
        Route::NotFound => error(StatusCode::NOT_FOUND, "page not found"),
        Route::ReadOnly => error(
            StatusCode::FORBIDDEN,
            format!("nspawn's Docker API is read-only: {method} {path} is not served"),
        ),
    }
}

fn version() -> Value {
    let version = env!("CARGO_PKG_VERSION");
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .unwrap_or_default()
        .trim()
        .to_string();
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };
    json!({
        "Platform": { "Name": "nspawn" },
        "Components": [{
            "Name": "Engine",
            "Version": version,
            "Details": {
                "ApiVersion": version_text(API_VERSION),
                "Arch": arch,
                "KernelVersion": kernel,
                "MinAPIVersion": version_text(MIN_API_VERSION),
                "Os": "linux",
            },
        }],
        "Version": version,
        "ApiVersion": version_text(API_VERSION),
        "MinAPIVersion": version_text(MIN_API_VERSION),
        "Os": "linux",
        "Arch": arch,
        "KernelVersion": kernel,
    })
}

fn info(ctx: &Context, snapshot: &Snapshot) -> Value {
    let read = |path: &str| std::fs::read_to_string(path).unwrap_or_default();
    let running = snapshot
        .machines
        .iter()
        .filter(|m| m.state() == "running")
        .count();
    let paused = snapshot
        .machines
        .iter()
        .filter(|m| m.state() == "paused")
        .count();
    let total = snapshot.machines.len();
    let os = read("/etc/os-release")
        .lines()
        .find_map(|l| l.strip_prefix("PRETTY_NAME="))
        .map(|v| v.trim_matches('"').to_string())
        .unwrap_or_else(|| "Linux".to_string());
    let mem_total: u64 = read("/proc/meminfo")
        .lines()
        .find_map(|l| l.strip_prefix("MemTotal:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
        .map_or(0, |kb| kb * 1024);
    let machine_id = read("/etc/machine-id").trim().to_string();
    let id = if machine_id.len() == 32 {
        format!(
            "{}-{}-{}-{}-{}",
            &machine_id[..8],
            &machine_id[8..12],
            &machine_id[12..16],
            &machine_id[16..20],
            &machine_id[20..]
        )
    } else {
        machine_id
    };
    let images = snapshot
        .machines
        .iter()
        .filter(|m| m.record.origin != "create")
        .count();
    let mut info = json!({
        "ID": id,
        "Containers": total,
        "ContainersRunning": running,
        "ContainersPaused": paused,
        "ContainersStopped": total - running - paused,
        "Images": images,
        "Driver": "overlay",
        "DriverStatus": [],
        "Plugins": { "Volume": ["local"], "Network": ["bridge", "host", "null"], "Authorization": null, "Log": ["journald"] },
        "MemoryLimit": true,
        "SwapLimit": true,
        "CpuCfsPeriod": true,
        "CpuCfsQuota": true,
        "CPUShares": true,
        "CPUSet": true,
        "PidsLimit": true,
        "IPv4Forwarding": true,
    });
    let rest = json!({
        "Debug": false,
        "NFd": 0,
        "OomKillDisable": false,
        "NGoroutines": 0,
        "SystemTime": util::rfc3339_nano(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_micros() as u64),
            false,
        ),
        "LoggingDriver": "journald",
        "CgroupDriver": "systemd",
        "CgroupVersion": "2",
        "NEventsListener": 0,
        "KernelVersion": read("/proc/sys/kernel/osrelease").trim(),
        "OperatingSystem": os,
        "OSVersion": "",
        "OSType": "linux",
        "Architecture": std::env::consts::ARCH,
        "IndexServerAddress": "https://index.docker.io/v1/",
        "RegistryConfig": null,
        "NCPU": std::thread::available_parallelism().map_or(1, |n| n.get()),
        "MemTotal": mem_total,
        "GenericResources": null,
        "DockerRootDir": ctx.config.state_dir.to_string_lossy(),
        "HttpProxy": "",
        "HttpsProxy": "",
        "NoProxy": "",
        "Name": read("/proc/sys/kernel/hostname").trim(),
        "Labels": [],
        "ExperimentalBuild": false,
        "ServerVersion": env!("CARGO_PKG_VERSION"),
        "Runtimes": { "systemd-nspawn": { "path": "systemd-nspawn" } },
        "DefaultRuntime": "systemd-nspawn",
        "Swarm": {
            "NodeID": "",
            "NodeAddr": "",
            "LocalNodeState": "inactive",
            "ControlAvailable": false,
            "Error": "",
            "RemoteManagers": null,
        },
        "LiveRestoreEnabled": false,
        "Isolation": "",
        "InitBinary": "",
        "SecurityOptions": [],
        "CDISpecDirs": ["/etc/cdi", "/var/run/cdi"],
        "Warnings": null,
    });
    if let (Some(info), Value::Object(rest)) = (info.as_object_mut(), rest) {
        info.extend(rest);
    }
    info
}

/// The keys GET /containers/json takes.
const CONTAINER_FILTERS: &[&str] = &[
    "ancestor",
    "before",
    "expose",
    "exited",
    "health",
    "id",
    "isolation",
    "is-task",
    "label",
    "name",
    "network",
    "publish",
    "since",
    "status",
    "volume",
];

const STATES: &[&str] = &[
    "created",
    "running",
    "paused",
    "restarting",
    "removing",
    "exited",
    "dead",
];

async fn list_containers(ctx: &Arc<Context>, query: &Query) -> Response<Body> {
    let filters = match Filters::parse(query.get("filters")) {
        Ok(f) => f,
        Err(filters::Invalid(message)) => return error(StatusCode::BAD_REQUEST, message),
    };
    if let Err(filters::Invalid(message)) = filters.only(CONTAINER_FILTERS) {
        return error(StatusCode::BAD_REQUEST, message);
    }
    if let Some(states) = filters.values("status") {
        if let Some(bad) = states.iter().find(|s| !STATES.contains(&s.as_str())) {
            return error(
                StatusCode::BAD_REQUEST,
                format!("invalid filter 'status={bad}': invalid value for state ({bad}): must be one of {}", STATES.join(", ")),
            );
        }
    }
    if let Some(health) = filters.values("health") {
        if let Some(bad) = health
            .iter()
            .find(|h| !["starting", "healthy", "unhealthy", "none"].contains(&h.as_str()))
        {
            return error(
                StatusCode::BAD_REQUEST,
                format!("invalid filter 'health={bad}': invalid value for health ({bad}): must be one of none, starting, healthy, unhealthy"),
            );
        }
    }
    let snapshot = match Snapshot::take(ctx).await {
        Ok(s) => s,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    let limit = query
        .get("limit")
        .and_then(|l| l.parse::<usize>().ok())
        .filter(|l| *l > 0);
    // As docker: a limit takes stopped containers too (docker ps -l).
    let all = query.flag("all", false) || filters.values("status").is_some() || limit.is_some();
    let mut machines: Vec<&model::Machine> = snapshot
        .machines
        .iter()
        .filter(|m| all || matches!(m.state(), "running" | "paused" | "restarting"))
        .filter(|m| container_matches(ctx, &snapshot, &filters, m))
        .collect();
    machines.sort_by(|a, b| {
        b.record
            .created
            .cmp(&a.record.created)
            .then(a.name().cmp(b.name()))
    });
    if let Some(limit) = limit {
        machines.truncate(limit);
    }
    let list: Vec<Value> = machines
        .iter()
        .map(|m| m.summary_json(ctx, &snapshot, &ImageInfo::of(ctx, &m.record)))
        .collect();
    json_response(StatusCode::OK, &Value::Array(list))
}

fn container_matches(
    ctx: &Context,
    snapshot: &Snapshot,
    filters: &Filters,
    m: &model::Machine,
) -> bool {
    if !filters.matches("status", m.state()) {
        return false;
    }
    // With the slash and without.
    if !filters.matches("name", &format!("/{}", m.name())) && !filters.matches("name", m.name()) {
        return false;
    }
    if !filters.matches("id", &m.id) {
        return false;
    }
    if !filters.labels("label", &m.record.effective_labels()) {
        return false;
    }
    let health = m
        .summary
        .health
        .as_ref()
        .map_or("none", |h| h.status.as_str());
    if !filters.exact("health", health) {
        return false;
    }
    if let Some(codes) = filters.values("exited") {
        let exited = m.state() == "exited"
            && codes
                .iter()
                .any(|c| c.parse::<i32>().ok() == m.summary.exit_code);
        if !exited {
            return false;
        }
    }
    if let Some(nets) = filters.values("network") {
        let named = |n: &str| {
            nets.contains(n)
                || snapshot
                    .network(n)
                    .is_some_and(|v| nets.iter().any(|w| v.id.starts_with(w.as_str())))
        };
        // A machine on the host's network or on none is listed under host or none, as
        // its NetworkSettings say.
        let joined = if crate::bridge::bridge_kind(&m.record) {
            crate::bridge::networks_of(&m.record)
                .iter()
                .any(|n| named(n))
        } else {
            let mode = m.network_mode(snapshot);
            matches!(mode.as_str(), "host" | "none") && named(&mode)
        };
        if !joined {
            return false;
        }
    }
    if let Some(images) = filters.values("ancestor") {
        let reference = &m.record.reference;
        let familiar = util::familiar(reference);
        if !images
            .iter()
            .any(|i| *i == familiar || i == reference || familiar.starts_with(&format!("{i}:")))
        {
            return false;
        }
    }
    if let Some(volumes) = filters.values("volume") {
        if !m
            .record
            .volumes
            .iter()
            .any(|v| volumes.contains(&v.source) || volumes.contains(&v.target))
        {
            return false;
        }
    }
    if let Some(ports) = filters.values("publish") {
        if !m.record.ports.iter().any(|p| {
            ports.contains(&p.host.to_string())
                || ports.contains(&format!("{}/{}", p.host, p.protocol.name()))
        }) {
            return false;
        }
    }
    if let Some(ports) = filters.values("expose") {
        if !m.record.ports.iter().any(|p| {
            ports.contains(&p.container.to_string())
                || ports.contains(&format!("{}/{}", p.container, p.protocol.name()))
        }) && !ImageInfo::of(ctx, &m.record)
            .exposed_ports
            .iter()
            .any(|e| ports.contains(e) || ports.contains(e.split('/').next().unwrap_or_default()))
        {
            return false;
        }
    }
    if filters
        .values("is-task")
        .is_some_and(|v| v.contains("true"))
    {
        return false;
    }
    let created_of = |key: &str| snapshot.find(key).ok().map(|o| o.record.created);
    if let Some(before) = filters.values("before") {
        if !before
            .iter()
            .all(|b| created_of(b).is_some_and(|c| m.record.created < c))
        {
            return false;
        }
    }
    if let Some(since) = filters.values("since") {
        if !since
            .iter()
            .all(|s| created_of(s).is_some_and(|c| m.record.created > c))
        {
            return false;
        }
    }
    true
}

async fn container_read(ctx: &Arc<Context>, id: &str, what: &str, query: &Query) -> Response<Body> {
    let snapshot = match Snapshot::take(ctx).await {
        Ok(s) => s,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    let machine = match snapshot.find(id) {
        Ok(m) => m,
        Err(e) => return error(lookup_status(&e), e),
    };
    match what {
        "json" => json_response(
            StatusCode::OK,
            &machine.inspect_json(ctx, &snapshot, &ImageInfo::of(ctx, &machine.record)),
        ),
        "top" => {
            if !machine.running() {
                return error(
                    StatusCode::CONFLICT,
                    format!("container {} is not running", machine.id),
                );
            }
            match crate::api::machines::processes(ctx, machine.name()).await {
                Ok(processes) => json_response(
                    StatusCode::OK,
                    &json!({
                        "Titles": ["PID", "USER", "TIME", "COMMAND"],
                        "Processes": processes.iter().map(|p| json!([p.pid.to_string(), p.user, p.time, p.command])).collect::<Vec<_>>(),
                    }),
                ),
                Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
            }
        }
        "logs" => logs(ctx, machine, query).await,
        "stats" => stats(ctx, machine, query).await,
        "changes" | "export" | "archive" => error(
            StatusCode::NOT_IMPLEMENTED,
            format!("/containers/{{id}}/{what} is not served by nspawn's Docker API"),
        ),
        _ => error(StatusCode::NOT_FOUND, "page not found"),
    }
}

/// A time of the API (unix seconds, with a fraction maybe) as journalctl takes it.
fn journal_time(text: Option<&str>) -> Option<String> {
    let text = text.filter(|t| !t.is_empty() && *t != "0")?;
    let seconds = text.split('.').next().unwrap_or(text);
    seconds.parse::<u64>().ok().map(|s| format!("@{s}"))
}

async fn logs(ctx: &Arc<Context>, machine: &model::Machine, query: &Query) -> Response<Body> {
    let stdout = query.flag("stdout", false);
    let stderr = query.flag("stderr", false);
    if !stdout && !stderr {
        return error(
            StatusCode::BAD_REQUEST,
            "Bad parameters: you must choose at least one stream",
        );
    }
    let lines = match query.get("tail") {
        None | Some("all") | Some("") => None,
        Some(n) => n.parse::<u32>().ok(),
    };
    let mut request = crate::api::machines::LogsRequest {
        machine: machine.name().to_string(),
        follow: query.flag("follow", false),
        lines,
        since: journal_time(query.get("since")),
        until: journal_time(query.get("until")),
        timestamps: query.flag("timestamps", false),
        json: true,
        ..Default::default()
    };
    if request.follow && request.lines.is_none() {
        // docker follows from the start without a tail; journalctl from its last lines.
        request.lines = Some(u32::MAX);
    }
    if machine.record.tuning.drops_output() {
        return error(
            StatusCode::NOT_IMPLEMENTED,
            "configured logging driver does not support reading",
        );
    }
    if let Err(e) = crate::api::machines::prepare_logs(ctx, &mut request).await {
        return error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
    }
    let (response, tx) = streaming("application/vnd.docker.multiplexed-stream");
    let query = streams::LogsQuery {
        stream: if stdout { 1 } else { 2 },
        request,
    };
    spawn_stream(streams::logs(query, tx), response)
}

async fn stats(ctx: &Arc<Context>, machine: &model::Machine, query: &Query) -> Response<Body> {
    let pids_limit = machine.record.limits.pids;
    if query.flag("stream", true) {
        let (response, tx) = streaming("application/json");
        tokio::spawn(streams::stats(
            ctx.clone(),
            machine.name().to_string(),
            machine.id.clone(),
            pids_limit,
            tx,
        ));
        return response;
    }
    let name = machine.name();
    let before = if query.flag("one-shot", false) {
        None
    } else {
        let first = streams::Reading::take(ctx, name).await;
        if first.is_some() {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        first
    };
    let now = streams::Reading::take(ctx, name).await;
    json_response(
        StatusCode::OK,
        &streams::stats_json(name, &machine.id, pids_limit, now.as_ref(), before.as_ref()),
    )
}

async fn list_networks(ctx: &Arc<Context>, query: &Query) -> Response<Body> {
    let filters = match Filters::parse(query.get("filters")) {
        Ok(f) => f,
        Err(filters::Invalid(message)) => return error(StatusCode::BAD_REQUEST, message),
    };
    if let Err(filters::Invalid(message)) =
        filters.only(&["dangling", "driver", "id", "label", "name", "scope", "type"])
    {
        return error(StatusCode::BAD_REQUEST, message);
    }
    let snapshot = match Snapshot::take(ctx).await {
        Ok(s) => s,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    let list: Vec<Value> = snapshot
        .network_views()
        .iter()
        .map(|view| model::network_json(&snapshot, view))
        .filter(|n| {
            let text = |k: &str| n[k].as_str().unwrap_or_default().to_string();
            let labels: BTreeMap<String, String> = n["Labels"]
                .as_object()
                .map(|m| {
                    m.iter()
                        .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                        .collect()
                })
                .unwrap_or_default();
            filters.matches("name", &text("Name"))
                && filters.matches("id", &text("Id"))
                && filters.exact("driver", &text("Driver"))
                && filters.exact("scope", "local")
                && filters.labels("label", &labels)
        })
        .collect();
    json_response(StatusCode::OK, &Value::Array(list))
}

fn events(ctx: &Arc<Context>, query: &Query) -> Response<Body> {
    let filters = match Filters::parse(query.get("filters")) {
        Ok(f) => f,
        Err(filters::Invalid(message)) => return error(StatusCode::BAD_REQUEST, message),
    };
    // docker takes any key here, and uses those it knows.
    let since = journal_time(query.get("since"));
    // An end still to come is waited for while the events follow; one gone by ends a
    // window of the past, which without a start has nothing in it.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let end = query
        .get("until")
        .filter(|t| !t.is_empty() && *t != "0")
        .and_then(|t| t.split('.').next().unwrap_or(t).parse::<u64>().ok());
    let (until, deadline) = match end {
        Some(end) if end > now => (None, Some(Duration::from_secs(end - now))),
        Some(end) if since.is_some() => (Some(format!("@{end}")), None),
        Some(_) => (None, Some(Duration::ZERO)),
        None => (None, None),
    };
    let (response, tx) = streaming("application/json");
    spawn_stream(
        streams::events(ctx.clone(), since, until, deadline, filters, tx),
        response,
    )
}

/// The one change served: a signal for a machine's program, through the service as
/// `nspawn kill` sends it, which acme-companion and docker-gen use to have nginx read
/// new certificates and configuration.
async fn kill(ctx: &Arc<Context>, id: &str, query: &Query) -> Response<Body> {
    // The signal first, checked before the container; every error says
    // which container it was for.
    let failed = |status: StatusCode, text: String| {
        error(status, format!("cannot kill container: {id}: {text}"))
    };
    let number = match query.get("signal").filter(|s| !s.is_empty()) {
        None => nix::libc::SIGKILL,
        Some(signal) => match docker_signal(signal) {
            Ok(number) => number,
            Err(text) => return failed(StatusCode::BAD_REQUEST, text),
        },
    };
    let snapshot = match Snapshot::take(ctx).await {
        Ok(s) => s,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    let machine = match snapshot.find(id) {
        Ok(m) => m,
        Err(e) => return failed(lookup_status(&e), e),
    };
    let restarting = machine.state() == "restarting";
    if !machine.running() && !restarting {
        return error(
            StatusCode::CONFLICT,
            format!(
                "cannot kill container: {id}: container {} is not running",
                machine.id
            ),
        );
    }
    // Between two runs of its restart policy no process gets the signal: SIGKILL or the
    // stop signal ends the loop, any signal does without a stop signal of
    // its own, and the rest is answered without doing anything.
    let signal = if restarting {
        if !ends_restarts(machine.record.effective_stop_signal(), number) {
            return respond(StatusCode::NO_CONTENT, "", Bytes::new());
        }
        nix::libc::SIGKILL
    } else {
        number
    };
    match crate::client::kill_machine(machine.name(), &signal.to_string()).await {
        Ok(()) => respond(StatusCode::NO_CONTENT, "", Bytes::new()),
        Err(e) => failed(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

/// The status of a lookup's failure: a prefix of more than one container is the
/// caller's to fix.
fn lookup_status(error: &str) -> StatusCode {
    if error.starts_with("multiple IDs") {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::NOT_FOUND
    }
}

/// moby/sys/signal's ParseSignal, then ValidSignalForPlatform on Linux: a number but 0,
/// or a name of its map with or without SIG, and only the signals of that map.
fn docker_signal(raw: &str) -> Result<i32, String> {
    const NAMES: [(&str, i32); 34] = [
        ("ABRT", 6),
        ("ALRM", 14),
        ("BUS", 7),
        ("CHLD", 17),
        ("CLD", 17),
        ("CONT", 18),
        ("FPE", 8),
        ("HUP", 1),
        ("ILL", 4),
        ("INT", 2),
        ("IO", 29),
        ("IOT", 6),
        ("KILL", 9),
        ("PIPE", 13),
        ("POLL", 29),
        ("PROF", 27),
        ("PWR", 30),
        ("QUIT", 3),
        ("SEGV", 11),
        ("STKFLT", 16),
        ("STOP", 19),
        ("SYS", 31),
        ("TERM", 15),
        ("TRAP", 5),
        ("TSTP", 20),
        ("TTIN", 21),
        ("TTOU", 22),
        ("URG", 23),
        ("USR1", 10),
        ("USR2", 12),
        ("VTALRM", 26),
        ("WINCH", 28),
        ("XCPU", 24),
        ("XFSZ", 25),
    ];
    let (rtmin, rtmax) = (34, 64);
    let invalid = || format!("invalid signal: {raw}");
    let number = match crate::compose::gostr::atoi(raw) {
        Ok(0) => return Err(invalid()),
        Ok(n) => n,
        Err(_) => {
            let upper = raw.to_uppercase();
            let name = upper.strip_prefix("SIG").unwrap_or(&upper);
            let offset = |prefix: &str, sign: char, most: i64| {
                let rest = name.strip_prefix(prefix)?;
                if rest.is_empty() {
                    return Some(0);
                }
                let n = rest.strip_prefix(sign)?;
                (!n.starts_with('0') && n.bytes().all(|b| b.is_ascii_digit()))
                    .then(|| n.parse::<i64>().ok())
                    .flatten()
                    .filter(|n| (1..=most).contains(n))
            };
            NAMES
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, number)| i64::from(*number))
                .or_else(|| offset("RTMIN", '+', 15).map(|n| rtmin + n))
                .or_else(|| offset("RTMAX", '-', 14).map(|n| rtmax - n))
                .ok_or_else(invalid)?
        }
    };
    if !(1..=31).contains(&number) && !(rtmin..=rtmax).contains(&number) {
        return Err(format!("the linux daemon does not support signal {number}"));
    }
    Ok(number as i32)
}

/// killWithSignal: whether `signal` keeps a container from being restarted.
fn ends_restarts(stop_signal: Option<&str>, signal: i32) -> bool {
    signal == nix::libc::SIGKILL
        || stop_signal.is_none_or(|s| crate::api::machines::signal_number(s).ok() == Some(signal))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kill_takes_the_signals_docker_takes() {
        for (raw, number) in [
            ("9", 9),
            ("+9", 9),
            ("KILL", 9),
            ("sigterm", 15),
            ("SIGRTMIN+3", 37),
            ("RTMAX-14", 50),
            ("rtmax", 64),
            ("34", 34),
        ] {
            assert_eq!(docker_signal(raw), Ok(number), "{raw}");
        }
        for (raw, text) in [
            ("0", "invalid signal: 0"),
            ("NOPE", "invalid signal: NOPE"),
            (" 9", "invalid signal:  9"),
            ("RTMIN+16", "invalid signal: RTMIN+16"),
            ("32", "the linux daemon does not support signal 32"),
            ("33", "the linux daemon does not support signal 33"),
            ("-9", "the linux daemon does not support signal -9"),
            ("65", "the linux daemon does not support signal 65"),
        ] {
            assert_eq!(docker_signal(raw), Err(text.to_string()), "{raw}");
        }
        assert_eq!(
            lookup_status("multiple IDs found with provided prefix: ab"),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(lookup_status("No such container: x"), StatusCode::NOT_FOUND);
    }

    #[test]
    fn a_signal_between_restarts_ends_them_as_docker_says() {
        assert!(ends_restarts(None, nix::libc::SIGHUP));
        assert!(ends_restarts(Some("SIGQUIT"), nix::libc::SIGKILL));
        assert!(ends_restarts(Some("SIGQUIT"), nix::libc::SIGQUIT));
        assert!(!ends_restarts(Some("SIGQUIT"), nix::libc::SIGHUP));
    }

    /// Every request the tools made of the Docker API, as recorded by
    /// tests/dockerapi/oracle.sh, is one this API answers.
    #[test]
    fn what_the_tools_ask_is_served() {
        let logs = [
            include_str!("../../tests/dockerapi/requests/caddy.log"),
            include_str!("../../tests/dockerapi/requests/traefik.log"),
            include_str!("../../tests/dockerapi/requests/nginx-proxy.log"),
            include_str!("../../tests/dockerapi/requests/acme.log"),
            include_str!("../../tests/dockerapi/requests/homepage.log"),
            include_str!("../../tests/dockerapi/requests/dozzle.log"),
        ];
        let mut seen = 0;
        for line in logs.iter().flat_map(|l| l.lines()) {
            let (method, target) = line.split_once(' ').unwrap();
            let path = target.split('?').next().unwrap();
            let (version, path) = split_version(path);
            assert!(
                version.is_none_or(|v| v >= MIN_API_VERSION && v <= API_VERSION),
                "{line}"
            );
            let method: Method = method.parse().unwrap();
            let route = classify(&method, path);
            assert!(
                !matches!(route, Route::NotFound | Route::Unserved | Route::ReadOnly),
                "{line}: {route:?}"
            );
            if let Route::Container(_, what) = &route {
                assert!(
                    ["json", "logs", "stats", "top"].contains(&what.as_str()),
                    "{line}"
                );
            }
            seen += 1;
        }
        assert!(seen > 50, "{seen} requests recorded");
    }

    #[test]
    fn routes() {
        let get = Method::GET;
        assert_eq!(classify(&Method::HEAD, "/_ping"), Route::Ping);
        assert_eq!(
            classify(&get, "/containers/web/json"),
            Route::Container("web".into(), "json".into())
        );
        assert_eq!(
            classify(&Method::POST, "/containers/web/kill"),
            Route::Kill("web".into())
        );
        assert_eq!(
            classify(&Method::POST, "/containers/create"),
            Route::ReadOnly
        );
        assert_eq!(
            classify(&Method::POST, "/containers/web/exec"),
            Route::ReadOnly
        );
        assert_eq!(
            classify(&Method::DELETE, "/containers/web"),
            Route::ReadOnly
        );
        assert_eq!(classify(&get, "/images/json"), Route::Unserved);
        assert_eq!(classify(&get, "/volumes"), Route::Unserved);
        assert_eq!(classify(&get, "/nothing/here"), Route::NotFound);
    }

    #[test]
    fn query_values_are_decoded() {
        assert_eq!(decode("a%20b+c%7B%22x%22%7D").unwrap(), "a b c{\"x\"}");
        // As Go's QueryUnescape refuses them.
        for (bad, escape) in [("100%", "%"), ("%zz", "%zz"), ("%+5", "%+5"), ("%2", "%2")] {
            assert_eq!(
                decode(bad),
                Err(format!("invalid URL escape \"{escape}\"")),
                "{bad}"
            );
        }
        let q = query("all=%zz&limit=3&a;b=1");
        assert_eq!(q.get("limit"), Some("3"));
        assert_eq!(q.get("all"), None);
        assert_eq!(q.bad.as_deref(), Some("invalid URL escape \"%zz\""));
        assert_eq!(
            query("a;b=1").bad.as_deref(),
            Some("invalid semicolon separator in query")
        );
        assert!(query("all=1&filters=%7B%7D").bad.is_none());
        assert_eq!(journal_time(Some("1792344103")), Some("@1792344103".into()));
        assert_eq!(
            journal_time(Some("1792344103.5")),
            Some("@1792344103".into())
        );
        assert_eq!(journal_time(Some("0")), None);
        assert_eq!(journal_time(None), None);
    }
}
