//! `extends`: a service built on another one, of the same file or of another, as
//! compose-go's ApplyExtends does: the base taken as it is after its own extends, the
//! extending file's `!reset` applied to it, then the service merged over it.

use super::loader::{self, ConfigFile, Details, Options, Tracker, Warn};
use super::merge;
use super::path::Path;
use super::paths::{self, Resolver};
use super::value::{Map, Value};
use super::yaml::{self, Resets};

pub fn apply(
    model: &mut Map,
    file: &str,
    opts: &Options,
    details: &Details,
    tracker: &Tracker,
    resets: &Resets,
    warn: Warn,
) -> Result<(), String> {
    for key in ["services", "jobs"] {
        let Some(entries) = model.get_mut(key) else {
            continue;
        };
        let Value::Map(entries) = entries else {
            return Err(format!("{key} must be a mapping"));
        };
        let names: Vec<String> = entries.keys().cloned().collect();
        for name in names {
            let merged = extend(
                &name,
                entries,
                file,
                opts,
                details,
                tracker.clone(),
                resets,
                warn,
            )?;
            entries.insert(name, merged);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn extend(
    name: &str,
    services: &mut Map,
    file: &str,
    opts: &Options,
    details: &Details,
    mut tracker: Tracker,
    post: &Resets,
    warn: Warn,
) -> Result<Value, String> {
    let service = match services.get(name) {
        None | Some(Value::Null) => return Ok(Value::Null),
        Some(Value::Map(service)) => service.clone(),
        Some(_) => return Err(format!("services.{name} must be a mapping")),
    };
    let Some(extends) = service.get("extends") else {
        return Ok(Value::Map(service));
    };
    let (reference, base_file) = match extends {
        Value::Map(v) => {
            let Some(Value::Str(reference)) = v.get("service") else {
                return Err(format!("extends.{name}.service is required"));
            };
            // A null file is none.
            (
                reference.clone(),
                v.get("file")
                    .filter(|f| !f.is_null())
                    .map(|f| f.to_string()),
            )
        }
        Value::Str(reference) => (reference.clone(), None),
        _ => (String::new(), None),
    };
    let mut filename = file.to_string();
    let base = match base_file {
        Some(base_file) => {
            // A file of a git repository or an OCI artifact is not fetched, as include's.
            if paths::remote(&base_file) {
                return Err(format!(
                    "services.{name}.extends.file: {base_file}: {}",
                    paths::REMOTE
                ));
            }
            let (mut base_services, base_resets) = load_base(
                name, &reference, file, &base_file, opts, details, &tracker, warn,
            )?;
            filename = base_file;
            tracker = add(tracker, &filename, name)?;
            extend(
                &reference,
                &mut base_services,
                file,
                opts,
                details,
                tracker,
                &base_resets,
                warn,
            )?
        }
        None => {
            if !services.contains_key(&reference) {
                return Err(format!(
                    "cannot extend service {} in {filename}: service {} not found",
                    super::golib::go_quote(name),
                    super::golib::go_quote(&reference)
                ));
            }
            tracker = add(tracker, &filename, name)?;
            extend(
                &reference, services, file, opts, details, tracker, post, warn,
            )?
        }
    };
    if base.is_null() {
        return Ok(Value::Map(service));
    }
    // The resets of the extending file reach into the base; one on the service
    // itself takes it from a mapping compose-go throws away, not from the base.
    let mut source = base;
    yaml::apply_resets(post, &mut source, &Path::root().next("services").next(name));
    let merged = merge::merge_at(source, Value::Map(service), &Path::new(&["services", "x"]))?;
    let Value::Map(mut merged) = merged else {
        return Err(format!("cannot override services.{name}"));
    };
    merged.remove("extends");
    services.insert(name.to_string(), Value::Map(merged.clone()));
    Ok(Value::Map(merged))
}

/// The tracker with one more service, or the cycle it closes.
fn add(mut tracker: Tracker, filename: &str, service: &str) -> Result<Tracker, String> {
    let entry = (filename.to_string(), service.to_string());
    if tracker.contains(&entry) {
        let mut lines = vec![
            "Circular reference:".to_string(),
            format!("  {} in {}", tracker[0].1, tracker[0].0),
        ];
        for (file, service) in tracker[1..].iter().chain([&entry]) {
            lines.push(format!("  extends {service} in {file}"));
        }
        return Err(lines.join("\n"));
    }
    tracker.push(entry);
    Ok(tracker)
}

/// The services of the file an `extends.file` names, loaded on their own with paths
/// relative to that file.
#[allow(clippy::too_many_arguments)]
fn load_base(
    name: &str,
    reference: &str,
    path: &str,
    ref_path: &str,
    opts: &Options,
    details: &Details,
    tracker: &Tracker,
    warn: Warn,
) -> Result<(Map, Resets), String> {
    let local = paths::abs(ref_path, &details.base_dir);
    let relative_dir = paths::loader_dir(&details.base_dir, &local);
    // A file read once for every service that extends from it.
    let key = (local.clone(), relative_dir.clone());
    let cached = opts.caches.extends.borrow().get(&key).cloned();
    let (services, resets) = match cached {
        Some(base) => base,
        None => {
            let base = read_base(name, &local, &relative_dir, opts, details, tracker, warn)?;
            opts.caches.extends.borrow_mut().insert(key, base.clone());
            base
        }
    };
    if !services.contains_key(reference) {
        return Err(format!(
            "cannot extend service {} in {path}: service {} not found in {ref_path}",
            super::golib::go_quote(name),
            super::golib::go_quote(reference)
        ));
    }
    Ok((services, resets))
}

/// loadExtendsBase: the services of a file, loaded on their own with paths relative
/// to it.
fn read_base(
    name: &str,
    local: &str,
    relative_dir: &str,
    opts: &Options,
    details: &Details,
    tracker: &Tracker,
    warn: Warn,
) -> Result<(Map, Resets), String> {
    let mut base_opts = opts.clone();
    base_opts.resolve_paths = false;
    base_opts.skip_normalization = true;
    base_opts.skip_consistency = true;
    base_opts.skip_include = true;
    base_opts.skip_extends = true;
    base_opts.skip_validation = true;
    base_opts.skip_default_values = true;
    let file = ConfigFile {
        filename: local.to_string(),
        content: None,
    };
    let nested = Details {
        working_dir: relative_dir.to_string(),
        base_dir: paths::dir(local),
        files: vec![file.clone()],
        env: details.env.clone(),
    };
    let mut tracker = tracker.clone();
    let (source, resets) = loader::load_yaml_file(
        &file,
        &base_opts,
        &nested,
        &mut tracker,
        Map::new(),
        &mut Vec::new(),
        warn,
    )?;
    let mut source = Value::Map(source);
    match source.get(&["services"]) {
        None => {
            return Err(format!(
                "cannot extend service {} in {local}: no services section",
                super::golib::go_quote(name)
            ))
        }
        Some(Value::Map(_)) => {}
        Some(_) => {
            return Err(format!(
                "cannot extend service {} in {local}: services must be a mapping",
                super::golib::go_quote(name)
            ))
        }
    }
    Resolver {
        working_dir: relative_dir,
        home: opts.home.as_deref(),
    }
    .resolve(&mut source, warn)?;
    let Some(Value::Map(services)) = source.get(&["services"]).cloned() else {
        unreachable!("checked above");
    };
    Ok((services, resets))
}
