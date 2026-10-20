//! `nspawn compose config`: the project parsed, resolved and written out in its
//! canonical form.

use std::collections::HashSet;
use std::io::Write;

use sha2::{Digest, Sha256};

use super::cli::{ComposeArgs, ConfigArgs};
use super::dotenv::Env;
use super::emit::{self, Out};
use super::loader::{self, Warn};
use super::model::{self, Format, Ty};
use super::options::{self, Flags};
use super::project::{self, Project};
use super::types;
use super::value::{Map, Value};

/// What a run of config needs from the host: the process's environment and directory,
/// and standard input for `-f -`.
pub struct Host {
    pub env: Env,
    pub cwd: String,
    pub stdin: Stdin,
    /// The defaults flags take from the environment at the start, before the env
    /// files are read: --progress from COMPOSE_PROGRESS, --env-file from
    /// COMPOSE_ENV_FILES.
    pub progress: Option<String>,
    pub compose_env_files: Vec<String>,
    pub digests: Digests,
}

/// Where `--resolve-image-digests` learns the digest a reference names.
pub enum Digests {
    /// The registries, asked through nspawn's service with the credentials of `nspawn
    /// login`; the service is reached on the first question.
    Service(std::cell::RefCell<Option<Asker>>),
    /// Given answers: a reference without one is not on its registry.
    #[cfg(test)]
    Given(std::collections::HashMap<String, String>),
}

/// A connection to the service, and the runtime its calls run on.
pub struct Asker {
    runtime: tokio::runtime::Runtime,
    client: crate::client::Client,
}

impl Asker {
    fn connect() -> Result<Asker, String> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        let client = runtime
            .block_on(crate::client::Client::connect())
            .map_err(|e| format!("{e:#}"))?;
        Ok(Asker { runtime, client })
    }

    fn digest(&self, reference: &str) -> Result<String, String> {
        self.runtime
            .block_on(
                self.client
                    .manager
                    .image_digest(reference, crate::client::Options::new()),
            )
            .map_err(|e| crate::client::error(e).to_string())
    }
}

/// What `-f -` reads.
pub enum Stdin {
    /// The process's standard input, as it is the first time; empty after.
    Process,
    /// Given bytes, taken the first time.
    #[cfg(test)]
    Given(std::cell::RefCell<Vec<u8>>),
}

impl Host {
    pub fn new(env: Env, cwd: String, stdin: Stdin) -> Host {
        let progress = env.get("COMPOSE_PROGRESS").cloned();
        let compose_env_files = env
            .get("COMPOSE_ENV_FILES")
            .map(|v| {
                v.split(',')
                    .filter(|f| !f.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        Host {
            env,
            cwd,
            stdin,
            progress,
            compose_env_files,
            digests: Digests::Service(std::cell::RefCell::new(None)),
        }
    }

    /// ImageDigestResolver: the digest a reference, written in full, names on its
    /// registry now.
    fn digest(&self, reference: &str) -> Result<String, String> {
        let answer = match &self.digests {
            Digests::Service(asker) => {
                let mut asker = asker.borrow_mut();
                match &*asker {
                    Some(asker) => asker.digest(reference),
                    None => Asker::connect()
                        .and_then(|connected| asker.insert(connected).digest(reference)),
                }
            }
            #[cfg(test)]
            Digests::Given(answers) => answers
                .get(reference)
                .cloned()
                .ok_or_else(|| format!("{reference}: not found")),
        };
        answer.map_err(|e| format!("failed to resolve digest for {reference}: {e}"))
    }

    /// io.ReadAll(os.Stdin).
    fn read_stdin(&self) -> Result<Vec<u8>, String> {
        match &self.stdin {
            Stdin::Process => {
                let mut bytes = Vec::new();
                std::io::Read::read_to_end(&mut std::io::stdin(), &mut bytes)
                    .map_err(|e| super::gostr::path_error("read", "/dev/stdin", &e))?;
                Ok(bytes)
            }
            #[cfg(test)]
            Stdin::Given(bytes) => Ok(std::mem::take(&mut *bytes.borrow_mut())),
        }
    }
}

/// How a run loads: ToProject, with its environment, or ToModel, whose loader sees none
/// but the project's name.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Load {
    Project { resolve_env: bool },
    Model,
}

fn prepared(
    flags: &Flags,
    config: &ConfigArgs,
    host: &Host,
    load: Load,
    warn: Warn,
) -> Result<options::Prepared, String> {
    let stdin_error = std::cell::RefCell::new(None);
    let stdin = || {
        host.read_stdin().unwrap_or_else(|e| {
            *stdin_error.borrow_mut() = Some(e);
            Vec::new()
        })
    };
    let prepared = options::prepare(flags, &host.env, &host.cwd, &stdin, warn);
    if let Some(e) = stdin_error.into_inner() {
        return Err(e);
    }
    let mut prepared = prepared?;
    let o = &mut prepared.options;
    o.skip_interpolation = config.no_interpolate;
    o.resolve_paths = !config.no_path_resolution;
    o.skip_normalization = config.no_normalize;
    o.skip_consistency = config.no_consistency;
    o.discard_env_files = true;
    o.skip_resolve_environment = match load {
        Load::Project { resolve_env } => !resolve_env || config.no_env_resolution,
        Load::Model => true,
    };
    if load == Load::Model {
        prepared.details.env = Env::new();
    }
    Ok(prepared)
}

/// ToProject: the project loaded, then postProcessProject:
/// the services asked for and their dependencies selected, and the resources no service
/// uses left out without --all-resources.
fn load_project(
    args: &ComposeArgs,
    flags: &Flags,
    config: &ConfigArgs,
    host: &Host,
    services: &[String],
    resolve_env: bool,
    warn: Warn,
) -> Result<Project, String> {
    let mut prepared = prepared(flags, config, host, Load::Project { resolve_env }, warn)?;
    let model = loader::load_model(&mut prepared.details, &mut prepared.options, warn)?;
    let mut project = project::from_model(model, &prepared.options, &prepared.details.env, warn)?;
    // What nspawn would not run, said and shown all the same by config; the commands
    // that make machines refuse it.
    if args
        .command
        .as_ref()
        .is_some_and(|c| matches!(c, super::cli::ComposeCommand::Config(_)))
    {
        for refusal in &project.refused {
            warn(refusal.line.clone());
        }
    }
    project.files = prepared
        .details
        .files
        .iter()
        .map(|f| f.filename.clone())
        .collect();
    project
        .working_dir
        .clone_from(&prepared.details.working_dir);
    if project.name.is_empty() {
        return Err(
            "project name can't be empty. Use ProjectName option to set a valid name".to_string(),
        );
    }
    project = project.with_services_enabled(services);
    project = project.with_selected_services(services, false)?;
    if !args.all_resources {
        project.without_unnecessary_resources();
    }
    Ok(project)
}

/// The project of a command that drives machines, as WithServices loads it: the services named and their dependencies, the environment of each
/// service resolved, its env files folded in.
pub fn project_for(
    args: &ComposeArgs,
    flags: &Flags,
    host: &Host,
    services: &[String],
    warn: Warn,
) -> Result<Project, String> {
    let mut project = load_project(
        args,
        flags,
        &ConfigArgs::default(),
        host,
        services,
        false,
        warn,
    )?;
    // Only the commands that make machines read the services' env files: down, ps,
    // pull and the rest work without them, as an env file may be gone by then.
    use super::cli::ComposeCommand as C;
    if args
        .command
        .as_ref()
        .is_some_and(|c| matches!(c, C::Up(_) | C::Create(_) | C::Run(_) | C::Scale(_)))
    {
        project.resolve_services_environment(true, warn)?;
    }
    Ok(project)
}

/// ToModel: the model as loaded, without its environment.
fn load_raw(flags: &Flags, config: &ConfigArgs, host: &Host, warn: Warn) -> Result<Map, String> {
    let mut prepared = prepared(flags, config, host, Load::Model, warn)?;
    let model = loader::load_model(&mut prepared.details, &mut prepared.options, warn)?;
    for refusal in prepared.options.refused.borrow().iter() {
        warn(refusal.line.clone());
    }
    Ok(model)
}

/// The separator of the names a project makes: `_` under --compatibility or
/// COMPOSE_COMPATIBILITY, `-` otherwise.
fn separator(args: &ComposeArgs, project: &Project) -> &'static str {
    let compatibility = project
        .environment
        .get("COMPOSE_COMPATIBILITY")
        .is_some_and(|v| string_to_bool(v));
    if args.compatibility || compatibility {
        "_"
    } else {
        "-"
    }
}

/// compose-go's utils.StringToBool: ParseBool of the lowered, trimmed text, false
/// when it is none.
fn string_to_bool(text: &str) -> bool {
    let text = super::gostr::to_lower_str(text.trim_matches(super::gostr::is_space));
    matches!(text.as_str(), "1" | "t" | "true")
}

/// applyPlatforms for one platform, as --hash applies it: DOCKER_DEFAULT_PLATFORM is the
/// platform of a service that builds and names none, and a service that builds for a
/// platform builds for that one.
fn apply_platforms(project: &mut Project) -> Result<(), String> {
    let default = project
        .environment
        .get("DOCKER_DEFAULT_PLATFORM")
        .cloned()
        .unwrap_or_default();
    for (name, service) in project.services.iter_mut() {
        let Some(map) = service.as_map_mut() else {
            continue;
        };
        let Some(Value::Map(build)) = map.get("build").cloned() else {
            continue;
        };
        let platforms: Vec<String> = build
            .get("platforms")
            .and_then(Value::as_seq)
            .map(|p| p.iter().map(|v| v.to_string()).collect())
            .unwrap_or_default();
        let mut platform = map
            .get("platform")
            .map(|v| v.to_string())
            .unwrap_or_default();
        if !default.is_empty() && platform.is_empty() {
            if !platforms.is_empty() && !platforms.contains(&default) {
                return Err(format!(
                    "service {} build.platforms does not support value set by DOCKER_DEFAULT_PLATFORM: {default}",
                    super::golib::go_quote(name)
                ));
            }
            platform.clone_from(&default);
            map.insert("platform".to_string(), Value::str(&default));
        }
        let mut build = build;
        if !platform.is_empty() {
            if !platforms.is_empty() && !platforms.contains(&platform) {
                return Err(format!(
                    "service {} build configuration does not support platform: {platform}",
                    super::golib::go_quote(name)
                ));
            }
            build.insert(
                "platforms".to_string(),
                Value::Seq(vec![Value::str(&platform)]),
            );
        } else if platforms.len() > 1 {
            // Several: the builder decides.
            build.remove("platforms");
        }
        map.insert("build".to_string(), Value::Map(build));
    }
    Ok(())
}

/// Runs `config` with the flags the checks before it left, writing to `out`; the error
/// is the message to print.
pub fn run(
    args: &ComposeArgs,
    flags: &Flags,
    config: &ConfigArgs,
    host: &Host,
    out: &mut dyn Write,
    warn: Warn,
) -> Result<(), String> {
    // --compatibility skips normalization for config alone, as its PreRunE sets it.
    let config = &ConfigArgs {
        no_normalize: config.no_normalize || args.compatibility,
        ..config.clone()
    };
    let format = config.format.clone().unwrap_or_default();
    let mut write = |text: &str| {
        out.write_all(&super::gostr::to_bytes(text))
            .map_err(|e| e.to_string())
    };
    if config.services {
        if config.no_interpolate {
            let model = load_raw(flags, config, host, warn)?;
            if let Some(Value::Map(services)) = model.get("services") {
                for name in services.keys() {
                    write(&format!("{name}\n"))?;
                }
            }
            return Ok(());
        }
        let project = load_project(args, flags, config, host, &[], false, warn)?;
        for name in dependencies_first(&project) {
            write(&format!("{name}\n"))?;
        }
        return Ok(());
    }
    if config.volumes || config.networks || config.models {
        let project = load_project(args, flags, config, host, &[], false, warn)?;
        let names: Vec<String> = if config.volumes {
            project.volumes.keys().cloned().collect()
        } else if config.networks {
            project.networks.keys().cloned().collect()
        } else {
            project
                .models
                .values()
                .filter_map(|m| m.get(&["model"]).map(|v| v.to_string()))
                .filter(|m| !m.is_empty())
                .collect()
        };
        for name in names {
            write(&format!("{name}\n"))?;
        }
        return Ok(());
    }
    if let Some(hash) = config.hash.as_deref().filter(|h| !h.is_empty()) {
        let mut project = load_project(args, flags, config, host, &[], false, warn)?;
        let wanted: Vec<String> = if hash == "*" {
            Vec::new()
        } else {
            hash.split(',').map(String::from).collect()
        };
        project = project.with_selected_services(&wanted, true)?;
        apply_platforms(&mut project)?;
        if !config.no_env_resolution {
            project.resolve_services_environment(true, warn)?;
        }
        for (name, service) in &project.services {
            write(&format!("{name} {}\n", service_hash(service)?))?;
        }
        return Ok(());
    }
    if config.profiles {
        let project = load_project(args, flags, config, host, &config.service, true, warn)?;
        let mut profiles = std::collections::BTreeSet::new();
        for service in project
            .services
            .values()
            .chain(project.disabled_services.values())
        {
            for p in service
                .get(&["profiles"])
                .and_then(Value::as_seq)
                .into_iter()
                .flatten()
            {
                profiles.insert(p.to_string());
            }
        }
        for p in profiles {
            write(&format!("{p}\n"))?;
        }
        return Ok(());
    }
    if config.images {
        let project = load_project(args, flags, config, host, &config.service, true, warn)?;
        let separator = separator(args, &project);
        for (name, service) in project.services.iter().chain(project.jobs.iter()) {
            let image = service
                .get(&["image"])
                .map(|i| i.to_string())
                .unwrap_or_default();
            let image = if image.is_empty() {
                format!("{}{separator}{name}", project.name)
            } else {
                image
            };
            write(&format!("{image}\n"))?;
            // GetDependentImages: those of the pre_start hooks, the service's own aside.
            let hooks = service.get(&["pre_start"]).and_then(Value::as_seq);
            for hook in hooks.into_iter().flatten() {
                let hook = hook
                    .get(&["image"])
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !hook.is_empty() && hook != image {
                    write(&format!("{hook}\n"))?;
                }
            }
        }
        return Ok(());
    }
    if config.variables {
        if !config.service.is_empty() {
            warn("service filtering is not applied when --variables is set, variables from the full model will be rendered".to_string());
        }
        let raw_config = ConfigArgs {
            no_interpolate: true,
            no_normalize: config.no_normalize,
            no_path_resolution: config.no_path_resolution,
            no_consistency: config.no_consistency,
            no_env_resolution: config.no_env_resolution,
            ..Default::default()
        };
        let mut prepared = prepared(flags, &raw_config, host, Load::Model, warn)?;
        prepared.options.skip_validation = true;
        let model = loader::load_model(&mut prepared.details, &mut prepared.options, warn)?;
        for refusal in prepared.options.refused.borrow().iter() {
            warn(refusal.line.clone());
        }
        let variables = super::variables::extract(&Value::Map(model));
        let text = super::variables::render(&variables, &format)?;
        // YAML goes to the standard output -q took away; the rest to the output set up
        // before it.
        if config.quiet && format == "yaml" {
            return Ok(());
        }
        return write(&text);
    }
    if config.environment {
        let project = load_project(args, flags, config, host, &config.service, true, warn)?;
        if config.quiet {
            return Ok(());
        }
        let mut lines: Vec<String> = project
            .environment
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        lines.sort();
        for line in lines {
            write(&format!("{line}\n"))?;
        }
        return Ok(());
    }
    let format = if format.is_empty() {
        "yaml".to_string()
    } else {
        format
    };
    let content = if config.no_interpolate {
        if !config.service.is_empty() {
            warn("service filtering is not applied when --no-interpolate is set, the full model will be rendered".to_string());
        }
        let mut model = load_raw(flags, config, host, warn)?;
        if config.resolve_image_digests || config.lock_image_digests {
            let mut images = Images::new(host);
            for key in ["services", "jobs"] {
                if let Some(Value::Map(services)) = model.get_mut(key) {
                    images.resolve(services, key == "services")?;
                }
            }
        }
        if config.lock_image_digests {
            if let Some(Value::Map(services)) = model.get("services") {
                warn_hooks_not_lockable(services, warn);
            }
            lock_model(&mut model);
        }
        let out = model::any(&Value::Map(model));
        match format.as_str() {
            "json" => emit::json(&out)?,
            "yaml" => emit::yaml(&out),
            other => {
                return Err(format!(
                    "unsupported format {}",
                    super::golib::go_quote(other)
                ))
            }
        }
    } else {
        let mut project = load_project(args, flags, config, host, &config.service, true, warn)?;
        if config.resolve_image_digests || config.lock_image_digests {
            Images::new(host).resolve(&mut project.services, true)?;
        }
        if !config.no_env_resolution {
            project.resolve_services_environment(true, warn)?;
        }
        if !config.no_consistency {
            project.check_container_name_unicity()?;
        }
        if config.lock_image_digests {
            warn_hooks_not_lockable(&project.services, warn);
            images_only(&mut project);
        }
        let content = match format.as_str() {
            "json" => project.json()?,
            "yaml" => project.yaml(),
            other => {
                return Err(format!(
                    "unsupported format {}",
                    super::golib::go_quote(other)
                ))
            }
        };
        content.replace('$', "$$")
    };
    if config.quiet {
        return Ok(());
    }
    match config.output.as_deref().filter(|o| !o.is_empty()) {
        Some(path) if !content.is_empty() => std::fs::write(
            super::paths::abs(path, &host.cwd),
            super::gostr::to_bytes(&content),
        )
        .map_err(|e| super::gostr::path_error("open", path, &e)),
        _ => write(&content),
    }
}

/// WithImagesResolved over a project's services, or a model's services and jobs: each
/// image pinned to the digest its registry names now, or spelled out when it is pinned
/// already, an image asked about once.
struct Images<'a> {
    host: &'a Host,
    known: std::collections::HashMap<String, String>,
}

impl<'a> Images<'a> {
    fn new(host: &'a Host) -> Images<'a> {
        Images {
            host,
            known: std::collections::HashMap::new(),
        }
    }

    /// The image of each service, of its pre_start hooks when `hooks`, and the source
    /// of its volumes of type image.
    fn resolve(&mut self, services: &mut Map, hooks: bool) -> Result<(), String> {
        for service in services.values_mut() {
            let Some(service) = service.as_map_mut() else {
                continue;
            };
            if let Some(Value::Str(image)) = service.get_mut("image") {
                *image = self.pinned(image)?;
            }
            for (key, field) in [("pre_start", "image"), ("volumes", "source")] {
                if key == "pre_start" && !hooks {
                    continue;
                }
                let Some(Value::Seq(items)) = service.get_mut(key) else {
                    continue;
                };
                for item in items.iter_mut().filter_map(Value::as_map_mut) {
                    if key == "volumes" && item.get("type").and_then(Value::as_str) != Some("image")
                    {
                        continue;
                    }
                    if let Some(Value::Str(image)) = item.get_mut(field) {
                        *image = self.pinned(image)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// resolveImageDigest.
    fn pinned(&mut self, image: &str) -> Result<String, String> {
        if image.is_empty() {
            return Ok(String::new());
        }
        if let Some(pinned) = self.known.get(image) {
            return Ok(pinned.clone());
        }
        let mut named = super::reference::parse_docker_ref(image)?;
        if named.digest.is_empty() {
            let digest = self.host.digest(&named.to_string())?;
            named = super::reference::with_digest(named, &digest)?;
        }
        let pinned = named.to_string();
        self.known.insert(image.to_string(), pinned.clone());
        Ok(pinned)
    }
}

/// A pre_start hook runs an image of its own, which the file --lock-image-digests
/// writes cannot pin: hooks are appended when files are merged.
fn warn_hooks_not_lockable(services: &Map, warn: Warn) {
    for (name, service) in services {
        let hooks = service.get(&["pre_start"]).and_then(Value::as_seq);
        if hooks.into_iter().flatten().any(|hook| {
            hook.get(&["image"])
                .and_then(Value::as_str)
                .is_some_and(|i| !i.is_empty())
        }) {
            warn(format!("service {}: pre_start hook images are not pinned in the override file produced by --lock-image-digests, use the full --resolve-image-digests output to pin them", super::golib::go_quote(name)));
        }
    }
}

/// The volumes of type image of a service or a job, alone.
fn image_volumes(service: &Map) -> Vec<Value> {
    service
        .get("volumes")
        .and_then(Value::as_seq)
        .into_iter()
        .flatten()
        .filter(|v| v.get(&["type"]).and_then(Value::as_str) == Some("image"))
        .cloned()
        .collect()
}

/// imagesOnly: the services and jobs with their images and volumes of type image, and
/// nothing else of the project.
fn images_only(project: &mut Project) {
    let strip = |services: &Map| -> Map {
        services
            .iter()
            .map(|(name, service)| {
                let mut kept = Map::new();
                if let Some(service) = service.as_map() {
                    if let Some(image) = service.get("image") {
                        kept.insert("image".to_string(), image.clone());
                    }
                    let volumes = image_volumes(service);
                    if !volumes.is_empty() {
                        kept.insert("volumes".to_string(), Value::Seq(volumes));
                    }
                }
                (name.clone(), Value::Map(kept))
            })
            .collect()
    };
    project.services = strip(&project.services);
    project.jobs = strip(&project.jobs);
    project.name.clear();
    for map in [
        &mut project.networks,
        &mut project.volumes,
        &mut project.secrets,
        &mut project.configs,
        &mut project.models,
        &mut project.extensions,
    ] {
        map.clear();
    }
}

/// lockModel: the model's services and jobs, with their images and volumes of type
/// image, and nothing else.
fn lock_model(model: &mut Map) {
    model.retain(|key, _| key == "services" || key == "jobs");
    for services in model.values_mut().filter_map(Value::as_map_mut) {
        for service in services.values_mut().filter_map(Value::as_map_mut) {
            let volumes = image_volumes(service);
            service.retain(|key, _| key == "image");
            if !volumes.is_empty() {
                service.insert("volumes".to_string(), Value::Seq(volumes));
            }
        }
    }
}

/// The services, each after the ones it depends on, as ForEachService visits them
/// (alphabetically, where the order of a Go map is random).
fn dependencies_first(project: &Project) -> Vec<String> {
    // `seen` is marked before the dependencies: with --no-consistency a cycle reaches
    // here, and each service is still listed once.
    fn visit(name: &str, project: &Project, seen: &mut HashSet<String>, order: &mut Vec<String>) {
        if !project.services.contains_key(name) || !seen.insert(name.to_string()) {
            return;
        }
        let deps = project.services[name]
            .get(&["depends_on"])
            .and_then(Value::as_map)
            .map(|d| d.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        for dep in deps {
            visit(&dep, project, seen, order);
        }
        order.push(name.to_string());
    }
    let mut seen = HashSet::new();
    let mut order = Vec::new();
    for name in project.services.keys() {
        visit(name, project, &mut seen, &mut order);
    }
    order
}

/// compose's ServiceHash: the service as compact JSON without what does not make its
/// machine (build, pull policy, scale, replicas, dependencies, profiles), its first
/// keys in a fixed order, then SHA-256.
pub fn service_hash(service: &Value) -> Result<String, String> {
    let mut service = service.as_map().cloned().unwrap_or_default();
    for key in ["build", "pull_policy", "scale", "depends_on", "profiles"] {
        service.remove(key);
    }
    if let Some(Value::Map(deploy)) = service.get_mut("deploy") {
        deploy.remove("replicas");
    }
    let out = model::encode(
        Some(&Value::Map(service)),
        &Ty::Struct(&types::SERVICE),
        Format::Json,
    );
    let Out::Struct(fields) = out else {
        unreachable!("a struct encodes to one");
    };
    let mut rest: Vec<(String, Out)> = fields;
    let mut pinned = Vec::new();
    for key in HASH_ORDER {
        if let Some(at) = rest.iter().position(|(k, _)| k == key) {
            pinned.push(rest.remove(at));
        }
    }
    rest.sort_by(|a, b| a.0.cmp(&b.0));
    pinned.extend(rest);
    let text = emit::json_compact(&Out::Struct(pinned))?;
    Ok(hex::encode(Sha256::digest(text.as_bytes())))
}

const HASH_ORDER: &[&str] = &[
    "profiles",
    "annotations",
    "attach",
    "build",
    "develop",
    "blkio_config",
    "cap_add",
    "cap_drop",
    "cgroup_parent",
    "cgroup",
    "cpu_count",
    "cpu_percent",
    "cpu_period",
    "cpu_quota",
    "cpu_rt_period",
    "cpu_rt_runtime",
    "cpus",
    "cpuset",
    "cpu_shares",
    "command",
    "configs",
    "container_name",
    "credential_spec",
    "depends_on",
    "deploy",
    "device_cgroup_rules",
    "devices",
    "dns",
    "dns_opt",
    "dns_search",
    "dockerfile",
    "domainname",
    "entrypoint",
    "provider",
    "environment",
    "env_file",
    "expose",
    "extends",
    "external_links",
    "extra_hosts",
    "group_add",
    "gpus",
    "hostname",
    "healthcheck",
    "image",
    "init",
    "ipc",
    "isolation",
    "labels",
    "label_file",
    "links",
    "logging",
    "log_driver",
    "log_opt",
    "mem_limit",
    "mem_reservation",
    "memswap_limit",
    "mem_swappiness",
    "mac_address",
    "models",
    "net",
    "network_mode",
    "networks",
    "oom_kill_disable",
    "oom_score_adj",
    "pid",
    "pids_limit",
    "platform",
    "ports",
    "privileged",
    "pull_policy",
    "read_only",
    "restart",
    "runtime",
    "scale",
    "secrets",
    "security_opt",
    "shm_size",
    "stdin_open",
    "stop_grace_period",
    "stop_signal",
    "storage_opt",
    "sysctls",
    "tmpfs",
    "tty",
    "ulimits",
    "use_api_socket",
    "user",
    "userns_mode",
    "uts",
    "volume_driver",
    "volumes",
    "volumes_from",
    "working_dir",
    "pre_start",
    "post_start",
    "pre_stop",
    "pull_refresh_after",
];
