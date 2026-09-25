//! docker secret, on systemd-creds: a secret is kept encrypted under the state directory
//! (bound to the host's TPM2 where there is one, to its credential key otherwise) and
//! decrypted for a machine into a tmpfs of root's alone while it runs, where the file is
//! bind-mounted read-only at its target.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::api::images::Removal;
use crate::api::{line, require_root, Context, Report};
use crate::settings::Bind;
use crate::store::{now_unix, write_atomically, ImageRecord, Store};
use crate::tuning::SecretRef;

/// Where a machine's secrets are decrypted while it runs.
pub const RUN_DIR: &str = "/run/nspawn/secrets";
/// What a secret may weigh, as docker limits it.
pub const MAX_SIZE: usize = 500 * 1024;

/// What is kept next to the encrypted blob.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretMeta {
    pub created: u64,
    /// Bytes of the plaintext.
    pub size: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

/// One secret as `secret ls` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretInfo {
    pub name: String,
    pub created: u64,
    pub size: u64,
    pub labels: BTreeMap<String, String>,
    /// The machines whose records name it, sorted.
    pub used_by: Vec<String>,
}

/// Letters, digits, '_', '.' and '-', not starting with a dot: a file name.
pub fn validate_secret_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || name.starts_with('.')
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
    {
        bail!("{name:?} is not a secret name: letters, digits, _ . and -, not starting with a dot");
    }
    Ok(())
}

fn blob_path(store: &Store, name: &str) -> PathBuf {
    store.secrets_dir().join(format!("{name}.cred"))
}

fn meta_path(store: &Store, name: &str) -> PathBuf {
    store.secrets_dir().join(format!("{name}.json"))
}

/// Which machines take each secret.
fn users(records: &[ImageRecord]) -> BTreeMap<String, BTreeSet<String>> {
    let mut users: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for record in records {
        for secret in &record.tuning.secrets {
            users
                .entry(secret.name.clone())
                .or_default()
                .insert(record.name.clone());
        }
    }
    users
}

fn read_meta(store: &Store, name: &str) -> Result<Option<SecretMeta>> {
    let path = meta_path(store, name);
    match fs::read(&path) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing {}", path.display()))?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

pub fn exists(store: &Store, name: &str) -> bool {
    validate_secret_name(name).is_ok() && blob_path(store, name).is_file()
}

pub fn list(store: &Store) -> Result<Vec<SecretInfo>> {
    let dir = store.secrets_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    let users = users(&store.list_images()?);
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
        let file = entry.file_name().to_string_lossy().into_owned();
        let Some(name) = file.strip_suffix(".cred") else {
            continue;
        };
        if validate_secret_name(name).is_err() {
            continue;
        }
        let meta = read_meta(store, name)?.unwrap_or_default();
        out.push(SecretInfo {
            used_by: users
                .get(name)
                .map(|u| u.iter().cloned().collect())
                .unwrap_or_default(),
            name: name.to_string(),
            created: meta.created,
            size: meta.size,
            labels: meta.labels,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

pub fn get(store: &Store, name: &str) -> Result<SecretInfo> {
    validate_secret_name(name)?;
    list(store)?
        .into_iter()
        .find(|s| s.name == name)
        .with_context(|| format!("no secret named {name}"))
}

/// docker secret create: the content, encrypted for this host, under the state directory.
/// A name in use is refused, as docker does.
pub async fn create(
    ctx: &Context,
    name: &str,
    content: &[u8],
    labels: BTreeMap<String, String>,
) -> Result<()> {
    require_root("secret create")?;
    validate_secret_name(name)?;
    if content.len() > MAX_SIZE {
        bail!(
            "secret {name}: {} bytes; a secret takes {MAX_SIZE} at most",
            content.len()
        );
    }
    let store = &ctx.store;
    let _lock = store.lock().await?;
    if blob_path(store, name).exists() {
        bail!("secret {name} exists already; remove it first");
    }
    let dir = store.secrets_dir();
    crate::store::create_private_dir(&dir.to_string_lossy())?;
    let blob = systemd_creds(&["encrypt", &format!("--name={name}"), "-", "-"], content)
        .with_context(|| format!("encrypting secret {name}"))?;
    write_atomically(&blob_path(store, name), &blob)?;
    let meta = SecretMeta {
        created: now_unix(),
        size: content.len() as u64,
        labels,
    };
    write_atomically(&meta_path(store, name), &serde_json::to_vec(&meta)?)?;
    crate::api::events::emit("secret", "create", name, &[]);
    Ok(())
}

/// docker secret rm: one a machine names, or unknown, is refused without stopping the
/// others.
pub async fn remove(ctx: &Context, names: &[String], report: Report<'_>) -> Result<Removal> {
    require_root("secret rm")?;
    let store = &ctx.store;
    let _lock = store.lock().await?;
    let users = users(&store.list_images_strict()?);
    let mut removal = Removal::default();
    for name in names {
        match remove_one(store, name, &users) {
            Ok(()) => {
                line(report, format!("removed {name}"));
                crate::api::events::emit("secret", "remove", name, &[]);
                removal.removed.push(name.clone());
            }
            Err(e) => removal.failed.push((name.clone(), format!("{e:#}"))),
        }
    }
    Ok(removal)
}

fn remove_one(store: &Store, name: &str, users: &BTreeMap<String, BTreeSet<String>>) -> Result<()> {
    if validate_secret_name(name).is_err() || !blob_path(store, name).is_file() {
        bail!("no secret named {name}");
    }
    if let Some(machines) = users.get(name) {
        let machines: Vec<&str> = machines.iter().map(String::as_str).collect();
        bail!(
            "secret {name} is in use by {}; start them with --secret none (or other secrets) first",
            machines.join(", ")
        );
    }
    for path in [blob_path(store, name), meta_path(store, name)] {
        if let Err(e) = fs::remove_file(&path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                return Err(e).with_context(|| format!("removing {}", path.display()));
            }
        }
    }
    Ok(())
}

/// The machine's secrets, decrypted under `RUN_DIR/MACHINE` with their mode and owner,
/// as the read-only binds of its settings. Repeatable; what was there goes first.
pub fn materialize(store: &Store, record: &ImageRecord) -> Result<Vec<Bind>> {
    clear(&record.name);
    if record.tuning.secrets.is_empty() {
        return Ok(Vec::new());
    }
    crate::store::create_private_dir(RUN_DIR)?;
    let dir = Path::new(RUN_DIR).join(&record.name);
    crate::store::create_private_dir(&dir.to_string_lossy())?;
    let mut binds = Vec::new();
    for (i, secret) in record.tuning.secrets.iter().enumerate() {
        if !exists(store, &secret.name) {
            bail!(
                "no secret named {}; nspawn secret create {} makes it",
                secret.name,
                secret.name
            );
        }
        let plain = systemd_creds(
            &[
                "decrypt",
                &format!("--name={}", secret.name),
                &blob_path(store, &secret.name).to_string_lossy(),
                "-",
            ],
            &[],
        )
        .with_context(|| format!("decrypting secret {}", secret.name))?;
        let path = dir.join(format!("{i}-{}", secret.name));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("writing {}", path.display()))?;
        file.write_all(&plain)
            .with_context(|| format!("writing {}", path.display()))?;
        drop(file);
        fs::set_permissions(&path, fs::Permissions::from_mode(secret.mode))
            .with_context(|| format!("setting the mode of {}", path.display()))?;
        nix::unistd::chown(
            &path,
            Some(nix::unistd::Uid::from_raw(secret.uid)),
            Some(nix::unistd::Gid::from_raw(secret.gid)),
        )
        .with_context(|| format!("setting the owner of {}", path.display()))?;
        binds.push(Bind {
            source: path,
            target: secret.target.clone(),
            read_only: true,
        });
    }
    Ok(binds)
}

/// Removes what `materialize` left for a machine.
pub fn clear(machine: &str) {
    let _ = fs::remove_dir_all(Path::new(RUN_DIR).join(machine));
}

/// Runs systemd-creds with `input` on its stdin; its stdout. Its warning about the
/// credential key not being on encrypted media is nothing to act on.
fn systemd_creds(args: &[&str], input: &[u8]) -> Result<Vec<u8>> {
    let mut child = Command::new("systemd-creds")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running systemd-creds (is systemd installed?)")?;
    child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(input)
        .context("feeding systemd-creds")?;
    let output = child
        .wait_with_output()
        .context("waiting for systemd-creds")?;
    if !output.status.success() {
        bail!(
            "systemd-creds {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

/// `--secret NAME[:TARGET[:MODE[:UID:GID]]]`: docker's fields, /run/secrets/NAME, 0444
/// and root by default.
pub fn parse_ref(text: &str) -> Result<SecretRef> {
    let fields: Vec<&str> = text.split(':').collect();
    let (name, target, mode, owner) = match fields[..] {
        [name] => (name, None, None, None),
        [name, target] => (name, Some(target), None, None),
        [name, target, mode] => (name, Some(target), Some(mode), None),
        [name, target, mode, uid, gid] => (name, Some(target), Some(mode), Some((uid, gid))),
        _ => bail!("--secret {text}: NAME[:TARGET[:MODE[:UID:GID]]]"),
    };
    validate_secret_name(name)?;
    let target = match target.filter(|t| !t.is_empty()) {
        Some(target) => {
            if !target.starts_with('/')
                || target.ends_with('/')
                || target.chars().any(char::is_whitespace)
            {
                bail!(
                    "--secret {text}: the target is an absolute path of a file inside the machine"
                );
            }
            target.to_string()
        }
        None => format!("/run/secrets/{name}"),
    };
    let mode = match mode.filter(|m| !m.is_empty()) {
        Some(mode) => u32::from_str_radix(mode, 8)
            .ok()
            .filter(|m| *m <= 0o777)
            .with_context(|| format!("--secret {text}: the mode is octal, like 0400"))?,
        None => 0o444,
    };
    let (uid, gid) = match owner {
        Some((uid, gid)) => (
            uid.parse()
                .with_context(|| format!("--secret {text}: {uid} is not a uid"))?,
            gid.parse()
                .with_context(|| format!("--secret {text}: {gid} is not a gid"))?,
        ),
        None => (0, 0),
    };
    Ok(SecretRef {
        name: name.to_string(),
        target,
        mode,
        uid,
        gid,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_references_read_like_docker() {
        let plain = parse_ref("db-password").unwrap();
        assert_eq!(
            (plain.target.as_str(), plain.mode, plain.uid, plain.gid),
            ("/run/secrets/db-password", 0o444, 0, 0)
        );
        let full = parse_ref("tls:/etc/tls/key.pem:0400:1000:1000").unwrap();
        assert_eq!(
            (full.target.as_str(), full.mode, full.uid),
            ("/etc/tls/key.pem", 0o400, 1000)
        );
        assert_eq!(parse_ref("a:/x:640").unwrap().mode, 0o640);
        for bad in [
            "",
            ".hidden",
            "a:x",
            "a:/x:999",
            "a:/x:0400:root:root",
            "a:/x:0400:1",
            "a b",
        ] {
            assert!(parse_ref(bad).is_err(), "{bad:?}");
        }
        assert!(validate_secret_name("db_pw.1-x").is_ok());
        assert!(validate_secret_name(&"x".repeat(65)).is_err());
    }

    #[test]
    fn listing_and_removal_follow_the_records() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(&tmp.path().join("machines"), &tmp.path().join("state"));
        fs::create_dir_all(store.secrets_dir()).unwrap();
        fs::create_dir_all(store.images_dir()).unwrap();
        fs::write(blob_path(&store, "pw"), b"blob").unwrap();
        fs::write(
            meta_path(&store, "pw"),
            serde_json::to_vec(&SecretMeta {
                created: 7,
                size: 3,
                labels: BTreeMap::from([("env".to_string(), "prod".to_string())]),
            })
            .unwrap(),
        )
        .unwrap();
        fs::write(blob_path(&store, "loose"), b"blob").unwrap();
        fs::write(store.secrets_dir().join("notes.txt"), b"x").unwrap();
        let mut record: ImageRecord = serde_json::from_str(
            r#"{"name": "web", "reference": "r", "manifest_digest": "d", "layers": [], "backend": "overlay", "created": 0}"#,
        )
        .unwrap();
        record.tuning.secrets.push(parse_ref("pw").unwrap());
        store.record_image(&record).unwrap();
        let listed = list(&store).unwrap();
        assert_eq!(listed.len(), 2, "{listed:?}");
        assert_eq!(listed[0].name, "loose");
        assert_eq!((listed[1].size, listed[1].created), (3, 7));
        assert_eq!(listed[1].used_by, ["web"]);
        assert_eq!(listed[1].labels["env"], "prod");
        assert!(exists(&store, "pw") && !exists(&store, "nope") && !exists(&store, "../pw"));
        let users = users(&[record]);
        assert!(remove_one(&store, "pw", &users).is_err(), "in use");
        assert!(remove_one(&store, "nope", &users).is_err());
        remove_one(&store, "loose", &users).unwrap();
        assert_eq!(list(&store).unwrap().len(), 1);
    }
}
