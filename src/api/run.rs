//! docker run, attached: the machine's output as it comes, its end and its exit code,
//! and, for an app, a terminal or an input for its program (src/attach.rs).
//!
//! The end is systemd's to tell, over D-Bus: the unit's PropertiesChanged carries the
//! main process's exit code and status as they were when it ended, which a later read
//! could miss once a restart began. The output is the journal's: systemd-nspawn copies
//! the machine's console to the unit's standard output, so what `run` shows is what
//! `logs` shows later, and the machine goes on if the caller goes away.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context as _, Result};
use futures_util::StreamExt;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use zbus::zvariant::OwnedValue;

use crate::systemd::Systemd;

/// How the main process of the unit ended, as systemd says it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ending {
    /// CLD_EXITED (1), CLD_KILLED (2) or CLD_DUMPED (3).
    pub code: i32,
    pub status: i32,
    /// The unit's Result=, "oom-kill" among them.
    pub result: String,
}

const CLD_EXITED: i32 = 1;
const CLD_KILLED: i32 = 2;
const CLD_DUMPED: i32 = 3;
/// What systemd-nspawn exits with when the machine asked for a reboot.
const REBOOT: i32 = 133;

/// The exit code `run` gives, docker's way: the program's, 128 plus the signal it died
/// of. systemd-nspawn exits 255 whatever signal killed an app's program; when nspawn
/// sent one itself (`sent`), that is the one. SIGKILL of the whole machine makes it exit
/// 1 on systemd 259 and newer, and die of it before; that and the OOM killer give 137.
pub fn exit_code(ending: &Ending, sent: Option<i32>, app: bool) -> i32 {
    if ending.result == "oom-kill" {
        return 128 + 9;
    }
    match ending.code {
        CLD_EXITED if ending.status == 255 && app => sent.map_or(255, |s| 128 + s),
        CLD_EXITED if ending.status == 1 && sent == Some(9) => 128 + 9,
        CLD_EXITED => ending.status,
        CLD_KILLED | CLD_DUMPED => 128 + ending.status,
        _ => 125,
    }
}

/// The unit of a run, watched from before its start: its PropertiesChanged signals say
/// when the main process started and how it ended.
pub struct UnitWatch {
    unit: String,
    changes: zbus::fdo::PropertiesChangedStream,
    main_pid: u32,
    code: i32,
    status: i32,
    result: String,
}

impl UnitWatch {
    pub async fn new(sd: &Systemd, unit: &str) -> Result<Self> {
        // systemd only sends a unit's changes while someone subscribed.
        sd.subscribe().await?;
        let path = sd.unit_path(unit).await?;
        let proxy = zbus::fdo::PropertiesProxy::builder(sd.connection())
            .destination("org.freedesktop.systemd1")?
            .path(path)?
            .build()
            .await
            .with_context(|| format!("watching {unit}"))?;
        let changes = proxy
            .receive_properties_changed()
            .await
            .with_context(|| format!("watching {unit}"))?;
        Ok(UnitWatch {
            unit: unit.to_string(),
            changes,
            main_pid: 0,
            code: 0,
            status: 0,
            result: String::new(),
        })
    }

    /// A main process learned otherwise (read after the start, in case its signal came
    /// before the watch was listening).
    pub fn saw(&mut self, pid: u32) {
        if pid != 0 && self.main_pid == 0 {
            self.main_pid = pid;
        }
    }

    /// Waits for the main process to end. A reboot asked from inside the machine is not
    /// an end: systemd starts it again, and the watch goes on with the new run.
    pub async fn ended(&mut self) -> Result<Ending> {
        while let Some(signal) = self.changes.next().await {
            let Ok(args) = signal.args() else { continue };
            if args.interface_name().as_str() != "org.freedesktop.systemd1.Service" {
                continue;
            }
            let changed: &HashMap<&str, zbus::zvariant::Value<'_>> = args.changed_properties();
            let get_i32 = |key: &str| changed.get(key).and_then(|v| i32::try_from(v).ok());
            if let Some(pid) = changed
                .get("ExecMainPID")
                .and_then(|v| u32::try_from(v).ok())
            {
                if pid != 0 && pid != self.main_pid {
                    // A new run: what the last one left is over.
                    self.main_pid = pid;
                    self.code = 0;
                    self.status = 0;
                }
            }
            if let Some(code) = get_i32("ExecMainCode") {
                self.code = code;
            }
            if let Some(status) = get_i32("ExecMainStatus") {
                self.status = status;
            }
            if let Some(result) = changed.get("Result").and_then(|v| <&str>::try_from(v).ok()) {
                self.result = result.to_string();
            }
            if self.code == 0 || self.main_pid == 0 {
                continue;
            }
            if self.code == CLD_EXITED && self.status == REBOOT {
                self.code = 0;
                continue;
            }
            return Ok(Ending {
                code: self.code,
                status: self.status,
                result: self.result.clone(),
            });
        }
        bail!("lost the signals of {}", self.unit)
    }
}

/// The journal's cursor now, so that a follower started later misses nothing written
/// after it. None without a journal to follow.
pub async fn journal_cursor() -> Option<String> {
    let output = tokio::process::Command::new("journalctl")
        .args(["--lines=0", "--show-cursor", "--quiet", "--no-pager"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await
        .ok()?;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("-- cursor: "))
        .map(|c| c.trim().to_string())
}

/// journalctl's arguments for what a unit's processes write, from `cursor` on.
pub fn follower_arguments(unit: &str, cursor: Option<&str>) -> Vec<String> {
    let mut argv = vec![
        "--no-pager".to_string(),
        "--quiet".to_string(),
        "--output=json".to_string(),
        "--follow".to_string(),
    ];
    match cursor {
        Some(cursor) => argv.push(format!("--after-cursor={cursor}")),
        None => argv.push("--lines=0".to_string()),
    }
    argv.push(format!("_SYSTEMD_UNIT={unit}"));
    argv.push("_TRANSPORT=stdout".to_string());
    argv
}

/// A journal entry's message as bytes: journalctl writes a message with control
/// characters or invalid UTF-8 as an array of numbers. The carriage return a console
/// ends its lines with is dropped.
pub fn message_bytes(entry: &Value) -> Option<Vec<u8>> {
    let mut bytes = match entry.get("MESSAGE")? {
        Value::String(text) => text.clone().into_bytes(),
        Value::Array(items) => items
            .iter()
            .map(|n| n.as_u64().and_then(|n| u8::try_from(n).ok()))
            .collect::<Option<Vec<u8>>>()?,
        _ => return None,
    };
    while bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    Some(bytes)
}

/// Whether a journal entry is what the machine wrote rather than what one of the unit's
/// hooks (`nspawn network ...`) said. The machine's output comes from systemd-nspawn
/// copying its console, or, with --console=pipe, from the program's own processes
/// writing into the same stream; journald names the writer of every line.
fn from_machine(entry: &Value) -> bool {
    entry.get("_COMM").and_then(Value::as_str) != Some("nspawn")
}

/// Copies what the machine writes into `out`, a line at a time. `finish` ends it once
/// the output has been quiet for a moment, so that the last lines are not cut.
pub struct Follower {
    stop: tokio::sync::watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl Follower {
    pub fn start(unit: &str, cursor: Option<&str>, out: OwnedFd) -> Result<Self> {
        let mut out = tokio::net::unix::pipe::Sender::from_owned_fd(out)
            .context("preparing the output's pipe")?;
        let mut child = tokio::process::Command::new("journalctl")
            .args(follower_arguments(unit, cursor))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("running journalctl")?;
        let entries = child.stdout.take().context("journalctl has no output")?;
        let (stop, mut stopping) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(entries).lines();
            let quiet = Duration::from_millis(300);
            let mut last_line = tokio::time::Instant::now();
            let mut deadline: Option<tokio::time::Instant> = None;
            loop {
                let wait = match deadline {
                    Some(deadline) => (last_line + quiet).min(deadline),
                    None => tokio::time::Instant::now() + Duration::from_secs(3600),
                };
                tokio::select! {
                    line = lines.next_line() => {
                        let Ok(Some(line)) = line else { break };
                        last_line = tokio::time::Instant::now();
                        let Ok(entry) = serde_json::from_str::<Value>(&line) else { continue };
                        if !from_machine(&entry) {
                            continue;
                        }
                        let Some(mut bytes) = message_bytes(&entry) else { continue };
                        bytes.push(b'\n');
                        if out.write_all(&bytes).await.is_err() {
                            break;
                        }
                    }
                    changed = stopping.changed(), if deadline.is_none() => {
                        if changed.is_err() || *stopping.borrow() {
                            deadline = Some(tokio::time::Instant::now() + Duration::from_secs(2));
                            last_line = tokio::time::Instant::now();
                        }
                    }
                    _ = tokio::time::sleep_until(wait), if deadline.is_some() => break,
                }
            }
            let _ = child.kill().await;
        });
        Ok(Follower { stop, task })
    }

    /// Ends the copy once the output has been quiet for a moment (two seconds at most),
    /// and closes the pipe.
    pub async fn finish(self) {
        let _ = self.stop.send(true);
        let _ = self.task.await;
    }
}

/// The terminal an attached run gives the program of an app, like docker run -t.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Terminal {
    pub rows: u16,
    pub cols: u16,
    pub term: String,
}

pub struct RunRequest {
    pub start: crate::api::machines::StartRequest,
    pub terminal: Option<Terminal>,
    /// The caller's standard input, for the program of an app (-i without -t).
    pub stdin: Option<OwnedFd>,
}

/// A machine started for an attached run, and what goes on watching it.
pub struct Attached {
    pub name: String,
    pub app: bool,
    /// The terminal's master for the caller (-t).
    pub terminal: Option<OwnedFd>,
    /// The service's own copy: the terminal stays open should the caller go, until the
    /// machine has been stopped the normal way.
    keep: Option<OwnedFd>,
    /// Where the caller reads the output (without -t).
    pub output: Option<OwnedFd>,
    follower: Option<Follower>,
    watch: UnitWatch,
    /// The main process, as the process object's PID.
    pub main_pid: u32,
}

/// Starts a machine attached: its terminal or input handed to its systemd-nspawn
/// (src/attach.rs), its end watched and its output followed from before the start.
pub async fn attach(
    ctx: &crate::api::Context,
    request: RunRequest,
    report: crate::api::Report<'_>,
) -> Result<Attached> {
    let name = request.start.name.clone();
    crate::reference::validate_machine_name(&name)?;
    let record = ctx
        .store
        .load_image(&name)?
        .with_context(|| format!("no machine named {name}; nspawn run makes one from an image"))?;
    let app = record.mode == crate::oci::Mode::App;
    if !app && (request.terminal.is_some() || request.stdin.is_some()) {
        bail!("{name} boots an init system; its console is shown without -i and -t, and a shell comes with -it");
    }
    let sd = ctx.sd().await?;
    let unit = format!("systemd-nspawn@{name}.service");
    let (mode, handed, keep, terminal) = match (&request.terminal, request.stdin) {
        (Some(t), _) => {
            let (master, slave) = crate::nsenter::host_pty(t.rows, t.cols)?;
            let keep = master
                .try_clone()
                .context("keeping the terminal's master")?;
            (
                Some(crate::attach::Mode::Tty {
                    term: t.term.clone(),
                }),
                Some(slave),
                Some(keep),
                Some(master),
            )
        }
        (None, Some(stdin)) => (Some(crate::attach::Mode::Stdin), Some(stdin), None, None),
        (None, None) => (None, None, None, None),
    };
    let handover = match (mode, handed) {
        (Some(mode), Some(fd)) => {
            let listener = crate::attach::Listener::bind(&name)?;
            Some(tokio::spawn(async move {
                listener
                    .hand_over(&mode, &fd, Duration::from_secs(120))
                    .await
            }))
        }
        _ => None,
    };
    let cursor = if terminal.is_none() {
        journal_cursor().await
    } else {
        None
    };
    let mut watch = UnitWatch::new(sd, &unit).await?;
    if let Err(e) = crate::api::machines::start(ctx, &request.start, report).await {
        if let Some(task) = handover {
            task.abort();
        }
        return Err(e);
    }
    if let Some(task) = handover {
        let handed = tokio::time::timeout(Duration::from_secs(10), task).await;
        if !matches!(handed, Ok(Ok(Ok(())))) {
            let reason = match handed {
                Ok(Ok(Err(e))) => format!("{e:#}"),
                _ => "it did not ask for them".to_string(),
            };
            let stop = crate::api::machines::StopRequest {
                name: name.clone(),
                force: true,
                wait: true,
                timeout: 0,
            };
            let _ = crate::api::machines::stop(ctx, &stop, report).await;
            bail!("{name} did not get its terminal or input ({reason}); was its unit changed by hand?");
        }
    }
    let main_pid = sd.exec_main_pid(&unit).await.unwrap_or(0);
    watch.saw(main_pid);
    let (output, follower) = if terminal.is_none() {
        let (read, write) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)
            .context("creating the output's pipe")?;
        let follower = Follower::start(&unit, cursor.as_deref(), write)?;
        (Some(read), Some(follower))
    } else {
        (None, None)
    };
    Ok(Attached {
        name,
        app,
        terminal,
        keep,
        output,
        follower,
        watch,
        main_pid,
    })
}

impl Attached {
    /// Waits for the run to end and gives its exit code. Should the caller go first, a
    /// machine with a terminal is stopped the normal way (nothing would read it any
    /// more), and any other one goes on alone: the wait ends there.
    pub async fn finish(
        mut self,
        ctx: Arc<crate::api::Context>,
        caller_gone: impl std::future::Future<Output = ()>,
        sent: Arc<Mutex<Option<i32>>>,
    ) -> i32 {
        let ending = tokio::select! {
            ending = self.watch.ended() => ending,
            _ = caller_gone => {
                let Some(keep) = self.keep.take() else {
                    if let Some(follower) = self.follower.take() {
                        follower.finish().await;
                    }
                    return 0;
                };
                // What the machine still writes is read and dropped, or it would block.
                std::thread::spawn(move || {
                    let mut file = std::fs::File::from(keep);
                    let mut buf = [0u8; 4096];
                    while matches!(std::io::Read::read(&mut file, &mut buf), Ok(n) if n > 0) {}
                });
                let stop = crate::api::machines::StopRequest {
                    name: self.name.clone(),
                    force: false,
                    wait: true,
                    timeout: 10,
                };
                let _ = crate::api::machines::stop(&ctx, &stop, &|_| {}).await;
                self.watch.ended().await
            }
        };
        // A signal forwarded through the process object, or one kill or stop sent, read
        // at once: run --rm removes the machine, and what it kept, right after.
        let sent = sent
            .lock()
            .unwrap()
            .or_else(|| ctx.store.last_signal(&self.name));
        if let Some(follower) = self.follower.take() {
            follower.finish().await;
        }
        match ending {
            Ok(ending) => exit_code(&ending, sent, self.app),
            Err(_) => 125,
        }
    }
}

/// Properties of a transient unit that runs `argv` once and goes away when done.
pub fn transient_service(description: &str, argv: &[String]) -> Vec<(String, OwnedValue)> {
    let value = |v: zbus::zvariant::Value<'_>| {
        OwnedValue::try_from(v).expect("plain values carry no file descriptor")
    };
    vec![
        ("Description".into(), value(description.into())),
        (
            "ExecStart".into(),
            value(vec![(argv[0].clone(), argv.to_vec(), false)].into()),
        ),
        ("CollectMode".into(), value("inactive-or-failed".into())),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ending(code: i32, status: i32, result: &str) -> Ending {
        Ending {
            code,
            status,
            result: result.into(),
        }
    }

    #[test]
    fn exit_codes_as_docker_gives_them() {
        assert_eq!(
            exit_code(&ending(CLD_EXITED, 3, "exit-code"), None, true),
            3
        );
        assert_eq!(exit_code(&ending(CLD_EXITED, 0, "success"), None, true), 0);
        assert_eq!(
            exit_code(&ending(CLD_EXITED, 255, "exit-code"), Some(2), true),
            130,
            "Ctrl-C forwarded to the program"
        );
        assert_eq!(
            exit_code(&ending(CLD_EXITED, 255, "exit-code"), None, true),
            255,
            "a signal nobody here sent stays unknown"
        );
        assert_eq!(
            exit_code(&ending(CLD_EXITED, 255, "exit-code"), Some(15), false),
            255,
            "a booted machine's 255 is its own"
        );
        assert_eq!(exit_code(&ending(CLD_KILLED, 9, "signal"), None, true), 137);
        assert_eq!(
            exit_code(&ending(CLD_EXITED, 1, "exit-code"), Some(9), true),
            137,
            "kill of the whole machine on systemd 259 and newer"
        );
        assert_eq!(
            exit_code(&ending(CLD_EXITED, 1, "exit-code"), Some(15), true),
            1
        );
        assert_eq!(
            exit_code(&ending(CLD_DUMPED, 11, "core-dump"), None, true),
            139
        );
        assert_eq!(
            exit_code(&ending(CLD_EXITED, 1, "oom-kill"), None, true),
            137
        );
    }

    #[test]
    fn messages_are_read_back_as_they_were_written() {
        let text: Value = serde_json::json!({"MESSAGE": "hello\r", "_COMM": "systemd-nspawn"});
        assert_eq!(message_bytes(&text).unwrap(), b"hello");
        assert!(from_machine(&text));
        assert!(from_machine(
            &serde_json::json!({"MESSAGE": "4", "_COMM": "wc"})
        ));
        assert!(!from_machine(
            &serde_json::json!({"MESSAGE": "note", "_COMM": "nspawn"})
        ));
        let colours: Value = serde_json::json!({"MESSAGE": [27, 91, 51, 49, 109, 104, 105, 13]});
        assert_eq!(message_bytes(&colours).unwrap(), b"\x1b[31mhi");
        assert_eq!(message_bytes(&serde_json::json!({"MESSAGE": [300]})), None);
        assert_eq!(message_bytes(&serde_json::json!({})), None);
    }

    #[test]
    fn the_follower_reads_the_unit_after_the_cursor() {
        let argv = follower_arguments("systemd-nspawn@web.service", Some("s=1;i=2"));
        assert!(argv.contains(&"--after-cursor=s=1;i=2".to_string()));
        assert!(argv.contains(&"_SYSTEMD_UNIT=systemd-nspawn@web.service".to_string()));
        assert!(argv.contains(&"--follow".to_string()));
        assert!(follower_arguments("u", None).contains(&"--lines=0".to_string()));
    }
}
