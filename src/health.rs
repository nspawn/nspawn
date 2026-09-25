//! docker's healthchecks: a probe run inside the machine at an interval by a transient
//! unit bound to the machine's, its verdict in /run/nspawn/health/NAME.json for ps and
//! inspect, and a journal event on every change.

use std::io::Read;
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

/// docker's defaults, in microseconds.
pub const DEFAULT_INTERVAL: u64 = 30_000_000;
pub const DEFAULT_TIMEOUT: u64 = 30_000_000;
pub const DEFAULT_START_INTERVAL: u64 = 5_000_000;
pub const DEFAULT_RETRIES: u32 = 3;
/// What is kept of a probe's output.
const OUTPUT_LIMIT: usize = 4096;
/// How many probes `inspect` shows.
const LOG_LIMIT: usize = 5;

/// An image's or a machine's healthcheck, as docker's HEALTHCHECK has it.
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
    /// --health-cmd: a shell line, as docker takes it.
    pub cmd: Option<String>,
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
        if hc.disabled() {
            bail!("the image has no healthcheck; --health-cmd gives the command to run");
        }
        hc.argv()?;
        for (given, field) in [
            (self.interval, &mut hc.interval),
            (self.timeout, &mut hc.timeout),
            (self.start_period, &mut hc.start_period),
            (self.start_interval, &mut hc.start_interval),
        ] {
            if let Some(value) = given {
                *field = value;
            }
        }
        if let Some(retries) = self.retries {
            hc.retries = retries;
        }
        Ok(hc)
    }
}

/// A duration as docker takes it: 10s, 1m30s, 500ms, 1.5h (units ns, us, ms, s, m, h);
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
    let total = total.round() as u64;
    if total > 0 && total < 1000 {
        bail!("{text}: a duration of at least a millisecond");
    }
    Ok(total)
}

/// A duration as docker prints it: 1m30s, 500ms.
pub fn format_duration(usec: u64) -> String {
    if usec == 0 {
        return "0s".to_string();
    }
    if !usec.is_multiple_of(1_000_000) {
        return format!("{}ms", usec / 1000);
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

/// docker's rule: a success makes the machine healthy; a failure counts towards
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
    properties.push(("BindsTo".into(), value(vec![machine_unit.clone()].into())));
    properties.push(("After".into(), value(vec![machine_unit].into())));
    sd.start_transient(&unit_name(name), properties, "replace")
        .await
}

pub async fn stop_runner(sd: &Systemd, name: &str) -> Result<()> {
    sd.stop_unit(&unit_name(name)).await
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
        .filter(|h| !h.disabled())
        .cloned()
    else {
        return Ok(());
    };
    let argv = hc.argv()?;
    let sd = ctx.sd().await?;
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
        if !sd.machine_exists(name).await? {
            return Ok(());
        }
        let probe = match probe(ctx, name, &argv, hc.timeout()).await {
            Ok(probe) => probe,
            Err(e) if !sd.machine_exists(name).await? => {
                let _ = e;
                return Ok(());
            }
            Err(e) => Probe {
                start: now_unix(),
                end: now_unix(),
                exit_code: -1,
                output: format!("{e:#}"),
            },
        };
        let in_start_period = started.elapsed() < hc.start_period();
        if let Some(status) = monitor.observe(probe, in_start_period) {
            crate::api::events::emit(
                "machine",
                "health_status",
                name,
                &[("image", &record.reference), ("status", status)],
            );
        }
        write_status(name, &monitor.status)?;
    }
}

/// One run of the test inside the machine, killed at `timeout`.
async fn probe(ctx: &Context, name: &str, argv: &[String], timeout: Duration) -> Result<Probe> {
    let start = now_unix();
    let process = crate::api::machines::spawn_in_namespaces(
        ctx,
        name,
        argv,
        "root",
        &[],
        None,
        nsenter::Stdio::Pipes,
    )
    .await?;
    let nsenter::Process {
        helper,
        pidfd,
        stdin,
        stdout,
        stderr,
        ..
    } = process;
    drop(stdin);
    let out = tokio::task::spawn_blocking(move || read_capped(stdout));
    let err = tokio::task::spawn_blocking(move || read_capped(stderr));
    let mut waited = tokio::task::spawn_blocking(move || nsenter::wait(helper));
    let exit_code = match tokio::time::timeout(timeout, &mut waited).await {
        Ok(joined) => joined.context("waiting for the probe")??,
        Err(_) => {
            let _ = nsenter::pidfd_signal(&pidfd, nix::libc::SIGKILL);
            let _ = waited.await;
            -1
        }
    };
    let mut output = out.await.context("reading the probe")?;
    output.push_str(&err.await.context("reading the probe")?);
    if exit_code == -1 {
        output = format!(
            "Health check exceeded timeout ({})",
            format_duration(timeout.as_micros() as u64)
        );
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

fn read_capped(fd: Option<OwnedFd>) -> String {
    let Some(fd) = fd else {
        return String::new();
    };
    let mut bytes = Vec::new();
    let _ = std::fs::File::from(fd)
        .take(OUTPUT_LIMIT as u64)
        .read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_as_docker_takes_them() {
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
        assert!(flags.apply(None).is_err(), "nothing to probe with");
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

    #[test]
    fn the_verdict_follows_docker() {
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
