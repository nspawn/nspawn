//! The image a build makes: its configuration as the OCI image config carries it, with
//! the history of the steps, and its manifest. Fields keep one order and maps are
//! sorted, so that the same steps give the same bytes, and a build taken whole from the
//! cache the same digest.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// What the history comment of a step names.
pub const HISTORY_COMMENT: &str = "nspawn.dockerfile.v0";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Healthcheck {
    #[serde(rename = "Test", default, skip_serializing_if = "Vec::is_empty")]
    pub test: Vec<String>,
    /// Nanoseconds, 0 for the default.
    #[serde(rename = "Interval", default, skip_serializing_if = "is_zero")]
    pub interval: i64,
    #[serde(rename = "Timeout", default, skip_serializing_if = "is_zero")]
    pub timeout: i64,
    #[serde(rename = "StartPeriod", default, skip_serializing_if = "is_zero")]
    pub start_period: i64,
    #[serde(rename = "StartInterval", default, skip_serializing_if = "is_zero")]
    pub start_interval: i64,
    #[serde(rename = "Retries", default, skip_serializing_if = "is_zero")]
    pub retries: i64,
}

// serde hands skip_serializing_if a reference.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero(n: &i64) -> bool {
    *n == 0
}

/// The settings a container of the image starts with.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(rename = "User", default, skip_serializing_if = "String::is_empty")]
    pub user: String,
    #[serde(
        rename = "ExposedPorts",
        default,
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub exposed_ports: BTreeMap<String, serde_json::Value>,
    #[serde(rename = "Env", default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
    #[serde(
        rename = "Entrypoint",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub entrypoint: Option<Vec<String>>,
    #[serde(rename = "Cmd", default, skip_serializing_if = "Option::is_none")]
    pub cmd: Option<Vec<String>>,
    #[serde(
        rename = "Volumes",
        default,
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub volumes: BTreeMap<String, serde_json::Value>,
    #[serde(
        rename = "WorkingDir",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub working_dir: String,
    #[serde(rename = "Labels", default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    #[serde(
        rename = "StopSignal",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub stop_signal: String,
    #[serde(
        rename = "ArgsEscaped",
        default,
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub args_escaped: bool,
    #[serde(
        rename = "Healthcheck",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub healthcheck: Option<Healthcheck>,
    #[serde(rename = "OnBuild", default, skip_serializing_if = "Vec::is_empty")]
    pub on_build: Vec<String>,
    #[serde(rename = "Shell", default, skip_serializing_if = "Vec::is_empty")]
    pub shell: Vec<String>,
}

impl Settings {
    /// Sets a variable, in place when it is there.
    pub fn set_env(&mut self, key: &str, value: &str) {
        let line = format!("{key}={value}");
        match self
            .env
            .iter_mut()
            .find(|v| v.split_once('=').map_or(v.as_str(), |(k, _)| k) == key)
        {
            Some(slot) => *slot = line,
            None => self.env.push(line),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct History {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub created_by: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub comment: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub empty_layer: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootFs {
    #[serde(rename = "type")]
    pub kind: String,
    pub diff_ids: Vec<String>,
}

/// An OCI image config, the fields a build reads and writes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    pub architecture: String,
    pub os: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    #[serde(default)]
    pub config: Settings,
    pub rootfs: RootFs,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<History>,
}

impl Config {
    /// An image with nothing in it, for this host's platform.
    pub fn scratch() -> Config {
        Config {
            architecture: crate::oci::host_architecture().to_string(),
            os: "linux".to_string(),
            rootfs: RootFs {
                kind: "layers".to_string(),
                diff_ids: Vec::new(),
            },
            ..Config::default()
        }
    }

    /// The bytes the config digest is taken of.
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("a config serializes")
    }

    /// A step that changed only the settings.
    pub fn note(&mut self, created_by: String, created: &str) {
        self.history.push(History {
            created: Some(created.to_string()),
            created_by,
            comment: HISTORY_COMMENT.to_string(),
            empty_layer: true,
        });
    }

    /// A step that made a layer.
    pub fn add_layer(&mut self, diff_id: String, created_by: String, created: &str) {
        self.rootfs.diff_ids.push(diff_id);
        self.history.push(History {
            created: Some(created.to_string()),
            created_by,
            comment: HISTORY_COMMENT.to_string(),
            empty_layer: false,
        });
    }

    /// The image's own time: its last step's.
    pub fn stamp(&mut self) {
        self.created = self.history.iter().rev().find_map(|h| h.created.clone());
    }
}

/// The OCI manifest of an image: its config and its layers, gzip blobs.
pub fn manifest(
    config: &[u8],
    layers: &[(String, String, u64)],
    labels: &BTreeMap<String, String>,
) -> Vec<u8> {
    let digest = |bytes: &[u8]| crate::layout::sha256_digest(bytes);
    let mut value = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": digest(config),
            "size": config.len(),
        },
        "layers": layers.iter().map(|(digest, media_type, size)| serde_json::json!({
            "mediaType": media_type,
            "digest": digest,
            "size": size,
        })).collect::<Vec<_>>(),
    });
    if !labels.is_empty() {
        value["annotations"] = serde_json::json!(labels);
    }
    serde_json::to_vec(&value).expect("a manifest serializes")
}

/// RFC 3339 with nanoseconds and Z, the form image configs use.
pub fn timestamp(unix_nanos: u128) -> String {
    let secs = (unix_nanos / 1_000_000_000) as i64;
    let nanos = (unix_nanos % 1_000_000_000) as u32;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{nanos:09}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 to a date (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configs_serialize_in_one_order() {
        let mut c = Config::scratch();
        c.config.set_env("PATH", "/bin");
        c.config.set_env("A", "1");
        c.config.set_env("PATH", "/usr/bin");
        assert_eq!(c.config.env, ["PATH=/usr/bin", "A=1"]);
        c.config.labels.insert("b".into(), "2".into());
        c.config.labels.insert("a".into(), "1".into());
        c.note("ENV A=1".into(), "2026-10-18T00:00:00.000000000Z");
        c.add_layer(
            "sha256:x".into(),
            "RUN true".into(),
            "2026-10-18T00:00:01.000000000Z",
        );
        c.stamp();
        let text = String::from_utf8(c.to_bytes()).unwrap();
        assert!(
            text.starts_with(r#"{"created":"2026-10-18T00:00:01.000000000Z","architecture":""#),
            "{text}"
        );
        assert!(text.contains(r#""Labels":{"a":"1","b":"2"}"#), "{text}");
        let back: Config = serde_json::from_str(&text).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn timestamps() {
        assert_eq!(timestamp(0), "1970-01-01T00:00:00.000000000Z");
        assert_eq!(
            timestamp(1_792_351_528_123_456_789),
            "2026-10-18T19:25:28.123456789Z"
        );
    }
}
