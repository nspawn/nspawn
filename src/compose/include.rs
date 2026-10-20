//! `include`: other projects loaded on their own (their directory, their `.env`) and
//! their resources brought into this one, as compose-go's ApplyInclude does. Remote
//! includes (git, oci://) are refused, as the support table says.

use super::dotenv::Env;
use super::gostr;
use super::loader::{self, ConfigFile, Details, Options, Warn};
use super::merge;
use super::model;
use super::path::Path;
use super::paths;
use super::types;
use super::value::{Map, Value};
use super::yaml::{self, Resets};

#[derive(Default)]
struct Include {
    paths: Vec<String>,
    project_directory: String,
    env_file: Vec<String>,
}

/// loadIncludeConfig: a list, each entry a path or a mapping, decoded into compose-go's
/// IncludeConfig as mapstructure decodes it.
fn configs(source: Option<&Value>, warn: Warn) -> Result<Vec<Include>, String> {
    let Some(source) = source.filter(|s| !s.is_null()) else {
        return Ok(Vec::new());
    };
    let Value::Seq(items) = source else {
        return Err(format!(
            "`include` must be a list, got {}",
            super::golib::go_s(source)
        ));
    };
    let items: Vec<Value> = items
        .iter()
        .map(|item| match item {
            Value::Str(s) => Value::Map(Map::from([("path".to_string(), Value::Str(s.clone()))])),
            other => other.clone(),
        })
        .collect();
    let decoded =
        model::decode(&Value::Seq(items), &types::INCLUDES, "", warn).map_err(|e| e.message())?;
    let strings = |value: Option<&Value>| -> Vec<String> {
        value
            .and_then(Value::as_seq)
            .map(|items| items.iter().map(|i| i.to_string()).collect())
            .unwrap_or_default()
    };
    Ok(decoded
        .as_seq()
        .into_iter()
        .flatten()
        .map(|entry| Include {
            paths: strings(entry.get(&["path"])),
            project_directory: entry
                .get(&["project_directory"])
                .map(|v| v.to_string())
                .unwrap_or_default(),
            env_file: strings(entry.get(&["env_file"])),
        })
        .collect())
}

pub fn apply(
    model: &mut Map,
    details: &Details,
    opts: &Options,
    included: &[String],
    resets: &Resets,
    warn: Warn,
) -> Result<(), String> {
    for mut include in configs(model.get("include"), warn)? {
        let mut relative_dir = String::new();
        for i in 0..include.paths.len() {
            let given = include.paths[i].clone();
            if paths::remote(&given) {
                return Err(format!("include: {given}: {}", paths::REMOTE));
            }
            let path = paths::abs(&given, &details.base_dir);
            if i == 0 {
                if include.project_directory.is_empty() {
                    relative_dir = paths::loader_dir(&details.base_dir, &path);
                    include.project_directory = paths::dir(&path);
                } else if !paths::is_abs(&include.project_directory) {
                    relative_dir = paths::loader_dir(
                        &details.base_dir,
                        &paths::abs(&include.project_directory, &details.base_dir),
                    );
                    include.project_directory =
                        paths::join(&[&details.working_dir, &include.project_directory]);
                } else {
                    relative_dir.clone_from(&include.project_directory);
                }
            }
            // compose-go looks for a cycle on the first file only, and goes round one
            // through another without end; it is refused on any file here.
            if included.contains(&path) {
                let mut chain = included.to_vec();
                chain.push(path.clone());
                return Err(format!(
                    "include cycle detected:\n{}\n include {}",
                    chain[0],
                    chain[1..].join("\n include ")
                ));
            }
            include.paths[i] = path;
        }
        let env_files = if include.env_file.is_empty() {
            let default = paths::join(&[&include.project_directory, ".env"]);
            match std::fs::metadata(paths::abs(&default, &opts.cwd)) {
                Ok(meta) if !meta.is_dir() => vec![default],
                _ => Vec::new(),
            }
        } else {
            let mut files = Vec::new();
            for file in &include.env_file {
                if file == "/dev/null" {
                    continue;
                }
                let mut file = file.clone();
                if !paths::is_abs(&file) {
                    // The include's directory may be relative: what it gives is then
                    // relative to the process's.
                    file = paths::join(&[&details.working_dir, &file]);
                    let meta = std::fs::metadata(paths::abs(&file, &opts.cwd))
                        .map_err(|e| gostr::path_error("stat", &file, &e))?;
                    if meta.is_dir() {
                        return Err(format!("{file} is not a file"));
                    }
                }
                files.push(file);
            }
            files
        };
        let from_files = loader::env_from_files(&details.env, &env_files, &opts.cwd, warn)?;
        let env: Env = loader::env_with(&details.env, &from_files);
        let nested = Details {
            working_dir: relative_dir.clone(),
            base_dir: include.project_directory.clone(),
            files: include
                .paths
                .iter()
                .map(|p| ConfigFile {
                    filename: p.clone(),
                    content: None,
                })
                .collect(),
            env,
        };
        let mut nested_opts = opts.clone();
        nested_opts.resolve_paths = true;
        nested_opts.skip_normalization = true;
        nested_opts.skip_consistency = true;
        let key = (
            include.paths.clone(),
            relative_dir.clone(),
            include.project_directory.clone(),
            nested.env.clone(),
        );
        let cached = opts.caches.includes.borrow().get(&key).cloned();
        let imported = match cached {
            Some(imported) => imported,
            None => {
                let imported = loader::load_yaml_model(
                    &nested,
                    &nested_opts,
                    &mut Vec::new(),
                    included,
                    warn,
                )?;
                opts.caches
                    .includes
                    .borrow_mut()
                    .insert(key, imported.clone());
                imported
            }
        };
        import_resources(imported, model, resets)?;
    }
    model.remove("include");
    Ok(())
}

fn import_resources(source: Map, target: &mut Map, resets: &Resets) -> Result<(), String> {
    for key in [
        "services", "jobs", "volumes", "networks", "secrets", "configs", "models",
    ] {
        let Some(Value::Map(from)) = source.get(key) else {
            continue;
        };
        // compose-go takes the including file's section as a mapping and crashes on
        // anything else; what the schema would say of it is said instead.
        let mut to = match target.remove(key) {
            None => Map::new(),
            Some(Value::Map(m)) => m,
            Some(_) => return Err(format!("{key} must be a mapping")),
        };
        for (name, resource) in from {
            match to.get(name) {
                None => {
                    to.insert(name.clone(), resource.clone());
                }
                Some(existing) if existing == resource => {}
                Some(existing) => {
                    // The resets of the including file reach into the resource; one on
                    // the resource itself takes it from a mapping compose-go throws away.
                    let mut resource = resource.clone();
                    yaml::apply_resets(resets, &mut resource, &Path::root().next(key).next(name));
                    let merged =
                        merge::merge_at(resource, existing.clone(), &Path::new(&[key, name]))?;
                    to.insert(name.clone(), merged);
                }
            }
        }
        target.insert(key.to_string(), Value::Map(to));
    }
    Ok(())
}
