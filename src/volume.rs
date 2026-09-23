//! Volumes as docker knows them: `-v /host/dir:/inside[:ro]` binds a host path, and
//! `-v name:/inside` a directory nspawn keeps under its own volumes directory. Both end up
//! as bind mounts in the machine's settings.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Volume {
    /// An absolute host path, or the name of a managed volume.
    pub source: String,
    /// Absolute path inside the machine.
    pub target: String,
    #[serde(default)]
    pub read_only: bool,
}

impl Volume {
    pub fn is_named(&self) -> bool {
        !self.source.starts_with('/')
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
        } else if !source
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
            || source.starts_with('.')
        {
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

/// Parses the -v values; "none" alone clears the list.
pub fn parse_volumes(values: &[String]) -> Result<Vec<Volume>> {
    if values.len() == 1 && values[0] == "none" {
        return Ok(Vec::new());
    }
    let mut out: Vec<Volume> = Vec::new();
    for value in values {
        let volume: Volume = value.parse()?;
        if out.iter().any(|v| v.target == volume.target) {
            bail!("{} is mounted twice", volume.target);
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

/// Parses the -e values: VAR=value as given, VAR alone copied from this environment.
/// "none" alone clears the list; a later value of the same variable wins.
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
                Err(_) => bail!("{value} is not set in the environment; give it as {value}=VALUE"),
            }
        })
        .collect()
}

pub fn parse_env(values: &[String]) -> Result<Vec<String>> {
    if values.len() == 1 && values[0] == "none" {
        return Ok(Vec::new());
    }
    let mut out: Vec<String> = Vec::new();
    for value in values {
        reject_control_characters(value)?;
        let (name, assignment) = match value.split_once('=') {
            Some((name, _)) => (name, value.clone()),
            None => match std::env::var(value) {
                Ok(current) => (value.as_str(), format!("{value}={current}")),
                Err(_) => bail!("{value} is not set in the environment; give it as {value}=VALUE"),
            },
        };
        if name.is_empty()
            || name.starts_with(|c: char| c.is_ascii_digit())
            || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            bail!("{value}: {name:?} is not a valid variable name");
        }
        out.retain(|v| v.split_once('=').map(|(n, _)| n) != Some(name));
        out.push(assignment);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(err.to_string().contains("not set in the environment"));
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
    }

    #[test]
    fn env_syntax() {
        std::env::set_var("NSPAWN_TEST_VAR", "from-host");
        let env = parse_env(&["A=1".into(), "NSPAWN_TEST_VAR".into(), "A=2".into()]).unwrap();
        assert_eq!(env, vec!["NSPAWN_TEST_VAR=from-host", "A=2"]);
        assert!(parse_env(&["NSPAWN_TEST_UNSET_VAR".into()]).is_err());
        assert!(parse_env(&["1A=x".into()]).is_err());
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
    fn root_and_control_characters_are_refused_as_volumes() {
        assert!("/srv:/".parse::<Volume>().is_err());
        assert!("/srv://".parse::<Volume>().is_err());
        assert!("/srv:/data\n".parse::<Volume>().is_err());
        assert!("/srv:/data".parse::<Volume>().is_ok());
    }
}
