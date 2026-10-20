//! nspawn's events as the Docker Engine API reports them.

use serde_json::{json, Map, Value};

use super::filters::Filters;
use super::util::{container_id, familiar, network_id};
use crate::api::events::Event;

/// The keys GET /events takes.
pub const ACCEPTED: &[&str] = &[
    "config",
    "container",
    "daemon",
    "event",
    "image",
    "label",
    "network",
    "node",
    "plugin",
    "scope",
    "secret",
    "service",
    "type",
    "volume",
];

/// docker's events for one of nspawn's: none for what docker has no word for, more for a
/// start or an end on bridge networks (the machine's own and a network connect or
/// disconnect for each network, given as name and ID).
pub fn translate(event: &Event, networks: &[(String, String)]) -> Vec<Value> {
    let secs = event.time_usec / 1_000_000;
    let nanos = event.time_usec * 1000;
    let base = |kind: &str, action: &str, id: &str, attributes: Map<String, Value>| {
        json!({
            "Type": kind,
            "Action": action,
            "Actor": { "ID": id, "Attributes": attributes },
            "scope": "local",
            "time": secs,
            "timeNano": nanos,
        })
    };
    match event.kind.as_str() {
        "machine" => {
            let action = match event.action.as_str() {
                "create" | "start" | "die" | "stop" | "kill" | "pause" | "unpause" | "oom"
                | "update" => event.action.clone(),
                "remove" => "destroy".to_string(),
                "pull" => "create".to_string(),
                "health_status" => format!(
                    "health_status: {}",
                    event.attributes.get("status").map_or("", String::as_str)
                ),
                _ => return Vec::new(),
            };
            let Some(created) = event
                .attributes
                .get("created")
                .and_then(|c| c.parse::<u64>().ok())
            else {
                return Vec::new();
            };
            let id = container_id(&event.name, created);
            let image = event
                .attributes
                .get("image")
                .map(|i| familiar(i))
                .unwrap_or_default();
            let mut attributes: Map<String, Value> = event
                .labels
                .iter()
                .map(|(k, v)| (k.clone(), json!(v)))
                .collect();
            attributes.insert("image".into(), json!(image));
            attributes.insert("name".into(), json!(event.name));
            if action == "die" {
                if let Some(code) = event.attributes.get("exit_code") {
                    attributes.insert("exitCode".into(), json!(code));
                }
            }
            if action == "kill" {
                if let Some(signal) = event.attributes.get("signal") {
                    attributes.insert("signal".into(), json!(signal));
                }
            }
            let mut message = base("container", &action, &id, attributes);
            // What clients of older API versions still read.
            message["status"] = json!(action);
            message["id"] = json!(id);
            message["from"] = json!(image);
            let mut out = vec![message];
            let network_action = match action.as_str() {
                "start" => Some("connect"),
                "die" => Some("disconnect"),
                _ => None,
            };
            if let Some(network_action) = network_action {
                for (net, net_id) in networks {
                    let mut attributes = Map::new();
                    attributes.insert("container".into(), json!(id));
                    attributes.insert("name".into(), json!(net));
                    attributes.insert("type".into(), json!("bridge"));
                    out.push(base("network", network_action, net_id, attributes));
                }
            }
            out
        }
        "network" => {
            let action = match event.action.as_str() {
                "create" => "create",
                "remove" => "destroy",
                _ => return Vec::new(),
            };
            let mut attributes = Map::new();
            attributes.insert("name".into(), json!(event.name));
            attributes.insert("type".into(), json!("bridge"));
            let created = event
                .attributes
                .get("created")
                .and_then(|c| c.parse().ok())
                .unwrap_or(0);
            vec![base(
                "network",
                action,
                &network_id(&event.name, created),
                attributes,
            )]
        }
        "volume" => {
            let action = match event.action.as_str() {
                "create" => "create",
                "remove" => "destroy",
                _ => return Vec::new(),
            };
            let mut attributes = Map::new();
            attributes.insert("driver".into(), json!("local"));
            vec![base("volume", action, &event.name, attributes)]
        }
        _ => Vec::new(),
    }
}

/// Whether a translated event passes the filters of GET /events, as docker matches
/// them: `event=health_status` takes every health status, a container by its ID, its
/// name or a prefix of its ID.
pub fn passes(filters: &Filters, message: &Value) -> bool {
    let field = |path: &[&str]| -> String {
        let mut v = message;
        for p in path {
            v = &v[*p];
        }
        v.as_str().unwrap_or_default().to_string()
    };
    let kind = field(&["Type"]);
    let action = field(&["Action"]);
    let id = field(&["Actor", "ID"]);
    let name = field(&["Actor", "Attributes", "name"]);
    if !filters.exact("type", &kind) {
        return false;
    }
    if !filters.exact("scope", "local") {
        return false;
    }
    if let Some(events) = filters.values("event") {
        let plain = action.split_once(':').map_or(action.as_str(), |(a, _)| a);
        if !events.contains(&action) && !events.contains(plain) {
            return false;
        }
    }
    if let Some(wanted) = filters.values("container") {
        if kind != "container"
            || !wanted
                .iter()
                .any(|w| *w == id || *w == name || (!w.is_empty() && id.starts_with(w.as_str())))
        {
            return false;
        }
    }
    if let Some(wanted) = filters.values("network") {
        if kind != "network" || !wanted.iter().any(|w| *w == id || *w == name) {
            return false;
        }
    }
    if let Some(wanted) = filters.values("volume") {
        if kind != "volume" || !wanted.contains(&id) {
            return false;
        }
    }
    if let Some(wanted) = filters.values("image") {
        let image = field(&["Actor", "Attributes", "image"]);
        if !wanted.contains(&image) {
            return false;
        }
    }
    for key in ["config", "daemon", "node", "plugin", "secret", "service"] {
        if filters.values(key).is_some() {
            return false;
        }
    }
    let labels: std::collections::BTreeMap<String, String> = message["Actor"]["Attributes"]
        .as_object()
        .map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                .collect()
        })
        .unwrap_or_default();
    filters.labels("label", &labels)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn event(kind: &str, action: &str, attributes: &[(&str, &str)]) -> Event {
        Event {
            time_usec: 1_792_344_103_496_502,
            kind: kind.into(),
            action: action.into(),
            name: "web".into(),
            attributes: attributes
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            labels: BTreeMap::from([("caddy".to_string(), "http://web.test".to_string())]),
        }
    }

    #[test]
    fn machines_become_containers() {
        let nets = vec![("front".to_string(), network_id("front", 5))];
        let die = translate(
            &event(
                "machine",
                "die",
                &[
                    ("image", "docker.io/library/busybox:1.37"),
                    ("exit_code", "3"),
                    ("created", "100"),
                ],
            ),
            &nets,
        );
        assert_eq!(die.len(), 2);
        let id = container_id("web", 100);
        assert_eq!(die[0]["Type"], "container");
        assert_eq!(die[0]["Action"], "die");
        assert_eq!(die[0]["status"], "die");
        assert_eq!(die[0]["id"], json!(id));
        assert_eq!(die[0]["from"], "busybox:1.37");
        assert_eq!(die[0]["Actor"]["Attributes"]["exitCode"], "3");
        assert_eq!(die[0]["Actor"]["Attributes"]["caddy"], "http://web.test");
        assert_eq!(die[0]["Actor"]["Attributes"]["name"], "web");
        assert_eq!(die[0]["time"], 1_792_344_103u64);
        assert_eq!(die[0]["timeNano"], 1_792_344_103_496_502_000u64);
        assert_eq!(die[1]["Type"], "network");
        assert_eq!(die[1]["Action"], "disconnect");
        assert_eq!(die[1]["Actor"]["Attributes"]["container"], json!(id));
        assert_eq!(die[1]["Actor"]["ID"], json!(network_id("front", 5)));
        let health = translate(
            &event(
                "machine",
                "health_status",
                &[("status", "healthy"), ("created", "100")],
            ),
            &[],
        );
        assert_eq!(health[0]["Action"], "health_status: healthy");
        // A removed machine's record is gone; its event says when it was made.
        let destroy = translate(&event("machine", "remove", &[("created", "100")]), &[]);
        assert_eq!(destroy[0]["Action"], "destroy");
        assert_eq!(destroy[0]["id"], json!(id));
        assert!(translate(&event("machine", "fail", &[("created", "100")]), &[]).is_empty());
        // Without its creation time no ID can be told.
        assert!(translate(&event("machine", "start", &[]), &[]).is_empty());
        assert!(translate(&event("secret", "create", &[]), &[]).is_empty());
    }

    #[test]
    fn filters_as_docker_applies_them() {
        let start = &translate(
            &event("machine", "start", &[("created", "100")]),
            &[("front".into(), network_id("front", 5))],
        );
        let health = &translate(
            &event(
                "machine",
                "health_status",
                &[("status", "healthy"), ("created", "100")],
            ),
            &[],
        )[0];
        let f = |text: &str| Filters::parse(Some(text)).unwrap();
        assert!(passes(&f(r#"{"type":{"container":true}}"#), &start[0]));
        assert!(!passes(&f(r#"{"type":{"container":true}}"#), &start[1]));
        assert!(passes(
            &f(
                r#"{"scope":{"local":true,"swarm":true},"type":{"config":true,"container":true,"network":true,"service":true}}"#
            ),
            &start[1]
        ));
        assert!(passes(&f(r#"{"event":["start","health_status"]}"#), health));
        assert!(!passes(&f(r#"{"event":["die"]}"#), health));
        assert!(passes(&f(r#"{"container":["web"]}"#), &start[0]));
        assert!(passes(
            &f(&format!(
                r#"{{"container":["{}"]}}"#,
                &container_id("web", 100)[..12]
            )),
            &start[0]
        ));
        assert!(!passes(&f(r#"{"container":["db"]}"#), &start[0]));
        assert!(passes(&f(r#"{"label":["caddy"]}"#), &start[0]));
        assert!(!passes(&f(r#"{"label":["traefik.enable"]}"#), &start[0]));
        assert!(!passes(&f(r#"{"service":["x"]}"#), &start[0]));
    }
}
