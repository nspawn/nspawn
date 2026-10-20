//! Extensions the Compose Specification adopted as attributes: a file may still write
//! `x-develop` for `develop`, and compose-go reads it as the attribute, merged under it
//! when the attribute is set as well.

use super::merge;
use super::path::Path;
use super::value::Value;

struct Alias {
    /// The path of the mapping that holds the attribute.
    parent: &'static str,
    from: &'static str,
    to: &'static str,
}

/// From the outermost attribute to the innermost.
const ALIASES: &[Alias] = &[
    Alias {
        parent: "services.*",
        from: "x-develop",
        to: "develop",
    },
    Alias {
        parent: "services.*.develop.watch.[]",
        from: "x-initialSync",
        to: "initial_sync",
    },
];

/// Whether `path` is the parent of an alias, or above one.
fn on_the_way(path: &Path) -> bool {
    let parts = path.parts();
    ALIASES.iter().any(|alias| {
        let pattern: Vec<&str> = alias.parent.split('.').collect();
        pattern.len() >= parts.len()
            && parts
                .iter()
                .zip(&pattern)
                .all(|(part, pattern)| *pattern == "*" || pattern == part)
    })
}

/// Renames the extensions to the attributes they stand for, in place.
pub fn promote(value: &mut Value, path: &Path) -> Result<(), String> {
    match value {
        Value::Map(map) => {
            for alias in ALIASES {
                if !path.matches_str(alias.parent) {
                    continue;
                }
                let Some(extension) = map.remove(alias.from) else {
                    continue;
                };
                let merged = match map.remove(alias.to) {
                    None | Some(Value::Null) => extension,
                    Some(attribute) => merge::merge_at(extension, attribute, &path.next(alias.to))?,
                };
                map.insert(alias.to.to_string(), merged);
            }
            for (key, entry) in map.iter_mut() {
                let next = path.next(key);
                if on_the_way(&next) {
                    promote(entry, &next)?;
                }
            }
        }
        Value::Seq(items) => {
            let next = path.next("[]");
            if on_the_way(&next) {
                for item in items {
                    promote(item, &next)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// The path with each extension of an alias replaced by its attribute; list indexes
/// in it match the `[]` of an alias's parent.
pub fn resolve_alias_path(path: &Path) -> Path {
    let mut parts: Vec<String> = path.parts().iter().map(|p| p.to_string()).collect();
    for alias in ALIASES {
        let depth = alias.parent.split('.').count();
        if parts.len() <= depth || parts[depth] != alias.from {
            continue;
        }
        let head: Vec<&str> = parts[..depth]
            .iter()
            .map(|part| {
                if part.parse::<i64>().is_ok() {
                    "[]"
                } else {
                    part.as_str()
                }
            })
            .collect();
        if Path::new(&head).matches_str(alias.parent) {
            parts[depth] = alias.to.to_string();
        }
    }
    let parts: Vec<&str> = parts.iter().map(String::as_str).collect();
    Path::new(&parts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compose::value::Map;

    #[test]
    fn x_develop_becomes_develop() {
        let watch = Value::Seq(vec![Value::Map(Map::from([
            ("path".to_string(), Value::str(".")),
            ("x-initialSync".to_string(), Value::Bool(true)),
        ]))]);
        let mut model = Value::Map(Map::from([(
            "services".to_string(),
            Value::Map(Map::from([(
                "web".to_string(),
                Value::Map(Map::from([(
                    "x-develop".to_string(),
                    Value::Map(Map::from([("watch".to_string(), watch)])),
                )])),
            )])),
        )]));
        promote(&mut model, &Path::root()).unwrap();
        let item = &model
            .get(&["services", "web", "develop", "watch"])
            .unwrap()
            .as_seq()
            .unwrap()[0];
        assert_eq!(item.get(&["initial_sync"]), Some(&Value::Bool(true)));
        assert_eq!(
            resolve_alias_path(&Path::new(&[
                "services",
                "web",
                "x-develop",
                "watch",
                "0",
                "x-initialSync"
            ]))
            .to_string(),
            "services.web.develop.watch.0.initial_sync"
        );
    }
}
