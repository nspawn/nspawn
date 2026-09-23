//! Registry credentials. `nspawn login` keeps them in /etc/nspawn/auth.json, the auth.json
//! format podman and skopeo use, and that file is the only one consulted: the service
//! answers many callers and has no home of theirs to look into. Everything is looked up
//! by registry host.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use anyhow::{bail, Context, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};

pub const STORE: &str = "/etc/nspawn/auth.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

#[derive(Default, Serialize, Deserialize)]
struct AuthFile {
    #[serde(default)]
    auths: BTreeMap<String, AuthEntry>,
}

#[derive(Default, Clone, Serialize, Deserialize)]
struct AuthEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    auth: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    password: Option<String>,
}

/// The key a registry is stored under: a bare host, with Docker Hub's many names folded
/// into docker.io (docker's own config uses "https://index.docker.io/v1/").
pub fn canonical(registry: &str) -> String {
    let host = registry
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let host = host.split('/').next().unwrap_or(host).to_ascii_lowercase();
    match host.as_str() {
        "docker.io" | "index.docker.io" | "registry-1.docker.io" | "registry.hub.docker.com" => {
            "docker.io".to_string()
        }
        _ => host,
    }
}

/// Where the registry's API lives for a stored name.
pub fn api_base(registry: &str) -> String {
    match canonical(registry).as_str() {
        "docker.io" => "https://registry-1.docker.io".to_string(),
        host => format!("https://{host}"),
    }
}

fn entry_credentials(entry: &AuthEntry) -> Option<Credentials> {
    if let Some(auth) = &entry.auth {
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(auth.trim())
            .ok()?;
        let text = String::from_utf8(decoded).ok()?;
        let (username, password) = text.split_once(':')?;
        return Some(Credentials {
            username: username.to_string(),
            password: password.to_string(),
        });
    }
    match (&entry.username, &entry.password) {
        (Some(u), Some(p)) => Some(Credentials {
            username: u.clone(),
            password: p.clone(),
        }),
        _ => None,
    }
}

/// Credentials for a registry from nspawn's store; none means anonymous.
pub fn lookup(registry: &str) -> Option<Credentials> {
    lookup_in(Path::new(STORE), registry)
}

pub fn lookup_in(path: &Path, registry: &str) -> Option<Credentials> {
    let key = canonical(registry);
    let text = fs::read_to_string(path).ok()?;
    let file = serde_json::from_str::<AuthFile>(&text).ok()?;
    file.auths
        .iter()
        .find(|(stored, _)| canonical(stored) == key)
        .and_then(|(_, entry)| entry_credentials(entry))
}

fn read_store(path: &Path) -> Result<AuthFile> {
    match fs::read_to_string(path) {
        Ok(text) => {
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AuthFile::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn write_store(path: &Path, file: &AuthFile) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let text = serde_json::to_string_pretty(file)?;
    // Created with 0600 from the first byte and renamed into place: at no point is the
    // file readable by others or half written.
    let tmp = path.with_file_name(format!(
        ".{}.{}",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        crate::store::unique_suffix()
    ));
    {
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        file.write_all(text.as_bytes())
            .with_context(|| format!("writing {}", tmp.display()))?;
    }
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    fs::rename(&tmp, path).with_context(|| format!("moving {} into place", path.display()))
}

/// Remembers credentials for a registry in nspawn's store (mode 0600).
pub fn store(registry: &str, credentials: &Credentials) -> Result<()> {
    store_in(Path::new(STORE), registry, credentials)
}

/// One writer at a time: the service takes several logins at once, and each one reads
/// the file, changes it and writes it back.
static WRITES: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn store_in(path: &Path, registry: &str, credentials: &Credentials) -> Result<()> {
    let _one_at_a_time = WRITES.lock().unwrap_or_else(|e| e.into_inner());
    let mut file = read_store(path)?;
    let encoded = base64::engine::general_purpose::STANDARD
        .encode(format!("{}:{}", credentials.username, credentials.password));
    file.auths.insert(
        canonical(registry),
        AuthEntry {
            auth: Some(encoded),
            ..AuthEntry::default()
        },
    );
    write_store(path, &file)
}

/// Forgets a registry's credentials; false when there were none in nspawn's store.
pub fn forget(registry: &str) -> Result<bool> {
    forget_in(Path::new(STORE), registry)
}

pub fn forget_in(path: &Path, registry: &str) -> Result<bool> {
    let _one_at_a_time = WRITES.lock().unwrap_or_else(|e| e.into_inner());
    let mut file = read_store(path)?;
    let key = canonical(registry);
    let before = file.auths.len();
    file.auths.retain(|stored, _| canonical(stored) != key);
    if file.auths.len() == before {
        return Ok(false);
    }
    write_store(path, &file)?;
    Ok(true)
}

/// Checks credentials against the registry the way docker login does: GET /v2/, and on a
/// challenge either basic authentication or a token request at the announced realm.
/// Ok(false) means the registry never asked for credentials.
pub async fn verify(
    registry: &str,
    credentials: &Credentials,
    ca_cert: Option<&Path>,
) -> Result<bool> {
    let mut builder =
        reqwest::Client::builder().user_agent(concat!("nspawn/", env!("CARGO_PKG_VERSION")));
    if let Some(path) = ca_cert {
        let pem = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        builder = builder.add_root_certificate(reqwest::Certificate::from_pem(&pem)?);
    }
    let client = builder.build()?;
    let base = api_base(registry);
    let probe = client
        .get(format!("{base}/v2/"))
        .send()
        .await
        .with_context(|| format!("reaching {base}"))?;
    if probe.status().is_success() {
        return Ok(false);
    }
    if probe.status() != reqwest::StatusCode::UNAUTHORIZED {
        bail!("{base}/v2/ answered {}", probe.status());
    }
    let challenge = probe
        .headers()
        .get(reqwest::header::WWW_AUTHENTICATE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let response = if let Some(params) = challenge.strip_prefix("Bearer ") {
        let realm = challenge_param(params, "realm")
            .context("the registry's Bearer challenge names no realm")?;
        let mut query = Vec::new();
        if let Some(service) = challenge_param(params, "service") {
            query.push(("service", service));
        }
        client
            .get(&realm)
            .query(&query)
            .basic_auth(&credentials.username, Some(&credentials.password))
            .send()
            .await
            .with_context(|| format!("reaching {realm}"))?
    } else {
        client
            .get(format!("{base}/v2/"))
            .basic_auth(&credentials.username, Some(&credentials.password))
            .send()
            .await
            .with_context(|| format!("reaching {base}"))?
    };
    match response.status() {
        s if s.is_success() => Ok(true),
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => {
            bail!(
                "{registry} rejected the credentials for {}",
                credentials.username
            )
        }
        s => bail!("{registry} answered {s} to the login"),
    }
}

/// Value of `key="..."` in a WWW-Authenticate challenge.
fn challenge_param(params: &str, key: &str) -> Option<String> {
    params.split(',').find_map(|part| {
        let (k, v) = part.trim().split_once('=')?;
        (k.trim() == key).then(|| v.trim().trim_matches('"').to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_names_fold() {
        assert_eq!(canonical("docker.io"), "docker.io");
        assert_eq!(canonical("https://index.docker.io/v1/"), "docker.io");
        assert_eq!(canonical("registry-1.docker.io"), "docker.io");
        assert_eq!(canonical("Hub.Nspawn.org"), "hub.nspawn.org");
        assert_eq!(
            canonical("https://hub.nspawn.test:8443/"),
            "hub.nspawn.test:8443"
        );
        assert_eq!(api_base("docker.io"), "https://registry-1.docker.io");
        assert_eq!(
            api_base("hub.nspawn.test:8443"),
            "https://hub.nspawn.test:8443"
        );
    }

    #[test]
    fn store_and_read_back() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("auth.json");
        let creds = Credentials {
            username: "eduard".into(),
            password: "s3cret:with:colons".into(),
        };
        store_in(&path, "https://index.docker.io/v1/", &creds).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let file: AuthFile = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(entry_credentials(&file.auths["docker.io"]).unwrap(), creds);
        // docker's own layout, with username/password fields, is understood too.
        let docker = AuthEntry {
            username: Some("u".into()),
            password: Some("p".into()),
            ..AuthEntry::default()
        };
        assert_eq!(
            entry_credentials(&docker).unwrap(),
            Credentials {
                username: "u".into(),
                password: "p".into()
            }
        );
        assert!(forget_in(&path, "docker.io").unwrap());
        assert!(!forget_in(&path, "docker.io").unwrap());
    }

    #[test]
    fn lookup_reads_the_store_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("auth.json");
        assert!(lookup_in(&path, "ghcr.io").is_none(), "no file yet");
        let creds = Credentials {
            username: "u".to_string(),
            password: "p".to_string(),
        };
        store_in(&path, "ghcr.io", &creds).unwrap();
        assert_eq!(lookup_in(&path, "https://ghcr.io/v2/").unwrap(), creds);
        assert!(lookup_in(&path, "docker.io").is_none());
    }

    #[test]
    fn challenge_parsing() {
        let c = "realm=\"https://auth.docker.io/token\",service=\"registry.docker.io\"";
        assert_eq!(
            challenge_param(c, "realm").as_deref(),
            Some("https://auth.docker.io/token")
        );
        assert_eq!(
            challenge_param(c, "service").as_deref(),
            Some("registry.docker.io")
        );
        assert_eq!(challenge_param(c, "scope"), None);
    }
}
