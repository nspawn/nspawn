//! What a project is made of before it is loaded, as docker compose works it out from
//! its flags and the environment (compose-go's cli.ProjectOptions): the files (`-f`,
//! COMPOSE_FILE, or found from the working directory up), the environment (the
//! process's, then `.env` or `--env-file`), the profiles and the name.

use std::collections::BTreeMap;

use super::dotenv::Env;
use super::golib::go_quote;
use super::gostr;
use super::loader::{self, ConfigFile, Details, Options, Warn};
use super::paths;

/// The flags of docker compose that make a project.
#[derive(Debug, Clone, Default)]
pub struct Flags {
    pub files: Vec<String>,
    pub project_name: String,
    pub project_directory: String,
    pub env_files: Vec<String>,
    pub profiles: Vec<String>,
}

const DEFAULT_FILES: &[&str] = &[
    "compose.yaml",
    "compose.yml",
    "docker-compose.yml",
    "docker-compose.yaml",
];
const DEFAULT_OVERRIDES: &[&str] = &[
    "compose.override.yml",
    "compose.override.yaml",
    "docker-compose.override.yml",
    "docker-compose.override.yaml",
];

/// The project's options, ready to load.
pub struct Prepared {
    pub details: Details,
    pub options: Options,
}

/// cli.ProjectOptions as its option functions build it.
struct State {
    working_dir: String,
    config_paths: Vec<String>,
    env: Env,
    env_files: Vec<String>,
    /// Where a file came from, for its errors: COMPOSE_FILE or the discovery.
    origin: BTreeMap<String, &'static str>,
    cwd: String,
}

impl State {
    /// NewProjectOptions with WithWorkingDirectory and WithOsEnv.
    fn new(flags: &Flags, os_env: &Env, cwd: &str) -> State {
        State {
            working_dir: if flags.project_directory.is_empty() {
                String::new()
            } else {
                paths::abs(&flags.project_directory, cwd)
            },
            config_paths: flags.files.clone(),
            env: os_env.clone(),
            env_files: Vec::new(),
            origin: BTreeMap::new(),
            cwd: cwd.to_string(),
        }
    }

    /// GetWorkingDir.
    fn working_dir(&self) -> String {
        if !self.working_dir.is_empty() {
            return self.working_dir.clone();
        }
        for path in &self.config_paths {
            if path != "-" {
                return paths::dir(&paths::abs(path, &self.cwd));
            }
        }
        self.cwd.clone()
    }

    /// WithEnvFiles: the files given, else the `.env` of the working directory.
    /// COMPOSE_DISABLE_ENV_FILE is read from the process's environment.
    fn with_env_files(&mut self, given: &[String], os_env: &Env, warn: Warn) -> Result<(), String> {
        if !given.is_empty() {
            self.env_files = given.to_vec();
            return Ok(());
        }
        if let Some(v) = os_env.get("COMPOSE_DISABLE_ENV_FILE") {
            match go_parse_bool(v) {
                Some(true) => return Ok(()),
                Some(false) => {}
                None => {
                    return Err(format!(
                        "strconv.ParseBool: parsing {}: invalid syntax",
                        go_quote(v)
                    ))
                }
            }
        }
        let default = paths::join(&[&self.working_dir(), ".env"]);
        match std::fs::metadata(&default) {
            Ok(meta) => {
                if !meta.is_dir() {
                    self.env_files = vec![default];
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => warn(format!(
                "cannot access {default}, ignoring default env file: {}",
                gostr::path_error("stat", &default, &e)
            )),
            Err(e) => return Err(gostr::path_error("stat", &default, &e)),
        }
        Ok(())
    }

    /// WithDotEnv: the env files' variables, the environment's own winning.
    fn with_dot_env(&mut self, warn: Warn) -> Result<(), String> {
        let from_files = loader::env_from_files(&self.env, &self.env_files, &self.cwd, warn)?;
        for (k, v) in from_files {
            self.env.entry(k).or_insert(v);
        }
        Ok(())
    }

    /// WithConfigFileEnv.
    fn with_config_file_env(&mut self) -> Result<(), String> {
        if !self.config_paths.is_empty() {
            return Ok(());
        }
        let Some(files) = self.env.get("COMPOSE_FILE").cloned() else {
            return Ok(());
        };
        let separator = self
            .env
            .get("COMPOSE_PATH_SEPARATOR")
            .filter(|s| !s.is_empty())
            .cloned()
            .unwrap_or_else(|| ":".to_string());
        const ORIGIN: &str = "set by COMPOSE_FILE environment variable";
        for entry in files.split(separator.as_str()) {
            let path = accept_file(entry, &self.cwd).map_err(|e| {
                format!("compose file {} {ORIGIN} is invalid: {e}", go_quote(entry))
            })?;
            self.origin.insert(path.clone(), ORIGIN);
            self.config_paths.push(path);
        }
        Ok(())
    }

    /// WithDefaultConfigPath: the first default name found from the working directory
    /// up, and an override next to it.
    fn with_default_config_path(&mut self, warn: Warn) {
        if !self.config_paths.is_empty() {
            return;
        }
        const ORIGIN: &str = "found by default file discovery";
        let mut dir = self.working_dir();
        loop {
            let candidates = find_files(DEFAULT_FILES, &dir);
            if let Some(winner) = candidates.first() {
                if candidates.len() > 1 {
                    warn(format!(
                        "Found multiple config files with supported names: {}",
                        candidates.join(", ")
                    ));
                    warn(format!("Using {winner}"));
                }
                self.config_paths.push(winner.clone());
                self.origin.insert(winner.clone(), ORIGIN);
                let overrides = find_files(DEFAULT_OVERRIDES, &dir);
                if let Some(first) = overrides.first() {
                    if overrides.len() > 1 {
                        warn(format!(
                            "Found multiple override files with supported names: {}",
                            overrides.join(", ")
                        ));
                        warn(format!("Using {first}"));
                    }
                    self.config_paths.push(first.clone());
                    self.origin.insert(first.clone(), ORIGIN);
                }
                return;
            }
            let parent = paths::dir(&dir);
            if parent == dir {
                return;
            }
            dir = parent;
        }
    }

    /// configFileError.
    fn file_error(&self, filename: &str, error: String) -> String {
        match self.origin.get(filename) {
            Some(origin) => format!(
                "compose file {} {origin} is invalid: {error}",
                go_quote(filename)
            ),
            None => format!("compose file {} is invalid: {error}", go_quote(filename)),
        }
    }
}

fn go_parse_bool(v: &str) -> Option<bool> {
    match v {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
        _ => None,
    }
}

/// acceptComposeFile: a file COMPOSE_FILE names, made absolute, existing and regular.
fn accept_file(path: &str, cwd: &str) -> Result<String, String> {
    if path == "-" {
        return Ok(path.to_string());
    }
    let abs = paths::abs(path, cwd);
    let meta = std::fs::metadata(&abs).map_err(|e| gostr::path_error("stat", &abs, &e))?;
    if meta.is_dir() {
        return Err(format!("{abs} is a directory"));
    }
    if !meta.is_file() {
        return Err(format!("{abs} is not a regular file"));
    }
    Ok(abs)
}

fn find_files(names: &[&str], dir: &str) -> Vec<String> {
    names
        .iter()
        .map(|n| paths::join(&[dir, n]))
        .filter(|f| std::fs::metadata(f).is_ok())
        .collect()
}

/// docker compose's setEnvWithDotEnv, before anything else: the COMPOSE_ variables of
/// the env files become the process's own, where it has none. It reads the env files as
/// they were given, before --workdir and the making of their paths absolute.
pub fn compose_env_from_files(
    flags: &Flags,
    env: &mut Env,
    cwd: &str,
    warn: Warn,
) -> Result<(), String> {
    let mut options = State::new(flags, env, cwd);
    options.with_env_files(&flags.env_files, env, warn)?;
    options.with_dot_env(warn)?;
    let from_files = loader::env_from_files(env, &options.env_files, cwd, warn)?;
    for (k, v) in from_files {
        if k.starts_with("COMPOSE_") && !env.contains_key(&k) {
            env.insert(k, v);
        }
    }
    Ok(())
}

/// docker compose's toProjectOptions, then ReadConfigFiles and the precedence of the
/// project's name.
pub fn prepare(
    flags: &Flags,
    os_env: &Env,
    cwd: &str,
    stdin: &dyn Fn() -> Vec<u8>,
    warn: Warn,
) -> Result<Prepared, String> {
    let mut state = State::new(flags, os_env, cwd);
    state
        .env
        .entry("PWD".to_string())
        .or_insert_with(|| cwd.to_string());
    state.with_env_files(&flags.env_files, os_env, warn)?;
    state.with_dot_env(warn)?;
    state.with_config_file_env()?;
    state.with_default_config_path(warn);
    state.with_env_files(&flags.env_files, os_env, warn)?;
    state.with_dot_env(warn)?;
    let profiles: Vec<String> = if flags.profiles.is_empty() {
        state
            .env
            .get("COMPOSE_PROFILES")
            .map(String::as_str)
            .unwrap_or("")
            .split(',')
            .map(|s| s.trim_matches(gostr::is_space).to_string())
            .collect()
    } else {
        flags.profiles.clone()
    };
    if !flags.project_name.is_empty()
        && loader::normalize_project_name(&flags.project_name) != flags.project_name
    {
        return Err(loader::invalid_project_name(&flags.project_name));
    }
    let working_dir = state.working_dir();
    if state.config_paths.is_empty() {
        return Err("no configuration file provided: not found".to_string());
    }
    // docker compose fetches every remote file before it reads any.
    if let Some(path) = state.config_paths.iter().find(|p| paths::remote(p)) {
        return Err(format!("--file {path}: {}", paths::REMOTE));
    }
    let mut files = Vec::new();
    for path in &state.config_paths {
        if path == "-" {
            files.push(ConfigFile {
                filename: "-".to_string(),
                content: Some(stdin()),
            });
            continue;
        }
        let abs = paths::abs(path, cwd);
        let content = match std::fs::read(&abs) {
            Ok(content) => content,
            Err(e) => {
                let reason = if std::fs::metadata(&abs).is_ok_and(|m| m.is_dir()) {
                    format!("{abs} is a directory")
                } else {
                    gostr::path_error("open", &abs, &e)
                };
                return Err(state.file_error(&abs, reason));
            }
        };
        files.push(ConfigFile {
            filename: abs,
            content: Some(content),
        });
    }
    // A file that names the project counts as -p; compose-go reads the first document
    // of each file into a struct with the name for that, which has errors of its own,
    // and stops at the first file with a name.
    let mut named_in_yaml = false;
    if flags.project_name.is_empty() {
        for file in &files {
            let content = file.content.as_deref().unwrap_or_default();
            let mut docs = super::yaml::Documents::new(content);
            let first = docs.next_document().map_err(|e| e.to_string())?;
            if let Some(doc) = first {
                if !super::yaml::decode_name(doc)?.is_empty() {
                    named_in_yaml = true;
                    break;
                }
            }
        }
    }
    let mut options = Options {
        cwd: cwd.to_string(),
        resolve_paths: true,
        profiles,
        home: os_env.get("HOME").filter(|h| !h.is_empty()).cloned(),
        ..Default::default()
    };
    if !flags.project_name.is_empty() {
        options.project_name.clone_from(&flags.project_name);
        options.project_name_explicit = true;
    } else if let Some(name) = state
        .env
        .get("COMPOSE_PROJECT_NAME")
        .filter(|n| !n.is_empty())
    {
        options.project_name.clone_from(name);
        options.project_name_explicit = true;
    } else if !named_in_yaml {
        let dirname = paths::base(&working_dir);
        if let Ok(real) = std::fs::canonicalize(&working_dir) {
            let real = real.to_string_lossy();
            if paths::base(&real) != dirname {
                warn(format!(
                    "project has been loaded without an explicit name from a symlink. Using name {}",
                    go_quote(&dirname)
                ));
            }
        }
        options.project_name = loader::normalize_project_name(&dirname);
    }
    Ok(Prepared {
        details: Details {
            working_dir: working_dir.clone(),
            base_dir: working_dir,
            files,
            env: state.env,
        },
        options,
    })
}
