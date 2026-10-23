//! Healthchecks: a probe run inside the machine at an interval by a transient
//! unit bound to the machine's, its verdict in /run/nspawn/health/NAME.json for ps and
//! inspect, and a journal event on every change.

use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{bail, Context as _, Result};
use serde::{Deserialize, Serialize};
use zbus::zvariant::OwnedValue;

use crate::api::Context;
use crate::nsenter;
use crate::store::now_unix;
use crate::systemd::Systemd;

pub const STATUS_DIR: &str = "/run/nspawn/health";

/// The defaults of HEALTHCHECK, in microseconds.
pub const DEFAULT_INTERVAL: u64 = 30_000_000;
pub const DEFAULT_TIMEOUT: u64 = 30_000_000;
pub const DEFAULT_START_INTERVAL: u64 = 5_000_000;
pub const DEFAULT_RETRIES: u32 = 3;
/// What is kept of a probe's output.
const OUTPUT_LIMIT: usize = 4096;
/// How many probes `inspect` shows.
const LOG_LIMIT: usize = 5;

/// An image's or a machine's healthcheck, with the fields of HEALTHCHECK.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Healthcheck {
    /// ["CMD", argv...], ["CMD-SHELL", "line"] or ["NONE"].
    pub test: Vec<String>,
    /// Microseconds; 0 stands for the default.
    #[serde(default)]
    pub interval: u64,
    #[serde(default)]
    pub timeout: u64,
    #[serde(default)]
    pub start_period: u64,
    #[serde(default)]
    pub start_interval: u64,
    #[serde(default)]
    pub retries: u32,
}

impl Healthcheck {
    /// The Healthcheck of an OCI image config, a docker extension: durations in
    /// nanoseconds under "config".
    pub fn from_config(config: &serde_json::Value) -> Option<Healthcheck> {
        let hc = config.get("config")?.get("Healthcheck")?;
        let test: Vec<String> = hc
            .get("Test")?
            .as_array()?
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(str::to_string)
            .collect();
        let usec = |key: &str| hc.get(key).and_then(serde_json::Value::as_u64).unwrap_or(0) / 1000;
        Some(Healthcheck {
            test,
            interval: usec("Interval"),
            timeout: usec("Timeout"),
            start_period: usec("StartPeriod"),
            start_interval: usec("StartInterval"),
            retries: hc
                .get("Retries")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0) as u32,
        })
    }

    pub fn disabled(&self) -> bool {
        self.test.is_empty() || self.test[0] == "NONE"
    }

    /// Whether it probes: not disabled, and a test a probe can run. An image's that is
    /// malformed is none, so that what waits for a verdict does not wait for ever.
    pub fn active(&self) -> bool {
        !self.disabled() && self.argv().is_ok()
    }

    /// The command the probe runs in an image whose SHELL is `shell`: CMD-SHELL goes
    /// through it, as getShell has it, /bin/sh -c when the image names none.
    pub fn argv_in(&self, shell: &[String]) -> Result<Vec<String>> {
        let mut argv = self.argv()?;
        if self.test.first().map(String::as_str) == Some("CMD-SHELL") && !shell.is_empty() {
            let line = argv.pop().unwrap_or_default();
            argv = shell.to_vec();
            argv.push(line);
        }
        Ok(argv)
    }

    /// The command the probe runs: CMD as it is, CMD-SHELL through /bin/sh -c.
    pub fn argv(&self) -> Result<Vec<String>> {
        match self.test.first().map(String::as_str) {
            Some("CMD") if self.test.len() > 1 => Ok(self.test[1..].to_vec()),
            Some("CMD-SHELL") if self.test.len() == 2 => Ok(vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                self.test[1].clone(),
            ]),
            _ => bail!(
                "healthcheck test {:?}: CMD with a command, CMD-SHELL with a line, or NONE",
                self.test
            ),
        }
    }

    pub fn interval(&self) -> Duration {
        usec(or_default(self.interval, DEFAULT_INTERVAL))
    }
    pub fn timeout(&self) -> Duration {
        usec(or_default(self.timeout, DEFAULT_TIMEOUT))
    }
    pub fn start_period(&self) -> Duration {
        usec(self.start_period)
    }
    pub fn start_interval(&self) -> Duration {
        usec(or_default(self.start_interval, DEFAULT_START_INTERVAL))
    }
    pub fn retries(&self) -> u32 {
        if self.retries > 0 {
            self.retries
        } else {
            DEFAULT_RETRIES
        }
    }
}

fn or_default(value: u64, default: u64) -> u64 {
    if value > 0 {
        value
    } else {
        default
    }
}

fn usec(value: u64) -> Duration {
    Duration::from_micros(value)
}

/// The --health-* flags: each one given replaces its part of the healthcheck the machine
/// has (its own, or the image's).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overrides {
    /// --health-cmd: a shell line.
    pub cmd: Option<String>,
    /// The whole test, as compose writes it: ["CMD", argv...] runs without a shell.
    pub test: Option<Vec<String>>,
    pub interval: Option<u64>,
    pub timeout: Option<u64>,
    pub start_period: Option<u64>,
    pub start_interval: Option<u64>,
    pub retries: Option<u32>,
    /// --no-healthcheck.
    pub disable: bool,
}

impl Overrides {
    pub fn is_empty(&self) -> bool {
        *self == Overrides::default()
    }

    /// The machine's healthcheck after these flags, on top of `base`.
    pub fn apply(&self, base: Option<&Healthcheck>) -> Result<Healthcheck> {
        if self.disable {
            if *self
                != (Overrides {
                    disable: true,
                    ..Overrides::default()
                })
            {
                bail!("--no-healthcheck excludes the other --health flags");
            }
            return Ok(Healthcheck {
                test: vec!["NONE".to_string()],
                ..Healthcheck::default()
            });
        }
        let mut hc = base.cloned().unwrap_or_default();
        if let Some(cmd) = &self.cmd {
            hc.test = vec!["CMD-SHELL".to_string(), cmd.clone()];
        }
        if let Some(test) = &self.test {
            hc.test.clone_from(test);
        }
        // Timings for an image without a healthcheck are taken and probe nothing.
        if !hc.disabled() {
            hc.argv()?;
        }
        for (given, field, name) in [
            (self.interval, &mut hc.interval, "Interval"),
            (self.timeout, &mut hc.timeout, "Timeout"),
            (self.start_period, &mut hc.start_period, "StartPeriod"),
            (self.start_interval, &mut hc.start_interval, "StartInterval"),
        ] {
            if let Some(value) = given {
                // What the bus brings as microseconds is held to Go's range too, and to
                // a millisecond at least.
                if value > (i64::MAX / 1000) as u64 {
                    bail!("a healthcheck duration of {value} microseconds is past Go's range");
                }
                if value != 0 && value < 1000 {
                    bail!("{name} in Healthcheck cannot be less than 1ms");
                }
                *field = value;
            }
        }
        if let Some(retries) = self.retries {
            hc.retries = retries;
        }
        Ok(hc)
    }
}

/// A duration: 10s, 1m30s, 500ms, 1.5h (units ns, us, ms, s, m, h);
/// microseconds, at least a millisecond unless 0.
pub fn parse_duration(text: &str) -> Result<u64> {
    let bad = || anyhow::anyhow!("{text}: expected a duration such as 10s, 1m30s or 500ms");
    if text == "0" {
        return Ok(0);
    }
    let mut total = 0f64;
    let mut rest = text;
    if rest.is_empty() {
        return Err(bad());
    }
    while !rest.is_empty() {
        let digits = rest
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .ok_or_else(bad)?;
        let number: f64 = rest[..digits].parse().map_err(|_| bad())?;
        rest = &rest[digits..];
        let unit = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let scale = match &rest[..unit] {
            "ns" => 0.001,
            "us" | "\u{b5}s" => 1.0,
            "ms" => 1000.0,
            "s" => 1_000_000.0,
            "m" => 60_000_000.0,
            "h" => 3_600_000_000.0,
            _ => return Err(bad()),
        };
        rest = &rest[unit..];
        total += number * scale;
    }
    // Go's Duration holds nanoseconds in an int64: about 292 years, past which it
    // refuses, and past which sums made of it would wrap.
    const MAX_USEC: f64 = (i64::MAX / 1000) as f64;
    if total > MAX_USEC {
        bail!("{text}: a duration of more than about 292 years");
    }
    let total = total.round() as u64;
    if total > 0 && total < 1000 {
        bail!("{text}: a duration of at least a millisecond");
    }
    Ok(total)
}

/// A duration as parse_duration reads it: 1m30s, 500ms.
pub fn format_duration(usec: u64) -> String {
    if usec == 0 {
        return "0s".to_string();
    }
    if !usec.is_multiple_of(1_000_000) {
        // What parse_duration reads back to the same microseconds.
        return if usec.is_multiple_of(1000) {
            format!("{}ms", usec / 1000)
        } else {
            format!("{usec}us")
        };
    }
    let seconds = usec / 1_000_000;
    let (h, m, s) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);
    let mut out = String::new();
    if h > 0 {
        out.push_str(&format!("{h}h"));
    }
    if m > 0 {
        out.push_str(&format!("{m}m"));
    }
    if s > 0 || out.is_empty() {
        out.push_str(&format!("{s}s"));
    }
    out
}

/// Go's time.Duration.String of microseconds: 1.5s, 500ms, 1m30s, 1h0m0s.
fn go_duration(usec: u64) -> String {
    let fraction = |value: u64, digits: usize| -> String {
        let text = format!("{value:0digits$}");
        let text = text.trim_end_matches('0');
        if text.is_empty() {
            String::new()
        } else {
            format!(".{text}")
        }
    };
    if usec == 0 {
        return "0s".to_string();
    }
    if usec < 1000 {
        return format!("{usec}\u{b5}s");
    }
    if usec < 1_000_000 {
        return format!("{}{}ms", usec / 1000, fraction(usec % 1000, 3));
    }
    let seconds = usec / 1_000_000;
    let (h, m, s) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);
    let mut out = String::new();
    if h > 0 {
        out.push_str(&format!("{h}h{m}m"));
    } else if m > 0 {
        out.push_str(&format!("{m}m"));
    }
    out.push_str(&format!("{s}{}s", fraction(usec % 1_000_000, 6)));
    out
}

/// One probe, as `inspect` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Probe {
    /// Unix seconds.
    pub start: u64,
    pub end: u64,
    /// -1 when it timed out.
    pub exit_code: i32,
    pub output: String,
}

/// The verdict on a machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// starting, healthy or unhealthy.
    pub status: String,
    pub failing_streak: u32,
    /// The last probes, oldest first.
    pub log: Vec<Probe>,
}

/// A success makes the machine healthy; a failure counts towards
/// `retries`, at which it is unhealthy, unless it comes during the start period of a
/// machine that was never healthy.
pub struct Monitor {
    pub status: Status,
    retries: u32,
}

impl Monitor {
    pub fn new(retries: u32) -> Self {
        Monitor {
            status: Status {
                status: "starting".to_string(),
                failing_streak: 0,
                log: Vec::new(),
            },
            retries,
        }
    }

    /// Records a probe; the new status when it changed.
    pub fn observe(&mut self, probe: Probe, in_start_period: bool) -> Option<&str> {
        let before = self.status.status.clone();
        if self.status.log.len() >= LOG_LIMIT {
            self.status.log.remove(0);
        }
        let failed = probe.exit_code != 0;
        self.status.log.push(probe);
        if !failed {
            self.status.status = "healthy".to_string();
            self.status.failing_streak = 0;
        } else if !(in_start_period && self.status.status == "starting") {
            self.status.failing_streak += 1;
            if self.status.failing_streak >= self.retries {
                self.status.status = "unhealthy".to_string();
            }
        }
        (self.status.status != before).then_some(self.status.status.as_str())
    }
}

fn status_path(name: &str) -> PathBuf {
    PathBuf::from(STATUS_DIR).join(format!("{name}.json"))
}

pub fn write_status(name: &str, status: &Status) -> Result<()> {
    crate::store::create_private_dir(STATUS_DIR)?;
    crate::store::write_atomically(&status_path(name), &serde_json::to_vec(status)?)
}

/// The verdict on a running machine, if it has a healthcheck.
pub fn read_status(name: &str) -> Option<Status> {
    let text = std::fs::read(status_path(name)).ok()?;
    serde_json::from_slice(&text).ok()
}

pub fn clear_status(name: &str) {
    let _ = std::fs::remove_file(status_path(name));
}

pub fn unit_name(name: &str) -> String {
    format!("nspawn-health-{name}.service")
}

/// Starts the runner of a machine's probes: a transient unit bound to the machine's,
/// gone with it. Idempotent: a runner already there is replaced.
pub async fn start_runner(ctx: &Context, name: &str) -> Result<()> {
    let sd = ctx.sd().await?;
    let mut argv = crate::settings::hook_argv(&ctx.config)?;
    argv.extend(["health-run".to_string(), name.to_string()]);
    let machine_unit = format!("systemd-nspawn@{name}.service");
    let value = |v: zbus::zvariant::Value<'_>| {
        OwnedValue::try_from(v).expect("plain values carry no file descriptor")
    };
    let mut properties =
        crate::api::run::transient_service(&format!("Health probes of machine {name}"), &argv);
    // Requisite, not BindsTo: the runner never pulls the machine in (an update of a
    // machine that stopped meanwhile would start it again), and it leaves by itself
    // once the machine is gone.
    properties.push(("Requisite".into(), value(vec![machine_unit.clone()].into())));
    properties.push(("After".into(), value(vec![machine_unit].into())));
    properties.push(("CollectMode".into(), value("inactive-or-failed".into())));
    // A runner of an earlier run goes first: a transient unit is made only where none
    // is loaded, and a restart of the machine may leave one.
    let unit = unit_name(name);
    if sd.unit_status(&unit).await?.busy() || sd.unit_queued(&unit).await? {
        sd.stop_unit(&unit).await?;
    }
    sd.start_transient(&unit, properties, "replace").await
}

pub async fn stop_runner(sd: &Systemd, name: &str) -> Result<()> {
    sd.stop_unit(&unit_name(name)).await
}

/// Queues the stop of a machine's runner, for its release hook: a machine whose program
/// ended on its own takes no stop job its runner would follow (Requisite= passes on
/// stops, not ends).
pub async fn stop_runner_soon(sd: &Systemd, name: &str) -> Result<()> {
    sd.queue_stop(&unit_name(name)).await
}

/// `nspawn health-run NAME`, from the transient unit: probes the machine until it is
/// gone.
pub async fn run(ctx: &Context, name: &str) -> Result<()> {
    crate::reference::validate_entry_name(name)?;
    let record = ctx
        .store
        .load_image(name)?
        .with_context(|| format!("{name} is not an image managed by nspawn"))?;
    let Some(hc) = record
        .effective_healthcheck()
        .filter(|h| h.active())
        .cloned()
    else {
        return Ok(());
    };
    let argv = hc.argv_in(&record.run.shell)?;
    let sd = ctx.sd().await?;
    let unit = format!("systemd-nspawn@{name}.service");
    // The run it probes: a later run of the machine, or a later machine of the name, has
    // a runner of its own, and maybe another healthcheck or none.
    let run = sd
        .invocation_id(&unit)
        .await
        .ok()
        .filter(|id| !id.is_empty());
    let started = Instant::now();
    let mut monitor = Monitor::new(hc.retries());
    write_status(name, &monitor.status)?;
    loop {
        let in_start_period = started.elapsed() < hc.start_period();
        let pause = if in_start_period && monitor.status.status == "starting" {
            hc.start_interval()
        } else {
            hc.interval()
        };
        tokio::time::sleep(pause).await;
        // machined restarting or a call that timed out is asked again at the next
        // probe; only a machine that is gone ends the runner, whose unit goes with it.
        match sd.machine_exists(name).await {
            Ok(true) => {}
            Ok(false) => return Ok(()),
            Err(e) => {
                eprintln!("warning: {e:#}; probing again in {pause:?}");
                continue;
            }
        }
        if let (Some(run), Ok(now)) = (&run, sd.invocation_id(&unit).await) {
            if &now != run {
                return Ok(());
            }
        }
        // A paused machine is not probed: nothing in it runs, and its verdict stays.
        if sd.freezer_state(&unit).await.ok().as_deref() == Some("frozen") {
            continue;
        }
        // A probe counts by when it started: one that ends past the start period still
        // belongs to it.
        let in_start_period = started.elapsed() < hc.start_period();
        let probe = match probe(ctx, name, &argv, hc.timeout()).await {
            Ok(probe) => probe,
            Err(_) if matches!(sd.machine_exists(name).await, Ok(false)) => return Ok(()),
            Err(e) => Probe {
                start: now_unix(),
                end: now_unix(),
                exit_code: -1,
                output: format!("{e:#}"),
            },
        };
        if let Some(status) = monitor.observe(probe, in_start_period) {
            crate::api::events::emit_machine(
                &ctx.store,
                "health_status",
                name,
                &[("image", &record.reference), ("status", status)],
            );
        }
        if let Err(e) = write_status(name, &monitor.status) {
            eprintln!("warning: {e:#}");
        }
    }
}

/// SIGKILL to the command's process group, which the command leads: what it forked and
/// left behind goes with it.
fn kill_group(pid: u32) {
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(-(pid as i32)),
        nix::sys::signal::Signal::SIGKILL,
    );
}

/// What a reader of the probe's output got. The pipes close once the command and
/// whatever it forked are gone: a child that keeps them open past the command is
/// killed after a moment, and the read given up on after another.
async fn drained(mut reader: tokio::task::JoinHandle<String>, pid: u32) -> String {
    let grace = Duration::from_secs(2);
    if let Ok(read) = tokio::time::timeout(grace, &mut reader).await {
        return read.unwrap_or_default();
    }
    kill_group(pid);
    match tokio::time::timeout(grace, &mut reader).await {
        Ok(read) => read.unwrap_or_default(),
        Err(_) => {
            reader.abort();
            String::new()
        }
    }
}

/// One run of the test inside the machine, killed at `timeout`.
async fn probe(ctx: &Context, name: &str, argv: &[String], timeout: Duration) -> Result<Probe> {
    let start = now_unix();
    // As the machine's user: the test comes with the image.
    let user = ctx
        .store
        .load_image(name)?
        .and_then(|r| r.effective_user().map(str::to_string))
        .unwrap_or_default();
    let process = crate::api::machines::spawn_in_namespaces(
        ctx,
        name,
        argv,
        &user,
        &[],
        None,
        nsenter::Stdio::Pipes,
    )
    .await?;
    let nsenter::Process {
        helper,
        pid,
        pidfd,
        stdin,
        stdout,
        stderr,
        ..
    } = process;
    drop(stdin);
    // Read by tasks that giving up on them ends, pipes closed with them: a blocking read
    // would stay, and its thread, for as long as some child of the probe holds the pipe.
    let out = tokio::spawn(read_capped(stdout));
    let err = tokio::spawn(read_capped(stderr));
    let mut waited = tokio::task::spawn_blocking(move || nsenter::wait(helper));
    let exit_code = match tokio::time::timeout(timeout, &mut waited).await {
        Ok(joined) => joined.context("waiting for the probe")??,
        Err(_) => {
            // The command and what it forked: a child left holding the pipes would
            // hold the probe's output, and the runner with it.
            kill_group(pid);
            let _ = nsenter::pidfd_signal(&pidfd, nix::libc::SIGKILL);
            let _ = waited.await;
            -1
        }
    };
    let mut output = drained(out, pid).await;
    output.push_str(&drained(err, pid).await);
    // A probe that ran out of time: the timeout said, with what it printed before it
    // was stopped.
    if exit_code == -1 {
        let limit = go_duration(timeout.as_micros() as u64);
        output = if output.is_empty() {
            format!("Health check exceeded timeout ({limit})")
        } else {
            format!("Health check exceeded timeout ({limit}): {output}")
        };
    }
    output.truncate(
        output
            .char_indices()
            .nth(OUTPUT_LIMIT)
            .map_or(output.len(), |(i, _)| i),
    );
    Ok(Probe {
        start,
        end: now_unix(),
        exit_code,
        output,
    })
}

/// The first OUTPUT_LIMIT bytes of a stream; the rest is read and dropped, so
/// that a probe which says more never dies of SIGPIPE on a pipe closed early.
async fn read_capped(fd: Option<OwnedFd>) -> String {
    use tokio::io::AsyncReadExt;
    let Some(fd) = fd else {
        return String::new();
    };
    let Ok(mut pipe) = tokio::net::unix::pipe::Receiver::from_owned_fd(fd) else {
        return String::new();
    };
    let mut kept = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match pipe.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                let room = OUTPUT_LIMIT.saturating_sub(kept.len());
                kept.extend_from_slice(&buf[..n.min(room)]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    // What the machine prints goes on the bus, which takes no NUL.
    String::from_utf8_lossy(&kept).replace('\0', "\u{FFFD}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timings_under_a_millisecond_are_refused() {
        let given = |interval: u64| Overrides {
            interval: Some(interval),
            ..Overrides::default()
        };
        let e = given(500).apply(None).unwrap_err().to_string();
        assert_eq!(e, "Interval in Healthcheck cannot be less than 1ms");
        assert_eq!(given(1000).apply(None).unwrap().interval, 1000);
        assert_eq!(given(0).apply(None).unwrap().interval, 0);
    }

    #[test]
    fn durations_past_go_s_range_are_refused() {
        assert!(parse_duration("6000000h").is_err());
        assert!(parse_duration("2000000h").is_ok());
    }

    #[test]
    fn durations_print_as_go_prints_them() {
        assert_eq!(go_duration(1_500_000), "1.5s");
        assert_eq!(go_duration(500_000), "500ms");
        assert_eq!(go_duration(1_500), "1.5ms");
        assert_eq!(go_duration(90_000_000), "1m30s");
        assert_eq!(go_duration(3_600_000_000), "1h0m0s");
        assert_eq!(go_duration(30_000_000), "30s");
        assert_eq!(go_duration(500), "500\u{b5}s");
    }

    #[test]
    fn durations_are_read_with_their_units() {
        assert_eq!(parse_duration("10s").unwrap(), 10_000_000);
        assert_eq!(parse_duration("1m30s").unwrap(), 90_000_000);
        assert_eq!(parse_duration("500ms").unwrap(), 500_000);
        assert_eq!(parse_duration("1.5h").unwrap(), 5_400_000_000);
        assert_eq!(parse_duration("0").unwrap(), 0);
        for bad in ["", "10", "s", "10x", "-1s", "1s2"] {
            assert!(parse_duration(bad).is_err(), "{bad:?}");
        }
        assert!(parse_duration("500us").is_err(), "less than a millisecond");
        assert_eq!(format_duration(90_000_000), "1m30s");
        assert_eq!(format_duration(500_000), "500ms");
        assert_eq!(format_duration(3_600_000_000), "1h");
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(1500), "1500us");
        assert_eq!(parse_duration(&format_duration(1500)).unwrap(), 1500);
    }

    #[test]
    fn the_image_healthcheck_is_read_from_the_config() {
        let config: serde_json::Value = serde_json::from_str(
            r#"{"config": {"Cmd": ["nginx"], "Healthcheck": {"Test": ["CMD-SHELL", "curl -f http://localhost/"], "Interval": 30000000000, "Timeout": 3000000000, "Retries": 3, "StartPeriod": 5000000000}}}"#,
        )
        .unwrap();
        let hc = Healthcheck::from_config(&config).unwrap();
        assert_eq!(hc.test, ["CMD-SHELL", "curl -f http://localhost/"]);
        assert_eq!(hc.interval, 30_000_000);
        assert_eq!(hc.timeout, 3_000_000);
        assert_eq!(hc.start_period, 5_000_000);
        assert_eq!(hc.start_interval(), Duration::from_secs(5), "the default");
        assert_eq!(
            hc.argv().unwrap(),
            ["/bin/sh", "-c", "curl -f http://localhost/"]
        );
        let none: serde_json::Value =
            serde_json::from_str(r#"{"config": {"Healthcheck": {"Test": ["NONE"]}}}"#).unwrap();
        assert!(Healthcheck::from_config(&none).unwrap().disabled());
        assert_eq!(
            Healthcheck::from_config(&serde_json::json!({"config": {}})),
            None
        );
        let cmd = Healthcheck {
            test: vec!["CMD".into(), "test".into(), "-f".into(), "/ok".into()],
            ..Healthcheck::default()
        };
        assert_eq!(cmd.argv().unwrap(), ["test", "-f", "/ok"]);
        assert!(Healthcheck {
            test: vec!["CMD".into()],
            ..Healthcheck::default()
        }
        .argv()
        .is_err());
    }

    #[test]
    fn flags_apply_on_top_of_the_image() {
        let image = Healthcheck {
            test: vec!["CMD-SHELL".into(), "curl -f http://localhost/".into()],
            interval: 30_000_000,
            ..Healthcheck::default()
        };
        let flags = Overrides {
            retries: Some(5),
            timeout: Some(2_000_000),
            ..Overrides::default()
        };
        let hc = flags.apply(Some(&image)).unwrap();
        assert_eq!(hc.test, image.test);
        assert_eq!(
            (hc.retries, hc.timeout, hc.interval),
            (5, 2_000_000, 30_000_000)
        );
        let own = Overrides {
            cmd: Some("test -f /ok".into()),
            ..Overrides::default()
        };
        assert_eq!(own.apply(None).unwrap().test, ["CMD-SHELL", "test -f /ok"]);
        let exec = Overrides {
            test: Some(vec!["CMD".into(), "/app/check".into(), "a b".into()]),
            ..Overrides::default()
        };
        let hc = exec.apply(None).unwrap();
        assert_eq!(hc.argv().unwrap(), ["/app/check", "a b"]);
        // Timings with nothing to probe are taken, and probe nothing.
        let bare = flags.apply(None).unwrap();
        assert!(!bare.active() && bare.retries == 5);
        // CMD-SHELL in the image's SHELL; CMD as it is.
        let shell = vec!["/busybox/sh".to_string(), "-c".to_string()];
        let line = Healthcheck {
            test: vec!["CMD-SHELL".into(), "wget -q localhost".into()],
            ..Healthcheck::default()
        };
        assert_eq!(
            line.argv_in(&shell).unwrap(),
            ["/busybox/sh", "-c", "wget -q localhost"]
        );
        assert_eq!(
            line.argv_in(&[]).unwrap(),
            ["/bin/sh", "-c", "wget -q localhost"]
        );
        let direct = Healthcheck {
            test: vec!["CMD".into(), "/check".into()],
            ..Healthcheck::default()
        };
        assert_eq!(direct.argv_in(&shell).unwrap(), ["/check"]);
        // An image's test no probe can run is none.
        let malformed = Healthcheck {
            test: vec!["CMD".into()],
            ..Healthcheck::default()
        };
        assert!(!malformed.disabled() && !malformed.active());
        let off = Overrides {
            disable: true,
            ..Overrides::default()
        };
        assert!(off.apply(Some(&image)).unwrap().disabled());
        assert!(Overrides {
            disable: true,
            retries: Some(1),
            ..Overrides::default()
        }
        .apply(Some(&image))
        .is_err());
        assert!(Overrides::default().is_empty() && !off.is_empty());
    }

    #[tokio::test]
    async fn giving_up_on_a_probe_s_output_closes_the_pipe() {
        use std::io::Write;
        // A child of the probe that kept the pipe: the reader is let go of, and the
        // pipe with it. Close-on-exec, as the probes' pipes are; a process another test
        // starts meanwhile still holds the read end between its fork and its exec, so
        // the write is tried until that copy is gone.
        let (read, write) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC).unwrap();
        let reader = tokio::spawn(read_capped(Some(read)));
        tokio::time::sleep(Duration::from_millis(50)).await;
        reader.abort();
        assert!(reader.await.unwrap_err().is_cancelled());
        let mut write = std::fs::File::from(write);
        let mut tries = 0;
        let e = loop {
            match write.write_all(b"x") {
                Err(e) => break e,
                Ok(()) if tries < 100 => tries += 1,
                Ok(()) => panic!("the pipe is still open after the reader was let go of"),
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert_eq!(e.kind(), std::io::ErrorKind::BrokenPipe);
    }

    #[tokio::test]
    async fn a_chatty_probe_is_drained_and_its_output_capped() {
        use std::io::Write;
        let (read, write) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC).unwrap();
        let writer = std::thread::spawn(move || {
            let mut pipe = std::fs::File::from(write);
            // Well past the pipe's buffer: a reader that stopped at the cap would leave
            // this write hanging, and one that closed the pipe would break it.
            let chunk = vec![b'x'; 65536];
            for _ in 0..4 {
                pipe.write_all(&chunk)?;
            }
            Ok::<(), std::io::Error>(())
        });
        let output = read_capped(Some(read)).await;
        assert_eq!(output.len(), OUTPUT_LIMIT);
        writer
            .join()
            .unwrap()
            .expect("the probe's writes all went through");
        let (read, write) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC).unwrap();
        std::fs::File::from(write).write_all(b"ok\n").unwrap();
        assert_eq!(read_capped(Some(read)).await, "ok\n");
        assert_eq!(read_capped(None).await, "");
    }

    #[test]
    fn the_verdict_follows_the_failing_streak() {
        let probe = |code: i32| Probe {
            start: 1,
            end: 2,
            exit_code: code,
            output: String::new(),
        };
        let mut m = Monitor::new(2);
        assert_eq!(m.status.status, "starting");
        assert_eq!(
            m.observe(probe(1), true),
            None,
            "a failure in the start period"
        );
        assert_eq!(m.status.failing_streak, 0);
        assert_eq!(m.observe(probe(1), false), None, "one short of the retries");
        assert_eq!(m.observe(probe(1), false), Some("unhealthy"));
        assert_eq!(m.observe(probe(0), false), Some("healthy"));
        assert_eq!(m.status.failing_streak, 0);
        assert_eq!(
            m.observe(probe(1), true),
            None,
            "once healthy, the start period is over"
        );
        assert_eq!(m.status.failing_streak, 1);
        for _ in 0..10 {
            m.observe(probe(0), false);
        }
        assert_eq!(m.status.log.len(), LOG_LIMIT);
        let text = serde_json::to_string(&m.status).unwrap();
        assert_eq!(serde_json::from_str::<Status>(&text).unwrap(), m.status);
    }
}
