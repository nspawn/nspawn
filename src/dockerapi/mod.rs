//! The Docker Engine API, read-only, for the machines that mount docker's socket:
//! reverse proxies that follow container labels, dashboards and log viewers find nspawn's
//! machines through it. It is served by `nspawn api-serve NAME`, which systemd starts
//! from the machine's own socket (`nspawn-api@NAME.socket`) when something connects, and
//! which leaves once nothing has for a minute. Nothing here changes a machine but a kill,
//! which goes to the service as `nspawn kill` does.

pub mod events;
pub mod filters;
pub mod model;
mod streams;
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
                let Ok((stream, _)) = accepted else { continue };
                open.fetch_add(1, Ordering::SeqCst);
                let (ctx, open, last) = (ctx.clone(), open.clone(), last.clone());
                tokio::spawn(async move {
                    let service = hyper::service::service_fn(move |request| {
                        let ctx = ctx.clone();
                        async move { Ok::<_, Infallible>(handle(ctx, request).await) }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
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

/// The query string's parameters, percent-decoded; a key given twice keeps its last
/// value except where all are read.
fn query(request: &Request<Incoming>) -> Vec<(String, String)> {
    request
        .uri()
        .query()
        .unwrap_or_default()
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (decode(k), decode(v))
        })
        .collect()
}

fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(b) => {
                        out.push(b);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

struct Query(Vec<(String, String)>);

impl Query {
    fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Go's strconv.ParseBool, which the API's flags go through; absent is `default`.
    fn flag(&self, key: &str, default: bool) -> bool {
        match self.get(key) {
            None => default,
            Some(v) => matches!(v, "1" | "t" | "T" | "true" | "TRUE" | "True"),
        }
    }
}

async fn handle(ctx: Arc<Context>, request: Request<Incoming>) -> Response<Body> {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let query = Query(query(&request));
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
    match classify(method, path) {
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
                format!("Unrecognised filter value for health: {bad}"),
            );
        }
    }
    let snapshot = match Snapshot::take(ctx).await {
        Ok(s) => s,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    let all = query.flag("all", false) || filters.values("status").is_some();
    let mut machines: Vec<&model::Machine> = snapshot
        .machines
        .iter()
        .filter(|m| all || matches!(m.state(), "running" | "paused" | "restarting"))
        .filter(|m| container_matches(&snapshot, &filters, m))
        .collect();
    machines.sort_by(|a, b| {
        b.record
            .created
            .cmp(&a.record.created)
            .then(a.name().cmp(b.name()))
    });
    if let Some(limit) = query
        .get("limit")
        .and_then(|l| l.parse::<usize>().ok())
        .filter(|l| *l > 0)
    {
        machines.truncate(limit);
    }
    let list: Vec<Value> = machines
        .iter()
        .map(|m| m.summary_json(ctx, &snapshot, &ImageInfo::of(ctx, &m.record)))
        .collect();
    json_response(StatusCode::OK, &Value::Array(list))
}

fn container_matches(snapshot: &Snapshot, filters: &Filters, m: &model::Machine) -> bool {
    if !filters.exact("status", m.state()) {
        return false;
    }
    if !filters.matches("name", &format!("/{}", m.name())) {
        return false;
    }
    if let Some(ids) = filters.values("id") {
        if !ids.iter().any(|i| m.id.starts_with(i.as_str())) {
            return false;
        }
    }
    if !filters.labels("label", &m.record.effective_labels()) {
        return false;
    }
    if let Some(health) = filters.values("health") {
        let status = m
            .summary
            .health
            .as_ref()
            .map_or("none", |h| h.status.as_str());
        if !health.contains(status) {
            return false;
        }
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
        let joined = crate::bridge::bridge_kind(&m.record)
            && crate::bridge::networks_of(&m.record).iter().any(|n| {
                nets.contains(*n)
                    || snapshot
                        .network(n)
                        .is_some_and(|v| nets.iter().any(|w| v.id.starts_with(w.as_str())))
            });
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
        }) && !m
            .record
            .run
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
    let created_of = |key: &str| snapshot.find(key).map(|o| o.record.created);
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
    let Some(machine) = snapshot.find(id) else {
        return error(StatusCode::NOT_FOUND, format!("No such container: {id}"));
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
    tokio::spawn(streams::logs(query, tx));
    response
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
                && filters
                    .values("id")
                    .is_none_or(|ids| ids.iter().any(|i| text("Id").starts_with(i.as_str())))
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
    if let Err(filters::Invalid(message)) = filters.only(events::ACCEPTED) {
        return error(StatusCode::BAD_REQUEST, message);
    }
    let since = journal_time(query.get("since"));
    let mut until = journal_time(query.get("until"));
    // A window of the past needs its start; without one it follows from now.
    if since.is_none() && until.is_some() {
        until = None;
    }
    let (response, tx) = streaming("application/json");
    tokio::spawn(streams::events(ctx.clone(), since, until, filters, tx));
    response
}

/// The one change served: a signal for a machine's program, through the service as
/// `nspawn kill` sends it, which acme-companion and docker-gen use to have nginx read
/// new certificates and configuration.
async fn kill(ctx: &Arc<Context>, id: &str, query: &Query) -> Response<Body> {
    let snapshot = match Snapshot::take(ctx).await {
        Ok(s) => s,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    let Some(machine) = snapshot.find(id) else {
        return error(StatusCode::NOT_FOUND, format!("No such container: {id}"));
    };
    if !machine.running() {
        return error(
            StatusCode::CONFLICT,
            format!(
                "cannot kill container: {id}: container {} is not running",
                machine.id
            ),
        );
    }
    let signal = query
        .get("signal")
        .filter(|s| !s.is_empty())
        .unwrap_or("KILL");
    if let Err(e) = crate::api::machines::signal_number(signal) {
        return error(StatusCode::BAD_REQUEST, format!("{e:#}"));
    }
    match crate::client::kill_machine(machine.name(), signal).await {
        Ok(()) => respond(StatusCode::NO_CONTENT, "", Bytes::new()),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every request the tools made of docker's API, as recorded by
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
        assert_eq!(decode("a%20b+c%7B%22x%22%7D"), "a b c{\"x\"}");
        assert_eq!(decode("100%"), "100%");
        assert_eq!(decode("%zz"), "%zz");
        assert_eq!(journal_time(Some("1792344103")), Some("@1792344103".into()));
        assert_eq!(
            journal_time(Some("1792344103.5")),
            Some("@1792344103".into())
        );
        assert_eq!(journal_time(Some("0")), None);
        assert_eq!(journal_time(None), None);
    }
}
