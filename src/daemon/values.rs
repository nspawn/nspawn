//! Dictionaries (a{sv}) for what the library returns, and option dictionaries for what
//! callers ask. Keys are the command line's spellings.

use std::collections::HashMap;

use zbus::zvariant::{OwnedValue, Value};

use crate::api::images::ImageSummary;
use crate::api::machines::MachineSummary;
use crate::api::network::{BridgeInfo, NetworkEntry, NetworkSummary};
use crate::api::stats::Sample;
use crate::api::volumes::VolumeInfo;
use crate::bridge::NetSpec;
use crate::daemon::jobs::Dict;
use crate::search::Hit;
use crate::store::ImageRecord;

/// A value without file descriptors, which is every value here.
pub fn v<'a, T: Into<Value<'a>>>(x: T) -> OwnedValue {
    OwnedValue::try_from(x.into()).expect("plain values carry no file descriptor")
}

pub fn strings(items: &[String]) -> OwnedValue {
    v(items.to_vec())
}

/// A string map as a{ss}.
pub fn map(items: &std::collections::BTreeMap<String, String>) -> OwnedValue {
    v(items
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect::<HashMap<String, String>>())
}

fn opt_string(item: Option<&str>) -> OwnedValue {
    v(item.unwrap_or(""))
}

pub fn image(i: &ImageSummary) -> Dict {
    HashMap::from([
        ("name".to_string(), v(i.name.as_str())),
        ("kind".to_string(), v(i.kind.as_str())),
        (
            "backend".to_string(),
            opt_string(
                i.backend
                    .map(|b| format!("{b:?}").to_lowercase())
                    .as_deref(),
            ),
        ),
        ("origin".to_string(), opt_string(i.origin.as_deref())),
        ("reference".to_string(), opt_string(i.reference.as_deref())),
        ("size".to_string(), v(i.size.unwrap_or(0))),
        ("read_only".to_string(), v(i.read_only)),
    ])
}

/// Everything nspawn keeps about an image.
pub fn record(r: &ImageRecord) -> Dict {
    HashMap::from([
        ("name".to_string(), v(r.name.as_str())),
        ("reference".to_string(), v(r.reference.as_str())),
        ("digest".to_string(), v(r.manifest_digest.as_str())),
        (
            "backend".to_string(),
            v(format!("{:?}", r.backend).to_lowercase()),
        ),
        ("origin".to_string(), v(r.origin.as_str())),
        ("mode".to_string(), v(r.mode.name())),
        ("created".to_string(), v(r.created)),
        // As --network spells it: bridge, veth, host, none or the primary network's name.
        (
            "network".to_string(),
            v(if r.no_network {
                "none".to_string()
            } else {
                r.network_name
                    .clone()
                    .unwrap_or_else(|| format!("{:?}", r.network).to_lowercase())
            }),
        ),
        (
            "address".to_string(),
            opt_string(r.address.map(|a| a.to_string()).as_deref()),
        ),
        (
            "networks".to_string(),
            strings(&if crate::bridge::bridge_kind(r) {
                crate::bridge::networks_of(r)
                    .into_iter()
                    .map(str::to_string)
                    .collect()
            } else {
                Vec::new()
            }),
        ),
        (
            "addresses".to_string(),
            map(&crate::bridge::networks_of(r)
                .into_iter()
                .filter(|_| crate::bridge::bridge_kind(r))
                .filter_map(|n| {
                    crate::bridge::address_on(r, n).map(|a| (n.to_string(), a.to_string()))
                })
                .collect()),
        ),
        (
            "aliases".to_string(),
            strings(
                &r.aliases
                    .iter()
                    .flat_map(|(network, names)| {
                        names.iter().map(move |a| format!("{network}={a}"))
                    })
                    .collect::<Vec<_>>(),
            ),
        ),
        (
            "ports".to_string(),
            strings(&r.ports.iter().map(|p| p.to_string()).collect::<Vec<_>>()),
        ),
        (
            "volumes".to_string(),
            strings(&r.volumes.iter().map(|p| p.to_string()).collect::<Vec<_>>()),
        ),
        ("env".to_string(), strings(&r.env)),
        (
            "entrypoint".to_string(),
            strings(r.entrypoint.as_deref().unwrap_or(r.run.entrypoint())),
        ),
        (
            "cmd".to_string(),
            strings(r.cmd.as_deref().unwrap_or(r.run.cmd())),
        ),
        ("command".to_string(), strings(&r.effective_command())),
        ("image_env".to_string(), strings(&r.run.env)),
        ("labels".to_string(), map(&r.effective_labels())),
        ("restart".to_string(), v(r.restart.name())),
        ("memory".to_string(), v(r.limits.memory)),
        ("cpus".to_string(), v(r.limits.cpus())),
        ("pids_limit".to_string(), v(r.limits.pids)),
        ("image_labels".to_string(), map(&r.run.labels)),
        (
            "working_dir".to_string(),
            opt_string(r.effective_working_dir()),
        ),
        ("user".to_string(), opt_string(r.effective_user())),
        (
            "stop_signal".to_string(),
            opt_string(r.effective_stop_signal()),
        ),
        (
            "hostname".to_string(),
            opt_string(r.tuning.hostname.as_deref()),
        ),
        ("cap_add".to_string(), strings(&r.tuning.cap_add)),
        ("cap_drop".to_string(), strings(&r.tuning.cap_drop)),
        ("privileged".to_string(), v(r.tuning.privileged)),
        ("read_only".to_string(), v(r.tuning.read_only)),
        ("tmpfs".to_string(), strings(&r.tuning.tmpfs)),
        ("shm_size".to_string(), v(r.tuning.shm_size.unwrap_or(0))),
        (
            "devices".to_string(),
            strings(
                &r.tuning
                    .devices
                    .iter()
                    .map(|d| format!("{}:{}:{}", d.host, d.container, d.permissions))
                    .collect::<Vec<_>>(),
            ),
        ),
        (
            "dns".to_string(),
            strings(
                &r.tuning
                    .dns
                    .iter()
                    .map(|a| a.to_string())
                    .collect::<Vec<_>>(),
            ),
        ),
        ("dns_search".to_string(), strings(&r.tuning.dns_search)),
        (
            "extra_hosts".to_string(),
            strings(
                &r.tuning
                    .extra_hosts
                    .iter()
                    .map(|h| format!("{}:{}", h.host, h.ip))
                    .collect::<Vec<_>>(),
            ),
        ),
        (
            "ulimits".to_string(),
            map(&r
                .tuning
                .ulimits
                .iter()
                .map(|(name, (soft, hard))| (name.clone(), format!("{soft}:{hard}")))
                .collect()),
        ),
        (
            "oom_score_adj".to_string(),
            v(i64::from(r.tuning.oom_score_adj.unwrap_or(0))),
        ),
        (
            "stop_timeout".to_string(),
            v(r.tuning.stop_timeout.unwrap_or(10)),
        ),
        ("init".to_string(), v(r.tuning.init)),
        ("sysctls".to_string(), map(&r.tuning.sysctls)),
        (
            "secrets".to_string(),
            strings(
                &r.tuning
                    .secrets
                    .iter()
                    .map(|s| format!("{}:{}:{:04o}:{}:{}", s.name, s.target, s.mode, s.uid, s.gid))
                    .collect::<Vec<_>>(),
            ),
        ),
        ("image_volumes".to_string(), strings(&r.run.volumes)),
        (
            "healthcheck".to_string(),
            v(r.effective_healthcheck()
                .map(healthcheck)
                .unwrap_or_default()),
        ),
    ])
}

/// A healthcheck: test (as), interval, timeout, start_period, start_interval (t,
/// microseconds, 0 for the default), retries (u).
fn healthcheck(h: &crate::health::Healthcheck) -> Dict {
    HashMap::from([
        ("test".to_string(), strings(&h.test)),
        ("interval".to_string(), v(h.interval)),
        ("timeout".to_string(), v(h.timeout)),
        ("start_period".to_string(), v(h.start_period)),
        ("start_interval".to_string(), v(h.start_interval)),
        ("retries".to_string(), v(h.retries)),
    ])
}

pub fn machine(m: &MachineSummary) -> Dict {
    let mut dict = m.record.as_ref().map(record).unwrap_or_default();
    dict.insert("name".to_string(), v(m.name.as_str()));
    dict.insert("state".to_string(), v(m.state.as_str()));
    dict.insert("started".to_string(), v(m.started.unwrap_or(0)));
    dict.insert("leader".to_string(), v(u64::from(m.leader.unwrap_or(0))));
    dict.insert("os".to_string(), opt_string(m.os.as_deref()));
    if let Some(health) = &m.health {
        dict.insert("health".to_string(), v(health.status.as_str()));
        dict.insert(
            "health_failing_streak".to_string(),
            v(health.failing_streak),
        );
        dict.insert(
            "health_log".to_string(),
            strings(
                &health
                    .log
                    .iter()
                    .map(|p| format!("{} {} {}", p.end, p.exit_code, p.output))
                    .collect::<Vec<_>>(),
            ),
        );
    }
    dict.insert(
        "machine_path".to_string(),
        v(if m.leader.is_some() {
            machined_path(&m.name)
        } else {
            String::new()
        }),
    );
    dict
}

/// machined's object path for a machine, for whoever wants its own view: systemd's bus
/// label escaping, which also escapes a leading digit.
pub fn machined_path(name: &str) -> String {
    let mut path = String::from("/org/freedesktop/machine1/machine/");
    for (i, byte) in name.bytes().enumerate() {
        if byte.is_ascii_alphabetic() || (i > 0 && byte.is_ascii_digit()) {
            path.push(byte as char);
        } else {
            path.push_str(&format!("_{byte:02x}"));
        }
    }
    path
}

/// A sample of `MachineStats`: what could not be read is left out.
pub fn sample(s: &Sample) -> Dict {
    let mut dict = HashMap::from([
        ("name".to_string(), v(s.name.as_str())),
        ("time_usec".to_string(), v(s.time_usec)),
    ]);
    for (key, value) in [
        ("cpu_usec", s.cpu_usec),
        ("memory", s.memory),
        ("memory_limit", s.memory_limit),
        ("pids", s.pids),
        ("io_read", s.io_read),
        ("io_write", s.io_write),
        ("net_rx", s.net_rx),
        ("net_tx", s.net_tx),
    ] {
        if let Some(value) = value {
            dict.insert(key.to_string(), v(value));
        }
    }
    dict
}

pub fn volume(vol: &VolumeInfo) -> Dict {
    HashMap::from([
        ("name".to_string(), v(vol.name.as_str())),
        (
            "path".to_string(),
            v(vol.path.to_string_lossy().into_owned()),
        ),
        ("used_by".to_string(), strings(&vol.used_by)),
        ("created".to_string(), v(vol.created)),
    ])
}

/// A secret: name, created, size, labels, used_by; never its content.
pub fn secret(s: &crate::api::secrets::SecretInfo) -> Dict {
    HashMap::from([
        ("name".to_string(), v(s.name.as_str())),
        ("created".to_string(), v(s.created)),
        ("size".to_string(), v(s.size)),
        ("labels".to_string(), map(&s.labels)),
        ("used_by".to_string(), strings(&s.used_by)),
    ])
}

pub fn bridge(b: &BridgeInfo) -> Dict {
    HashMap::from([
        ("bridge".to_string(), v(b.bridge.as_str())),
        ("subnet".to_string(), v(b.subnet.to_string())),
        ("gateway".to_string(), v(b.gateway.to_string())),
        ("host_name".to_string(), v(b.host_name.as_str())),
    ])
}

/// A network: name, interface, subnet, gateway, internal, created (unix seconds, 0 for
/// the default one), labels.
pub fn network(n: &NetSpec) -> Dict {
    HashMap::from([
        ("name".to_string(), v(n.name.as_str())),
        ("interface".to_string(), v(n.interface.as_str())),
        ("subnet".to_string(), v(n.subnet.to_string())),
        ("gateway".to_string(), v(n.subnet.gateway().to_string())),
        ("internal".to_string(), v(n.internal)),
        ("created".to_string(), v(n.created)),
        ("labels".to_string(), map(&n.labels)),
    ])
}

/// A network with the machines it has, as `network ls` lists it.
pub fn network_summary(s: &NetworkSummary) -> Dict {
    let mut dict = network(&s.spec);
    dict.insert("machines".to_string(), strings(&s.machines));
    dict
}

pub fn network_entry(e: &NetworkEntry) -> Dict {
    HashMap::from([
        ("name".to_string(), v(e.name.as_str())),
        (
            "address".to_string(),
            opt_string(e.address.map(|a| a.to_string()).as_deref()),
        ),
        ("aliases".to_string(), strings(&e.aliases)),
        (
            "ports".to_string(),
            strings(&e.ports.iter().map(|p| p.to_string()).collect::<Vec<_>>()),
        ),
        ("running".to_string(), v(e.running)),
    ])
}

pub fn hit(h: &Hit) -> Dict {
    HashMap::from([
        ("source".to_string(), v(h.source.as_str())),
        ("name".to_string(), v(h.name.as_str())),
        ("description".to_string(), v(h.description.as_str())),
        ("stars".to_string(), v(h.stars.unwrap_or(0))),
        ("official".to_string(), v(h.official)),
    ])
}

/// Options a caller passed, read by name and type; a key nobody expects is an error, so
/// that a typo does not pass as "default".
pub struct Options<'a> {
    dict: &'a HashMap<String, OwnedValue>,
    known: Vec<&'static str>,
}

impl<'a> Options<'a> {
    pub fn new(dict: &'a HashMap<String, OwnedValue>) -> Self {
        Options {
            dict,
            known: Vec::new(),
        }
    }

    fn take(&mut self, key: &'static str) -> Option<&'a OwnedValue> {
        self.known.push(key);
        self.dict.get(key)
    }

    pub fn string(&mut self, key: &'static str) -> anyhow::Result<Option<String>> {
        self.take(key)
            .map(|value| {
                String::try_from(value.clone())
                    .map_err(|_| anyhow::anyhow!("option {key} must be a string (s)"))
            })
            .transpose()
    }

    pub fn strings(&mut self, key: &'static str) -> anyhow::Result<Vec<String>> {
        Ok(self
            .take(key)
            .map(|value| {
                Vec::<String>::try_from(value.clone())
                    .map_err(|_| anyhow::anyhow!("option {key} must be an array of strings (as)"))
            })
            .transpose()?
            .unwrap_or_default())
    }

    /// A boolean when one was given.
    pub fn maybe_bool(&mut self, key: &'static str) -> anyhow::Result<Option<bool>> {
        self.take(key)
            .map(|value| {
                bool::try_from(value.clone())
                    .map_err(|_| anyhow::anyhow!("option {key} must be a boolean (b)"))
            })
            .transpose()
    }

    /// A signed integer of any kind when one was given.
    pub fn i64(&mut self, key: &'static str) -> anyhow::Result<Option<i64>> {
        self.take(key)
            .map(|value| match &**value {
                Value::I16(n) => Ok(i64::from(*n)),
                Value::I32(n) => Ok(i64::from(*n)),
                Value::I64(n) => Ok(*n),
                Value::U8(n) => Ok(i64::from(*n)),
                Value::U16(n) => Ok(i64::from(*n)),
                Value::U32(n) => Ok(i64::from(*n)),
                _ => Err(anyhow::anyhow!("option {key} must be an integer (i)")),
            })
            .transpose()
    }

    pub fn bool(&mut self, key: &'static str, default: bool) -> anyhow::Result<bool> {
        Ok(self
            .take(key)
            .map(|value| {
                bool::try_from(value.clone())
                    .map_err(|_| anyhow::anyhow!("option {key} must be a boolean (b)"))
            })
            .transpose()?
            .unwrap_or(default))
    }

    /// Any unsigned integer (y, q, u or t) will do.
    pub fn u64(&mut self, key: &'static str, default: u64) -> anyhow::Result<u64> {
        Ok(self.maybe_u64(key)?.unwrap_or(default))
    }

    /// An unsigned integer when one was given: absent and zero are not the same here.
    pub fn maybe_u64(&mut self, key: &'static str) -> anyhow::Result<Option<u64>> {
        self.take(key)
            .map(|value| match &**value {
                Value::U8(n) => Ok(*n as u64),
                Value::U16(n) => Ok(*n as u64),
                Value::U32(n) => Ok(*n as u64),
                Value::U64(n) => Ok(*n),
                _ => Err(anyhow::anyhow!(
                    "option {key} must be an unsigned integer (t, u, q or y)"
                )),
            })
            .transpose()
    }

    /// A number (d), or an integer of any kind, when one was given.
    pub fn f64(&mut self, key: &'static str) -> anyhow::Result<Option<f64>> {
        self.take(key)
            .map(|value| match &**value {
                Value::F64(n) => Ok(*n),
                Value::U8(n) => Ok(*n as f64),
                Value::U16(n) => Ok(*n as f64),
                Value::U32(n) => Ok(*n as f64),
                Value::U64(n) => Ok(*n as f64),
                Value::I16(n) => Ok(*n as f64),
                Value::I32(n) => Ok(*n as f64),
                Value::I64(n) => Ok(*n as f64),
                _ => Err(anyhow::anyhow!("option {key} must be a number (d)")),
            })
            .transpose()
    }

    /// Fails on keys nothing asked for.
    pub fn finish(self) -> anyhow::Result<()> {
        let mut unknown: Vec<&str> = self
            .dict
            .keys()
            .filter(|k| !self.known.contains(&k.as_str()))
            .map(|k| k.as_str())
            .collect();
        if unknown.is_empty() {
            return Ok(());
        }
        unknown.sort();
        anyhow::bail!(
            "unknown option(s) {}; known: {}",
            unknown.join(", "),
            self.known.join(", ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machined_paths_escape_like_systemd() {
        assert_eq!(
            machined_path("web1"),
            "/org/freedesktop/machine1/machine/web1"
        );
        assert_eq!(
            machined_path("e2e-a"),
            "/org/freedesktop/machine1/machine/e2e_2da"
        );
        assert_eq!(
            machined_path("1web"),
            "/org/freedesktop/machine1/machine/_31web",
            "a leading digit is escaped"
        );
    }

    #[test]
    fn options_are_typed_and_unknown_keys_refused() {
        let dict: HashMap<String, OwnedValue> = HashMap::from([
            ("name".to_string(), v("web")),
            ("force".to_string(), v(true)),
            ("publish".to_string(), strings(&["80:80".to_string()])),
            ("timeout".to_string(), v(5u64)),
        ]);
        let mut options = Options::new(&dict);
        assert_eq!(options.string("name").unwrap().as_deref(), Some("web"));
        assert!(options.bool("force", false).unwrap());
        assert_eq!(options.strings("publish").unwrap(), ["80:80"]);
        assert_eq!(options.u64("timeout", 10).unwrap(), 5);
        assert_eq!(options.u64("missing", 10).unwrap(), 10);
        let narrow: HashMap<String, OwnedValue> = HashMap::from([
            ("rows".to_string(), v(40u32)),
            ("cols".to_string(), OwnedValue::from(120u16)),
            ("lines".to_string(), v("many")),
        ]);
        let mut numbers = Options::new(&narrow);
        assert_eq!(numbers.u64("rows", 24).unwrap(), 40, "u will do for t");
        assert_eq!(numbers.u64("cols", 80).unwrap(), 120, "so will q");
        assert!(numbers
            .u64("lines", 0)
            .unwrap_err()
            .to_string()
            .contains("unsigned integer"));
        assert!(options.string("force").is_err());
        options.finish().unwrap();

        let limits: HashMap<String, OwnedValue> = HashMap::from([
            ("memory".to_string(), v(0u64)),
            ("cpus".to_string(), v(0.5f64)),
            ("pids_limit".to_string(), v(100u32)),
        ]);
        let mut given = Options::new(&limits);
        assert_eq!(given.maybe_u64("memory").unwrap(), Some(0), "zero is given");
        assert_eq!(given.maybe_u64("absent").unwrap(), None);
        assert_eq!(given.f64("cpus").unwrap(), Some(0.5));
        assert_eq!(given.f64("pids_limit").unwrap(), Some(100.0));
        assert_eq!(given.f64("nothing").unwrap(), None);
        given.finish().unwrap();
        let mut wrong = Options::new(&dict);
        assert!(wrong.f64("name").is_err());

        let mut typo = Options::new(&dict);
        typo.string("name").unwrap();
        let err = typo.finish().unwrap_err().to_string();
        assert!(err.contains("force") && err.contains("publish") && err.contains("timeout"));
    }
}
