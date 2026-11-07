//! The changes the Docker API makes to containers: create, start, stop, restart, pause,
//! unpause, wait, update, remove and prune, each through the service's method on the
//! bus, as the command line asks for them. The name of the error the bus answers with
//! decides the status; the words are the engine's, with the container's ID where it
//! gives one.

use std::sync::Arc;

use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::{HeaderMap, Response, StatusCode};
use serde_json::{json, Value};
use zbus::zvariant::Value as Bus;

use super::create::{self, Object, Opt, Opts, Refusal};
use super::filters::{Filters, Invalid};
use super::model::{Machine, Snapshot};
use super::util::container_id;
use super::{error, json_response, lookup_status, respond, Body, Query};
use crate::api::Context;
use crate::client::{self, Client, Options};
use crate::store::ImageRecord;

/// A change of one container, by the last word of its path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    Start,
    Stop,
    Restart,
    Pause,
    Unpause,
    Wait,
    Update,
    Resize,
}

impl Act {
    pub fn of(word: &str) -> Option<Act> {
        Some(match word {
            "start" => Act::Start,
            "stop" => Act::Stop,
            "restart" => Act::Restart,
            "pause" => Act::Pause,
            "unpause" => Act::Unpause,
            "wait" => Act::Wait,
            "update" => Act::Update,
            "resize" => Act::Resize,
            _ => return None,
        })
    }
}

/// What a request sent besides its path and query.
pub struct Sent {
    pub headers: HeaderMap,
    pub body: Incoming,
    /// The connection, for a request that asked to upgrade it.
    pub upgrade: Option<hyper::upgrade::OnUpgrade>,
    /// The API version its path names.
    pub version: Option<(u32, u32)>,
    /// Whom its place in the socket's limits is counted for.
    pub holder: super::access::Holder,
}

/// An answer that is no success: its status, and the engine's words unless it has none
/// (304).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer(pub StatusCode, pub Option<String>);

impl Answer {
    fn says(status: StatusCode, text: impl Into<String>) -> Answer {
        Answer(status, Some(text.into()))
    }

    pub fn respond(self) -> Response<Body> {
        match self.1 {
            Some(text) => error(self.0, text),
            None => empty(self.0),
        }
    }
}

impl From<Refusal> for Answer {
    fn from(refusal: Refusal) -> Answer {
        Answer(refusal.status, Some(refusal.message))
    }
}

type Served = Result<Response<Body>, Answer>;

fn empty(status: StatusCode) -> Response<Body> {
    respond(status, "", Bytes::new())
}

fn no_such(key: &str) -> String {
    format!("No such container: {key}")
}

fn failed(e: impl std::fmt::Display) -> Answer {
    Answer::says(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

/// The status of an error of the bus, by its name.
pub fn status_of(kind: Option<&str>) -> StatusCode {
    match kind {
        Some("NoSuchMachine" | "NoSuchImage" | "NoSuchNetwork" | "NoSuchVolume" | "NoSuchPath") => {
            StatusCode::NOT_FOUND
        }
        Some("Exists" | "InUse" | "NotRunning" | "Paused") => StatusCode::CONFLICT,
        Some("AlreadyRunning") => StatusCode::NOT_MODIFIED,
        Some("Invalid") => StatusCode::BAD_REQUEST,
        Some("NotAuthorized" | "AccessDenied") => StatusCode::FORBIDDEN,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// An error of the bus as it is: its status, and its message unless that is 304.
fn as_is(kind: Option<&str>, message: &str) -> Answer {
    match status_of(kind) {
        StatusCode::NOT_MODIFIED => Answer(StatusCode::NOT_MODIFIED, None),
        status => Answer::says(status, message),
    }
}

/// What a change needs of the container a request names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Its machine's name, which the bus knows it by.
    pub name: String,
    /// Its container ID.
    pub id: String,
    /// Running as the engine says it: paused or between two runs too.
    pub running: bool,
    pub paused: bool,
    /// Between two runs of its restart policy.
    pub restarting: bool,
    pub remove_on_exit: bool,
}

impl Target {
    fn of(m: &Machine) -> Target {
        Target {
            name: m.name().to_string(),
            id: m.id.clone(),
            running: m.running() || m.state() == "restarting",
            paused: m.state() == "paused",
            restarting: m.state() == "restarting",
            remove_on_exit: m.record.remove_on_exit,
        }
    }
}

async fn target(ctx: &Context, key: &str) -> Result<Target, Answer> {
    let snapshot = Snapshot::take(ctx)
        .await
        .map_err(|e| failed(format!("{e:#}")))?;
    let found = snapshot
        .find(key)
        .map_err(|e| Answer::says(lookup_status(&e), e))?;
    Ok(Target::of(found))
}

async fn connect() -> Result<Client, Answer> {
    Client::connect()
        .await
        .map_err(|e| failed(format!("{e:#}")))
}

/// A request's body, up to what a request of the API ever holds. Its JSON is read whole
/// into values some thirty times its size, under the server's MemoryMax: a megabyte is
/// more than a program's whole environment can be.
pub(super) async fn read_body(body: Incoming) -> Result<Bytes, Answer> {
    const MOST: usize = 1024 * 1024;
    match http_body_util::Limited::new(body, MOST).collect().await {
        Ok(collected) => Ok(collected.to_bytes()),
        Err(e) => Err(Answer::says(
            StatusCode::BAD_REQUEST,
            format!("reading the request body: {e}"),
        )),
    }
}

/// The length of a request's body as Go's server has it: -1 when it comes in chunks,
/// 0 when nothing says.
fn body_length(headers: &HeaderMap) -> i64 {
    let chunked = headers
        .get_all(hyper::header::TRANSFER_ENCODING)
        .iter()
        .any(|v| {
            v.to_str()
                .is_ok_and(|v| v.to_ascii_lowercase().contains("chunked"))
        });
    if chunked {
        return -1;
    }
    headers
        .get(hyper::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
}

/// CheckForJSON: no Content-Type is fine without a body; any other than
/// application/json is not.
pub fn check_json(headers: &HeaderMap) -> Result<(), Answer> {
    let given = headers
        .get(hyper::header::CONTENT_TYPE)
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
        .unwrap_or_default();
    if given.is_empty() && body_length(headers) == 0 {
        return Ok(());
    }
    let media = given
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let bad = |text: String| Err(Answer::says(StatusCode::BAD_REQUEST, text));
    if media.is_empty() {
        return bad(format!(
            "malformed Content-Type header ({given}): mime: no media type"
        ));
    }
    if !media.contains('/') {
        return bad(format!(
            "malformed Content-Type header ({given}): mime: expected slash after first token"
        ));
    }
    if media != "application/json" {
        return bad(format!(
            "unsupported Content-Type header ({given}): must be 'application/json'"
        ));
    }
    Ok(())
}

/// A start with a body is refused: one was taken by API versions long gone.
pub fn start_body(headers: &HeaderMap) -> Option<Answer> {
    let length = body_length(headers);
    (length > 7 || length == -1).then(|| {
        Answer::says(
            StatusCode::BAD_REQUEST,
            "starting container with non-empty request body was deprecated since API v1.22 and removed in v1.24",
        )
    })
}

/// A change of the container `key` names.
pub async fn act(
    ctx: &Arc<Context>,
    key: &str,
    act: Act,
    query: &Query,
    sent: Sent,
) -> Response<Body> {
    let served = match act {
        Act::Start => start(ctx, key).await,
        Act::Stop => stop(ctx, key, query).await,
        Act::Restart => restart(ctx, key, query).await,
        Act::Pause => pause(ctx, key, true).await,
        Act::Unpause => pause(ctx, key, false).await,
        Act::Update => update(ctx, key, sent).await,
        Act::Wait => wait(ctx, key, query, sent.holder).await,
        Act::Resize => resize(ctx, key, query).await,
    };
    served.unwrap_or_else(Answer::respond)
}

/// What a failed start answers.
pub fn start_failed(kind: Option<&str>, key: &str, message: &str) -> Answer {
    match kind {
        Some("Paused") => Answer::says(
            StatusCode::CONFLICT,
            "cannot start a paused container, try unpause instead",
        ),
        Some("NoSuchMachine") => Answer::says(StatusCode::NOT_FOUND, no_such(key)),
        kind => as_is(kind, message),
    }
}

/// Whether a start has nothing to do: the engine counts a container between two runs of
/// its restart policy as running.
pub fn start_not_needed(target: &Target) -> bool {
    target.restarting
}

/// Whether a start of `target` runs it with the input an attach keeps for it, if one
/// does: any but a paused one, which is answered as such. One that looks running may be
/// a run whose program ended, its machine going away, after a wait for that end
/// answered: RunMachine waits for it, and answers one that runs as StartMachine does.
pub fn takes_input(target: &Target) -> bool {
    !target.paused
}

/// Whether a restart of `target` only starts it, with the input an attach keeps for it:
/// one that does not run, or whose program ended (`program_ended`).
pub fn restart_only_starts(target: &Target, program_ended: bool) -> bool {
    !target.running || program_ended
}

/// Starts `name`: attached, with the input an attach of this server keeps for it, or
/// with StartMachine.
async fn start_one(client: &Client, name: &str, input: bool) -> anyhow::Result<()> {
    if input {
        if let Some(outcome) = super::attach::start_with_input(client, name).await {
            return outcome;
        }
    }
    client
        .manager
        .start_machine(name, Options::new())
        .await
        .map(drop)
        .map_err(client::error)
}

async fn start(ctx: &Context, key: &str) -> Served {
    let target = target(ctx, key).await?;
    if start_not_needed(&target) {
        return Err(Answer(StatusCode::NOT_MODIFIED, None));
    }
    let client = connect().await?;
    // One that runs or is paused is answered as such, its binds made at its own start.
    let made = if target.running || target.paused {
        Ok(())
    } else {
        make_sources(ctx, &target.name)
    };
    let outcome = match made {
        Ok(()) => start_one(&client, &target.name, takes_input(&target)).await,
        Err(e) => Err(e),
    };
    match outcome {
        Ok(()) => Ok(empty(StatusCode::NO_CONTENT)),
        Err(e) => {
            let kind = client::error_name(&e);
            // A container removed when it ends goes when it cannot start, and a client
            // waiting for its removal hears of it.
            if target.remove_on_exit
                && !matches!(kind, Some("AlreadyRunning" | "Paused" | "NoSuchMachine"))
            {
                remove_quietly(&client, &target.name).await;
            }
            Err(start_failed(kind, key, &format!("{e:#}")))
        }
    }
}

async fn remove_quietly(client: &Client, name: &str) {
    let names = [name.to_string()];
    let mut options = Options::new();
    options.insert("force", Bus::from(true));
    let removal = client
        .run_job_silent(|| client.manager.remove_machines(&names, options), true)
        .await;
    if let Err(e) = removal {
        eprintln!("warning: removing {name}, whose start failed: {e:#}");
    }
}

/// Whether a number is one of Linux's signals, 1 to SIGRTMAX.
fn is_signal(number: i64) -> bool {
    (1..=64).contains(&number)
}

/// `t` of stop and restart: seconds, any below 0 for no limit, as strconv.Atoi reads
/// them.
pub fn stop_timeout(text: Option<&str>) -> Result<Option<u64>, Answer> {
    let Some(text) = text.filter(|t| !t.is_empty()) else {
        return Ok(None);
    };
    match crate::compose::gostr::atoi(text) {
        Ok(n) => Ok(Some(
            u64::try_from(n).unwrap_or(crate::policy::NO_STOP_TIMEOUT),
        )),
        Err(e) => Err(failed(crate::compose::gostr::num_error("Atoi", text, e))),
    }
}

/// What a failed stop answers: the engine gives every failure of a container that runs
/// as its own, a bad signal too.
pub fn stop_failed(kind: Option<&str>, key: &str, message: &str) -> Answer {
    match kind {
        Some("NoSuchMachine") => Answer::says(StatusCode::NOT_FOUND, no_such(key)),
        Some("NotRunning") => Answer(StatusCode::NOT_MODIFIED, None),
        Some(k @ ("NotAuthorized" | "AccessDenied")) => as_is(Some(k), message),
        _ => failed(format!("cannot stop container: {key}: {message}")),
    }
}

async fn stop(ctx: &Context, key: &str, query: &Query) -> Served {
    let timeout = stop_timeout(query.get("t"))?;
    let target = target(ctx, key).await?;
    let mut options = Options::new();
    if let Some(timeout) = timeout {
        options.insert("timeout", Bus::from(timeout));
    }
    if let Some(raw) = query.get("signal").filter(|s| !s.is_empty()) {
        match super::parse_signal(raw) {
            Ok(number) if is_signal(number) => {
                options.insert("signal", Bus::from(number.to_string()));
            }
            // A number no signal has cannot be sent: the stop goes on as the machine's
            // own, its SIGKILL after the timeout.
            Ok(_) => {}
            // A stopped container is stopped already whatever the signal says.
            Err(text) if target.running => return Err(stop_failed(None, key, &text)),
            Err(_) => return Ok(empty(StatusCode::NOT_MODIFIED)),
        }
    }
    let client = connect().await?;
    let outcome = client
        .manager
        .stop_machine(&target.name, options)
        .await
        .map_err(client::error);
    match outcome {
        Ok((outcome, _)) if outcome == "was-not-running" => Ok(empty(StatusCode::NOT_MODIFIED)),
        Ok(_) => Ok(empty(StatusCode::NO_CONTENT)),
        Err(e) => Err(stop_failed(client::error_name(&e), key, &format!("{e:#}"))),
    }
}

/// What a failed restart answers: the engine gives every failure but a missing
/// container as its own.
pub fn restart_failed(kind: Option<&str>, key: &str, message: &str) -> Answer {
    match kind {
        Some("NoSuchMachine") => Answer::says(StatusCode::NOT_FOUND, no_such(key)),
        Some(k @ ("NotAuthorized" | "AccessDenied")) => as_is(Some(k), message),
        _ => failed(format!("Cannot restart container {key}: {message}")),
    }
}

async fn restart(ctx: &Context, key: &str, query: &Query) -> Served {
    let timeout = stop_timeout(query.get("t"))?;
    let target = target(ctx, key).await?;
    let mut options = Options::new();
    if let Some(timeout) = timeout {
        options.insert("timeout", Bus::from(timeout));
    }
    // Only a stop reads the signal: a container that does not run is started alone.
    if let Some(raw) = query
        .get("signal")
        .filter(|s| !s.is_empty() && target.running)
    {
        let number = super::parse_signal(raw).map_err(|text| restart_failed(None, key, &text))?;
        if is_signal(number) {
            options.insert("signal", Bus::from(number.to_string()));
        }
    }
    let client = connect().await?;
    make_sources(ctx, &target.name).map_err(|e| restart_failed(None, key, &format!("{e:#}")))?;
    // A container that does not run, or whose program ended, is only started, with the
    // input an attach keeps for it, as a start would.
    let only_starts = super::attach::input_pending(&target.name)
        && restart_only_starts(
            &target,
            target.running && super::attach::program_ended(ctx, &target.name).await,
        );
    let attached = if only_starts {
        super::attach::start_with_input(&client, &target.name).await
    } else {
        None
    };
    match attached {
        Some(outcome) => outcome,
        None => client
            .manager
            .restart_machine(&target.name, options)
            .await
            .map(drop)
            .map_err(client::error),
    }
    .map_err(|e| restart_failed(client::error_name(&e), key, &format!("{e:#}")))?;
    Ok(empty(StatusCode::NO_CONTENT))
}

/// `h` or `w` of a resize, as the engine reads it (Uint32Value), and its words for one
/// that is no such number.
pub fn resize_value(text: Option<&str>, what: &str) -> Result<u32, Answer> {
    let raw = text.unwrap_or_default();
    let bad = |why: &str| {
        Answer::says(
            StatusCode::BAD_REQUEST,
            format!(
                "invalid resize {what} {}: {why}",
                crate::compose::gostr::quote(raw)
            ),
        )
    };
    let (digits, negative) = match raw.strip_prefix('-') {
        Some(digits) => (digits, true),
        None => (raw, false),
    };
    if digits.is_empty() || digits.starts_with('+') {
        return Err(bad("invalid syntax"));
    }
    let mut value: u64 = 0;
    for c in digits.bytes() {
        if !c.is_ascii_digit() {
            return Err(bad("invalid syntax"));
        }
        value = value * 10 + u64::from(c - b'0');
        if value > u64::from(u32::MAX) {
            return Err(bad("value out of range"));
        }
    }
    if negative {
        return Err(bad("value out of range"));
    }
    Ok(value as u32)
}

/// What a resize does with a container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resizing {
    /// It runs: the terminal this server holds of it gets the size, if it holds one.
    Terminal,
    /// It does not run, and an attach keeps it an input with a terminal: the size is the
    /// start's.
    Kept,
    /// It does not run: the engine's 409.
    NotRunning,
}

pub fn resizing(target: &Target, terminal_pending: bool) -> Resizing {
    if target.running || target.paused {
        Resizing::Terminal
    } else if terminal_pending {
        Resizing::Kept
    } else {
        Resizing::NotRunning
    }
}

/// POST /containers/{id}/resize: the size of the terminal of its run, rows `h` and
/// columns `w`.
async fn resize(ctx: &Context, key: &str, query: &Query) -> Served {
    let rows = resize_value(query.get("h"), "height")?;
    let cols = resize_value(query.get("w"), "width")?;
    let target = target(ctx, key).await?;
    let clamp = |n: u32| u16::try_from(n).unwrap_or(u16::MAX);
    let (rows, cols) = (clamp(rows), clamp(cols));
    match resizing(&target, super::attach::terminal_pending(&target.name)) {
        // One that runs without a terminal this server holds has nothing to resize.
        Resizing::Terminal => {
            super::attach::resize_terminal(&target.name, rows, cols).map_err(|e| {
                failed(format!(
                    "resizing the terminal of container {}: {e}",
                    target.id
                ))
            })?;
        }
        Resizing::Kept => super::attach::remember_size(&target.name, rows, cols),
        Resizing::NotRunning => {
            return Err(Answer::says(
                StatusCode::CONFLICT,
                format!("container {} is not running", target.id),
            ))
        }
    }
    Ok(empty(StatusCode::OK))
}

/// The size a create gives the terminal of a container that has one: ConsoleSize, rows
/// and columns, when neither is 0.
pub fn console_size(body: &Value) -> Option<(u16, u16)> {
    if body.get("Tty").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let size = body.pointer("/HostConfig/ConsoleSize")?.as_array()?;
    let side = |n: Option<&Value>| {
        n.and_then(Value::as_u64)
            .filter(|n| *n > 0)
            .map(|n| u16::try_from(n).unwrap_or(u16::MAX))
    };
    Some((side(size.first())?, side(size.get(1))?))
}

/// What a failed pause answers.
pub fn pause_failed(kind: Option<&str>, target: &Target, key: &str, message: &str) -> Answer {
    let id = &target.id;
    match kind {
        Some("Paused") => Answer::says(
            StatusCode::CONFLICT,
            format!("Container {id} is already paused"),
        ),
        Some("NotRunning") => Answer::says(
            StatusCode::CONFLICT,
            format!("container {id} is not running"),
        ),
        Some("NoSuchMachine") => Answer::says(StatusCode::NOT_FOUND, no_such(key)),
        Some(k @ ("Invalid" | "NotAuthorized" | "AccessDenied")) => as_is(Some(k), message),
        _ => failed(format!("cannot pause container {id}: {message}")),
    }
}

/// What a failed unpause answers: one that does not run is not paused either.
pub fn unpause_failed(kind: Option<&str>, target: &Target, key: &str, message: &str) -> Answer {
    let id = &target.id;
    match kind {
        Some("NotPaused" | "NotRunning") => failed(format!("Container {id} is not paused")),
        Some("NoSuchMachine") => Answer::says(StatusCode::NOT_FOUND, no_such(key)),
        Some(k @ ("Invalid" | "NotAuthorized" | "AccessDenied")) => as_is(Some(k), message),
        _ => failed(format!("Cannot unpause container {id}: {message}")),
    }
}

async fn pause(ctx: &Context, key: &str, on: bool) -> Served {
    let target = target(ctx, key).await?;
    let client = connect().await?;
    let outcome = if on {
        client.manager.pause_machine(&target.name).await
    } else {
        client.manager.unpause_machine(&target.name).await
    }
    .map_err(client::error);
    match outcome {
        Ok(()) => Ok(empty(StatusCode::NO_CONTENT)),
        Err(e) => {
            let (kind, message) = (client::error_name(&e), format!("{e:#}"));
            Err(if on {
                pause_failed(kind, &target, key, &message)
            } else {
                unpause_failed(kind, &target, key, &message)
            })
        }
    }
}

/// UpdateConfig as UpdateMachine's options: what is not zero of the limits and the
/// restart policy, and a warning for anything else that is not zero.
pub fn update_options(body: &Object) -> Result<(Opts, Vec<String>), Refusal> {
    if body.int("NanoCpus")? > 0 {
        if body.int("CpuPeriod")? > 0 {
            return Err(create::bad(
                "Conflicting options: Nano CPUs and CPU Period cannot both be set",
            ));
        }
        if body.int("CpuQuota")? > 0 {
            return Err(create::bad(
                "Conflicting options: Nano CPUs and CPU Quota cannot both be set",
            ));
        }
    }
    let policy = body.object("RestartPolicy", "RestartPolicy")?;
    let restart = create::restart_text(&policy)?;
    let (mut opts, mut warnings) = create::resources(body, false)?;
    if let Some(restart) = restart {
        if let Some(warning) = create::retry_count_warning(&restart) {
            warnings.push(warning.to_string());
        }
        opts.insert("restart", Opt::Text(restart));
    }
    Ok((opts, warnings))
}

/// What a failed update answers.
pub fn update_failed(kind: Option<&str>, target: &Target, key: &str, message: &str) -> Answer {
    match kind {
        Some("NoSuchMachine") => Answer::says(StatusCode::NOT_FOUND, no_such(key)),
        kind => Answer::says(
            match status_of(kind) {
                StatusCode::NOT_MODIFIED => StatusCode::INTERNAL_SERVER_ERROR,
                status => status,
            },
            format!("Cannot update container {}: {message}", target.id),
        ),
    }
}

/// The words of an update refused because the container is removed when it ends.
fn removed_at_end(target: &Target) -> Answer {
    Answer::says(
        StatusCode::CONFLICT,
        format!(
            "Cannot update container {}: Restart policy cannot be updated because AutoRemove is enabled for the container",
            target.id
        ),
    )
}

async fn update(ctx: &Context, key: &str, sent: Sent) -> Served {
    check_json(&sent.headers)?;
    let length = body_length(&sent.headers);
    let bytes = read_body(sent.body).await?;
    // No body is no change, as the engine reads one.
    let body = if bytes.is_empty() && length != -1 {
        json!({})
    } else {
        create::decode(&bytes)?
    };
    let object = Object::of(&body, "container.UpdateConfig")?;
    let (opts, warnings) = update_options(&object)?;
    let target = target(ctx, key).await?;
    if target.remove_on_exit
        && opts
            .get("restart")
            .is_some_and(|r| *r != Opt::Text("no".to_string()))
    {
        return Err(removed_at_end(&target));
    }
    if !opts.is_empty() {
        let client = connect().await?;
        client
            .manager
            .update_machine(&target.name, create::to_bus(&opts))
            .await
            .map_err(client::error)
            .map_err(|e| update_failed(client::error_name(&e), &target, key, &format!("{e:#}")))?;
    }
    Ok(json_response(
        StatusCode::OK,
        &json!({ "Warnings": if warnings.is_empty() { Value::Null } else { json!(warnings) } }),
    ))
}

/// What a wait whose call failed answers, before any header.
pub fn wait_failed(kind: Option<&str>, key: &str, message: &str) -> Answer {
    match kind {
        Some("NoSuchMachine") => Answer::says(StatusCode::NOT_FOUND, no_such(key)),
        kind => as_is(kind, message),
    }
}

/// The condition of a wait as the bus names it; the engine's words for one it has not.
pub fn wait_condition(text: Option<&str>) -> Result<&str, Answer> {
    match text.unwrap_or_default() {
        "" => Ok("not-running"),
        c @ ("not-running" | "next-exit" | "removed") => Ok(c),
        other => Err(Answer::says(
            StatusCode::BAD_REQUEST,
            format!("invalid condition: {}", crate::compose::gostr::quote(other)),
        )),
    }
}

/// The body of a wait: the exit code, or why there is none.
pub fn wait_body(outcome: Result<i32, String>) -> String {
    let value = match outcome {
        Ok(code) => json!({ "StatusCode": code }),
        Err(message) => json!({ "Error": { "Message": message }, "StatusCode": -1 }),
    };
    format!("{value}\n")
}

/// POST /containers/{id}/wait: the headers once the bus took the wait, the body when
/// the condition holds. The client going away drops the wait with the bus connection
/// it was asked on.
async fn wait(ctx: &Context, key: &str, query: &Query, holder: super::access::Holder) -> Served {
    let condition = wait_condition(query.get("condition"))?;
    let target = target(ctx, key).await?;
    let Some(permit) = super::WAITS.take(holder) else {
        return Err(Answer::says(
            StatusCode::SERVICE_UNAVAILABLE,
            "too many waits are open on this socket",
        ));
    };
    let client = connect().await?;
    let watch = client
        .watch_jobs_quiet(true)
        .await
        .map_err(|e| failed(format!("{e:#}")))?;
    let mut options = Options::new();
    options.insert("condition", Bus::from(condition));
    let job = client
        .manager
        .wait_machine(&target.name, options)
        .await
        .map_err(client::error)
        .map_err(|e| wait_failed(client::error_name(&e), key, &format!("{e:#}")))?;
    let (response, tx) = super::streaming("application/json");
    let name = target.name.clone();
    tokio::spawn(async move {
        let _held = (client, permit);
        tokio::select! {
            outcome = watch.finish(job) => {
                // The output first, as the engine gives it: an attach of this socket
                // hands over what the run wrote before the code comes.
                super::attach::drained(&name).await;
                let outcome = outcome
                    .map(|result| {
                        result
                            .get("exit_code")
                            .and_then(|v| i32::try_from(v.clone()).ok())
                            .unwrap_or(0)
                    })
                    .map_err(|e| format!("{e:#}"));
                let _ = tx.send(Bytes::from(wait_body(outcome))).await;
            }
            _ = tx.closed() => {}
        }
    });
    Ok(response)
}

/// What a failed removal answers.
pub fn remove_failed(
    kind: Option<&str>,
    target: &Target,
    key: &str,
    force: bool,
    message: &str,
) -> Answer {
    let refused = |why: &str| format!("cannot remove container \"{key}\": {why}");
    match kind {
        // Without force, a container that runs is refused for that before anything else;
        // with it, the service says why it stayed.
        Some("InUse") if target.paused && !force => Answer::says(
            StatusCode::CONFLICT,
            refused("container is paused and must be unpaused first"),
        ),
        Some("InUse") if target.running && !force => Answer::says(
            StatusCode::CONFLICT,
            refused("container is running: stop the container before removing or force remove"),
        ),
        Some("NoSuchMachine") => Answer::says(StatusCode::NOT_FOUND, no_such(key)),
        kind => Answer::says(
            match status_of(kind) {
                StatusCode::NOT_MODIFIED => StatusCode::INTERNAL_SERVER_ERROR,
                status => status,
            },
            refused(message),
        ),
    }
}

/// Whether a forced removal that found nothing to remove did what it was asked: a
/// container removed when its run ends goes on its own once the removal stops it.
pub fn gone_at_its_end(kind: Option<&str>, target: &Target, force: bool) -> bool {
    force && target.running && target.remove_on_exit && kind == Some("NoSuchMachine")
}

/// DELETE /containers/{id}: RemoveMachines, with force when asked. `v` asks for the
/// container's anonymous volumes to go too, and nspawn makes none; `link` asks for a
/// legacy link of that name instead, and nspawn has none: the answer is the engine's
/// for a container's own name.
pub async fn remove(ctx: &Arc<Context>, key: &str, query: &Query) -> Response<Body> {
    let served: Served = async {
        let target = target(ctx, key).await?;
        if query.flag("link", false) {
            return Err(failed(
                "Conflict, cannot remove the default link name of the container",
            ));
        }
        let client = connect().await?;
        let names = [target.name.clone()];
        let force = query.flag("force", false);
        let mut options = Options::new();
        options.insert("force", Bus::from(force));
        let removal = client
            .run_job_silent(|| client.manager.remove_machines(&names, options), true)
            .await;
        if let Err(e) = removal {
            let kind = client::error_name(&e);
            if !gone_at_its_end(kind, &target, force) {
                return Err(remove_failed(kind, &target, key, force, &format!("{e:#}")));
            }
        }
        Ok(empty(StatusCode::NO_CONTENT))
    }
    .await;
    served.unwrap_or_else(Answer::respond)
}

/// The filters of POST /containers/prune as PruneMachines' options: until (a time or
/// a duration before `now`, as the engine reads it; those made at it or before), label
/// and label!. Only containers made from an image go: a pulled or built record that ran
/// is the image as well.
pub fn prune_options(filters: &Filters, now: f64, offset: i64) -> Result<Opts, Answer> {
    let bad = |text: String| Answer::says(StatusCode::BAD_REQUEST, text);
    filters
        .only(&["label", "label!", "until"])
        .map_err(|Invalid(text)| bad(text))?;
    let mut opts = Opts::from([("made", Opt::Flag(true))]);
    if let Some(values) = filters.values("until") {
        if values.len() > 1 {
            return Err(bad("more than one until filter specified".to_string()));
        }
        let value = values.iter().next().map(String::as_str).unwrap_or_default();
        let stamp = crate::compose::gotime::get_timestamp(value, now, offset).map_err(bad)?;
        let seconds = stamp.split('.').next().unwrap_or_default();
        let seconds: i64 = seconds.parse().map_err(|_| {
            bad(format!(
                "failed to parse value as time or duration: {}",
                crate::compose::gostr::quote(value)
            ))
        })?;
        // The bus takes those made before a second: the next one.
        opts.insert(
            "until",
            Opt::Unsigned(u64::try_from(seconds.saturating_add(1)).unwrap_or(0)),
        );
    }
    for (key, option) in [("label", "labels"), ("label!", "labels_not")] {
        if let Some(values) = filters.values(key) {
            opts.insert(option, Opt::Texts(values.iter().cloned().collect()));
        }
    }
    Ok(opts)
}

/// The IDs of the machines a prune removed, the last made first.
pub fn deleted_ids(snapshot: &Snapshot, removed: &[String]) -> Vec<String> {
    let mut gone: Vec<&Machine> = snapshot
        .machines
        .iter()
        .filter(|m| removed.iter().any(|r| r == m.name()))
        .collect();
    gone.sort_by(|a, b| {
        b.record
            .created
            .cmp(&a.record.created)
            .then(a.name().cmp(b.name()))
    });
    gone.into_iter().map(|m| m.id.clone()).collect()
}

/// The body of a prune.
pub fn prune_body(ids: Vec<String>, reclaimed: u64) -> Value {
    json!({
        "ContainersDeleted": if ids.is_empty() { Value::Null } else { json!(ids) },
        "SpaceReclaimed": reclaimed,
    })
}

/// POST /containers/prune: every stopped container the filters select. One that could
/// not go is left, as the engine leaves it.
pub async fn prune(ctx: &Arc<Context>, query: &Query) -> Response<Body> {
    let served: Served = async {
        let filters = Filters::parse(query.get("filters"))
            .map_err(|Invalid(text)| Answer::says(StatusCode::BAD_REQUEST, text))?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        let offset = crate::compose::gotime::local_offset(now as i64);
        let opts = prune_options(&filters, now, offset)?;
        let snapshot = Snapshot::take(ctx)
            .await
            .map_err(|e| failed(format!("{e:#}")))?;
        let client = connect().await?;
        let outcome = client
            .run_job_silent(
                || client.manager.prune_machines(create::to_bus(&opts)),
                true,
            )
            .await;
        let result = match outcome {
            Ok(result) => result,
            Err(e) => match e.downcast::<client::JobFailed>() {
                Ok(partly) => {
                    eprintln!("warning: pruning containers: {}", partly.error);
                    partly.result
                }
                Err(e) => return Err(as_is(client::error_name(&e), &format!("{e:#}"))),
            },
        };
        let ids = deleted_ids(&snapshot, &client::strings(&result, "removed"));
        Ok(json_response(
            StatusCode::OK,
            &prune_body(ids, client::u64(&result, "reclaimed")),
        ))
    }
    .await;
    served.unwrap_or_else(Answer::respond)
}

/// The host as a create looks things up in it.
struct Live<'a>(&'a Snapshot);

impl create::World for Live<'_> {
    fn container(&self, key: &str) -> Option<create::Linked> {
        let m = self.0.find(key).ok()?;
        let record = &m.record;
        let address = crate::bridge::bridge_kind(record)
            .then(|| {
                crate::bridge::networks_of(record)
                    .first()
                    .and_then(|net| crate::bridge::address_on(record, net))
            })
            .flatten();
        Some(create::Linked {
            machine: m.name().to_string(),
            address,
        })
    }

    fn network(&self, key: &str) -> Option<String> {
        self.0.network(key).map(|view| view.name)
    }

    fn exists(&self, path: &str) -> bool {
        std::path::Path::new(path).symlink_metadata().is_ok()
    }
}

/// The architecture as the engine names it.
fn go_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    }
}

/// `platform` of a create: the host's own, or none.
pub fn check_platform(platform: &str) -> Result<(), Answer> {
    if platform.is_empty() {
        return Ok(());
    }
    let mut parts = platform.split('/');
    let (os, arch) = (parts.next(), parts.next());
    if os == Some("linux") && arch.is_none_or(|a| a == go_arch()) {
        return Ok(());
    }
    Err(Answer::says(
        StatusCode::BAD_REQUEST,
        format!(
            "platform {platform} is not linux/{}: nspawn runs images of the host's platform",
            go_arch()
        ),
    ))
}

/// The engine's rule for a container's name, and its words for one that breaks it.
pub fn check_name(name: &str) -> Result<String, Answer> {
    if !create::valid_name(name) {
        return Err(Answer::says(
            StatusCode::BAD_REQUEST,
            format!("Invalid container name ({name}), only [a-zA-Z0-9][a-zA-Z0-9_.-] are allowed"),
        ));
    }
    Ok(name.strip_prefix('/').unwrap_or(name).to_string())
}

/// What a failed create answers: a name taken names the container that holds it.
pub fn create_failed(
    kind: Option<&str>,
    name: &str,
    holder: Option<&str>,
    message: &str,
) -> Answer {
    match (kind, holder) {
        (Some("Exists"), Some(id)) => Answer::says(
            StatusCode::CONFLICT,
            format!(
                "Conflict. The container name \"/{name}\" is already in use by container \"{id}\". You have to remove (or rename) that container to be able to reuse that name."
            ),
        ),
        (kind, _) => Answer::says(
            match status_of(kind) {
                StatusCode::NOT_MODIFIED => StatusCode::INTERNAL_SERVER_ERROR,
                status => status,
            },
            message,
        ),
    }
}

/// Four random bytes, for a generated name.
fn random() -> u32 {
    use std::io::Read as _;
    let mut bytes = [0u8; 4];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .is_ok()
    {
        return u32::from_ne_bytes(bytes);
    }
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos() ^ std::process::id())
}

/// The host directories a container's binds name that are missing, made when it starts
/// as the engine makes them. The API's socket is the machine's own, which its start
/// makes.
fn make_sources(ctx: &Context, name: &str) -> anyhow::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    let Some(record) = ctx.store.load_image(name)? else {
        return Ok(());
    };
    for source in missing_sources(&record, |path| std::path::Path::new(path).exists()) {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(source)
            .map_err(|e| {
                anyhow::anyhow!("error while creating mount source path '{source}': {e}")
            })?;
    }
    Ok(())
}

/// The absolute host sources of a record's binds that `exists` does not find.
fn missing_sources(record: &ImageRecord, exists: impl Fn(&str) -> bool) -> Vec<&str> {
    record
        .volumes
        .iter()
        .map(|v| v.source.as_str())
        .filter(|s| s.starts_with('/') && !crate::volume::is_api_socket_path(s) && !exists(s))
        .collect()
}

/// The body of a create: the new container's ID and the warnings, an empty list for
/// none.
pub fn created_body(id: &str, warnings: &[String]) -> Value {
    json!({ "Id": id, "Warnings": warnings })
}

/// POST /containers/create, checked in the engine's order: the body, the settings, the
/// image, the networks and containers it names, the name; the name's uniqueness is the
/// service's to keep.
pub async fn create(ctx: &Arc<Context>, query: &Query, sent: Sent) -> Response<Body> {
    let served: Served = async {
        check_json(&sent.headers)?;
        let bytes = read_body(sent.body).await?;
        let body = create::decode(&bytes)?;
        check_platform(query.get("platform").unwrap_or_default())?;
        let snapshot = Snapshot::take(ctx)
            .await
            .map_err(|e| failed(format!("{e:#}")))?;
        let mut plan = create::translate(&body, &Live(&snapshot))?;
        let all = super::images::list(ctx).map_err(|e| failed(format!("{e:#}")))?;
        let resolved = super::images::resolve(&plan.image)
            .await
            .map_err(|e| failed(format!("{e:#}")))?;
        let image = super::images::find(&all, &plan.image, resolved.as_ref())
            .map_err(|e| Answer::says(lookup_status(&e), e))?;
        if let Some(network) = &plan.missing_network {
            return Err(Answer::says(
                StatusCode::NOT_FOUND,
                format!("network {network} not found"),
            ));
        }
        if let Some(container) = &plan.missing_container {
            return Err(Answer::says(StatusCode::NOT_FOUND, no_such(container)));
        }
        if plan.image_cmd {
            let cmd: Vec<String> = image
                .config
                .pointer("/config/Cmd")
                .and_then(Value::as_array)
                .map(|words| {
                    words
                        .iter()
                        .filter_map(|w| w.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            if !cmd.is_empty() {
                plan.opts.insert("command", Opt::Texts(cmd));
            }
        }
        let name = match query.get("name").unwrap_or_default() {
            "" => create::generated_name(&plan.image, random()),
            given => check_name(given)?,
        };
        let source =
            super::images::source(image, &plan.image, resolved.as_ref()).ok_or_else(|| {
                Answer::says(
                    StatusCode::NOT_FOUND,
                    format!("No such image: {}", plan.image),
                )
            })?;
        let mut opts = plan.opts.clone();
        opts.insert("api_name", Opt::Text(name.clone()));
        let client = connect().await?;
        let outcome = client
            .run_job_silent(
                || {
                    client
                        .manager
                        .create_machine(&source, "", create::to_bus(&opts))
                },
                true,
            )
            .await;
        match outcome {
            Ok(result) => {
                let machine = client::string(&result, "name");
                let record = ctx
                    .store
                    .load_image(&machine)
                    .ok()
                    .flatten()
                    .ok_or_else(|| failed(format!("the record of {machine} is missing")))?;
                if let Some((rows, cols)) = console_size(&body) {
                    super::attach::remember_size(&machine, rows, cols);
                }
                Ok(json_response(
                    StatusCode::CREATED,
                    &created_body(&container_id(&machine, record.created), &plan.warnings),
                ))
            }
            Err(e) => {
                let kind = client::error_name(&e);
                let holder = match kind {
                    Some("Exists") => Snapshot::take(ctx).await.ok().and_then(|s| {
                        s.machines
                            .iter()
                            .find(|m| m.api_name() == name)
                            .map(|m| m.id.clone())
                    }),
                    _ => None,
                };
                Err(create_failed(
                    kind,
                    &name,
                    holder.as_deref(),
                    &format!("{e:#}"),
                ))
            }
        }
    }
    .await;
    served.unwrap_or_else(Answer::respond)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dockerapi::shapes::{diff, golden};

    #[test]
    fn a_start_makes_the_missing_directories_of_binds() {
        let record: ImageRecord = serde_json::from_value(json!({
            "name": "web",
            "reference": "r",
            "manifest_digest": "d",
            "layers": [],
            "backend": "overlay",
            "created": 0,
            "volumes": [
                { "source": "/srv/there", "target": "/a" },
                { "source": "/srv/new/dir", "target": "/b" },
                { "source": "data", "target": "/data" },
                { "source": "/run/docker.sock", "target": "/run/docker.sock" },
            ],
        }))
        .unwrap();
        assert_eq!(
            missing_sources(&record, |path| path == "/srv/there"),
            ["/srv/new/dir"]
        );
    }

    /// The ID of `life` in the recorded run.
    const LIFE: &str = "d95f52d7c1586398e268b7e7145dc20a694fb48f030b2a8aa03e200ef51228c0";
    /// The ID of `created`.
    const CREATED: &str = "1ae0f847e93332d9f2c616b045a2a0b84e937c2b64ff57b35efcfb70b95aa2a4";

    fn file(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/tests/dockerapi/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    /// A case of the recorded run: its status and the message of its body, if any.
    fn case(name: &str) -> (StatusCode, Option<String>) {
        let status: u16 = file(&format!("golden/{name}.status"))
            .trim()
            .parse()
            .unwrap();
        let body = file(&format!("golden/{name}.body"));
        let message = (!body.trim().is_empty())
            .then(|| golden(&body)["message"].as_str().map(str::to_string))
            .flatten();
        (StatusCode::from_u16(status).unwrap(), message)
    }

    fn answer(a: Answer) -> (StatusCode, Option<String>) {
        (a.0, a.1)
    }

    /// The request of a case: its method and path, its Content-Type and body.
    fn request(name: &str) -> (String, String, Option<String>, String) {
        let text = file(&format!("requests/{name}.request"));
        let (line, rest) = text.split_once('\n').unwrap_or((&text, ""));
        let (method, target) = line.split_once(' ').unwrap();
        let (content_type, body) = match rest.split_once("\n\n") {
            Some((head, body)) => (
                head.strip_prefix("Content-Type: ").map(str::to_string),
                body.to_string(),
            ),
            None => (None, String::new()),
        };
        (method.into(), target.into(), content_type, body)
    }

    fn target(id: &str, running: bool, paused: bool) -> Target {
        Target {
            name: "life".into(),
            id: id.into(),
            running,
            paused,
            restarting: false,
            remove_on_exit: false,
        }
    }

    #[test]
    fn a_start_between_two_runs_of_a_restart_policy_has_nothing_to_do() {
        let restarting = Target {
            restarting: true,
            ..target(LIFE, true, false)
        };
        assert!(start_not_needed(&restarting));
        assert!(!start_not_needed(&target(LIFE, false, false)));
        assert_eq!(
            Answer(StatusCode::NOT_MODIFIED, None).respond().status(),
            StatusCode::NOT_MODIFIED
        );
    }

    #[test]
    fn a_start_takes_the_kept_input_of_a_container_that_does_not_run() {
        assert!(takes_input(&target(LIFE, false, false)));
        // Its program may have ended, its machine going away: RunMachine waits for it.
        assert!(takes_input(&target(LIFE, true, false)));
        assert!(!takes_input(&target(LIFE, true, true)));
        // A restart stops one whose program runs, and only starts the others.
        assert!(restart_only_starts(&target(LIFE, false, false), false));
        assert!(restart_only_starts(&target(LIFE, true, false), true));
        assert!(!restart_only_starts(&target(LIFE, true, false), false));
    }

    #[test]
    fn resizes_answer_as_the_engine() {
        use crate::dockerapi::{classify, Route};
        assert_eq!(
            classify(&hyper::Method::POST, "/containers/life/resize"),
            Route::Lifecycle("life".into(), Act::Resize)
        );
        assert_eq!(
            classify(&hyper::Method::POST, "/containers/life/resize").need(),
            crate::dockerapi::access::Need::Manage
        );
        assert_eq!(resize_value(Some("24"), "height").unwrap(), 24);
        assert_eq!(resize_value(Some("0"), "width").unwrap(), 0);
        assert_eq!(resize_value(Some("4294967295"), "width").unwrap(), u32::MAX);
        for (text, what, message) in [
            (None, "height", "invalid resize height \"\": invalid syntax"),
            (
                Some("x"),
                "height",
                "invalid resize height \"x\": invalid syntax",
            ),
            (
                Some("+3"),
                "height",
                "invalid resize height \"+3\": invalid syntax",
            ),
            (
                Some("-"),
                "width",
                "invalid resize width \"-\": invalid syntax",
            ),
            (
                Some("-3"),
                "width",
                "invalid resize width \"-3\": value out of range",
            ),
            (
                Some("-x"),
                "width",
                "invalid resize width \"-x\": invalid syntax",
            ),
            (
                Some("1x"),
                "width",
                "invalid resize width \"1x\": invalid syntax",
            ),
            (
                Some("4294967296"),
                "width",
                "invalid resize width \"4294967296\": value out of range",
            ),
            (
                Some("99999999999x"),
                "width",
                "invalid resize width \"99999999999x\": value out of range",
            ),
        ] {
            assert_eq!(
                resize_value(text, what).unwrap_err(),
                Answer(StatusCode::BAD_REQUEST, Some(message.into())),
                "{text:?}"
            );
        }
        // A container that runs gets the size on the terminal this server holds, or
        // nothing is done; one that does not is the engine's 409, unless an attach keeps
        // it a terminal: then the size is its start's.
        assert_eq!(
            resizing(&target(LIFE, true, false), false),
            Resizing::Terminal
        );
        assert_eq!(
            resizing(&target(LIFE, true, true), false),
            Resizing::Terminal
        );
        assert_eq!(resizing(&target(LIFE, false, false), true), Resizing::Kept);
        assert_eq!(
            resizing(&target(LIFE, false, false), false),
            Resizing::NotRunning
        );
    }

    #[test]
    fn a_create_keeps_the_size_of_the_terminal() {
        let body = json!({ "Tty": true, "HostConfig": { "ConsoleSize": [40, 120] } });
        assert_eq!(console_size(&body), Some((40, 120)));
        let huge = json!({ "Tty": true, "HostConfig": { "ConsoleSize": [70000, 120] } });
        assert_eq!(console_size(&huge), Some((u16::MAX, 120)));
        for body in [
            json!({ "Tty": false, "HostConfig": { "ConsoleSize": [40, 120] } }),
            json!({ "Tty": true, "HostConfig": { "ConsoleSize": [0, 0] } }),
            json!({ "Tty": true, "HostConfig": { "ConsoleSize": [40] } }),
            json!({ "Tty": true, "HostConfig": {} }),
            json!({ "Tty": true }),
        ] {
            assert_eq!(console_size(&body), None, "{body}");
        }
    }

    #[test]
    fn statuses_come_from_the_names_of_the_bus_errors() {
        for (kind, status) in [
            ("NoSuchMachine", 404),
            ("NoSuchImage", 404),
            ("NoSuchNetwork", 404),
            ("NoSuchVolume", 404),
            ("NoSuchPath", 404),
            ("Exists", 409),
            ("InUse", 409),
            ("NotRunning", 409),
            ("Paused", 409),
            ("AlreadyRunning", 304),
            ("NotPaused", 500),
            ("Invalid", 400),
            ("NotAuthorized", 403),
            ("Failed", 500),
        ] {
            assert_eq!(status_of(Some(kind)).as_u16(), status, "{kind}");
        }
        assert_eq!(status_of(None), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn the_recorded_requests_are_these_routes() {
        use crate::dockerapi::{classify, Route};
        for (name, route) in [
            ("create", Route::Create),
            ("start", Route::Lifecycle("life".into(), Act::Start)),
            ("stop", Route::Lifecycle("life".into(), Act::Stop)),
            ("restart", Route::Lifecycle("life".into(), Act::Restart)),
            ("pause", Route::Lifecycle("life".into(), Act::Pause)),
            ("unpause", Route::Lifecycle("life".into(), Act::Unpause)),
            ("wait-exited", Route::Lifecycle("exited".into(), Act::Wait)),
            ("update", Route::Lifecycle("life".into(), Act::Update)),
            ("rm", Route::Remove("life".into())),
            ("prune", Route::Prune),
        ] {
            let (method, target, _, _) = request(name);
            let path = target.split('?').next().unwrap();
            assert_eq!(classify(&method.parse().unwrap(), path), route, "{name}");
        }
    }

    #[test]
    fn starts_answer_as_the_engine() {
        assert_eq!(
            answer(start_failed(
                Some("AlreadyRunning"),
                "life",
                "machine life is already running"
            )),
            case("start-again")
        );
        assert_eq!(
            start_failed(Some("Paused"), "life", "x").1.unwrap(),
            "cannot start a paused container, try unpause instead"
        );
        assert_eq!(
            answer(start_failed(Some("NoSuchMachine"), "nope", "x")),
            (
                StatusCode::NOT_FOUND,
                Some("No such container: nope".into())
            )
        );
        assert_eq!(
            start_failed(None, "life", "it broke"),
            Answer(StatusCode::INTERNAL_SERVER_ERROR, Some("it broke".into()))
        );
        let mut headers = HeaderMap::new();
        assert!(start_body(&headers).is_none());
        headers.insert("content-length", "2".parse().unwrap());
        assert!(start_body(&headers).is_none());
        headers.insert("content-length", "8".parse().unwrap());
        assert_eq!(start_body(&headers).unwrap().0, StatusCode::BAD_REQUEST);
        let mut chunked = HeaderMap::new();
        chunked.insert("transfer-encoding", "chunked".parse().unwrap());
        assert!(start_body(&chunked).is_some());
    }

    #[test]
    fn stops_and_restarts_answer_as_the_engine() {
        assert_eq!(stop_timeout(None).unwrap(), None);
        assert_eq!(stop_timeout(Some("")).unwrap(), None);
        assert_eq!(stop_timeout(Some("1")).unwrap(), Some(1));
        assert_eq!(
            stop_timeout(Some("-1")).unwrap(),
            Some(crate::policy::NO_STOP_TIMEOUT)
        );
        assert_eq!(
            stop_timeout(Some("x")).unwrap_err(),
            Answer(
                StatusCode::INTERNAL_SERVER_ERROR,
                Some("strconv.Atoi: parsing \"x\": invalid syntax".into())
            )
        );
        assert_eq!(
            answer(stop_failed(Some("NotRunning"), "life", "x")),
            case("stop-again")
        );
        assert_eq!(
            stop_failed(Some("Invalid"), "life", "invalid signal: X"),
            Answer(
                StatusCode::INTERNAL_SERVER_ERROR,
                Some("cannot stop container: life: invalid signal: X".into())
            )
        );
        assert_eq!(
            stop_failed(None, "life", "it broke").1.unwrap(),
            "cannot stop container: life: it broke"
        );
        assert_eq!(
            answer(restart_failed(Some("NoSuchMachine"), "nope", "x")),
            (
                StatusCode::NOT_FOUND,
                Some("No such container: nope".into())
            )
        );
        assert_eq!(
            restart_failed(Some("Invalid"), "life", "invalid signal: X"),
            Answer(
                StatusCode::INTERNAL_SERVER_ERROR,
                Some("Cannot restart container life: invalid signal: X".into())
            )
        );
    }

    #[test]
    fn pauses_answer_as_the_engine() {
        let life = target(LIFE, true, false);
        assert_eq!(
            answer(pause_failed(
                Some("Paused"),
                &life,
                "life",
                "machine life is already paused"
            )),
            case("pause-again")
        );
        let stopped = target(LIFE, false, false);
        assert_eq!(
            answer(pause_failed(
                Some("NotRunning"),
                &stopped,
                "life",
                "machine life is not running"
            )),
            case("pause-stopped")
        );
        assert_eq!(
            answer(unpause_failed(
                Some("NotPaused"),
                &life,
                "life",
                "machine life is not paused"
            )),
            case("unpause-again")
        );
        // A container that does not run is not paused either.
        assert_eq!(
            answer(unpause_failed(Some("NotRunning"), &stopped, "life", "x")),
            case("unpause-again")
        );
    }

    #[test]
    fn only_numbers_of_signals_are_sent() {
        assert!(is_signal(15) && is_signal(64));
        assert!(!is_signal(0) && !is_signal(65) && !is_signal(99));
    }

    #[test]
    fn removals_answer_as_the_engine() {
        let life = target(LIFE, true, false);
        assert_eq!(
            answer(remove_failed(
                Some("InUse"),
                &life,
                "life",
                false,
                "machine life is running; stop it first"
            )),
            case("rm-running")
        );
        // Forced, the reason it stayed is the service's.
        assert_eq!(
            remove_failed(Some("InUse"), &life, "life", true, "web needs it"),
            Answer(
                StatusCode::CONFLICT,
                Some("cannot remove container \"life\": web needs it".into())
            )
        );
        let paused = target(LIFE, true, true);
        assert_eq!(
            remove_failed(Some("InUse"), &paused, "life", false, "x")
                .1
                .unwrap(),
            "cannot remove container \"life\": container is paused and must be unpaused first"
        );
        assert_eq!(
            answer(remove_failed(
                Some("NoSuchMachine"),
                &life,
                "life",
                true,
                "x"
            )),
            case("rm-missing")
        );
        // One removed at its end that the forced stop already took is gone, as asked.
        let removed_at_end = Target {
            remove_on_exit: true,
            ..life.clone()
        };
        assert!(gone_at_its_end(
            Some("NoSuchMachine"),
            &removed_at_end,
            true
        ));
        assert!(!gone_at_its_end(
            Some("NoSuchMachine"),
            &removed_at_end,
            false
        ));
        assert!(!gone_at_its_end(Some("NoSuchMachine"), &life, true));
        assert!(!gone_at_its_end(Some("InUse"), &removed_at_end, true));
        let stopped = target(LIFE, false, false);
        assert!(!gone_at_its_end(
            Some("NoSuchMachine"),
            &Target {
                remove_on_exit: true,
                ..stopped.clone()
            },
            true
        ));
        assert_eq!(
            remove_failed(Some("InUse"), &stopped, "life", false, "web needs it"),
            Answer(
                StatusCode::CONFLICT,
                Some("cannot remove container \"life\": web needs it".into())
            )
        );
    }

    #[test]
    fn waits_answer_as_the_engine() {
        assert_eq!(wait_condition(None).unwrap(), "not-running");
        assert_eq!(wait_condition(Some("removed")).unwrap(), "removed");
        assert_eq!(
            answer(wait_condition(Some("bogus")).unwrap_err()),
            case("wait-badcondition")
        );
        assert_eq!(
            answer(wait_failed(
                Some("NoSuchMachine"),
                "nope",
                "no machine or image named nope"
            )),
            case("wait-missing")
        );
        assert_eq!(wait_body(Ok(3)), file("golden/wait-exited.body"));
        assert_eq!(wait_body(Ok(0)), file("golden/wait-created.body"));
        let failed: Value = serde_json::from_str(&wait_body(Err("gone".into()))).unwrap();
        assert_eq!(
            failed,
            json!({ "Error": { "Message": "gone" }, "StatusCode": -1 })
        );
        // The headers the engine sends at once, before the body.
        let headers = file("golden/wait-exited.headers");
        assert!(headers.contains("Content-Type: application/json"));
        assert!(headers.contains("Transfer-Encoding: chunked"));
    }

    #[test]
    fn updates_take_the_limits_and_the_restart_policy() {
        let (method, _, content_type, body) = request("update");
        assert_eq!(method, "POST");
        assert_eq!(content_type.as_deref(), Some("application/json"));
        let body = create::decode(body.as_bytes()).unwrap();
        let (opts, warnings) = update_options(&Object::of(&body, "UpdateConfig").unwrap()).unwrap();
        assert_eq!(
            opts,
            Opts::from([
                ("cpus", Opt::Float(0.5)),
                ("memory", Opt::Unsigned(67_108_864)),
                ("memory_swap", Opt::Signed(134_217_728)),
                ("pids_limit", Opt::Unsigned(0)),
                ("restart", Opt::Text("on-failure:3".into())),
            ])
        );
        // The engine keeps the count of retries; nspawn says it does not.
        assert_eq!(
            warnings,
            [create::retry_count_warning("on-failure:3").unwrap()]
        );
        let ours = json!({ "Warnings": Value::Null });
        assert_eq!(format!("{ours}\n"), file("golden/update.body"));
        // A zero is no change; what nspawn does not update is said.
        let body = json!({ "Memory": 0, "CpuShares": 512, "BlkioWeight": 0, "PidsLimit": null });
        let (opts, warnings) = update_options(&Object::of(&body, "UpdateConfig").unwrap()).unwrap();
        assert!(opts.is_empty());
        assert_eq!(
            warnings,
            ["CpuShares is ignored: nspawn sets no CPU weight"]
        );
        // The restart policy of a container removed when it ends stays.
        let mut created = target(CREATED, false, false);
        created.remove_on_exit = true;
        assert_eq!(answer(removed_at_end(&created)), case("update-autoremove"));
        assert_eq!(
            update_failed(Some("Invalid"), &created, "created", "bad"),
            Answer(
                StatusCode::BAD_REQUEST,
                Some(format!("Cannot update container {CREATED}: bad"))
            )
        );
        assert_eq!(
            update_failed(Some("NoSuchMachine"), &created, "nope", "x").0,
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn prunes_take_until_and_labels() {
        let (_, target, _, _) = request("prune");
        let query = crate::dockerapi::query(target.split_once('?').unwrap().1);
        let filters = Filters::parse(query.get("filters")).unwrap();
        let now = 1_793_849_751.0;
        assert_eq!(
            prune_options(&filters, now, 0).unwrap(),
            Opts::from([
                ("made", Opt::Flag(true)),
                ("labels", Opt::Texts(vec!["e2e=lifecycle".into()]))
            ])
        );
        let f = |text: &str| Filters::parse(Some(text)).unwrap();
        let opts = prune_options(&f(r#"{"until":["10m"],"label!":["keep"]}"#), now, 0).unwrap();
        assert_eq!(opts["until"], Opt::Unsigned(1_793_849_751 - 600 + 1));
        assert_eq!(opts["labels_not"], Opt::Texts(vec!["keep".into()]));
        let opts = prune_options(&f(r#"{"until":["1793849000"]}"#), now, 0).unwrap();
        assert_eq!(opts["until"], Opt::Unsigned(1_793_849_001));
        let opts = prune_options(&f(r#"{"until":["2026-11-05T03:35:46Z"]}"#), now, 0).unwrap();
        assert_eq!(opts["until"], Opt::Unsigned(1_793_849_747));
        for (text, message) in [
            (r#"{"dangling":["true"]}"#, "invalid filter 'dangling'"),
            (
                r#"{"until":["1","2"]}"#,
                "more than one until filter specified",
            ),
        ] {
            assert_eq!(
                prune_options(&f(text), now, 0).unwrap_err(),
                Answer(StatusCode::BAD_REQUEST, Some(message.into()))
            );
        }
        assert!(prune_options(&f(r#"{"until":["soon"]}"#), now, 0).is_err());
        let recorded = golden(&file("golden/prune.body"));
        let ours = prune_body(vec![LIFE.into(), CREATED.into()], 4096);
        assert_eq!(diff(&ours, &recorded), (vec![], vec![]));
        assert_eq!(prune_body(Vec::new(), 0)["ContainersDeleted"], Value::Null);
    }

    #[test]
    fn creates_answer_as_the_engine() {
        let (method, _, content_type, body) = request("create");
        assert_eq!(method, "POST");
        assert_eq!(content_type.as_deref(), Some("application/json"));
        create::decode(body.as_bytes()).unwrap();
        let ours = created_body(LIFE, &[]);
        assert_eq!(format!("{ours}\n"), file("golden/create.body"));
        assert_eq!(check_name("life").unwrap(), "life");
        assert_eq!(check_name("/life").unwrap(), "life");
        assert_eq!(
            answer(check_name("-life").unwrap_err()),
            case("create-badname")
        );
        assert_eq!(
            answer(create_failed(
                Some("Exists"),
                "life",
                Some(LIFE),
                "the name life is in use by machine life"
            )),
            case("create-conflict")
        );
        assert_eq!(
            create_failed(
                Some("Exists"),
                "my_app",
                None,
                "the name my_app would be machine m, which exists already"
            ),
            Answer(
                StatusCode::CONFLICT,
                Some("the name my_app would be machine m, which exists already".into())
            )
        );
        assert_eq!(
            create_failed(Some("Invalid"), "x", None, "bad").0,
            StatusCode::BAD_REQUEST
        );
        // The Content-Type, before the body is read.
        let (_, _, content_type, body) = request("create-contenttype");
        let mut headers = HeaderMap::new();
        headers.insert("content-type", content_type.unwrap().parse().unwrap());
        headers.insert("content-length", body.len().to_string().parse().unwrap());
        assert_eq!(
            answer(check_json(&headers).unwrap_err()),
            case("create-contenttype")
        );
        headers.insert(
            "content-type",
            "application/json; charset=utf-8".parse().unwrap(),
        );
        assert!(check_json(&headers).is_ok());
        assert!(check_json(&HeaderMap::new()).is_ok());
        let (_, _, _, body) = request("create-badjson");
        assert_eq!(
            answer(create::decode(body.as_bytes()).unwrap_err().into()),
            case("create-badjson")
        );
        let (_, _, _, body) = request("create-autoremove-restart");
        let refusal =
            create::translate(&create::decode(body.as_bytes()).unwrap(), &Nothing).unwrap_err();
        assert_eq!(answer(refusal.into()), case("create-autoremove-restart"));
        // The image named in full, said short with its tag.
        let (_, _, _, body) = request("create-noimage");
        let image = create::translate(&create::decode(body.as_bytes()).unwrap(), &Nothing)
            .unwrap()
            .image;
        let e = crate::dockerapi::images::find(&[], &image, None).unwrap_err();
        assert_eq!((lookup_status(&e), Some(e)), case("create-noimage"));
        assert!(check_platform("").is_ok());
        assert!(check_platform(&format!("linux/{}", go_arch())).is_ok());
        assert!(check_platform("linux").is_ok());
        assert_eq!(
            check_platform("windows/amd64").unwrap_err().0,
            StatusCode::BAD_REQUEST
        );
    }

    /// A host with nothing on it.
    struct Nothing;

    impl create::World for Nothing {
        fn container(&self, _: &str) -> Option<create::Linked> {
            None
        }
        fn network(&self, _: &str) -> Option<String> {
            None
        }
        fn exists(&self, _: &str) -> bool {
            false
        }
    }

    #[test]
    fn the_lookup_words_are_the_engine_s() {
        let snapshot = Snapshot {
            machines: Vec::new(),
            networks: Vec::new(),
        };
        let e = snapshot.find("nope").err().unwrap();
        assert_eq!((lookup_status(&e), Some(e)), case("wait-missing"));
        let e = snapshot.find("life").err().unwrap();
        assert_eq!((lookup_status(&e), Some(e)), case("rm-missing"));
        assert!(deleted_ids(&snapshot, &["x".into()]).is_empty());
    }
}
