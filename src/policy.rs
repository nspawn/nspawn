//! What a machine's unit carries besides the hooks: docker's restart policies and the
//! resource limits of `-m`, `--cpus` and `--pids-limit`, both remembered in the record
//! and written into the unit's drop-in.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::oci::Mode;

/// docker's restart policies. `always` and `unless-stopped` also start the machine at
/// boot; `nspawn stop` of an `unless-stopped` machine takes that back until the next
/// `start`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Restart {
    /// Never restart (the default).
    #[default]
    No,
    /// Restart when the program or the machine fails.
    OnFailure,
    /// Restart whenever it ends, and start it at boot.
    Always,
    /// Like always, until nspawn stop.
    UnlessStopped,
}

impl Restart {
    pub fn name(self) -> &'static str {
        match self {
            Restart::No => "no",
            Restart::OnFailure => "on-failure",
            Restart::Always => "always",
            Restart::UnlessStopped => "unless-stopped",
        }
    }

    pub fn parse(text: &str) -> Result<Self> {
        Ok(match text {
            "no" => Restart::No,
            "on-failure" => Restart::OnFailure,
            "always" => Restart::Always,
            "unless-stopped" => Restart::UnlessStopped,
            other => {
                bail!("unknown restart policy {other}: no, on-failure, always or unless-stopped")
            }
        })
    }

    /// The unit's Restart= for this policy, none for `no`.
    pub fn unit_setting(self) -> Option<&'static str> {
        match self {
            Restart::No => None,
            Restart::OnFailure => Some("on-failure"),
            Restart::Always | Restart::UnlessStopped => Some("always"),
        }
    }

    /// Whether the machine's unit is enabled, so that it starts at boot.
    pub fn enabled_at_boot(self) -> bool {
        matches!(self, Restart::Always | Restart::UnlessStopped)
    }
}

/// Resource limits of a machine; zero means none. They bound the machine's unit, i.e.
/// systemd-nspawn and everything in the machine together.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    /// Bytes (MemoryMax=).
    #[serde(default)]
    pub memory: u64,
    /// Thousandths of a CPU (CPUQuota=, 1000 = one CPU).
    #[serde(default)]
    pub milli_cpus: u64,
    /// Processes and threads (TasksMax=).
    #[serde(default)]
    pub pids: u64,
}

/// Below this systemd would kill the supervisor before the program had a chance.
const MIN_MEMORY: u64 = 4 * 1024 * 1024;
/// A booted image runs a service manager, journald and friends before anything else.
const MIN_BOOT_PIDS: u64 = 16;

impl Limits {
    /// Refuses limits a machine cannot live with.
    pub fn check(&self, mode: Mode) -> Result<()> {
        if self.memory > 0 && self.memory < MIN_MEMORY {
            bail!("--memory must be at least 4m (or 0 for no limit)");
        }
        if mode == Mode::Boot && self.pids > 0 && self.pids < MIN_BOOT_PIDS {
            bail!("--pids-limit must be at least {MIN_BOOT_PIDS} for a booted machine (or 0 for no limit)");
        }
        Ok(())
    }

    /// CPUQuota= for the CPU limit: percent of one CPU, with one decimal only when it
    /// is needed.
    pub fn cpu_quota(&self) -> Option<String> {
        if self.milli_cpus == 0 {
            return None;
        }
        let whole = self.milli_cpus / 10;
        let tenth = self.milli_cpus % 10;
        Some(if tenth == 0 {
            format!("{whole}%")
        } else {
            format!("{whole}.{tenth}%")
        })
    }

    /// The CPU limit as docker's --cpus writes it (0.5, 2).
    pub fn cpus(&self) -> f64 {
        self.milli_cpus as f64 / 1000.0
    }

    /// The limits as the unit properties systemd changes on a running unit, no limit
    /// being infinity (u64::MAX); the drop-in's MemoryMax=, MemorySwapMax=, CPUQuota=
    /// and TasksMax=.
    pub fn unit_properties(&self) -> [(&'static str, u64); 4] {
        let or_infinity = |value: u64| if value == 0 { u64::MAX } else { value };
        [
            ("MemoryMax", or_infinity(self.memory)),
            ("MemorySwapMax", or_infinity(self.memory)),
            // Thousandths of a CPU are milliseconds of CPU time per second.
            (
                "CPUQuotaPerSecUSec",
                or_infinity(self.milli_cpus.saturating_mul(1000)),
            ),
            ("TasksMax", or_infinity(self.pids)),
        ]
    }
}

/// docker's --memory: a number of bytes, or one with b, k, m, g or t (1024-based,
/// case-insensitive, an optional trailing b or ib); decimals allowed; 0 for none.
pub fn parse_memory(text: &str) -> Result<u64> {
    let lower = text.trim().to_ascii_lowercase();
    let unit_start = lower
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(lower.len());
    let (number, unit) = lower.split_at(unit_start);
    let factor: u64 = match unit.trim_end_matches("ib").trim_end_matches('b') {
        "" => 1,
        "k" => 1 << 10,
        "m" => 1 << 20,
        "g" => 1 << 30,
        "t" => 1 << 40,
        _ => bail!("{text}: a size is a number with b, k, m, g or t, like 512m"),
    };
    let value: f64 = number.parse().map_err(|_| {
        anyhow::anyhow!("{text}: a size is a number with b, k, m, g or t, like 512m")
    })?;
    if !value.is_finite() || value < 0.0 {
        bail!("{text}: not a size");
    }
    let bytes = (value * factor as f64).round();
    if bytes >= u64::MAX as f64 {
        bail!("{text}: too large");
    }
    let bytes = bytes as u64;
    if bytes > 0 && bytes < MIN_MEMORY {
        bail!("{text}: --memory must be at least 4m (or 0 for no limit)");
    }
    Ok(bytes)
}

/// docker's --cpus (0.5, 2) as thousandths of a CPU; 0 for none.
pub fn milli_cpus_from(cpus: f64) -> Result<u64> {
    if !cpus.is_finite() || cpus < 0.0 {
        bail!("--cpus {cpus}: expected a number of CPUs like 0.5 or 2");
    }
    let milli = (cpus * 1000.0).round();
    if milli == 0.0 && cpus != 0.0 {
        bail!("--cpus {cpus}: too small; the least is 0.001");
    }
    if milli > 1_000_000_000.0 {
        bail!("--cpus {cpus}: too large");
    }
    Ok(milli as u64)
}

/// clap's parser for --cpus.
pub fn parse_cpus(text: &str) -> Result<f64> {
    let cpus: f64 = text
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("{text}: expected a number of CPUs like 0.5 or 2"))?;
    milli_cpus_from(cpus)?;
    Ok(cpus)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_as_properties_of_a_running_unit() {
        let set = Limits {
            memory: 64 * 1024 * 1024,
            milli_cpus: 500,
            pids: 100,
        };
        assert_eq!(
            set.unit_properties(),
            [
                ("MemoryMax", 67108864),
                ("MemorySwapMax", 67108864),
                ("CPUQuotaPerSecUSec", 500_000),
                ("TasksMax", 100),
            ]
        );
        for (_, value) in Limits::default().unit_properties() {
            assert_eq!(value, u64::MAX, "no limit is infinity");
        }
    }

    #[test]
    fn restart_policies_are_spelled_like_docker_everywhere() {
        for (policy, name, setting, boot) in [
            (Restart::No, "no", None, false),
            (Restart::OnFailure, "on-failure", Some("on-failure"), false),
            (Restart::Always, "always", Some("always"), true),
            (
                Restart::UnlessStopped,
                "unless-stopped",
                Some("always"),
                true,
            ),
        ] {
            assert_eq!(policy.name(), name);
            assert_eq!(Restart::parse(name).unwrap(), policy);
            assert_eq!(policy.unit_setting(), setting);
            assert_eq!(policy.enabled_at_boot(), boot);
            assert_eq!(
                serde_json::to_string(&policy).unwrap(),
                format!("\"{name}\"")
            );
            let value = <Restart as clap::ValueEnum>::from_str(name, false).unwrap();
            assert_eq!(value, policy);
        }
        assert!(Restart::parse("sometimes").is_err());
        assert_eq!(Restart::default(), Restart::No);
    }

    #[test]
    fn memory_sizes_read_like_docker() {
        assert_eq!(parse_memory("64m").unwrap(), 64 << 20);
        assert_eq!(parse_memory("64M").unwrap(), 64 << 20);
        assert_eq!(parse_memory("64mb").unwrap(), 64 << 20);
        assert_eq!(parse_memory("64MiB").unwrap(), 64 << 20);
        assert_eq!(parse_memory("1.5g").unwrap(), 3 << 29);
        assert_eq!(parse_memory("5120k").unwrap(), 5 << 20);
        assert_eq!(parse_memory("8388608").unwrap(), 8 << 20);
        assert_eq!(parse_memory("0").unwrap(), 0);
        for bad in ["abc", "-1", "1x", "1m", "m", "", "1.2.3g"] {
            assert!(parse_memory(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn cpus_are_kept_in_thousandths() {
        assert_eq!(milli_cpus_from(0.5).unwrap(), 500);
        assert_eq!(milli_cpus_from(1.5).unwrap(), 1500);
        assert_eq!(milli_cpus_from(0.333).unwrap(), 333);
        assert_eq!(milli_cpus_from(0.0).unwrap(), 0);
        for bad in [f64::NAN, f64::INFINITY, -1.0, 0.0004] {
            assert!(milli_cpus_from(bad).is_err(), "{bad}");
        }
        assert!(parse_cpus("x").is_err());
        assert_eq!(parse_cpus("2").unwrap(), 2.0);
    }

    #[test]
    fn limits_render_and_check() {
        let limits = |milli_cpus| Limits {
            memory: 0,
            milli_cpus,
            pids: 0,
        };
        assert_eq!(limits(500).cpu_quota().as_deref(), Some("50%"));
        assert_eq!(limits(1250).cpu_quota().as_deref(), Some("125%"));
        assert_eq!(limits(333).cpu_quota().as_deref(), Some("33.3%"));
        assert_eq!(limits(0).cpu_quota(), None);
        assert_eq!(limits(500).cpus(), 0.5);
        let small_memory = Limits {
            memory: 1 << 20,
            ..Limits::default()
        };
        assert!(small_memory.check(Mode::App).is_err());
        let few_pids = Limits {
            pids: 8,
            ..Limits::default()
        };
        assert!(few_pids.check(Mode::Boot).is_err());
        assert!(few_pids.check(Mode::App).is_ok());
        let old: Limits = serde_json::from_str("{}").unwrap();
        assert_eq!(old, Limits::default());
    }
}
