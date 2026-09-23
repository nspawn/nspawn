//! Dictionaries (a{sv}) for what the library returns, and option dictionaries for what
//! callers ask. Keys are the command line's spellings.

use std::collections::HashMap;

use zbus::zvariant::{OwnedValue, Value};

use crate::api::images::ImageSummary;
use crate::api::machines::MachineSummary;
use crate::api::network::{BridgeInfo, NetworkEntry};
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
        (
            "network".to_string(),
            v(format!("{:?}", r.network).to_lowercase()),
        ),
        (
            "address".to_string(),
            opt_string(r.address.map(|a| a.to_string()).as_deref()),
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
        ("image_labels".to_string(), map(&r.run.labels)),
        (
            "working_dir".to_string(),
            opt_string(r.run.working_dir.as_deref()),
        ),
        ("user".to_string(), opt_string(r.run.user.as_deref())),
        (
            "stop_signal".to_string(),
            opt_string(r.run.stop_signal.as_deref()),
        ),
    ])
}

pub fn machine(m: &MachineSummary) -> Dict {
    let mut dict = m.record.as_ref().map(record).unwrap_or_default();
    dict.insert("name".to_string(), v(m.name.as_str()));
    dict.insert("state".to_string(), v(m.state.as_str()));
    dict.insert("started".to_string(), v(m.started.unwrap_or(0)));
    dict.insert("leader".to_string(), v(u64::from(m.leader.unwrap_or(0))));
    dict.insert("os".to_string(), opt_string(m.os.as_deref()));
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

/// machined's object path for a machine, for whoever wants its own view.
pub fn machined_path(name: &str) -> String {
    let mut path = String::from("/org/freedesktop/machine1/machine/");
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() {
            path.push(byte as char);
        } else {
            path.push_str(&format!("_{byte:02x}"));
        }
    }
    path
}

pub fn bridge(b: &BridgeInfo) -> Dict {
    HashMap::from([
        ("bridge".to_string(), v(b.bridge.as_str())),
        ("subnet".to_string(), v(b.subnet.to_string())),
        ("gateway".to_string(), v(b.gateway.to_string())),
        ("host_name".to_string(), v(b.host_name.as_str())),
    ])
}

pub fn network_entry(e: &NetworkEntry) -> Dict {
    HashMap::from([
        ("name".to_string(), v(e.name.as_str())),
        (
            "address".to_string(),
            opt_string(e.address.map(|a| a.to_string()).as_deref()),
        ),
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
        Ok(self
            .take(key)
            .map(|value| match &**value {
                Value::U8(n) => Ok(*n as u64),
                Value::U16(n) => Ok(*n as u64),
                Value::U32(n) => Ok(*n as u64),
                Value::U64(n) => Ok(*n),
                _ => Err(anyhow::anyhow!(
                    "option {key} must be an unsigned integer (t, u, q or y)"
                )),
            })
            .transpose()?
            .unwrap_or(default))
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

        let mut typo = Options::new(&dict);
        typo.string("name").unwrap();
        let err = typo.finish().unwrap_err().to_string();
        assert!(err.contains("force") && err.contains("publish") && err.contains("timeout"));
    }
}
