//! Paths as compose-go handles them: Go's filepath functions, and the relative paths
//! of a model made absolute against the project directory (paths.ResolveRelativePaths).

use super::canonical::clean;
use super::path::Path;
use super::value::Value;

pub fn is_abs(path: &str) -> bool {
    path.starts_with('/')
}

/// filepath.Join: the parts joined and cleaned, empty ones skipped.
pub fn join(parts: &[&str]) -> String {
    let parts: Vec<&str> = parts.iter().copied().filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return String::new();
    }
    clean(&parts.join("/"))
}

/// filepath.Dir.
pub fn dir(path: &str) -> String {
    match path.rfind('/') {
        Some(at) => clean(&path[..=at]),
        None => ".".to_string(),
    }
}

/// filepath.Base.
pub fn base(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".to_string();
    }
    trimmed.rsplit('/').next().unwrap_or(trimmed).to_string()
}

/// filepath.Abs against `cwd`.
pub fn abs(path: &str, cwd: &str) -> String {
    if is_abs(path) {
        clean(path)
    } else {
        join(&[cwd, path])
    }
}

/// filepath.Rel: `target` relative to `base`, both made clean; None when one is
/// absolute and the other not.
pub fn rel(base_path: &str, target: &str) -> Option<String> {
    let (b, t) = (clean(base_path), clean(target));
    if is_abs(&b) != is_abs(&t) {
        return None;
    }
    if b == t {
        return Some(".".to_string());
    }
    let split = |p: &str| -> Vec<String> {
        if p == "." || p == "/" {
            Vec::new()
        } else {
            p.trim_start_matches('/')
                .split('/')
                .map(String::from)
                .collect()
        }
    };
    let (bp, tp) = (split(&b), split(&t));
    let common = bp.iter().zip(&tp).take_while(|(x, y)| x == y).count();
    if bp[common..].iter().any(|p| p == "..") {
        return None;
    }
    let mut parts: Vec<String> = vec!["..".to_string(); bp.len() - common];
    parts.extend(tp[common..].iter().cloned());
    Some(parts.join("/"))
}

/// localResourceLoader.Dir: the directory of `path` (or `path` itself, a directory)
/// relative to the loader's own.
pub fn loader_dir(base_dir: &str, path: &str) -> String {
    let dir = match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => path.to_string(),
        _ => dir(path),
    };
    rel(base_dir, &dir).unwrap_or(dir)
}

/// paths.ExpandUser: `~` as the user's home.
pub fn expand_user(path: &str, home: Option<&str>, warn: &mut dyn FnMut(String)) -> String {
    if let Some(rest) = path.strip_prefix('~') {
        match home {
            Some(home) => return join(&[home, rest]),
            None => warn("cannot expand '~', because the environment lacks HOME".to_string()),
        }
    }
    path.to_string()
}

/// Why a file to fetch from elsewhere is refused.
pub const REMOTE: &str = "files on this host: a git or oci:// address is refused";

/// Whether a path names a file to fetch rather than read: `oci://` for the OCI loader,
/// what buildkit's ParseGitRef takes for the git one (the query of the address is not
/// checked).
pub fn remote(path: &str) -> bool {
    if path.starts_with("oci://") || path.starts_with("github.com/") {
        return true;
    }
    if path.starts_with("./") || path.starts_with("../") {
        return false;
    }
    if let Some((proto, rest)) = path.split_once("://") {
        if !proto.is_empty() && proto.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return match proto.to_ascii_lowercase().as_str() {
                "ssh" | "git" => true,
                // An http(s) address is a repository when its path ends in .git.
                "http" | "https" => {
                    let rest = &rest[..rest.find(['?', '#']).unwrap_or(rest.len())];
                    rest.find('/')
                        .is_some_and(|at| rest[at..].ends_with(".git"))
                }
                _ => false,
            };
        }
    }
    // scp-style: user@host:path.
    let Some((user, after)) = path.split_once('@') else {
        return false;
    };
    let Some((host, _)) = after.split_once(':') else {
        return false;
    };
    !user.is_empty()
        && user
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        && !host.is_empty()
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
}

pub struct Resolver<'a> {
    pub working_dir: &'a str,
    pub home: Option<&'a str>,
}

impl Resolver<'_> {
    /// Makes the relative paths of `model` absolute.
    pub fn resolve(&self, model: &mut Value, warn: &mut dyn FnMut(String)) -> Result<(), String> {
        self.walk(model, &Path::root(), warn)
    }

    fn walk(
        &self,
        value: &mut Value,
        path: &Path,
        warn: &mut dyn FnMut(String),
    ) -> Result<(), String> {
        let parts = path.parts();
        let handled = match parts.as_slice() {
            ["services" | "jobs", _, "build", "context"]
            | ["services" | "jobs", _, "build", "additional_contexts", _] => {
                self.context(value, warn);
                true
            }
            ["services" | "jobs", _, "build", "ssh", _] | ["configs" | "secrets", _, "file"] => {
                self.unix_path(value, warn);
                true
            }
            ["services" | "jobs", _, "env_file", _, "path"]
            | ["services" | "jobs", _, "label_file", _]
            | ["services", _, "pre_start", _, "env_file", _, "path"]
            | ["services", _, "pre_start", _, "label_file", _]
            | ["include", "path"]
            | ["include", "project_directory"]
            | ["include", "env_file"] => {
                self.abs_path(value, warn)?;
                true
            }
            ["services", _, "develop", "watch", _, "path"] => {
                self.abs_path(value, warn)?;
                if let Value::Str(s) = value {
                    *s = resolve_symbolic_link(s)?;
                }
                true
            }
            ["services" | "jobs", _, "extends", "file"] => {
                self.abs_path(value, warn)?;
                true
            }
            ["services" | "jobs", _, "volumes", _]
            | ["services", _, "pre_start", _, "volumes", _] => {
                self.volume_mount(value, warn)?;
                true
            }
            ["volumes", _] => {
                self.volume_driver_opts(value, warn);
                true
            }
            _ => false,
        };
        if handled {
            return Ok(());
        }
        match value {
            Value::Map(map) => {
                for (k, v) in map.iter_mut() {
                    self.walk(v, &path.next(k), warn)?;
                }
            }
            Value::Seq(items) => {
                let next = path.next("[]");
                for item in items {
                    self.walk(item, &next, warn)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn abs_path(&self, value: &mut Value, warn: &mut dyn FnMut(String)) -> Result<(), String> {
        match value {
            Value::Seq(items) => {
                for item in items {
                    self.abs_path(item, warn)?;
                }
                Ok(())
            }
            Value::Str(s) => {
                let expanded = expand_user(s, self.home, warn);
                *s = if is_abs(&expanded) || expanded.is_empty() {
                    expanded
                } else {
                    join(&[self.working_dir, &expanded])
                };
                Ok(())
            }
            other => Err(format!("unexpected type {}", other.go_type())),
        }
    }

    fn unix_path(&self, value: &mut Value, warn: &mut dyn FnMut(String)) {
        if let Value::Str(s) = value {
            let expanded = expand_user(s, self.home, warn);
            *s = if is_abs(&expanded) || windows_abs(&expanded) {
                expanded
            } else {
                join(&[self.working_dir, &expanded])
            };
        }
    }

    fn context(&self, value: &mut Value, warn: &mut dyn FnMut(String)) {
        let Value::Str(s) = value else { return };
        let remote = [
            "https://",
            "http://",
            "git://",
            "ssh://",
            "github.com/",
            "git@",
        ]
        .iter()
        .any(|p| s.starts_with(p));
        if s.contains("://") || s.starts_with("service:") || remote {
            return;
        }
        let _ = self.abs_path(value, warn);
    }

    fn volume_mount(&self, value: &mut Value, warn: &mut dyn FnMut(String)) -> Result<(), String> {
        let Value::Map(volume) = value else {
            return Ok(());
        };
        if volume.get("type") != Some(&Value::str("bind")) {
            return Ok(());
        }
        let Some(source) = volume.get_mut("source") else {
            return Err(
                "invalid mount config for type \"bind\": field Source must not be empty"
                    .to_string(),
            );
        };
        self.unix_path(source, warn);
        Ok(())
    }

    fn volume_driver_opts(&self, value: &mut Value, warn: &mut dyn FnMut(String)) {
        let Value::Map(volume) = value else { return };
        if volume.get("driver") != Some(&Value::str("local")) {
            return;
        }
        let Some(Value::Map(options)) = volume.get_mut("driver_opts") else {
            return;
        };
        if options.get("o") == Some(&Value::str("bind")) {
            if let Some(device) = options.get_mut("device") {
                self.unix_path(device, warn);
            }
        }
    }
}

/// A Windows absolute path, `C:\` or `C:/`, which compose-go leaves alone.
/// compose-go's utils.ResolveSymbolicLink: the first part of the path that is a
/// symbolic link replaced by where it leads, links and all.
fn resolve_symbolic_link(path: &str) -> Result<String, String> {
    let mut current = if is_abs(path) {
        "/".to_string()
    } else {
        String::new()
    };
    for part in path.split('/').filter(|p| !p.is_empty()) {
        current = join(&[&current, part]);
        let link = std::fs::symlink_metadata(&current).is_ok_and(|m| m.file_type().is_symlink());
        if link {
            let target = std::fs::canonicalize(&current)
                .map_err(|e| super::gostr::path_error("lstat", &current, &e))?;
            return Ok(path.replacen(&current, &target.to_string_lossy(), 1));
        }
    }
    Ok(path.to_string())
}

/// compose-go's paths.IsWindowsAbs: a drive (any byte before a `:`) then a separator,
/// or a UNC or device path.
fn windows_abs(path: &str) -> bool {
    let p = path.as_bytes();
    let sep = |c: u8| c == b'\\' || c == b'/';
    let l = volume_name_len(p);
    if l == 0 {
        return false;
    }
    if sep(p[0]) && sep(p[1]) {
        return true;
    }
    p.get(l).is_some_and(|&c| sep(c))
}

fn volume_name_len(p: &[u8]) -> usize {
    let sep = |c: u8| c == b'\\' || c == b'/';
    let has_prefix_fold = |prefix: &[u8]| {
        p.len() >= prefix.len()
            && prefix.iter().zip(p).all(|(&a, &b)| {
                if sep(a) {
                    sep(b)
                } else {
                    a.eq_ignore_ascii_case(&b)
                }
            })
            && p.get(prefix.len()).is_none_or(|&c| sep(c))
    };
    let unc_len = |prefix: usize| {
        let mut count = 0;
        for (i, &c) in p.iter().enumerate().skip(prefix) {
            if sep(c) {
                count += 1;
                if count == 2 {
                    return i;
                }
            }
        }
        p.len()
    };
    if p.len() >= 2 && p[1] == b':' {
        return 2;
    }
    if p.is_empty() || !sep(p[0]) {
        return 0;
    }
    if has_prefix_fold(b"\\\\.\\UNC") {
        return unc_len(b"\\\\.\\UNC\\".len());
    }
    if has_prefix_fold(b"\\\\.") || has_prefix_fold(b"\\\\?") || has_prefix_fold(b"\\??") {
        if p.len() == 3 {
            return 3;
        }
        let rest = &p[4..];
        return match rest.iter().position(|&c| sep(c)) {
            Some(i) => p.len() - (rest.len() - i - 1) - 1,
            None => p.len(),
        };
    }
    if p.len() >= 2 && sep(p[1]) {
        return unc_len(2);
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_paths_are_those_docker_compose_fetches() {
        for path in [
            "oci://registry.example/app:1",
            "git@github.com:a/b.git",
            "deploy-1@host.example:repo",
            "ssh://host/a/b",
            "GIT://host/a/b",
            "https://host/a/b.git#main:sub",
            "http://host/a/b.git?ref=v1",
            "github.com/a/b",
        ] {
            assert!(remote(path), "{path}");
        }
        for path in [
            "compose.yaml",
            "./git@host:x",
            "/srv/a@b:c",
            "https://host/a/b",
            "https://host.git",
            "ftp://host/a.git",
            "a.b://host/a.git",
            "us.er@host:x",
            "user@ho_st:x",
        ] {
            assert!(!remote(path), "{path}");
        }
    }

    #[test]
    fn windows_paths_are_absolute_as_compose_go_says() {
        assert!(windows_abs("C:\\x"));
        assert!(windows_abs("1:/x"));
        assert!(windows_abs("\\\\server\\share\\dir"));
        assert!(!windows_abs("C:x"));
        assert!(!windows_abs("x/y"));
    }

    #[test]
    fn go_s_filepath_functions() {
        assert_eq!(join(&["/a", "b/../c"]), "/a/c");
        assert_eq!(dir("/a/b/c.yml"), "/a/b");
        assert_eq!(dir("c.yml"), ".");
        assert_eq!(base("/a/b/"), "b");
        assert_eq!(rel("/a", "/a/b/c").as_deref(), Some("b/c"));
        assert_eq!(rel("/a/x", "/a/b").as_deref(), Some("../b"));
        assert_eq!(rel("/a", "/a").as_deref(), Some("."));
        assert_eq!(abs("sub", "/w"), "/w/sub");
    }

    #[test]
    fn relative_paths_become_absolute() {
        let mut docs = super::super::yaml::parse(
            "services:\n  web:\n    env_file: [{path: .env, required: true}]\n    \
             volumes: [{type: bind, source: ./data, target: /d}, {type: volume, source: db, target: /v}]\n    \
             build: {context: .}\nsecrets:\n  s:\n    file: ~/s.txt\n",
        )
        .unwrap();
        let mut doc = docs.remove(0);
        super::super::yaml::take_resets(&mut doc).unwrap();
        let mut model = super::super::yaml::construct(&doc).unwrap();
        Resolver {
            working_dir: "/w",
            home: Some("/home/u"),
        }
        .resolve(&mut model, &mut |_| {})
        .unwrap();
        let web = model.get(&["services", "web"]).unwrap();
        assert_eq!(
            web.get(&["env_file"]).unwrap().as_seq().unwrap()[0].get(&["path"]),
            Some(&Value::str("/w/.env"))
        );
        let volumes = web.get(&["volumes"]).unwrap().as_seq().unwrap();
        assert_eq!(volumes[0].get(&["source"]), Some(&Value::str("/w/data")));
        assert_eq!(volumes[1].get(&["source"]), Some(&Value::str("db")));
        assert_eq!(web.get(&["build", "context"]), Some(&Value::str("/w")));
        assert_eq!(
            model.get(&["secrets", "s", "file"]),
            Some(&Value::str("/home/u/s.txt"))
        );
    }
}
