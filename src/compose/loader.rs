//! Loading a project as compose-go's loader does: each document of each file read,
//! interpolated, its includes brought in, its `!reset` and `!override` applied, its
//! services extended, merged over the files before it, kept unique, checked against
//! the schema and set in its long form; then defaults, the semantic checks, paths and
//! environment resolved, the support table consulted and the model normalized.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use super::aliases;
use super::canonical;
use super::dotenv::{self, Env};
use super::interpolate;
use super::merge;
use super::path::Path;
use super::paths::{self, Resolver};
use super::schema;
use super::support;
use super::unicity;
use super::value::{Map, Value};
use super::yaml::{self, Resets};

#[derive(Debug, Clone)]
pub struct ConfigFile {
    pub filename: String,
    /// The content, when it does not come from the file (standard input).
    pub content: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct Details {
    /// The directory relative paths of the model are resolved against; relative to the
    /// including project for an included one.
    pub working_dir: String,
    /// The directory the files this load names (include, extends) are read from.
    pub base_dir: String,
    pub files: Vec<ConfigFile>,
    pub env: Env,
}

/// What one load keeps from file to file and include to include, as compose-go keeps
/// it in its context: the models includes loaded, the services the `extends.file` of
/// the files being read loaded, and the files it warned of an obsolete `version` in
/// (once a process, there).
#[derive(Debug, Clone, Default)]
pub struct Caches {
    pub includes: Rc<RefCell<HashMap<IncludeKey, Map>>>,
    pub extends: Rc<RefCell<HashMap<(String, String), ExtendsBase>>>,
    pub versions: Rc<RefCell<std::collections::BTreeSet<String>>>,
}

/// The services of a file `extends.file` names, and the resets of that file.
pub type ExtendsBase = (Map, Resets);

/// What an included model depends on: its files, its directories, its environment.
pub type IncludeKey = (Vec<String>, String, String, Env);

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub caches: Caches,
    /// The process's working directory, which relative paths Go opens are relative to.
    pub cwd: String,
    pub skip_validation: bool,
    pub skip_interpolation: bool,
    pub skip_normalization: bool,
    pub resolve_paths: bool,
    pub skip_consistency: bool,
    pub skip_extends: bool,
    pub skip_include: bool,
    pub skip_resolve_environment: bool,
    pub skip_resolve_labels: bool,
    pub skip_default_values: bool,
    pub discard_env_files: bool,
    pub project_name: String,
    pub project_name_explicit: bool,
    pub profiles: Vec<String>,
    pub selected_services: Vec<String>,
    pub prune: bool,
    /// The home `~` stands for, the HOME of the process.
    pub home: Option<String>,
    /// What the support table refuses in the model, found as it loads.
    pub refused: Rc<RefCell<Vec<Refusal>>>,
}

/// Something of the model nspawn compose does not do: config says so, the commands
/// that make machines refuse it for the services they start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// The service or job it is in, "" for the project's own resources.
    pub service: String,
    /// Its path as the messages write it.
    pub path: String,
    /// The line to print.
    pub line: String,
}

/// The warnings a load gives, in the order they are printed.
pub type Warn<'a> = &'a mut dyn FnMut(String);

/// The project name the options and the files give: an explicit name as it is (it
/// must be normalized already), else the last `name:` of the files, interpolated and
/// normalized.
pub fn project_name(details: &mut Details, opts: &mut Options, warn: Warn) -> Result<(), String> {
    let result = (|| {
        if opts.project_name_explicit {
            if normalize_project_name(&opts.project_name) != opts.project_name {
                return Err(invalid_project_name(&opts.project_name));
            }
            return Ok(());
        }
        let mut from_files = String::new();
        for file in &details.files {
            let content = read_file(file, &opts.cwd)?;
            // Each document read into a struct with the name; a file that fails stops
            // there, its error left to the load.
            let mut docs = yaml::Documents::new(&content);
            while let Ok(Some(doc)) = docs.next_document() {
                match yaml::decode_name(doc) {
                    Ok(name) if !name.is_empty() => from_files = name,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        }
        if !opts.skip_interpolation {
            let env = details.env.clone();
            let lookup = move |k: &str| env.get(k).cloned();
            let model = Map::from([("name".to_string(), Value::Str(from_files))]);
            let interpolated = interpolate::interpolate(model, &lookup, warn)?;
            from_files = interpolated
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
        }
        if !opts.skip_normalization {
            from_files = normalize_project_name(&from_files);
        }
        if !from_files.is_empty() {
            opts.project_name = from_files;
        }
        Ok(())
    })();
    details.env.insert(
        "COMPOSE_PROJECT_NAME".to_string(),
        opts.project_name.clone(),
    );
    result
}

pub fn invalid_project_name(name: &str) -> String {
    format!(
        "invalid project name {}: must consist only of lowercase alphanumeric characters, hyphens, and underscores as well as start with a letter or number",
        super::golib::go_quote(name)
    )
}

pub fn normalize_project_name(name: &str) -> String {
    let lower = name.to_lowercase();
    let kept: String = lower
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_' || *c == '-')
        .collect();
    kept.trim_start_matches(['_', '-']).to_string()
}

/// os.ReadFile: a directory opens, and fails as it is read; a relative path is relative
/// to `cwd`.
fn read_file(file: &ConfigFile, cwd: &str) -> Result<Vec<u8>, String> {
    use std::io::Read;
    if let Some(content) = &file.content {
        return Ok(content.clone());
    }
    let path = &file.filename;
    let mut f = std::fs::File::open(paths::abs(path, cwd))
        .map_err(|e| super::gostr::path_error("open", path, &e))?;
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes)
        .map_err(|e| super::gostr::path_error("read", path, &e))?;
    Ok(bytes)
}

/// loadModelWithContext then load: the model of the project, normalized.
pub fn load_model(details: &mut Details, opts: &mut Options, warn: Warn) -> Result<Map, String> {
    if details.files.is_empty() {
        return Err("no compose file specified".to_string());
    }
    project_name(details, opts, warn)?;
    load(details, opts, &[], warn)
}

fn load(details: &Details, opts: &Options, loaded: &[String], warn: Warn) -> Result<Map, String> {
    let main = &details.files[0].filename;
    if loaded.contains(main) {
        let mut chain = loaded.to_vec();
        chain.push(main.clone());
        return Err(format!(
            "include cycle detected:\n{}\n include {}",
            chain[0],
            chain[1..].join("\n include ")
        ));
    }
    let mut model = load_yaml_model(details, opts, &mut Vec::new(), &[], warn)?;
    if model.is_empty() {
        return Err("empty compose file".to_string());
    }
    if !opts.skip_validation && opts.project_name.is_empty() {
        return Err("project name must not be empty".to_string());
    }
    let refused = check_support(&model, warn);
    opts.refused.borrow_mut().extend(refused);
    if !opts.skip_normalization {
        model.insert("name".to_string(), Value::Str(opts.project_name.clone()));
        model = super::normalize::normalize(model, &details.env)?;
    }
    Ok(model)
}

/// The services being extended, for the cycle check: (file, service).
pub type Tracker = Vec<(String, String)>;

pub fn load_yaml_model(
    details: &Details,
    opts: &Options,
    tracker: &mut Tracker,
    included: &[String],
    warn: Warn,
) -> Result<Map, String> {
    // The bases extends.file loads are known within this call: its environment and
    // interpolation hold for all its files.
    let mut opts = opts.clone();
    opts.caches.extends = Rc::default();
    let opts = &opts;
    let mut model = Map::new();
    for file in &details.files {
        // Each file starts from the chain of files that included the project.
        let mut chain = included.to_vec();
        model = load_yaml_file(file, opts, details, tracker, model, &mut chain, warn)?.0;
    }
    if !opts.skip_default_values {
        model = canonical::set_defaults(model)?;
    }
    if !opts.skip_validation {
        canonical::validate(&model)?;
    }
    let mut value = Value::Map(model);
    if opts.resolve_paths {
        Resolver {
            working_dir: &details.working_dir,
            home: opts.home.as_deref(),
        }
        .resolve(&mut value, warn)?;
    }
    let Value::Map(mut model) = value else {
        unreachable!("a mapping stays one");
    };
    resolve_environment(&mut model, &details.env);
    Ok(model)
}

/// What a file brought: the model merged so far, and the resets of its last document.
pub fn load_yaml_file(
    file: &ConfigFile,
    opts: &Options,
    details: &Details,
    tracker: &mut Tracker,
    mut model: Map,
    included: &mut Vec<String>,
    warn: Warn,
) -> Result<(Map, Resets), String> {
    let content = read_file(file, &opts.cwd)?;
    let mut docs = yaml::Documents::new(&content);
    let mut last = Resets::default();
    while let Some(doc) = docs
        .next_document()
        .map_err(|e| format!("failed to parse {}: {e}", file.filename))?
    {
        let (resets, raw) = yaml::read(doc).map_err(|e| match e {
            yaml::ReadError::Yaml(e) => format!("failed to parse {}: {e}", file.filename),
            yaml::ReadError::Key(e) => e,
        })?;
        model = process_raw(
            raw, &resets, model, file, opts, details, tracker, included, warn,
        )?;
        last = resets;
    }
    Ok((model, last))
}

#[allow(clippy::too_many_arguments)]
fn process_raw(
    raw: Value,
    resets: &Resets,
    mut model: Map,
    file: &ConfigFile,
    opts: &Options,
    details: &Details,
    tracker: &mut Tracker,
    included: &mut Vec<String>,
    warn: Warn,
) -> Result<Map, String> {
    let Value::Map(mut cfg) = raw else {
        return Err("top-level object must be a mapping".to_string());
    };
    if !opts.skip_interpolation {
        let env = details.env.clone();
        let lookup = move |k: &str| env.get(k).cloned();
        cfg = interpolate::interpolate(cfg, &lookup, warn)?;
    }
    let mut value = Value::Map(cfg);
    aliases::promote(&mut value, &Path::root())?;
    let Value::Map(mut cfg) = value else {
        unreachable!("a mapping stays one");
    };
    if !opts.skip_include {
        included.push(file.filename.clone());
        super::include::apply(&mut cfg, details, opts, included, resets, warn)?;
    }
    let mut current = Value::Map(model);
    yaml::apply_resets(resets, &mut current, &Path::root());
    if !opts.skip_extends {
        super::extends::apply(
            &mut cfg,
            &file.filename,
            opts,
            details,
            tracker,
            resets,
            warn,
        )?;
    }
    let merged = merge::merge(current, Value::Map(cfg))?;
    let merged = unicity::enforce(merged)?;
    let Value::Map(mut merged) = merged else {
        unreachable!("a mapping stays one");
    };
    if !opts.skip_validation {
        schema::validate(&Value::Map(merged.clone()))
            .map_err(|e| format!("validating {}: {e}", file.filename))?;
        if merged.remove("version").is_some()
            && opts
                .caches
                .versions
                .borrow_mut()
                .insert(file.filename.clone())
        {
            warn(format!(
                "{}: the attribute `version` is obsolete, it will be ignored, please remove it to avoid potential confusion",
                file.filename
            ));
        }
    }
    model = canonical::canonical(merged, opts.skip_interpolation, warn)?;
    model = canonical::omit_empty(model);
    match unicity::enforce(Value::Map(model))? {
        Value::Map(m) => Ok(m),
        _ => unreachable!("a mapping stays one"),
    }
}

/// ResolveEnvironment: a bare variable of a list environment takes the project's
/// value; a secret or config from the environment takes its content.
fn resolve_environment(model: &mut Map, env: &Env) {
    for key in ["services", "jobs"] {
        let Some(Value::Map(containers)) = model.get_mut(key) else {
            continue;
        };
        for container in containers.values_mut() {
            let Some(Value::Seq(list)) = container
                .as_map_mut()
                .and_then(|c| c.get_mut("environment"))
            else {
                continue;
            };
            let resolved: Vec<Value> = list
                .iter()
                .filter_map(|e| e.as_str())
                .map(|name| match env.get(name) {
                    Some(found) => Value::Str(format!("{name}={found}")),
                    None => Value::str(name),
                })
                .collect();
            *list = resolved;
        }
    }
    if let Some(Value::Map(secrets)) = model.get_mut("secrets") {
        for secret in secrets.values_mut() {
            let Some(map) = secret.as_map_mut() else {
                continue;
            };
            let Some(Value::Str(var)) = map.get("environment").cloned() else {
                continue;
            };
            if let Some(found) = env.get(&var) {
                map.insert("x-#value".to_string(), Value::Str(found.clone()));
            }
        }
    }
    if let Some(Value::Map(configs)) = model.get_mut("configs") {
        for config in configs.values_mut() {
            let Some(map) = config.as_map_mut() else {
                continue;
            };
            let Some(Value::Str(var)) = map.get("environment").cloned() else {
                continue;
            };
            if let Some(found) = env.get(&var) {
                map.insert("content".to_string(), Value::Str(found.clone()));
            }
        }
    }
}

/// The support table over the merged model: an ignored attribute or value is a
/// warning, a refused one comes back. Paths are written as those of the attributes
/// left to Swarm are.
fn check_support(model: &Map, warn: Warn) -> Vec<Refusal> {
    let mut refused = Vec::new();
    let mut ignored = Vec::new();
    walk_support(
        &Value::Map(model.clone()),
        "",
        &mut Vec::new(),
        &mut refused,
        &mut ignored,
    );
    // By service, then by path.
    ignored.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    for finding in ignored {
        warn(finding.line());
    }
    refused.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    refused
        .iter()
        .map(|f| Refusal {
            service: f.0.clone(),
            path: f.1.clone(),
            line: f.line(),
        })
        .collect()
}

/// An attribute the table speaks of: its service, its path as the messages write it,
/// and the table's text.
struct Finding(String, String, &'static str);

impl Finding {
    /// api.UnsupportedAttribute's String.
    fn line(&self) -> String {
        if self.0.is_empty() {
            format!("{}: {}", self.1, self.2)
        } else {
            format!(
                "service {}: {}: {}",
                super::golib::go_quote(&self.0),
                self.1,
                self.2
            )
        }
    }
}

/// The paths of the schema, to tell a key of the file's own (`*`) from an attribute.
fn schema_paths() -> &'static std::collections::BTreeSet<String> {
    static PATHS: std::sync::OnceLock<std::collections::BTreeSet<String>> =
        std::sync::OnceLock::new();
    PATHS.get_or_init(|| {
        include_str!("upstream/compose-go/attribute-paths.txt")
            .lines()
            .map(String::from)
            .collect()
    })
}

/// A step of a concrete path, as written in a message: a key, or an item of a list,
/// which some messages name by what it holds.
#[derive(Clone)]
enum Step {
    Key(String),
    Item(Value),
}

fn walk_support(
    value: &Value,
    pattern: &str,
    concrete: &mut Vec<Step>,
    refused: &mut Vec<Finding>,
    ignored: &mut Vec<Finding>,
) {
    if !pattern.is_empty() {
        if let Some(attribute) = support::attribute(pattern) {
            let rule = match value {
                Value::Str(_) | Value::Bool(_) | Value::Int(_) | Value::Float(_) => {
                    support::value(pattern, &value.to_string())
                }
                _ => Some(attribute.support),
            };
            match rule {
                Some(support::Support::Refused(text)) => {
                    refused.push(finding(concrete, pattern, text));
                    return;
                }
                Some(support::Support::Ignored(text)) => {
                    ignored.push(finding(concrete, pattern, text));
                    return;
                }
                _ if attribute.below && attribute.path != pattern => return,
                _ => {}
            }
        }
    }
    match value {
        Value::Map(map) => {
            for (key, entry) in map {
                if key.starts_with("x-") || key == super::model::EXTENSIONS {
                    continue;
                }
                let literal = if pattern.is_empty() {
                    key.clone()
                } else {
                    format!("{pattern}.{key}")
                };
                let next = if schema_paths().contains(&literal) {
                    literal
                } else if pattern.is_empty() {
                    key.clone()
                } else {
                    format!("{pattern}.*")
                };
                concrete.push(Step::Key(key.clone()));
                walk_support(entry, &next, concrete, refused, ignored);
                concrete.pop();
            }
        }
        Value::Seq(items) => {
            let next = format!("{pattern}.[]");
            for item in items {
                concrete.push(Step::Item(item.clone()));
                walk_support(item, &next, concrete, refused, ignored);
                concrete.pop();
            }
        }
        _ => {}
    }
}

/// What is reported of an attribute: the service it is in, and its path
/// in the service, list items written `[]`, but those its Swarm checks name by what
/// they hold: a port by its target and protocol, a volume by its source, a secret or
/// a config by its source.
fn finding(concrete: &[Step], pattern: &str, text: &'static str) -> Finding {
    let mut steps = concrete.to_vec();
    let service = match steps.as_slice() {
        [Step::Key(kind), Step::Key(name), ..] if kind == "services" || kind == "jobs" => {
            let name = name.clone();
            steps.drain(..2);
            name
        }
        _ => String::new(),
    };
    let field = |item: &Value, key: &str| match item.get(&[key]) {
        Some(v) => v.to_string(),
        None => "<nil>".to_string(),
    };
    let named = match (pattern, steps.as_slice()) {
        ("services.*.ports.[].mode", [Step::Key(_), Step::Item(port), ..]) => Some(format!(
            "ports[{}/{}].mode",
            field(port, "target"),
            field(port, "protocol")
        )),
        ("services.*.volumes.[].type", [Step::Key(_), Step::Item(volume), ..]) => {
            Some(format!("volumes[{}].type", field(volume, "source")))
        }
        (_, [Step::Key(kind), Step::Item(reference), Step::Key(attribute)])
            if (kind == "secrets" || kind == "configs")
                && matches!(attribute.as_str(), "uid" | "gid" | "mode") =>
        {
            let source = match reference.get(&["source"]) {
                Some(Value::Str(s)) if !s.is_empty() => s.clone(),
                _ => "(anonymous)".to_string(),
            };
            Some(format!("{kind}.{source}.{attribute}"))
        }
        _ => None,
    };
    let path = named.unwrap_or_else(|| {
        steps
            .iter()
            .map(|step| match step {
                Step::Key(k) => k.as_str(),
                Step::Item(_) => "[]",
            })
            .collect::<Vec<_>>()
            .join(".")
    });
    Finding(service, path, text)
}

/// The variables of the environment a model reads, for the include of its files.
pub fn env_with(base: &Env, extra: &Env) -> Env {
    let mut env: BTreeMap<String, String> = base.clone();
    for (k, v) in extra {
        env.entry(k.clone()).or_insert_with(|| v.clone());
    }
    env
}

/// dotenv.GetEnvFromFile: the variables of env files, each read with the environment
/// and the variables before it.
pub fn env_from_files(
    current: &Env,
    files: &[String],
    cwd: &str,
    warn: Warn,
) -> Result<Env, String> {
    let mut env = Env::new();
    for file in files {
        let file = paths::abs(file, cwd);
        let meta = match std::fs::metadata(&file) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(format!("couldn't find env file: {file}"))
            }
            Err(e) => return Err(super::gostr::path_error("stat", &file, &e)),
        };
        if meta.is_dir() {
            return Err(format!("{file} is a directory"));
        }
        let text = match std::fs::read(&file) {
            Ok(bytes) => super::gostr::from_bytes(&bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(format!("couldn't read env file: {file}"))
            }
            Err(e) => return Err(super::gostr::path_error("open", &file, &e)),
        };
        let lookup = |k: &str| current.get(k).cloned();
        let mut unset = |name: &str| {
            warn(format!(
                "The {} variable is not set. Defaulting to a blank string.",
                super::golib::go_quote(name)
            ))
        };
        dotenv::parse(&text, &mut env, &lookup, &mut unset)
            .map_err(|e| format!("failed to read {file}: {e}"))?;
    }
    Ok(env)
}
