//! The `filters` query parameter: a JSON object of keys to values, in its current form
//! (`{"label":["a=b"]}`) or the old one (`{"label":{"a=b":true}}`), which clients still
//! send.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

/// Each key's values, and which of them the object form gave as false (or null): docker
/// keeps that bool, and only its exact matches read it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Filters {
    values: BTreeMap<String, BTreeSet<String>>,
    off: BTreeSet<(String, String)>,
}

/// What a request gets back for a filter it cannot take, in the API's words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invalid(pub String);

impl Filters {
    /// As FromJSON reads them: the object form, else the array form, and
    /// "invalid filter" with no more said for anything neither of them is.
    pub fn parse(text: Option<&str>) -> Result<Filters, Invalid> {
        let Some(text) = text.filter(|t| !t.is_empty()) else {
            return Ok(Filters::default());
        };
        let invalid = || Invalid("invalid filter".to_string());
        let map = match serde_json::from_str(text).map_err(|_| invalid())? {
            Value::Object(map) => map,
            Value::Null => return Ok(Filters::default()),
            _ => return Err(invalid()),
        };
        let mut out = Filters::default();
        let (mut objects, mut arrays) = (false, false);
        for (key, values) in map {
            let set = out.values.entry(key.clone()).or_default();
            match values {
                // No values, in either form.
                Value::Null => {}
                Value::Array(list) => {
                    arrays = true;
                    for value in list {
                        let Value::String(value) = value else {
                            return Err(invalid());
                        };
                        set.insert(value);
                    }
                }
                Value::Object(old) => {
                    objects = true;
                    for (value, on) in old {
                        match on {
                            Value::Bool(true) => {}
                            // Go leaves a null the bool's zero.
                            Value::Bool(false) | Value::Null => {
                                out.off.insert((key.clone(), value.clone()));
                            }
                            _ => return Err(invalid()),
                        }
                        set.insert(value);
                    }
                }
                _ => return Err(invalid()),
            }
        }
        // One form or the other: both at once is neither.
        if objects && arrays {
            return Err(invalid());
        }
        Ok(out)
    }

    /// From the command line's KEY=VALUE pairs (`--filter name=web`).
    pub fn from_pairs(pairs: &[String]) -> Result<Filters, Invalid> {
        let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        // As FilterOpt has it: an empty --filter is none (one of blanks is no key=value),
        // key and value are trimmed.
        for pair in pairs.iter().filter(|p| !p.is_empty()) {
            let (key, value) = pair.split_once('=').ok_or_else(|| {
                Invalid(format!(
                    "bad format of filter (expected name=value): {pair}"
                ))
            })?;
            out.entry(key.trim().to_lowercase())
                .or_default()
                .insert(value.trim().to_string());
        }
        Ok(Filters {
            values: out,
            off: BTreeSet::new(),
        })
    }

    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.values.keys()
    }

    /// Every value of `key`, whatever its bool, as Get gives them.
    pub fn values(&self, key: &str) -> Option<&BTreeSet<String>> {
        self.values.get(key).filter(|v| !v.is_empty())
    }

    /// Refuses a key outside `accepted`.
    pub fn only(&self, accepted: &[&str]) -> Result<(), Invalid> {
        match self.keys().find(|k| !accepted.contains(&k.as_str())) {
            Some(key) => Err(Invalid(format!("invalid filter '{key}'"))),
            None => Ok(()),
        }
    }

    /// ExactMatch: no values for `key`, or `value` among them, given as true.
    pub fn exact(&self, key: &str, value: &str) -> bool {
        self.values(key).is_none_or(|v| {
            v.contains(value) && !self.off.contains(&(key.to_string(), value.to_string()))
        })
    }

    /// FuzzyMatch: an exact match, or a value, whatever its bool, that `value`
    /// starts with.
    pub fn fuzzy(&self, key: &str, value: &str) -> bool {
        self.exact(key, value)
            || self
                .values(key)
                .is_some_and(|v| v.iter().any(|prefix| value.starts_with(prefix.as_str())))
    }

    /// Match: an exact match, or a value, whatever its bool, that is a regular
    /// expression found in `value`.
    pub fn matches(&self, key: &str, value: &str) -> bool {
        self.exact(key, value)
            || self.values(key).is_some_and(|v| {
                v.iter()
                    .any(|p| regex::Regex::new(p).is_ok_and(|re| re.is_match(value)))
            })
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
            r#"{"label":{"caddy":true,"a=b":false},"status":{"running":true}}"#,
        ))
        .unwrap();
        // A value of the object form is there whatever its bool, as Get gives it.
        assert_eq!(new.values("label"), old.values("label"));
        assert_eq!(new.values("status"), old.values("status"));
        for bad in [
            "[1]",
            "true",
            "{",
            r#"{"label":"x"}"#,
            r#"{"name":["web",1]}"#,
            r#"{"label":{"a":"yes"}}"#,
            r#"{"label":["a"],"status":{"running":true}}"#,
        ] {
            assert_eq!(
                Filters::parse(Some(bad)),
                Err(Invalid("invalid filter".to_string())),
                "{bad}"
            );
        }
        // null is no value: no filter; inside the object form, a value given as false.
        let null = Filters::parse(Some(r#"{"label":null}"#)).unwrap();
        assert!(null.values("label").is_none());
        let label = Filters::parse(Some(r#"{"label":{"traefik.enable":null}}"#)).unwrap();
        assert!(label.values("label").unwrap().contains("traefik.enable"));
        assert_eq!(Filters::parse(Some("null")).unwrap(), Filters::default());
        assert_eq!(Filters::parse(None).unwrap(), Filters::default());
        assert_eq!(Filters::parse(Some("")).unwrap(), Filters::default());
    }

    #[test]
    fn a_value_given_as_false_matches_only_by_more_than_its_name() {
        let f = Filters::parse(Some(
            r#"{"health":{"healthy":false},"name":{"web":false},"event":{"health_status":false}}"#,
        ))
        .unwrap();
        // ExactMatch reads the bool; Match and FuzzyMatch also take every value.
        assert!(!f.exact("health", "healthy") && !f.exact("health", "unhealthy"));
        assert!(f.matches("name", "web") && f.matches("name", "/web-1"));
        assert!(f.fuzzy("event", "health_status: healthy"));
        assert!(!f.exact("event", "health_status"));
        let on = Filters::parse(Some(r#"{"health":{"healthy":true}}"#)).unwrap();
        assert!(on.exact("health", "healthy") && !on.exact("health", "unhealthy"));
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
