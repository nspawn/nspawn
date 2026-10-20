//! `nspawn compose config`: the project parsed, resolved and written out in its
//! canonical form, as docker compose config writes it.

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
    /// The defaults docker compose gives flags from its environment as it starts,
    /// before the env files are read: --progress from COMPOSE_PROGRESS, --env-file from
    /// COMPOSE_ENV_FILES.
    pub progress: Option<String>,
    pub compose_env_files: Vec<String>,
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
        }
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
    args: &ComposeArgs,
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
    // --compatibility skips normalization, as config's PreRunE sets it.
    o.skip_normalization = config.no_normalize || args.compatibility;
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

/// ToProject: the project loaded, then what docker compose does after (postProcessProject):
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
    let mut prepared = prepared(
        args,
        flags,
        config,
        host,
        Load::Project { resolve_env },
        warn,
    )?;
    let model = loader::load_model(&mut prepared.details, &mut prepared.options, warn)?;
    let mut project = project::from_model(model, &prepared.options, &prepared.details.env, warn)?;
    project.files = prepared
        .details
        .files
        .iter()
        .map(|f| f.filename.clone())
        .collect();
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

/// ToModel: the model as loaded, without its environment.
fn load_raw(
    args: &ComposeArgs,
    flags: &Flags,
    config: &ConfigArgs,
    host: &Host,
    warn: Warn,
) -> Result<Map, String> {
    let mut prepared = prepared(args, flags, config, host, Load::Model, warn)?;
    loader::load_model(&mut prepared.details, &mut prepared.options, warn)
}

/// The separator of the names docker compose makes: `_` under --compatibility or
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

/// Runs `config` with the flags docker compose's checks left, writing to `out`; the
/// error is docker compose's message.
pub fn run(
    args: &ComposeArgs,
    flags: &Flags,
    config: &ConfigArgs,
    host: &Host,
    out: &mut dyn Write,
    warn: Warn,
) -> Result<(), String> {
    let format = config.format.clone().unwrap_or_default();
    let mut write = |text: &str| {
        out.write_all(&super::gostr::to_bytes(text))
            .map_err(|e| e.to_string())
    };
    if config.services {
        if config.no_interpolate {
            let model = load_raw(args, flags, config, host, warn)?;
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
        let mut prepared = prepared(args, flags, &raw_config, host, Load::Model, warn)?;
        prepared.options.skip_validation = true;
        let model = loader::load_model(&mut prepared.details, &mut prepared.options, warn)?;
        let variables = super::variables::extract(&Value::Map(model));
        let text = super::variables::render(&variables, &format)?;
        // YAML goes to the standard output -q took away; the rest to docker's own.
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
        let model = load_raw(args, flags, config, host, warn)?;
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
        if !config.no_env_resolution {
            project.resolve_services_environment(true, warn)?;
        }
        if !config.no_consistency {
            project.check_container_name_unicity()?;
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

/// The services, each after the ones it depends on, as ForEachService visits them
/// (alphabetically where docker compose follows the order of a Go map).
fn dependencies_first(project: &Project) -> Vec<String> {
    fn visit(name: &str, project: &Project, order: &mut Vec<String>) {
        if order.iter().any(|o| o == name) || !project.services.contains_key(name) {
            return;
        }
        let deps = project.services[name]
            .get(&["depends_on"])
            .and_then(Value::as_map)
            .map(|d| d.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        for dep in deps {
            visit(&dep, project, order);
        }
        order.push(name.to_string());
    }
    let mut order = Vec::new();
    for name in project.services.keys() {
        visit(name, project, &mut order);
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
