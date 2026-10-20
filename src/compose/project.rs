//! The project as compose-go's ModelToProject builds it from the normalized model:
//! extensions set apart, the model bound to the types, profiles applied, consistency
//! checked, the services selected, their env and label files read. It writes itself
//! out as `config` does.

use std::collections::{BTreeMap, BTreeSet};

use super::dotenv::{self, Env};
use super::emit::{self, Out};
use super::golib::go_quote;
use super::gostr;
use super::loader::{Options, Refusal, Warn};
use super::model::{self, Format, Ty, EXTENSIONS};
use super::paths;
use super::types;
use super::value::{Map, Value};

#[derive(Debug, Clone, Default)]
pub struct Project {
    pub name: String,
    pub environment: Env,
    /// Each service, decoded.
    pub services: Map,
    pub disabled_services: Map,
    pub jobs: Map,
    pub disabled_jobs: Map,
    pub networks: Map,
    pub volumes: Map,
    pub secrets: Map,
    pub configs: Map,
    pub models: Map,
    pub extensions: Map,
    pub profiles: Vec<String>,
    pub files: Vec<String>,
    /// The process's directory, which a path the model leaves relative is relative to.
    pub cwd: String,
    /// The project's working directory: --project-directory, or the first file's.
    pub working_dir: String,
    /// What nspawn compose does not do in the model, the services' and the project's.
    pub refused: Vec<Refusal>,
}

fn invalid(message: String) -> String {
    format!("{message}: invalid compose project")
}

/// The keys under which the names are the file's own, where `x-` starts a name.
fn user_defined(path: &[String]) -> bool {
    matches!(
        path.iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .as_slice(),
        ["services"]
            | ["services", _, "depends_on"]
            | ["volumes"]
            | ["networks"]
            | ["secrets"]
            | ["configs"]
    )
}

/// processExtensions: the `x-*` keys of every mapping moved under EXTENSIONS.
fn process_extensions(value: &mut Value, path: &mut Vec<String>) {
    match value {
        Value::Map(map) => {
            let skip = user_defined(path);
            let mut extras = Map::new();
            if !skip {
                let keys: Vec<String> = map
                    .keys()
                    .filter(|k| k.starts_with("x-"))
                    .cloned()
                    .collect();
                for key in keys {
                    extras.insert(key.clone(), map.remove(&key).unwrap());
                }
            }
            for (key, entry) in map.iter_mut() {
                path.push(key.clone());
                process_extensions(entry, path);
                path.pop();
            }
            if !extras.is_empty() {
                map.insert(EXTENSIONS.to_string(), Value::Map(extras));
            }
        }
        Value::Seq(items) => {
            for (i, item) in items.iter_mut().enumerate() {
                if let Value::Map(_) = item {
                    path.push(i.to_string());
                    process_extensions(item, path);
                    path.pop();
                }
            }
        }
        _ => {}
    }
}

fn get<'a>(map: &'a Map, key: &str) -> Option<&'a Value> {
    map.get(key)
}

fn string(map: &Map, key: &str) -> String {
    map.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn int(map: &Map, key: &str) -> Option<i64> {
    match map.get(key) {
        Some(Value::Int(i)) => Some(*i),
        _ => None,
    }
}

fn sub<'a>(map: &'a Map, key: &str) -> Option<&'a Map> {
    map.get(key).and_then(Value::as_map)
}

/// ModelToProject.
pub fn from_model(
    mut model: Map,
    opts: &Options,
    environment: &Env,
    warn: Warn,
) -> Result<Project, String> {
    model.remove("name");
    let mut value = Value::Map(model);
    process_extensions(&mut value, &mut Vec::new());
    let Value::Map(mut model) = value else {
        unreachable!("a mapping stays one");
    };
    // nameServices: each service and job knows its name.
    for key in ["services", "jobs"] {
        if let Some(Value::Map(entries)) = model.get_mut(key) {
            for (name, entry) in entries.iter_mut() {
                if let Value::Map(e) = entry {
                    e.insert("name".to_string(), Value::str(name));
                }
            }
        }
    }
    let decoded = model::decode(&Value::Map(model), &Ty::Struct(&types::PROJECT), "", warn)
        .map_err(|e| e.message())?;
    let Value::Map(mut decoded) = decoded else {
        unreachable!("a struct decodes to a mapping");
    };
    let mut take = |key: &str| match decoded.remove(key) {
        Some(Value::Map(m)) => m,
        _ => Map::new(),
    };
    let mut project = Project {
        name: opts.project_name.clone(),
        environment: environment.clone(),
        services: take("services"),
        jobs: take("jobs"),
        networks: take("networks"),
        volumes: take("volumes"),
        secrets: take("secrets"),
        configs: take("configs"),
        models: take("models"),
        extensions: take(EXTENSIONS),
        cwd: opts.cwd.clone(),
        refused: opts.refused.borrow().clone(),
        ..Default::default()
    };
    // secretConfigDecoderHook: a secret from the environment carries its content.
    for secret in project.secrets.values_mut() {
        let Some(map) = secret.as_map_mut() else {
            continue;
        };
        let Some(Value::Map(ext)) = map.get_mut(EXTENSIONS) else {
            continue;
        };
        if let Some(Value::Str(content)) = ext.remove("x-#value") {
            let empty = ext.is_empty();
            if empty {
                map.remove(EXTENSIONS);
            }
            map.insert("content".to_string(), Value::Str(content));
        }
    }
    project = project.with_profiles(&opts.profiles);
    if !opts.skip_consistency {
        project.check_consistency()?;
    }
    if !opts.selected_services.is_empty() {
        project = project.with_services_enabled(&opts.selected_services);
        project = project.with_selected_services(&opts.selected_services, false)?;
    }
    if opts.prune {
        project.without_unnecessary_resources();
    }
    if !opts.skip_resolve_environment {
        project.resolve_services_environment(opts.discard_env_files, warn)?;
    }
    if !opts.skip_resolve_labels {
        project.resolve_services_labels(warn)?;
    }
    Ok(project)
}

fn profiles_of(service: &Value) -> Vec<String> {
    service
        .get(&["profiles"])
        .and_then(Value::as_seq)
        .map(|p| p.iter().map(|v| v.to_string()).collect())
        .unwrap_or_default()
}

fn has_profile(service: &Value, active: &[String]) -> bool {
    let declared = profiles_of(service);
    declared.is_empty() || active.iter().any(|p| p == "*" || declared.contains(p))
}

impl Project {
    pub fn with_profiles(mut self, profiles: &[String]) -> Project {
        let mut all = std::mem::take(&mut self.services);
        all.extend(std::mem::take(&mut self.disabled_services));
        for (name, service) in all {
            if has_profile(&service, profiles) {
                self.services.insert(name, service);
            } else {
                self.disabled_services.insert(name, service);
            }
        }
        if !self.jobs.is_empty() || !self.disabled_jobs.is_empty() {
            let mut all = std::mem::take(&mut self.jobs);
            all.extend(std::mem::take(&mut self.disabled_jobs));
            for (name, job) in all {
                if has_profile(&job, profiles) {
                    self.jobs.insert(name, job);
                } else {
                    self.disabled_jobs.insert(name, job);
                }
            }
        }
        self.profiles = profiles.to_vec();
        self
    }

    pub fn with_services_enabled(self, names: &[String]) -> Project {
        let mut profiles = self.profiles.clone();
        for name in names {
            if self.services.contains_key(name) {
                continue;
            }
            if let Some(service) = self.disabled_services.get(name) {
                profiles.extend(profiles_of(service));
            }
        }
        self.with_profiles(&profiles)
    }

    fn depends_on(service: &Value) -> Vec<(String, bool)> {
        service
            .get(&["depends_on"])
            .and_then(Value::as_map)
            .map(|deps| {
                deps.iter()
                    .map(|(name, d)| {
                        (
                            name.clone(),
                            d.get(&["required"]) == Some(&Value::Bool(true)),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// WithSelectedServices: the services named (shell patterns, as filepath.Match
    /// reads them) and, unless `ignore_dependencies`, their dependencies; the others
    /// disabled.
    pub fn with_selected_services(
        mut self,
        names: &[String],
        ignore_dependencies: bool,
    ) -> Result<Project, String> {
        if names.is_empty() {
            return Ok(self);
        }
        let mut selected = BTreeSet::new();
        self.visit(names, &BTreeMap::new(), ignore_dependencies, &mut selected)?;
        let names: Vec<String> = self.services.keys().cloned().collect();
        for name in names {
            if selected.contains(&name) {
                if let Some(Value::Map(deps)) = self
                    .services
                    .get_mut(&name)
                    .and_then(Value::as_map_mut)
                    .and_then(|s| s.get_mut("depends_on"))
                {
                    deps.retain(|d, _| selected.contains(d));
                }
            } else {
                self.disable(&name);
            }
        }
        Ok(self)
    }

    /// withServices: the services the patterns match, then theirs, depth first; a
    /// pattern that matches nothing is an error unless it is a dependency that is not
    /// required, and the first such pattern, in the order given, is the one reported.
    fn visit(
        &self,
        names: &[String],
        dependencies: &BTreeMap<String, bool>,
        ignore_dependencies: bool,
        seen: &mut BTreeSet<String>,
    ) -> Result<(), String> {
        let mut matched = BTreeSet::new();
        for pattern in names {
            let hits: Vec<&String> = self
                .services
                .keys()
                .filter(|s| super::golib::filepath_match(pattern, s) == Ok(true))
                .collect();
            if hits.is_empty() && dependencies.get(pattern).copied().unwrap_or(true) {
                return Err(format!("no such service: {pattern}"));
            }
            matched.extend(hits.into_iter().cloned());
        }
        for name in matched {
            if !seen.insert(name.clone()) || ignore_dependencies {
                continue;
            }
            let deps: BTreeMap<String, bool> = Self::depends_on(&self.services[&name])
                .into_iter()
                .collect();
            if !deps.is_empty() {
                let keys: Vec<String> = deps.keys().cloned().collect();
                self.visit(&keys, &deps, ignore_dependencies, seen)?;
            }
        }
        Ok(())
    }

    fn disable(&mut self, name: &str) {
        for service in self.services.values_mut() {
            if let Some(Value::Map(deps)) =
                service.as_map_mut().and_then(|s| s.get_mut("depends_on"))
            {
                deps.remove(name);
            }
        }
        if let Some(service) = self.services.remove(name) {
            self.disabled_services.insert(name.to_string(), service);
        }
    }

    pub fn without_unnecessary_resources(&mut self) {
        let mut networks = BTreeSet::new();
        let mut volumes = BTreeSet::new();
        let mut secrets = BTreeSet::new();
        let mut configs = BTreeSet::new();
        let mut models = BTreeSet::new();
        for service in self.services.values() {
            let Some(s) = service.as_map() else { continue };
            if let Some(n) = sub(s, "networks") {
                networks.extend(n.keys().cloned());
            }
            for v in s
                .get("volumes")
                .and_then(Value::as_seq)
                .into_iter()
                .flatten()
            {
                let Some(v) = v.as_map() else { continue };
                if string(v, "type") == "volume" && !string(v, "source").is_empty() {
                    volumes.insert(string(v, "source"));
                }
            }
            for v in s
                .get("secrets")
                .and_then(Value::as_seq)
                .into_iter()
                .flatten()
            {
                secrets.insert(
                    v.get(&["source"])
                        .map(|s| s.to_string())
                        .unwrap_or_default(),
                );
            }
            for v in s
                .get("build")
                .and_then(|b| b.get(&["secrets"]))
                .and_then(Value::as_seq)
                .into_iter()
                .flatten()
            {
                secrets.insert(
                    v.get(&["source"])
                        .map(|s| s.to_string())
                        .unwrap_or_default(),
                );
            }
            for v in s
                .get("configs")
                .and_then(Value::as_seq)
                .into_iter()
                .flatten()
            {
                configs.insert(
                    v.get(&["source"])
                        .map(|s| s.to_string())
                        .unwrap_or_default(),
                );
            }
            if let Some(m) = sub(s, "models") {
                models.extend(m.keys().cloned());
            }
        }
        self.networks.retain(|k, _| networks.contains(k));
        self.volumes.retain(|k, _| volumes.contains(k));
        self.secrets.retain(|k, _| secrets.contains(k));
        self.configs.retain(|k, _| configs.contains(k));
        self.models.retain(|k, _| models.contains(k));
    }

    /// compose-go's checkConsistency, with nspawn's own checks.
    pub fn check_consistency(&mut self) -> Result<(), String> {
        let names: Vec<String> = self.services.keys().cloned().collect();
        for name in &names {
            let s = self.services[name].as_map().cloned().unwrap_or_default();
            let quoted = go_quote(name);
            let build = sub(&s, "build");
            if build.is_none() && string(&s, "image").is_empty() && get(&s, "provider").is_none() {
                return Err(invalid(format!(
                    "service {quoted} has neither an image nor a build context specified"
                )));
            }
            if build.is_some() && string(&s, "image").is_empty() && get(&s, "provider").is_none() {
                self.refused.push(Refusal {
                    service: name.clone(),
                    path: "build".to_string(),
                    line: format!(
                        "service {quoted} has a build section and no image: nspawn runs images published on Docker Hub or any registry, but does not build Dockerfiles; building is not supported"
                    ),
                });
            }
            if let Some(build) = build {
                if !string(build, "dockerfile_inline").is_empty()
                    && !string(build, "dockerfile").is_empty()
                {
                    return Err(invalid(format!(
                        "service {quoted} declares mutualy exclusive dockerfile and dockerfile_inline"
                    )));
                }
                if let Some(contexts) = sub(build, "additional_contexts") {
                    for (add, c) in contexts {
                        let Some(target) = c.as_str().and_then(|c| c.strip_prefix("service:"))
                        else {
                            continue;
                        };
                        let t = self
                            .services
                            .get(target)
                            .or_else(|| self.disabled_services.get(target));
                        match t {
                            None => {
                                return Err(format!(
                                    "service {quoted} declares unknown service {} as additional contexts {add}",
                                    go_quote(target)
                                ))
                            }
                            Some(t) if t.get(&["build"]).is_none() => {
                                return Err(format!(
                                    "service {quoted} declares non-buildable service {} as additional contexts {add}",
                                    go_quote(target)
                                ))
                            }
                            _ => {}
                        }
                    }
                }
                let platforms: Vec<String> = build
                    .get("platforms")
                    .and_then(Value::as_seq)
                    .map(|p| p.iter().map(|v| v.to_string()).collect())
                    .unwrap_or_default();
                let platform = string(&s, "platform");
                if !platforms.is_empty() && !platform.is_empty() && !platforms.contains(&platform) {
                    return Err(invalid(format!(
                        "service.build.platforms MUST include service.platform {}",
                        go_quote(&platform)
                    )));
                }
            }
            let networks = sub(&s, "networks").cloned().unwrap_or_default();
            if !string(&s, "network_mode").is_empty() && !networks.is_empty() {
                return Err(invalid(format!(
                    "service {name} declares mutually exclusive `network_mode` and `networks`"
                )));
            }
            for network in networks.keys() {
                if !self.networks.contains_key(network) {
                    return Err(invalid(format!(
                        "service {quoted} refers to undefined network {network}"
                    )));
                }
            }
            if let Some(test) = s
                .get("healthcheck")
                .and_then(|h| h.get(&["test"]))
                .and_then(Value::as_seq)
            {
                if let Some(first) = test.first() {
                    if !matches!(first.to_string().as_str(), "CMD" | "CMD-SHELL" | "NONE") {
                        return Err(
                            "healthcheck.test must start either by \"CMD\", \"CMD-SHELL\" or \"NONE\"".to_string(),
                        );
                    }
                }
            }
            for (dep, required) in Self::depends_on(&Value::Map(s.clone())) {
                if self.services.contains_key(&dep) {
                    continue;
                }
                if self.disabled_services.contains_key(&dep) && !required {
                    continue;
                }
                if self.jobs.contains_key(&dep) {
                    return Err(invalid(format!(
                        "service {quoted} cannot depend on job {}: services can only depend on other services",
                        go_quote(&dep)
                    )));
                }
                return Err(invalid(format!(
                    "service {quoted} depends on undefined service {}",
                    go_quote(&dep)
                )));
            }
            if let Some(shared) = string(&s, "network_mode").strip_prefix("service:") {
                if !self.services.contains_key(shared) {
                    return Err(format!(
                        "service {} not found for network_mode 'service:{shared}'",
                        go_quote(shared)
                    ));
                }
            }
            for v in s
                .get("volumes")
                .and_then(Value::as_seq)
                .into_iter()
                .flatten()
            {
                let Some(v) = v.as_map() else { continue };
                let source = string(v, "source");
                if string(v, "type") == "volume"
                    && !source.is_empty()
                    && !self.volumes.contains_key(&source)
                {
                    return Err(invalid(format!(
                        "service {quoted} refers to undefined volume {source}"
                    )));
                }
            }
            for v in build
                .and_then(|b| b.get("secrets"))
                .and_then(Value::as_seq)
                .into_iter()
                .flatten()
            {
                let source = v
                    .get(&["source"])
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                if !self.secrets.contains_key(&source) {
                    return Err(invalid(format!(
                        "service {quoted} refers to undefined build secret {source}"
                    )));
                }
            }
            for v in s
                .get("configs")
                .and_then(Value::as_seq)
                .into_iter()
                .flatten()
            {
                let source = v
                    .get(&["source"])
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                if !self.configs.contains_key(&source) {
                    return Err(invalid(format!(
                        "service {quoted} refers to undefined config {source}"
                    )));
                }
            }
            for model in sub(&s, "models")
                .map(|m| m.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default()
            {
                if !self.models.contains_key(&model) {
                    return Err(invalid(format!(
                        "service {quoted} refers to undefined model {model}"
                    )));
                }
            }
            for v in s
                .get("secrets")
                .and_then(Value::as_seq)
                .into_iter()
                .flatten()
            {
                let source = v
                    .get(&["source"])
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                if !self.secrets.contains_key(&source) {
                    return Err(invalid(format!(
                        "service {quoted} refers to undefined secret {source}"
                    )));
                }
            }
            let scale = int(&s, "scale");
            let deploy = sub(&s, "deploy");
            let replicas = deploy.and_then(|d| int(d, "replicas"));
            if let (Some(scale), Some(_)) = (scale, deploy) {
                if replicas.is_some_and(|r| r != scale) {
                    return Err(invalid(format!(
                        "services.{name}: can't set distinct values on 'scale' and 'deploy.replicas'"
                    )));
                }
                // compose-go sets the replicas to the scale through the shared pointer.
                if let Some(Value::Map(d)) = self
                    .services
                    .get_mut(name)
                    .and_then(Value::as_map_mut)
                    .and_then(|s| s.get_mut("deploy"))
                {
                    d.insert("replicas".to_string(), Value::Int(scale));
                }
            }
            if scale.is_some_and(|s| s < 0) {
                return Err(format!(
                    "services.{name}.scale: must be greater than or equal to 0"
                ));
            }
            if replicas.is_some_and(|r| r < 0) {
                return Err(format!(
                    "services.{name}.deploy.replicas: must be greater than or equal to 0"
                ));
            }
            let limits = deploy
                .and_then(|d| sub(d, "resources"))
                .and_then(|r| sub(r, "limits"));
            let reservations = deploy
                .and_then(|d| sub(d, "resources"))
                .and_then(|r| sub(r, "reservations"));
            let float = |m: &Map, k: &str| match m.get(k) {
                Some(Value::Float(x)) => *x as f32,
                Some(Value::Int(i)) => *i as f32,
                _ => 0.0,
            };
            let cpus = float(&s, "cpus");
            if cpus != 0.0 && deploy.is_some() {
                if let Some(limits) = limits {
                    if float(limits, "cpus") != cpus {
                        return Err(invalid(format!(
                            "services.{name}: can't set distinct values on 'cpus' and 'deploy.resources.limits.cpus'"
                        )));
                    }
                }
            }
            for (own, limit, label) in [
                (
                    "mem_limit",
                    limits.map(|l| int(l, "memory")),
                    "'mem_limit' and 'deploy.resources.limits.memory'",
                ),
                (
                    "mem_reservation",
                    reservations.map(|l| int(l, "memory")),
                    "'mem_reservation' and 'deploy.resources.reservations.memory'",
                ),
                (
                    "pids_limit",
                    limits.map(|l| int(l, "pids")),
                    "'pids_limit' and 'deploy.resources.limits.pids'",
                ),
            ] {
                let mine = int(&s, own).unwrap_or(0);
                if mine != 0 && deploy.is_some() {
                    if let Some(theirs) = limit {
                        if theirs.unwrap_or(0) != mine {
                            return Err(invalid(format!(
                                "services.{name}: can't set distinct values on {label}"
                            )));
                        }
                    }
                }
            }
            let effective_scale = scale.or(replicas).unwrap_or(1);
            let container_name = string(&s, "container_name");
            if effective_scale > 1 && !container_name.is_empty() {
                let attribute = if scale.is_none() {
                    "deploy.replicas"
                } else {
                    "scale"
                };
                // compose-go swaps the two in its message.
                return Err(invalid(format!(
                    "services.{attribute}: can't set container_name and {name} as container name must be unique"
                )));
            }
            if let Some(watch) = s
                .get("develop")
                .and_then(|d| d.get(&["watch"]))
                .and_then(Value::as_seq)
            {
                for w in watch {
                    let Some(w) = w.as_map() else { continue };
                    let action = string(w, "action");
                    if string(w, "target").is_empty() && action != "rebuild" && action != "restart"
                    {
                        return Err(invalid(format!(
                            "services.{name}.develop.watch: target is required for non-rebuild actions"
                        )));
                    }
                }
            }
            let mut mounts: BTreeMap<String, String> = BTreeMap::new();
            for (i, tmpfs) in s
                .get("tmpfs")
                .and_then(Value::as_seq)
                .into_iter()
                .flatten()
                .enumerate()
            {
                let location = format!("services.{name}.tmpfs[{i}]");
                let tmpfs = tmpfs.to_string();
                let path = tmpfs
                    .split_once(':')
                    .map_or(tmpfs.as_str(), |(p, _)| p)
                    .to_string();
                if let Some(previous) = mounts.get(&path) {
                    return Err(format!(
                        "{location}: target {path} already mounted as {previous}"
                    ));
                }
                mounts.insert(path, location);
            }
            for (i, volume) in s
                .get("volumes")
                .and_then(Value::as_seq)
                .into_iter()
                .flatten()
                .enumerate()
            {
                let location = format!("services.{name}.volumes[{i}]");
                let target = volume
                    .get(&["target"])
                    .map(|t| t.to_string())
                    .unwrap_or_default();
                if let Some(previous) = mounts.get(&target) {
                    return Err(format!(
                        "{location}: target {target} already mounted as {previous}"
                    ));
                }
                mounts.insert(target, location);
            }
        }
        let mut jobs: Vec<String> = self.jobs.keys().cloned().collect();
        jobs.extend(self.disabled_jobs.keys().cloned());
        for name in &jobs {
            if self.services.contains_key(name) || self.disabled_services.contains_key(name) {
                return Err(invalid(format!(
                    "{} is declared both as a service and a job: service and job names must be unique",
                    go_quote(name)
                )));
            }
        }
        for (name, job) in &self.jobs {
            for (dep, required) in Self::depends_on(job) {
                if self.jobs.contains_key(&dep) || self.services.contains_key(&dep) {
                    continue;
                }
                if self.disabled_services.contains_key(&dep) && !required {
                    continue;
                }
                return Err(invalid(format!(
                    "job {} depends on undefined service or job {}",
                    go_quote(name),
                    go_quote(&dep)
                )));
            }
        }
        for (name, secret) in &self.secrets {
            let Some(s) = secret.as_map() else { continue };
            if s.get("external") == Some(&Value::Bool(true)) {
                continue;
            }
            if string(s, "file").is_empty() && string(s, "environment").is_empty() {
                return Err(invalid(format!(
                    "secret {} must declare either `file` or `environment`",
                    go_quote(name)
                )));
            }
        }
        self.check_cycle()
    }

    /// graph.CheckCycle over the services' dependencies.
    fn check_cycle(&mut self) -> Result<(), String> {
        let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let names: Vec<String> = self.services.keys().cloned().collect();
        for name in &names {
            let mut kids = Vec::new();
            for (dep, required) in Self::depends_on(&self.services[name]) {
                if self.services.contains_key(&dep) {
                    kids.push(dep);
                    continue;
                }
                if required {
                    if let Some(disabled) = self.disabled_services.get(&dep) {
                        return Err(format!(
                            "service {} is required by {} but is disabled. Can be enabled by profiles [{}]",
                            go_quote(&dep),
                            go_quote(name),
                            profiles_of(disabled).join(" ")
                        ));
                    }
                    return Err(format!(
                        "service {} depends on unknown service {}",
                        go_quote(name),
                        go_quote(&dep)
                    ));
                }
            }
            kids.sort();
            children.insert(name.clone(), kids);
        }
        fn search(
            path: &mut Vec<String>,
            node: &str,
            children: &BTreeMap<String, Vec<String>>,
        ) -> Result<(), String> {
            for child in &children[node] {
                if let Some(at) = path.iter().position(|p| p == child) {
                    return Err(format!(
                        "dependency cycle detected: {} -> {child}",
                        path[at..].join(" -> ")
                    ));
                }
                path.push(child.clone());
                search(path, child, children)?;
                path.pop();
            }
            Ok(())
        }
        for name in &names {
            search(&mut vec![name.clone()], name, &children)?;
        }
        Ok(())
    }

    /// WithServicesEnvironmentResolved: the environment of each service with its env
    /// files read, its own entries over theirs.
    pub fn resolve_services_environment(
        &mut self,
        discard: bool,
        warn: Warn,
    ) -> Result<(), String> {
        let project_env = self.environment.clone();
        let cwd = self.cwd.clone();
        for service in self.services.values_mut() {
            let Some(s) = service.as_map_mut() else {
                continue;
            };
            let mut own: Map = sub(s, "environment").cloned().unwrap_or_default();
            for (k, v) in own.iter_mut() {
                if v.is_null() {
                    if let Some(found) = project_env.get(k) {
                        *v = Value::Str(found.clone());
                    }
                }
            }
            let mut environment: Env = own
                .iter()
                .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                .collect();
            for file in s
                .get("env_file")
                .and_then(Value::as_seq)
                .cloned()
                .unwrap_or_default()
            {
                let Some(file) = file.as_map() else { continue };
                let path = string(file, "path");
                let required = file.get("required") != Some(&Value::Bool(false));
                let format = string(file, "format");
                // Only a file that is not there is missing; any other error comes from
                // reading it.
                if let Err(e) = std::fs::metadata(paths::abs(&path, &cwd)) {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        if required {
                            return Err(format!(
                                "env file {path} not found: {}",
                                gostr::path_error("stat", &path, &e)
                            ));
                        }
                        continue;
                    }
                }
                let own_snapshot = own.clone();
                let lookup = |k: &str| {
                    project_env.get(k).cloned().or_else(|| {
                        own_snapshot
                            .get(k)
                            .and_then(|v| v.as_str().map(String::from))
                    })
                };
                load_mapping_file(&path, &cwd, &format, &mut environment, &lookup, warn)?;
            }
            let mut resolved: Map = environment
                .into_iter()
                .map(|(k, v)| (k, Value::Str(v)))
                .collect();
            for (k, v) in own {
                resolved.insert(k, v);
            }
            if resolved.is_empty() {
                s.remove("environment");
            } else {
                s.insert("environment".to_string(), Value::Map(resolved));
            }
            if discard {
                s.remove("env_file");
            }
        }
        Ok(())
    }

    /// WithServicesLabelsResolved: the label files of each service read, its own labels
    /// over theirs.
    pub fn resolve_services_labels(&mut self, warn: Warn) -> Result<(), String> {
        let cwd = self.cwd.clone();
        for service in self.services.values_mut() {
            let Some(s) = service.as_map_mut() else {
                continue;
            };
            let files: Vec<String> = s
                .get("label_file")
                .and_then(Value::as_seq)
                .map(|f| f.iter().map(|v| v.to_string()).collect())
                .unwrap_or_default();
            let mut labels = Env::new();
            for file in files {
                if let Err(e) = std::fs::metadata(paths::abs(&file, &cwd)) {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        return Err(format!(
                            "label file {file} not found: {}",
                            gostr::path_error("stat", &file, &e)
                        ));
                    }
                }
                let snapshot = labels.clone();
                let lookup = |k: &str| snapshot.get(k).cloned();
                let mut read = Env::new();
                load_mapping_file(&file, &cwd, "", &mut read, &lookup, warn)?;
                labels.extend(read);
            }
            if let Some(own) = sub(s, "labels") {
                for (k, v) in own {
                    labels.insert(k.clone(), v.to_string());
                }
            }
            if !labels.is_empty() {
                s.insert(
                    "labels".to_string(),
                    Value::Map(
                        labels
                            .into_iter()
                            .map(|(k, v)| (k, Value::Str(v)))
                            .collect(),
                    ),
                );
            }
        }
        Ok(())
    }

    /// CheckContainerNameUnicity.
    pub fn check_container_name_unicity(&self) -> Result<(), String> {
        let mut names: BTreeMap<String, String> = BTreeMap::new();
        for (name, service) in &self.services {
            let container_name = service
                .get(&["container_name"])
                .map(|c| c.to_string())
                .unwrap_or_default();
            if container_name.is_empty() {
                continue;
            }
            if names.contains_key(&container_name) {
                return Err(format!(
                    "services.{name}: container name {} is already in use by service {container_name}\"",
                    go_quote(&container_name)
                ));
            }
            names.insert(container_name.clone(), name.clone());
        }
        Ok(())
    }

    fn encode_map(map: &Map, def: &'static model::Def, format: Format) -> Out {
        Out::Dict(
            map.iter()
                .map(|(k, v)| (k.clone(), model::encode(Some(v), &Ty::Struct(def), format)))
                .collect(),
        )
    }

    /// The project as `config` writes it in YAML.
    pub fn yaml(&self) -> String {
        let mut fields = Vec::new();
        if !self.name.is_empty() {
            fields.push(("name".to_string(), Out::str(&self.name)));
        }
        fields.push((
            "services".to_string(),
            Self::encode_map(&self.services, &types::SERVICE, Format::Yaml),
        ));
        for (key, map, def) in [
            ("jobs", &self.jobs, &types::JOB),
            ("networks", &self.networks, &types::NETWORK),
            ("volumes", &self.volumes, &types::VOLUME),
            ("secrets", &self.secrets, &types::SECRET),
            ("configs", &self.configs, &types::CONFIG),
            ("models", &self.models, &types::MODEL),
        ] {
            if !map.is_empty() {
                fields.push((key.to_string(), Self::encode_map(map, def, Format::Yaml)));
            }
        }
        let mut extensions: Vec<(String, Out)> = self
            .extensions
            .iter()
            .map(|(k, v)| (k.clone(), model::any(v)))
            .collect();
        extensions.sort_by(|a, b| emit::natural_order(&a.0, &b.0));
        fields.extend(extensions);
        emit::yaml(&Out::Struct(fields))
    }

    /// The project as `config --format json` writes it.
    pub fn json(&self) -> Result<String, String> {
        let mut fields = vec![
            ("name".to_string(), Out::str(&self.name)),
            (
                "services".to_string(),
                Self::encode_map(&self.services, &types::SERVICE, Format::Json),
            ),
        ];
        for (key, map, def) in [
            ("networks", &self.networks, &types::NETWORK),
            ("volumes", &self.volumes, &types::VOLUME),
            ("secrets", &self.secrets, &types::SECRET),
            ("configs", &self.configs, &types::CONFIG),
            ("jobs", &self.jobs, &types::JOB),
        ] {
            if !map.is_empty() {
                fields.push((key.to_string(), Self::encode_map(map, def, Format::Json)));
            }
        }
        for (k, v) in &self.extensions {
            fields.push((k.clone(), model::any(v)));
        }
        emit::json(&Out::Dict(fields))
    }
}

/// A name machined takes for a machine: letters, digits and hyphens, up to 64.
/// loadMappingFile: an env or label file read in its format, compose-go's `.env` or
/// `raw`, into `vars`; a relative path is relative to `cwd`.
fn load_mapping_file(
    path: &str,
    cwd: &str,
    format: &str,
    vars: &mut Env,
    lookup: &dyn Fn(&str) -> Option<String>,
    warn: Warn,
) -> Result<(), String> {
    use std::io::Read;
    // A directory opens, and fails as it is read.
    let mut file = std::fs::File::open(paths::abs(path, cwd))
        .map_err(|e| gostr::path_error("open", path, &e))?;
    let read = |file: &mut std::fs::File| {
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map(|_| bytes)
            .map_err(|e| gostr::path_error("read", path, &e))
    };
    match format {
        "" | ".env" => {
            let bytes = read(&mut file).map_err(|e| format!("failed to read {path}: {e}"))?;
            let mut unset = |name: &str| {
                warn(format!(
                    "The {} variable is not set. Defaulting to a blank string.",
                    go_quote(name)
                ))
            };
            dotenv::parse(&gostr::from_bytes(&bytes), vars, &lookup, &mut unset)
                .map_err(|e| format!("failed to read {path}: {e}"))
        }
        "raw" => {
            let bytes =
                read(&mut file).map_err(|e| format!("failed to parse env_file {path}: {e}"))?;
            for line in kv_file(&bytes, lookup)
                .map_err(|e| format!("failed to parse env_file {path}: {e}"))?
            {
                let (key, value) = line.split_once('=').unwrap_or((&line, ""));
                vars.insert(key.to_string(), value.to_string());
            }
            Ok(())
        }
        other => Err(format!("unsupported env_file format {}", go_quote(other))),
    }
}

/// kvfile: `KEY=value` lines read with bufio.Scanner (lines of up to 64 KiB, a
/// carriage return before each line end dropped), each line UTF-8, a BOM at the start
/// of the first dropped, a `KEY` alone taking the value lookup has.
fn kv_file(bytes: &[u8], lookup: &dyn Fn(&str) -> Option<String>) -> Result<Vec<String>, String> {
    const MAX_TOKEN: usize = 64 * 1024;
    let mut lines = Vec::new();
    let mut rest = bytes;
    let mut number = 1;
    while !rest.is_empty() {
        let (mut line, next) = match rest.iter().position(|&b| b == b'\n') {
            Some(i) if i < MAX_TOKEN => (&rest[..i], &rest[i + 1..]),
            None if rest.len() < MAX_TOKEN => (rest, &rest[rest.len()..]),
            _ => return Err("bufio.Scanner: token too long".to_string()),
        };
        rest = next;
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        let Ok(text) = std::str::from_utf8(line) else {
            let decimal: Vec<String> = line.iter().map(u8::to_string).collect();
            return Err(format!(
                "invalid utf8 bytes at line {number}: [{}]",
                decimal.join(" ")
            ));
        };
        let text = if number == 1 {
            text.strip_prefix('\u{feff}').unwrap_or(text)
        } else {
            text
        };
        number += 1;
        let line = text.trim_start_matches(gostr::is_space);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, has_value) = match line.split_once('=') {
            Some((key, _)) => (key, true),
            None => (line, false),
        };
        if key.is_empty() {
            return Err(format!("no variable name on line '{line}'"));
        }
        if key.contains([' ', '\t']) {
            return Err(format!("variable '{key}' contains whitespaces"));
        }
        if has_value {
            lines.push(line.to_string());
        } else if let Some(value) = lookup(line) {
            lines.push(format!("{key}={value}"));
        }
    }
    Ok(lines)
}
