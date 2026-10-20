//! The `filters` query parameter: a JSON object of keys to values, in its current form
//! (`{"label":["a=b"]}`) or the old one (`{"label":{"a=b":true}}`), which clients still
//! send.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Filters(BTreeMap<String, BTreeSet<String>>);

/// What a request gets back for a filter it cannot take, as docker words it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invalid(pub String);

impl Filters {
    pub fn parse(text: Option<&str>) -> Result<Filters, Invalid> {
        let Some(text) = text.filter(|t| !t.is_empty()) else {
            return Ok(Filters::default());
        };
        let value: Value =
            serde_json::from_str(text).map_err(|e| Invalid(format!("invalid filter: {e}")))?;
        let Value::Object(map) = value else {
            return Err(Invalid("invalid filter: not an object".to_string()));
        };
        let mut out = BTreeMap::new();
        for (key, values) in map {
            let set: BTreeSet<String> = match values {
                Value::Array(list) => list
                    .into_iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect(),
                Value::Object(old) => old
                    .into_iter()
                    .filter(|(_, on)| on.as_bool() == Some(true))
                    .map(|(k, _)| k)
                    .collect(),
                _ => return Err(Invalid(format!("invalid filter '{key}'"))),
            };
            out.insert(key, set);
        }
        Ok(Filters(out))
    }

    /// From the command line's KEY=VALUE pairs (`--filter name=web`).
    pub fn from_pairs(pairs: &[String]) -> Result<Filters, Invalid> {
        let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for pair in pairs {
            let (key, value) = pair.split_once('=').ok_or_else(|| {
                Invalid(format!(
                    "bad format of filter (expected name=value): {pair}"
                ))
            })?;
            out.entry(key.trim().to_lowercase())
                .or_default()
                .insert(value.to_string());
        }
        Ok(Filters(out))
    }

    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.0.keys()
    }

    pub fn values(&self, key: &str) -> Option<&BTreeSet<String>> {
        self.0.get(key).filter(|v| !v.is_empty())
    }

    /// Refuses a key outside `accepted`.
    pub fn only(&self, accepted: &[&str]) -> Result<(), Invalid> {
        match self.keys().find(|k| !accepted.contains(&k.as_str())) {
            Some(key) => Err(Invalid(format!("invalid filter '{key}'"))),
            None => Ok(()),
        }
    }

    /// No values for `key`, or one equal to `value`.
    pub fn exact(&self, key: &str, value: &str) -> bool {
        self.values(key).is_none_or(|v| v.contains(value))
    }

    /// No values for `key`, or one of them equal to `value` or a regular expression
    /// found in it.
    pub fn matches(&self, key: &str, value: &str) -> bool {
        let Some(patterns) = self.values(key) else {
            return true;
        };
        patterns
            .iter()
            .any(|p| p == value || regex::Regex::new(p).is_ok_and(|re| re.is_match(value)))
    }

    /// Labels: each value KEY or KEY=VALUE must hold.
    pub fn labels(&self, key: &str, labels: &BTreeMap<String, String>) -> bool {
        let Some(wanted) = self.values(key) else {
            return true;
        };
        wanted.iter().all(|w| match w.split_once('=') {
            Some((k, v)) => labels.get(k).is_some_and(|have| have == v),
            None => labels.contains_key(w),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_encodings() {
        let new =
            Filters::parse(Some(r#"{"label":["caddy","a=b"],"status":["running"]}"#)).unwrap();
        let old = Filters::parse(Some(
            r#"{"label":{"caddy":true,"a=b":true,"off":false},"status":{"running":true}}"#,
        ))
        .unwrap();
        assert_eq!(new, old);
        assert!(Filters::parse(Some("[1]")).is_err());
        assert!(Filters::parse(Some(r#"{"label":"x"}"#)).is_err());
        assert_eq!(Filters::parse(None).unwrap(), Filters::default());
        assert_eq!(Filters::parse(Some("")).unwrap(), Filters::default());
    }

    #[test]
    fn matching() {
        let f = Filters::parse(Some(
            r#"{"name":["^/web$","db"],"label":["caddy","a=b"],"status":["running"]}"#,
        ))
        .unwrap();
        assert!(f.matches("name", "/web"));
        assert!(f.matches("name", "/mydb-1"));
        assert!(!f.matches("name", "/webby"));
        assert!(f.matches("network", "anything"));
        let labels = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<BTreeMap<_, _>>()
        };
        assert!(f.labels("label", &labels(&[("caddy", "x"), ("a", "b")])));
        assert!(!f.labels("label", &labels(&[("caddy", "x"), ("a", "c")])));
        assert!(f.exact("status", "running") && !f.exact("status", "exited"));
        assert_eq!(f.only(&["name", "label", "status"]), Ok(()));
        assert_eq!(
            f.only(&["name"]),
            Err(Invalid("invalid filter 'label'".into()))
        );
    }
}
