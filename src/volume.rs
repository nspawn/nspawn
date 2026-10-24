//! Volumes: `-v /host/dir:/inside[:ro]` binds a host path, and
//! `-v name:/inside` a directory nspawn keeps under its own volumes directory. Both end up
//! as bind mounts in the machine's settings.

use std::fmt;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Fills a named volume made on first use with what the image has at its mount point,
/// owner and mode included: a program that runs as a user finds its
/// data directory its own, and a volume over a directory with files starts with them.
/// Devices, sockets and fifos are left out.
pub fn seed(from: &Path, to: &Path) -> Result<()> {
    let meta = from
        .symlink_metadata()
        .with_context(|| format!("looking at {}", from.display()))?;
    if !meta.is_dir() {
        return Ok(());
    }
    copy_tree(from, to)?;
    take_over(&meta, to)
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    for entry in fs::read_dir(from).with_context(|| format!("reading {}", from.display()))? {
        let entry = entry.with_context(|| format!("reading {}", from.display()))?;
        let source = entry.path();
        let target = to.join(entry.file_name());
        let meta = source
            .symlink_metadata()
            .with_context(|| format!("looking at {}", source.display()))?;
        let kind = meta.file_type();
        if kind.is_dir() {
            fs::create_dir(&target).with_context(|| format!("creating {}", target.display()))?;
            copy_tree(&source, &target)?;
        } else if kind.is_symlink() {
            let link = fs::read_link(&source)
                .with_context(|| format!("reading the link {}", source.display()))?;
            std::os::unix::fs::symlink(&link, &target)
                .with_context(|| format!("creating the link {}", target.display()))?;
        } else if kind.is_file() {
            fs::copy(&source, &target).with_context(|| format!("copying {}", source.display()))?;
        } else {
            continue;
        }
        take_over(&meta, &target)?;
    }
    Ok(())
}

/// Fills a named volume with what an image of several layers has at `relative`, layer by
/// layer from the bottom, as the assembled image would show it: a higher layer adds and
/// replaces entries, its whiteouts (overlayfs character devices 0:0) remove what a lower
/// one had, and an opaque directory hides everything below it. Whether a directory was
/// found; owner and mode are the topmost layer's, as with `seed`.
pub fn seed_from_layers(layers: &[PathBuf], relative: &str, to: &Path) -> Result<bool> {
    let relative = Path::new(relative);
    let mut top: Option<fs::Metadata> = None;
    for layer in layers {
        // A whiteout or an opaque directory on the way hides what lower layers had.
        let mut hidden = false;
        let mut prefix = PathBuf::new();
        for component in relative.iter() {
            prefix.push(component);
            let Some(path) = crate::backend::chase(layer, &prefix) else {
                break;
            };
            match path.symlink_metadata() {
                Err(_) => break,
                Ok(meta) if is_whiteout(&meta) => {
                    hidden = true;
                    break;
                }
                Ok(meta) if meta.is_dir() && is_opaque(&path) => hidden = true,
                Ok(_) => {}
            }
        }
        if hidden {
            clear_dir(to)?;
            top = None;
        }
        let Some(path) = crate::backend::chase(layer, relative) else {
            continue;
        };
        let Ok(meta) = path.symlink_metadata() else {
            continue;
        };
        if is_whiteout(&meta) || !meta.is_dir() {
            continue;
        }
        merge_tree(&path, to)?;
        top = Some(meta);
    }
    match top {
        Some(meta) => take_over(&meta, to).map(|()| true),
        None => Ok(false),
    }
}

/// A higher layer's directory over what the copy holds so far.
fn merge_tree(from: &Path, to: &Path) -> Result<()> {
    for entry in fs::read_dir(from).with_context(|| format!("reading {}", from.display()))? {
        let entry = entry.with_context(|| format!("reading {}", from.display()))?;
        let source = entry.path();
        let target = to.join(entry.file_name());
        let meta = source
            .symlink_metadata()
            .with_context(|| format!("looking at {}", source.display()))?;
        let kind = meta.file_type();
        if is_whiteout(&meta) {
            remove_entry(&target)?;
            continue;
        }
        if kind.is_dir() {
            // A link a lower layer left there goes first: clearing through it would empty
            // whatever it points at on the host.
            if target.symlink_metadata().is_ok_and(|m| !m.is_dir()) {
                remove_entry(&target)?;
            }
            if is_opaque(&source) {
                clear_dir(&target)?;
            }
            if !target.is_dir() {
                fs::create_dir(&target)
                    .with_context(|| format!("creating {}", target.display()))?;
            }
            merge_tree(&source, &target)?;
        } else if kind.is_symlink() {
            remove_entry(&target)?;
            let link = fs::read_link(&source)
                .with_context(|| format!("reading the link {}", source.display()))?;
            std::os::unix::fs::symlink(&link, &target)
                .with_context(|| format!("creating the link {}", target.display()))?;
        } else if kind.is_file() {
            remove_entry(&target)?;
            fs::copy(&source, &target).with_context(|| format!("copying {}", source.display()))?;
        } else {
            continue;
        }
        take_over(&meta, &target)?;
    }
    Ok(())
}

/// An overlayfs whiteout: a character device 0:0.
fn is_whiteout(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::FileTypeExt;
    meta.file_type().is_char_device() && meta.rdev() == 0
}

/// An overlayfs opaque directory: nothing below it in lower layers shows.
fn is_opaque(dir: &Path) -> bool {
    ["trusted.overlay.opaque", "user.overlay.opaque"]
        .iter()
        .any(|name| xattr::get(dir, name).ok().flatten().as_deref() == Some(b"y"))
}

fn remove_entry(path: &Path) -> Result<()> {
    match path.symlink_metadata() {
        Ok(meta) if meta.is_dir() => {
            crate::store::remove_tree(path).with_context(|| format!("removing {}", path.display()))
        }
        Ok(_) => fs::remove_file(path).with_context(|| format!("removing {}", path.display())),
        Err(_) => Ok(()),
    }
}

/// Empties a volume a seed left half filled; best effort.
pub fn empty(dir: &Path) {
    let _ = clear_dir(dir);
}

/// Empties a directory, keeping the directory itself; a link is never followed.
fn clear_dir(dir: &Path) -> Result<()> {
    if !dir.symlink_metadata().is_ok_and(|m| m.is_dir()) {
        return Ok(());
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in entries {
        let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
        remove_entry(&entry.path())?;
    }
    Ok(())
}

/// The owner, mode and times of the image's entry, on the copy.
fn take_over(meta: &fs::Metadata, target: &Path) -> Result<()> {
    std::os::unix::fs::lchown(target, Some(meta.uid()), Some(meta.gid()))
        .with_context(|| format!("owning {}", target.display()))?;
    if !meta.file_type().is_symlink() {
        fs::set_permissions(target, fs::Permissions::from_mode(meta.mode() & 0o7777))
            .with_context(|| format!("setting the mode of {}", target.display()))?;
        let times = fs::FileTimes::new()
            .set_modified(meta.modified()?)
            .set_accessed(meta.accessed()?);
        fs::File::open(target)
            .and_then(|f| f.set_times(times))
            .with_context(|| format!("setting the times of {}", target.display()))?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Volume {
    /// An absolute host path, or the name of a managed volume.
    pub source: String,
    /// Absolute path inside the machine.
    pub target: String,
    #[serde(default)]
    pub read_only: bool,
}

/// The paths clients of the Docker API look for its socket at; a machine that mounts
/// either from the host gets nspawn's Docker API there.
pub const API_SOCKET_PATHS: [&str; 2] = ["/var/run/docker.sock", "/run/docker.sock"];

impl Volume {
    pub fn is_named(&self) -> bool {
        !self.source.starts_with('/')
    }

    /// A bind of docker's socket, which becomes the machine's Docker API socket.
    pub fn is_api_socket(&self) -> bool {
        API_SOCKET_PATHS.contains(&self.source.as_str())
    }

    /// Where the volume lives on the host.
    pub fn host_path(&self, volumes_dir: &Path) -> PathBuf {
        if self.is_named() {
            volumes_dir.join(&self.source)
        } else {
            PathBuf::from(&self.source)
        }
    }
}

impl FromStr for Volume {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        let parts: Vec<&str> = text.split(':').collect();
        let (source, target, mode) = match parts.as_slice() {
            [source, target] => (*source, *target, "rw"),
            [source, target, mode] => (*source, *target, *mode),
            _ => bail!("{text}: expected SOURCE:TARGET[:ro]"),
        };
        let read_only = match mode {
            "rw" => false,
            "ro" => true,
            other => bail!("{text}: unknown option {other} (ro or rw)"),
        };
        if source.is_empty() || target.is_empty() {
            bail!("{text}: source and target must not be empty");
        }
        if !target.starts_with('/') {
            bail!("{text}: the target must be an absolute path inside the machine");
        }
        if source.starts_with('/') {
            if source.contains("/../") || source.ends_with("/..") {
                bail!("{text}: the source must be a plain absolute path");
            }
        } else if validate_volume_name(source).is_err() {
            bail!("{text}: a volume name may only have letters, digits, _ . and -");
        }
        if [source, target]
            .iter()
            .any(|p| p.chars().any(char::is_whitespace))
        {
            bail!("{text}: paths with whitespace are not supported");
        }
        if target.trim_end_matches('/').is_empty() {
            bail!("{text}: the machine's root cannot be a volume target");
        }
        if !plain_inside_path(target) {
            bail!("{text}: the target is a plain path inside the machine, without . or .. in it");
        }
        reject_control_characters(text)?;
        Ok(Volume {
            source: source.to_string(),
            target: target.to_string(),
            read_only,
        })
    }
}

impl fmt::Display for Volume {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.source, self.target)?;
        if self.read_only {
            write!(f, ":ro")?;
        }
        Ok(())
    }
}

/// The names a managed volume may have: what `-v NAME:/path` accepts, which is also a
/// safe single directory name under the volumes directory.
/// An absolute path inside the machine with no `.` or `..` component: one of those
/// would resolve to another place once systemd-nspawn mounts there, the root itself
/// included.
pub fn plain_inside_path(path: &str) -> bool {
    path.starts_with('/') && path.split('/').all(|c| c != "." && c != "..")
}

/// The place a path inside the machine mounts at, as systemd-nspawn compares mount
/// points: repeated and trailing slashes do not count.
pub fn mount_point(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
    format!("/{}", parts.join("/"))
}

/// A regular file opened for reading, and nothing else: the path is pinned with O_PATH
/// first, which runs no driver's open and waits on no FIFO, its type checked on that,
/// and only then opened through /proc/self/fd. A device node an image or a machine has
/// where a file is read would otherwise reach the host's device.
pub fn open_regular(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let pinned = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_PATH | nix::libc::O_NOFOLLOW)
        .open(path)?;
    if !pinned.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", path.display()),
        ));
    }
    std::fs::File::open(format!("/proc/self/fd/{}", pinned.as_raw_fd()))
}

pub fn validate_volume_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.starts_with('.')
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
    {
        bail!("{name:?} is not a volume name: letters, digits, _ . and -, not starting with a dot");
    }
    Ok(())
}

/// Parses the --label values, KEY=VALUE each; "none" alone clears them, a later value of
/// the same key wins.
pub fn parse_labels(values: &[String]) -> Result<std::collections::BTreeMap<String, String>> {
    let mut out = std::collections::BTreeMap::new();
    if values.len() == 1 && values[0] == "none" {
        return Ok(out);
    }
    for value in values {
        reject_control_characters(value)?;
        let Some((key, val)) = value.split_once('=') else {
            bail!("{value}: expected KEY=VALUE");
        };
        if key.is_empty() || key.chars().any(char::is_whitespace) {
            bail!("{value}: a label needs a key without whitespace");
        }
        out.insert(key.to_string(), val.to_string());
    }
    Ok(out)
}

/// Parses the -v values; "none" alone clears the list.
pub fn parse_volumes(values: &[String]) -> Result<Vec<Volume>> {
    if values.len() == 1 && values[0] == "none" {
        return Ok(Vec::new());
    }
    let mut out: Vec<Volume> = Vec::new();
    for value in values {
        let volume: Volume = value.parse()?;
        let target = mount_point(&volume.target);
        if out.iter().any(|v| mount_point(&v.target) == target) {
            bail!("Duplicate mount point: {target}");
        }
        out.push(volume);
    }
    Ok(out)
}

/// The settings file is line based: a control character in a value would become another
/// directive, or nothing systemd can read.
pub fn reject_control_characters(text: &str) -> Result<()> {
    if text.chars().any(|c| c.is_control()) {
        bail!("{text:?}: control characters are not allowed");
    }
    Ok(())
}

/// The same for a value the settings file carries in quotes (a variable, an argument
/// of the program), where a tab is a character like any other, as images have them.
pub fn reject_control_characters_but_tab(text: &str) -> Result<()> {
    if text.chars().any(|c| c.is_control() && c != '\t') {
        bail!("{text:?}: control characters other than tabs are not allowed");
    }
    Ok(())
}

/// Environment as a program sees it when `extra` comes after `base`: one entry per
/// variable, the later value winning (execve keeps duplicates and getenv reads the
/// first, which is not what a later Environment= line means to systemd).
pub fn merge_env(base: &[String], extra: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for var in base.iter().chain(extra) {
        let name = var.split_once('=').map(|(n, _)| n).unwrap_or(var);
        out.retain(|v| v.split_once('=').map(|(n, _)| n).unwrap_or(v) != name);
        out.push(var.clone());
    }
    out
}

/// Copies the value of every bare `VAR` from this process's environment, so that a
/// caller's variables reach a service that has an environment of its own. `VAR=value`
/// and "none" pass through as they are.
pub fn expand_env(values: &[String]) -> Result<Vec<String>> {
    values
        .iter()
        .map(|value| {
            if value.contains('=') || value == "none" {
                return Ok(value.clone());
            }
            match std::env::var(value) {
                Ok(current) => Ok(format!("{value}={current}")),
                Err(_) => bail!("{value} has no value here; give it as {value}=VALUE"),
            }
        })
        .collect()
}

/// Parses the -e values: VAR=value as given, VAR alone copied from this environment.
/// "none" alone clears the list; a later value of the same variable wins.
pub fn parse_env(values: &[String]) -> Result<Vec<String>> {
    if values.len() == 1 && values[0] == "none" {
        return Ok(Vec::new());
    }
    let mut out: Vec<String> = Vec::new();
    for value in values {
        reject_control_characters_but_tab(value)?;
        let (name, assignment) = match value.split_once('=') {
            Some((name, _)) => (name, value.clone()),
            None => match std::env::var(value) {
                Ok(current) => (value.as_str(), format!("{value}={current}")),
                Err(_) => bail!("{value} has no value here; give it as {value}=VALUE"),
            },
        };
        // Any name but an empty one: Elasticsearch reads discovery.type, and the
        // settings file carries such names as they are.
        if name.is_empty() {
            bail!("invalid environment variable: {value}");
        }
        out.retain(|v| v.split_once('=').map(|(n, _)| n) != Some(name));
        out.push(assignment);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {

    #[test]
    fn only_a_regular_file_is_opened() {
        use std::io::Read;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("passwd");
        std::fs::write(&file, "root:x:0:0::/root:/bin/sh\n").unwrap();
        let mut text = String::new();
        open_regular(&file)
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert!(text.starts_with("root:"));
        // A FIFO is refused at once, a device node, a link and a directory as well.
        let fifo = dir.path().join("fifo");
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRWXU).unwrap();
        assert!(open_regular(&fifo).is_err());
        assert!(open_regular(Path::new("/dev/null")).is_err());
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(open_regular(&link).is_err());
        assert!(open_regular(dir.path()).is_err());
    }

    use super::*;

    #[test]
    fn labels_are_key_value_pairs() {
        let labels = parse_labels(&[
            "caddy=example.com".to_string(),
            "caddy.reverse_proxy={{upstreams 80}}".to_string(),
            "empty=".to_string(),
            "caddy=example.org".to_string(),
        ])
        .unwrap();
        assert_eq!(labels.len(), 3);
        assert_eq!(labels["caddy"], "example.org", "the later value wins");
        assert_eq!(labels["caddy.reverse_proxy"], "{{upstreams 80}}");
        assert_eq!(labels["empty"], "");
        assert!(parse_labels(&["none".to_string()]).unwrap().is_empty());
        for bad in ["novalue", "=x", "a b=c", "a=\nb"] {
            assert!(parse_labels(&[bad.to_string()]).is_err(), "{bad}");
        }
    }

    #[test]
    fn volume_names_are_single_plain_directory_names() {
        for good in ["data", "pg-data_1", "a.b", "X"] {
            assert!(validate_volume_name(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            ".",
            "..",
            ".hidden",
            "a/b",
            "a b",
            "../x",
            "a:b",
            "caf\u{e9}",
        ] {
            assert!(validate_volume_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn bare_variables_are_copied_from_the_environment_before_a_call() {
        std::env::set_var("NSPAWN_TEST_EXPAND", "yes");
        let values = [
            "A=1".to_string(),
            "NSPAWN_TEST_EXPAND".to_string(),
            "none".to_string(),
        ];
        assert_eq!(
            expand_env(&values).unwrap(),
            ["A=1", "NSPAWN_TEST_EXPAND=yes", "none"]
        );
        let err = expand_env(&["NSPAWN_TEST_UNSET_XYZ".to_string()]).unwrap_err();
        assert!(err.to_string().contains("has no value here"));
    }

    #[test]
    fn volume_syntax() {
        let v: Volume = "/srv/data:/data".parse().unwrap();
        assert_eq!(
            (v.source.as_str(), v.target.as_str(), v.read_only),
            ("/srv/data", "/data", false)
        );
        assert!(!v.is_named());
        assert_eq!(
            v.host_path(Path::new("/var/lib/nspawn/volumes")),
            PathBuf::from("/srv/data")
        );
        let v: Volume = "pgdata:/var/lib/postgresql:ro".parse().unwrap();
        assert!(v.is_named() && v.read_only);
        assert_eq!(
            v.host_path(Path::new("/var/lib/nspawn/volumes")),
            PathBuf::from("/var/lib/nspawn/volumes/pgdata")
        );
        assert_eq!(v.to_string(), "pgdata:/var/lib/postgresql:ro");
        for bad in [
            "/only",
            "/a:relative",
            "a b:/x",
            "../x:/x",
            "/a/../b:/x",
            "a:/x:rx",
            ".hidden:/x",
            ":/x",
        ] {
            assert!(bad.parse::<Volume>().is_err(), "{bad}");
        }
        assert!(parse_volumes(&["none".to_string()]).unwrap().is_empty());
        assert!(parse_volumes(&["/a:/x".to_string(), "/b:/x".to_string()]).is_err());
        // The same place however it is spelled.
        for (one, other) in [("/a:/x/y", "/b:/x//y"), ("/a:/x", "/b:/x/")] {
            assert_eq!(
                parse_volumes(&[one.to_string(), other.to_string()])
                    .unwrap_err()
                    .to_string(),
                format!("Duplicate mount point: {}", mount_point(&one[3..])),
            );
        }
        assert_eq!(mount_point("/var//lib/pg/"), "/var/lib/pg");
        assert_eq!(mount_point("/"), "/");
    }

    #[test]
    fn env_syntax() {
        std::env::set_var("NSPAWN_TEST_VAR", "from-host");
        let env = parse_env(&["A=1".into(), "NSPAWN_TEST_VAR".into(), "A=2".into()]).unwrap();
        assert_eq!(env, vec!["NSPAWN_TEST_VAR=from-host", "A=2"]);
        assert!(parse_env(&["NSPAWN_TEST_UNSET_VAR".into()]).is_err());
        assert_eq!(
            parse_env(&["discovery.type=single-node".into(), "1A=x".into()]).unwrap(),
            ["discovery.type=single-node", "1A=x"]
        );
        assert!(parse_env(&["=x".into()]).is_err());
        assert!(parse_env(&["none".into()]).unwrap().is_empty());
        assert_eq!(parse_env(&["X=a=b".into()]).unwrap(), vec!["X=a=b"]);
        assert!(
            parse_env(&["X=line\nbreak".into()]).is_err(),
            "a newline would be another directive"
        );
    }

    #[test]
    fn later_environment_wins_once_merged() {
        let base = vec!["PATH=/usr/bin".to_string(), "HOME=/root".to_string()];
        let extra = vec![
            "PATH=/opt/app/bin:/usr/bin".to_string(),
            "MODE=prod".to_string(),
        ];
        assert_eq!(
            merge_env(&base, &extra),
            vec!["HOME=/root", "PATH=/opt/app/bin:/usr/bin", "MODE=prod"]
        );
        assert_eq!(merge_env(&[], &[]), Vec::<String>::new());
    }

    #[test]
    fn variables_keep_their_tabs() {
        assert_eq!(
            parse_env(&["DEPS=a\tb".to_string()]).unwrap(),
            ["DEPS=a\tb"]
        );
        assert!(parse_env(&["A=1\nB=2".to_string()]).is_err());
    }

    #[test]
    fn root_and_control_characters_are_refused_as_volumes() {
        assert!("/srv:/".parse::<Volume>().is_err());
        assert!("/srv://".parse::<Volume>().is_err());
        assert!("/srv:/data\n".parse::<Volume>().is_err());
        assert!("/srv:/data".parse::<Volume>().is_ok());
        // A dot component would land the mount elsewhere, on the root for one.
        for bad in [
            "/srv:/.",
            "/srv:/..",
            "/srv:/a/..",
            "/srv:/a/./b",
            "/srv:/a/../b",
        ] {
            assert!(bad.parse::<Volume>().is_err(), "{bad}");
        }
        assert!(plain_inside_path("/a/b") && plain_inside_path("/a.b/..c"));
        assert!(!plain_inside_path("a/b") && !plain_inside_path("/a/./b"));
    }
    #[test]
    fn a_volume_is_seeded_from_every_layer_of_the_image() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("base");
        let top = tmp.path().join("top");
        fs::create_dir_all(base.join("data/sub")).unwrap();
        fs::write(base.join("data/a"), "base").unwrap();
        fs::write(base.join("data/sub/b"), "b").unwrap();
        fs::write(base.join("data/gone"), "gone").unwrap();
        fs::create_dir_all(top.join("data/sub")).unwrap();
        fs::write(top.join("data/a"), "top").unwrap();
        fs::write(top.join("data/c"), "c").unwrap();
        fs::set_permissions(top.join("data"), fs::Permissions::from_mode(0o750)).unwrap();
        let volume = tmp.path().join("volume");
        fs::create_dir(&volume).unwrap();
        let layers = vec![base.clone(), top.clone()];
        assert!(seed_from_layers(&layers, "data", &volume).unwrap());
        // The top layer's files replace, the base's stay, the top's mode wins.
        assert_eq!(fs::read_to_string(volume.join("a")).unwrap(), "top");
        assert_eq!(fs::read_to_string(volume.join("sub/b")).unwrap(), "b");
        assert_eq!(fs::read_to_string(volume.join("c")).unwrap(), "c");
        assert_eq!(fs::read_to_string(volume.join("gone")).unwrap(), "gone");
        assert_eq!(fs::metadata(&volume).unwrap().mode() & 0o7777, 0o750);
        // A path only the base has is seeded from the base alone.
        let other = tmp.path().join("other");
        fs::create_dir(&other).unwrap();
        fs::create_dir_all(base.join("only")).unwrap();
        fs::write(base.join("only/x"), "x").unwrap();
        assert!(seed_from_layers(&layers, "only", &other).unwrap());
        assert_eq!(fs::read_to_string(other.join("x")).unwrap(), "x");
        // Nothing anywhere: nothing seeded, the directory untouched.
        assert!(!seed_from_layers(&layers, "nowhere", &other).unwrap());
        // An opaque directory in the top layer hides the base's content.
        if xattr::set(top.join("data"), "user.overlay.opaque", b"y").is_ok() {
            let opaque = tmp.path().join("opaque");
            fs::create_dir(&opaque).unwrap();
            assert!(seed_from_layers(&layers, "data", &opaque).unwrap());
            assert!(opaque.join("c").is_file() && !opaque.join("gone").exists());
        }
    }

    #[test]
    fn an_opaque_directory_over_a_link_clears_nothing_outside() {
        let tmp = tempfile::tempdir().unwrap();
        let victim = tmp.path().join("victim");
        fs::create_dir(&victim).unwrap();
        fs::write(victim.join("keep"), "keep").unwrap();
        let base = tmp.path().join("base");
        let top = tmp.path().join("top");
        fs::create_dir_all(base.join("data")).unwrap();
        std::os::unix::fs::symlink(&victim, base.join("data/cache")).unwrap();
        fs::create_dir_all(top.join("data/cache")).unwrap();
        fs::write(top.join("data/cache/new"), "new").unwrap();
        if xattr::set(top.join("data/cache"), "user.overlay.opaque", b"y").is_err() {
            return;
        }
        let volume = tmp.path().join("volume");
        fs::create_dir(&volume).unwrap();
        assert!(seed_from_layers(&[base, top], "data", &volume).unwrap());
        assert_eq!(fs::read_to_string(victim.join("keep")).unwrap(), "keep");
        assert!(!victim.join("new").exists());
        let cache = volume.join("cache");
        assert!(cache.symlink_metadata().unwrap().is_dir());
        assert_eq!(fs::read_to_string(cache.join("new")).unwrap(), "new");
        // Clearing a link leaves its target alone.
        std::os::unix::fs::symlink(&victim, tmp.path().join("link")).unwrap();
        clear_dir(&tmp.path().join("link")).unwrap();
        assert!(victim.join("keep").exists());
    }

    #[test]
    fn a_new_volume_takes_what_the_image_has() {
        let tmp = tempfile::tempdir().unwrap();
        let image = tmp.path().join("image/data");
        fs::create_dir_all(image.join("sub")).unwrap();
        fs::write(image.join("sub/file"), "x").unwrap();
        fs::set_permissions(image.join("sub/file"), fs::Permissions::from_mode(0o640)).unwrap();
        fs::set_permissions(&image, fs::Permissions::from_mode(0o750)).unwrap();
        std::os::unix::fs::symlink("sub/file", image.join("link")).unwrap();
        let volume = tmp.path().join("volume");
        fs::create_dir(&volume).unwrap();
        seed(&image, &volume).unwrap();
        assert_eq!(fs::read_to_string(volume.join("sub/file")).unwrap(), "x");
        assert_eq!(
            fs::metadata(volume.join("sub/file")).unwrap().mode() & 0o777,
            0o640
        );
        assert_eq!(fs::metadata(&volume).unwrap().mode() & 0o777, 0o750);
        assert_eq!(
            fs::read_link(volume.join("link")).unwrap(),
            Path::new("sub/file")
        );
        // Nothing to take from a path the image lacks or that is not a directory.
        let other = tmp.path().join("other");
        fs::create_dir(&other).unwrap();
        seed(&image.join("sub/file"), &other).unwrap();
        assert_eq!(fs::read_dir(&other).unwrap().count(), 0);
    }
}
