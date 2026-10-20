//! compose-go's tree.Path: a dotted path to a value of the model, `*` matching any key
//! of a mapping and `[]` any item of a list in a pattern. A dot inside a key stands
//! escaped, so that `labels.com.example` keeps its key whole.

use std::fmt;

/// What a dot inside a key becomes in a path; compose-go uses a character no key holds
/// as well.
const ESCAPED_DOT: char = '\u{1F47B}';

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Path(String);

impl Path {
    pub fn root() -> Path {
        Path(String::new())
    }

    /// A path written as compose-go writes a pattern: parts joined by dots.
    pub fn new(parts: &[&str]) -> Path {
        Path(parts.join("."))
    }

    /// The path one step further. As in compose-go, the first step keeps its dots.
    pub fn next(&self, part: &str) -> Path {
        if self.0.is_empty() {
            return Path(part.to_string());
        }
        Path(format!(
            "{}.{}",
            self.0,
            part.replace('.', &ESCAPED_DOT.to_string())
        ))
    }

    pub fn parts(&self) -> Vec<&str> {
        self.0.split('.').collect()
    }

    /// The path with escaped dots as they are, to compare paths with.
    pub fn raw(&self) -> &str {
        &self.0
    }

    pub fn matches(&self, pattern: &Path) -> bool {
        let pattern = pattern.parts();
        let parts = self.parts();
        pattern.len() == parts.len()
            && parts
                .iter()
                .zip(&pattern)
                .all(|(part, pattern)| *pattern == "*" || pattern == part)
    }

    pub fn matches_str(&self, pattern: &str) -> bool {
        self.matches(&Path(pattern.to_string()))
    }

    /// The path without the first `.<<` in it: what a merge key brings counts where
    /// the mapping holding the merge key is.
    pub fn without_merge_key(self) -> Path {
        // compose-go looks at the path written out, where an escaped dot is a dot
        // again, and makes the path from that.
        let written = self.to_string();
        if written.contains(".<<") {
            Path(written.replacen(".<<", "", 1))
        } else {
            self
        }
    }

    /// The rest of this path under `base`, empty for `base` itself.
    pub fn relative_to(&self, base: &Path) -> Option<Path> {
        if base.0.is_empty() {
            return Some(self.clone());
        }
        if self.0 == base.0 {
            return Some(Path::root());
        }
        self.0
            .strip_prefix(&format!("{}.", base.0))
            .map(|rest| Path(rest.to_string()))
    }

    pub fn join(&self, relative: &Path) -> Path {
        if relative.0.is_empty() {
            return self.clone();
        }
        if self.0.is_empty() {
            return relative.clone();
        }
        Path(format!("{}.{}", self.0, relative.0))
    }

    /// Whether the two paths go through two different services.
    pub fn in_different_services(&self, other: &Path) -> bool {
        let (a, b) = (self.parts(), other.parts());
        for i in 0..a.len().min(b.len()) {
            if a[i] == "services" && i + 1 < a.len() && b[i] == "services" && i + 1 < b.len() {
                return a[i + 1] != b[i + 1];
            }
        }
        false
    }
}

impl fmt::Display for Path {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.replace(ESCAPED_DOT, "."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_with_dots_stay_whole() {
        let path = Path::root()
            .next("services")
            .next("web")
            .next("labels")
            .next("com.example");
        assert_eq!(path.parts().len(), 4);
        assert_eq!(path.to_string(), "services.web.labels.com.example");
        assert!(path.matches_str("services.*.labels.*"));
        assert!(!path.matches_str("services.*.labels"));
    }

    #[test]
    fn relative_paths_join_back() {
        let base = Path::new(&["services", "web"]);
        let full = Path::new(&["services", "web", "ports", "0"]);
        let rel = full.relative_to(&base).unwrap();
        assert_eq!(rel.raw(), "ports.0");
        assert_eq!(base.join(&rel), full);
        assert_eq!(base.relative_to(&base).unwrap(), Path::root());
        assert!(Path::new(&["volumes"]).relative_to(&base).is_none());
    }
}
