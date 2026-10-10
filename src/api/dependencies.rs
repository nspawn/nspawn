//! --depends-on: machines started before another one. systemd starts them and orders
//! the starts (the Wants= and After= of the machine's drop-in), at boot as well; the
//! machine's first hook waits for what is asked of each one and fails its start when one
//! fails.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{bail, Result};

use crate::api::Context;
use crate::reference::validate_entry_name;
use crate::store::{ImageRecord, Store};
use crate::systemd::{Systemd, UnitState};
use crate::tuning::{Condition, Dependency};

/// Each machine named is one of nspawn's, other than this one, and none of them leads
/// back to it, through dependencies or the network of another machine (`owner`, its
/// --network container:): a machine waits for both.
pub fn check(store: &Store, name: &str, deps: &[Dependency], owner: Option<&str>) -> Result<()> {
    if deps.is_empty() && owner.is_none() {
        return Ok(());
    }
    let records = store.list_images()?;
    for dep in deps {
        if dep.name == name {
            bail!(
                "--depends-on {}: a machine cannot depend on itself",
                dep.name
            );
        }
        if !records.iter().any(|r| r.name == dep.name) {
            bail!(
                "--depends-on {}: no machine of nspawn's has that name (nspawn ps -a lists them)",
                dep.name
            );
        }
    }
    let mut graph = graph(&records);
    graph.insert(
        name.to_string(),
        deps.iter()
            .map(|d| d.name.clone())
            .chain(owner.map(str::to_string))
            .collect(),
    );
    if let Some(cycle) = cycle(name, &graph) {
        bail!(
            "--depends-on would make a cycle, {}; nothing could start first",
            cycle.join(" -> ")
        );
    }
    Ok(())
}

/// What every machine waits for, by name: its dependencies and the machine whose
/// network it takes.
fn graph(records: &[ImageRecord]) -> BTreeMap<String, Vec<String>> {
    records
        .iter()
        .map(|r| {
            (
                r.name.clone(),
                r.tuning
                    .depends_on
                    .iter()
                    .map(|d| d.name.clone())
                    .chain(r.network_container.clone())
                    .collect(),
            )
        })
        .collect()
}

/// Whether a start of `name` can wait as long as another machine takes: a machine it
/// depends on, directly or through others, has a condition that waits, and systemd
/// holds each start in the queue until the one before it is through.
pub fn waits(records: &[ImageRecord], name: &str) -> bool {
    let by_name: BTreeMap<&str, &ImageRecord> =
        records.iter().map(|r| (r.name.as_str(), r)).collect();
    let mut seen = std::collections::BTreeSet::new();
    let mut next = vec![name];
    while let Some(at) = next.pop() {
        if !seen.insert(at) {
            continue;
        }
        for dep in by_name
            .get(at)
            .map(|r| r.tuning.depends_on.as_slice())
            .unwrap_or_default()
        {
            if dep.condition.waits() {
                return true;
            }
            next.push(dep.name.as_str());
        }
    }
    false
}

/// `names` in the order to remove them: each machine before the ones it depends on or
/// takes its network from, so that those go only once it has; otherwise as given.
pub fn removal_order(records: &[ImageRecord], names: &[String]) -> Vec<String> {
    fn visit<'a>(
        name: &'a str,
        names: &[String],
        records: &'a [ImageRecord],
        seen: &mut std::collections::BTreeSet<&'a str>,
        out: &mut Vec<String>,
    ) {
        if !seen.insert(name) {
            return;
        }
        if let Some(r) = records.iter().find(|r| r.name == name) {
            let held = r
                .tuning
                .depends_on
                .iter()
                .map(|d| d.name.as_str())
                .chain(r.network_container.as_deref());
            for other in held {
                if names.iter().any(|n| n == other) {
                    visit(other, names, records, seen, out);
                }
            }
        }
        out.push(name.to_string());
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for name in names.iter().rev() {
        visit(name.as_str(), names, records, &mut seen, &mut out);
    }
    out.reverse();
    out
}

/// A path from `name` back to itself through the dependencies, both ends included.
fn cycle(name: &str, graph: &BTreeMap<String, Vec<String>>) -> Option<Vec<String>> {
    fn walk(
        at: &str,
        name: &str,
        graph: &BTreeMap<String, Vec<String>>,
        path: &mut Vec<String>,
        seen: &mut std::collections::BTreeSet<String>,
    ) -> bool {
        for next in graph.get(at).into_iter().flatten() {
            path.push(next.clone());
            if next == name {
                return true;
            }
            if seen.insert(next.clone()) && walk(next, name, graph, path, seen) {
                return true;
            }
            path.pop();
        }
        false
    }
    let mut path = vec![name.to_string()];
    let mut seen = std::collections::BTreeSet::new();
    walk(name, name, graph, &mut path, &mut seen).then_some(path)
}

/// The machines among `records` that need `name` to stay, by depending on it or taking
/// its network, leaving out those in `gone`.
pub fn holders<'a>(records: &'a [ImageRecord], name: &str, gone: &[String]) -> Vec<&'a str> {
    records
        .iter()
        .filter(|r| !gone.contains(&r.name))
        .filter(|r| {
            r.network_container.as_deref() == Some(name)
                || r.tuning.depends_on.iter().any(|d| d.name == name)
        })
        .map(|r| r.name.as_str())
        .collect()
}

/// The machines among `records` that depend on `name`, leaving out those in `going`.
pub fn dependents<'a>(records: &'a [ImageRecord], name: &str, going: &[String]) -> Vec<&'a str> {
    records
        .iter()
        .filter(|r| !going.contains(&r.name))
        .filter(|r| r.tuning.depends_on.iter().any(|d| d.name == name))
        .map(|r| r.name.as_str())
        .collect()
}

/// A machine depended on, as it is now.
#[derive(Debug, Clone)]
pub struct Observed {
    /// Whether it has a healthcheck that runs; None when nspawn has no such machine.
    healthcheck: Option<bool>,
    unit: UnitState,
    /// Its healthcheck's verdict while it runs: starting, healthy or unhealthy.
    health: Option<String>,
    /// How its program ended, once it has.
    exit_code: Option<i32>,
    /// systemd's Result= of its last run, once it ended: "success" or why not.
    result: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Met,
    Waiting,
    Failed(String),
}

/// What `dep` is to `name` as `seen` shows it.
pub fn verdict(name: &str, dep: &Dependency, seen: &Observed) -> Verdict {
    let other = &dep.name;
    let failed = || {
        Verdict::Failed(format!(
            "{other}, which {name} depends on, failed; see journalctl -u systemd-nspawn@{other}.service"
        ))
    };
    let Some(has_healthcheck) = seen.healthcheck else {
        return Verdict::Failed(format!(
            "{other}, which {name} depends on, is not a machine of nspawn's"
        ));
    };
    let unit = &seen.unit;
    match dep.condition {
        // Its start went through when this one runs: systemd orders them.
        Condition::Started if unit.active == "failed" => failed(),
        Condition::Started => Verdict::Met,
        Condition::Healthy if !has_healthcheck => Verdict::Failed(format!(
            "{other} has no healthcheck, and {name} waits for it to be healthy; give {other} one with --health-cmd, or depend on it as started"
        )),
        Condition::Healthy if unit.active == "failed" => failed(),
        // Brought back by its restart policy: its healthcheck starts over.
        Condition::Healthy if unit.restarting() => Verdict::Waiting,
        Condition::Healthy if !unit.busy() => Verdict::Failed(format!(
            "{other} ended without being healthy, which {name} waits for; see journalctl -u systemd-nspawn@{other}.service"
        )),
        Condition::Healthy => match seen.health.as_deref() {
            Some("healthy") => Verdict::Met,
            Some("unhealthy") => Verdict::Failed(format!(
                "{other}, which {name} depends on, is unhealthy; nspawn inspect {other} has its last probes"
            )),
            _ => Verdict::Waiting,
        },
        // systemd's verdict on the run, which a failure before the program started
        // shares; the exit code may be an earlier run's then, so it only explains.
        Condition::Completed => {
            let ended = unit.restarting() || matches!(unit.active.as_str(), "inactive" | "failed");
            match seen.exit_code {
                _ if !ended => Verdict::Waiting,
                _ if seen.result == "success" => Verdict::Met,
                Some(code) if code != 0 => Verdict::Failed(format!(
                    "{other}, which {name} waits for to complete, exited with {code}; nspawn logs {other} shows why"
                )),
                _ => failed(),
            }
        }
    }
}

/// `dep` as it is now.
pub async fn observe(sd: &Systemd, store: &Store, dep: &Dependency) -> Result<Observed> {
    let record = store.load_image(&dep.name)?;
    let unit = sd
        .unit_status(&format!("systemd-nspawn@{}.service", dep.name))
        .await?;
    let ended = unit.restarting() || matches!(unit.active.as_str(), "inactive" | "failed");
    let (exit_code, result) = match &record {
        Some(r) if ended && dep.condition == Condition::Completed => (
            crate::api::machines::last_exit_code(sd, store, r).await,
            sd.unit_result(&format!("systemd-nspawn@{}.service", dep.name))
                .await?,
        ),
        _ => (None, String::new()),
    };
    Ok(Observed {
        healthcheck: record
            .as_ref()
            .map(|r| r.effective_healthcheck().is_some_and(|h| !h.disabled())),
        unit,
        health: crate::health::read_status(&dep.name).map(|s| s.status),
        exit_code,
        result,
    })
}

/// Why `record`'s machine could not start because of a machine it depends on, if that
/// is why: what the hook said, for the caller of a start that failed.
pub async fn failure(sd: &Systemd, store: &Store, record: &ImageRecord) -> Option<String> {
    for dep in record.tuning.depends_on.iter().filter(|d| !d.optional) {
        let seen = observe(sd, store, dep).await.ok()?;
        if let Verdict::Failed(why) = verdict(&record.name, dep, &seen) {
            return Some(why);
        }
    }
    None
}

/// `nspawn await-dependencies NAME`, the machine's first ExecStartPre: waits, in order,
/// until each machine it depends on is what it asks of it. A failure fails this start,
/// or is a warning for an optional one.
pub async fn await_all(ctx: &Context, name: &str) -> Result<()> {
    validate_entry_name(name)?;
    let Some(record) = ctx.store.load_image(name)? else {
        return Ok(());
    };
    if record.tuning.depends_on.is_empty() {
        return Ok(());
    }
    let sd = ctx.sd().await?;
    for dep in &record.tuning.depends_on {
        let mut said = false;
        loop {
            match verdict(name, dep, &observe(sd, &ctx.store, dep).await?) {
                Verdict::Met => break,
                Verdict::Waiting => {
                    if !said {
                        eprintln!("{name} waits for {} {}", dep.name, dep.condition.awaited());
                        said = true;
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                Verdict::Failed(why) if dep.optional => {
                    eprintln!("warning: {why}; it is optional, so {name} starts anyway");
                    break;
                }
                Verdict::Failed(why) => bail!(why),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(active: &str, sub: &str) -> UnitState {
        UnitState {
            load: "loaded".to_string(),
            active: active.to_string(),
            sub: sub.to_string(),
        }
    }

    fn seen(active: &str, sub: &str) -> Observed {
        Observed {
            healthcheck: Some(true),
            unit: unit(active, sub),
            health: None,
            exit_code: None,
            result: String::new(),
        }
    }

    fn dep(text: &str) -> Dependency {
        crate::tuning::dependency(text).unwrap()
    }

    fn failed_with(v: Verdict) -> String {
        match v {
            Verdict::Failed(why) => why,
            other => panic!("not a failure: {other:?}"),
        }
    }

    #[test]
    fn a_cycle_is_found_with_its_path() {
        let graph: BTreeMap<String, Vec<String>> = [
            ("web", vec!["db", "cache"]),
            ("db", vec!["disk"]),
            ("cache", vec![]),
            ("disk", vec!["web"]),
            ("other", vec!["other2"]),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.into_iter().map(str::to_string).collect()))
        .collect();
        assert_eq!(cycle("web", &graph).unwrap(), ["web", "db", "disk", "web"]);
        assert_eq!(cycle("cache", &graph), None);
        assert_eq!(
            cycle("other", &graph),
            None,
            "a name without a record ends a path"
        );
        let mut fine = graph.clone();
        fine.insert("disk".to_string(), vec![]);
        assert_eq!(cycle("web", &fine), None);
    }

    #[test]
    fn the_machines_that_depend_on_one_are_found() {
        let record = |name: &str, deps: &[&str]| {
            let mut r: ImageRecord = serde_json::from_value(serde_json::json!({
                "name": name,
                "reference": "r",
                "manifest_digest": "d",
                "layers": [],
                "backend": "overlay",
                "created": 0,
            }))
            .unwrap();
            r.tuning.depends_on = deps.iter().map(|d| dep(d)).collect();
            r
        };
        let records = [
            record("db", &[]),
            record("web", &["db:healthy"]),
            record("api", &["cache", "db"]),
            record("cache", &[]),
        ];
        assert_eq!(dependents(&records, "db", &[]), ["web", "api"]);
        assert_eq!(dependents(&records, "db", &["web".to_string()]), ["api"]);
        assert!(dependents(&records, "web", &[]).is_empty());
        assert_eq!(graph(&records)["api"], ["cache", "db"]);
    }

    #[test]
    fn started_is_met_unless_the_start_failed() {
        let d = dep("db");
        for (active, sub) in [
            ("active", "running"),
            ("inactive", "dead"),
            ("activating", "auto-restart"),
        ] {
            assert_eq!(
                verdict("web", &d, &seen(active, sub)),
                Verdict::Met,
                "{active}"
            );
        }
        assert_eq!(
            failed_with(verdict("web", &d, &seen("failed", "failed"))),
            "db, which web depends on, failed; see journalctl -u systemd-nspawn@db.service"
        );
        let gone = Observed {
            healthcheck: None,
            ..seen("inactive", "dead")
        };
        assert_eq!(
            failed_with(verdict("web", &d, &gone)),
            "db, which web depends on, is not a machine of nspawn's"
        );
    }

    #[test]
    fn healthy_waits_for_the_verdict_of_the_healthcheck() {
        let d = dep("db:healthy");
        let with = |health: Option<&str>| Observed {
            health: health.map(str::to_string),
            ..seen("active", "running")
        };
        assert_eq!(verdict("web", &d, &with(None)), Verdict::Waiting);
        assert_eq!(
            verdict("web", &d, &with(Some("starting"))),
            Verdict::Waiting
        );
        assert_eq!(verdict("web", &d, &with(Some("healthy"))), Verdict::Met);
        assert!(failed_with(verdict("web", &d, &with(Some("unhealthy"))))
            .starts_with("db, which web depends on, is unhealthy"));
        assert_eq!(
            verdict("web", &d, &seen("activating", "auto-restart")),
            Verdict::Waiting
        );
        assert!(failed_with(verdict("web", &d, &seen("inactive", "dead")))
            .starts_with("db ended without being healthy"));
        let none = Observed {
            healthcheck: Some(false),
            ..with(Some("healthy"))
        };
        assert!(failed_with(verdict("web", &d, &none)).starts_with("db has no healthcheck"));
    }

    #[test]
    fn completed_waits_for_the_end_and_its_exit_code() {
        let d = dep("migrate:completed");
        let ended = |active: &str, sub: &str, code: Option<i32>, result: &str| Observed {
            exit_code: code,
            result: result.to_string(),
            ..seen(active, sub)
        };
        assert_eq!(
            verdict("web", &d, &seen("active", "running")),
            Verdict::Waiting
        );
        assert_eq!(
            verdict("web", &d, &seen("deactivating", "stop")),
            Verdict::Waiting
        );
        assert_eq!(
            verdict("web", &d, &ended("inactive", "dead", Some(0), "success")),
            Verdict::Met
        );
        assert_eq!(
            verdict("web", &d, &ended("inactive", "dead", None, "success")),
            Verdict::Met
        );
        assert_eq!(
            failed_with(verdict("web", &d, &ended("failed", "failed", Some(3), "exit-code"))),
            "migrate, which web waits for to complete, exited with 3; nspawn logs migrate shows why"
        );
        assert!(failed_with(verdict(
            "web",
            &d,
            &ended("failed", "failed", None, "exit-code")
        ))
        .starts_with("migrate, which web depends on, failed"));
        // A run that failed before its program started keeps the exit code of the one
        // before, a success: systemd's verdict on this run decides.
        assert!(failed_with(verdict(
            "web",
            &d,
            &ended("failed", "failed", Some(0), "exit-code")
        ))
        .starts_with("migrate, which web depends on, failed"));
        assert_eq!(
            verdict(
                "web",
                &d,
                &ended("activating", "auto-restart", Some(0), "success")
            ),
            Verdict::Met,
            "restart always brings back a program that completed"
        );
        assert!(matches!(
            verdict(
                "web",
                &d,
                &ended("activating", "auto-restart", Some(1), "exit-code")
            ),
            Verdict::Failed(_)
        ));
    }

    #[test]
    fn a_wait_further_down_the_chain_counts() {
        let record = |name: &str, deps: &[&str]| {
            let mut r: ImageRecord = serde_json::from_value(serde_json::json!({
                "name": name, "reference": "r", "manifest_digest": "d", "layers": [],
                "backend": "overlay", "created": 0,
            }))
            .unwrap();
            r.tuning.depends_on = deps.iter().map(|d| dep(d)).collect();
            r
        };
        let records = [
            record("web", &["api"]),
            record("api", &["migrate:completed", "cache"]),
            record("migrate", &[]),
            record("cache", &[]),
            record("loop-a", &["loop-b"]),
            record("loop-b", &["loop-a"]),
        ];
        assert!(waits(&records, "web"), "through api");
        assert!(waits(&records, "api"));
        assert!(!waits(&records, "cache"));
        assert!(!waits(&records, "loop-a"), "a cycle ends the walk");
        let names = |list: &[&str]| list.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        assert_eq!(
            removal_order(&records, &names(&["migrate", "cache", "web", "api"])),
            names(&["web", "api", "cache", "migrate"])
        );
        assert_eq!(
            removal_order(&records, &names(&["cache", "migrate"])),
            names(&["cache", "migrate"]),
            "unrelated machines keep their order"
        );
    }

    #[test]
    fn a_network_owner_goes_after_the_machines_on_its_network() {
        let mut side: ImageRecord = serde_json::from_value(serde_json::json!({
            "name": "side", "reference": "r", "manifest_digest": "d", "layers": [],
            "backend": "overlay", "created": 0,
        }))
        .unwrap();
        side.network_container = Some("own".into());
        let mut own = side.clone();
        own.name = "own".into();
        own.network_container = None;
        let records = [side, own];
        let names = |list: &[&str]| list.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        assert_eq!(
            removal_order(&records, &names(&["own", "side"])),
            names(&["side", "own"])
        );
        assert_eq!(holders(&records, "own", &[]), ["side"]);
        assert!(holders(&records, "own", &names(&["side"])).is_empty());
    }

    #[test]
    fn a_cycle_through_a_shared_network_is_found() {
        let mut side: ImageRecord = serde_json::from_value(serde_json::json!({
            "name": "side", "reference": "r", "manifest_digest": "d", "layers": [],
            "backend": "overlay", "created": 0,
        }))
        .unwrap();
        side.network_container = Some("web".into());
        let mut graph = graph(&[side]);
        graph.insert("web".into(), vec!["side".into()]);
        assert_eq!(cycle("web", &graph).unwrap(), ["web", "side", "web"]);
    }

    #[test]
    fn check_counts_the_network_owner_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(&tmp.path().join("machines"), &tmp.path().join("state"));
        store.init().unwrap();
        let record = |name: &str, deps: &[&str]| {
            let mut r: ImageRecord = serde_json::from_value(serde_json::json!({
                "name": name, "reference": "r", "manifest_digest": "d", "layers": [],
                "backend": "overlay", "created": 0,
            }))
            .unwrap();
            r.tuning.depends_on = deps.iter().map(|d| dep(d)).collect();
            r
        };
        store.record_image(&record("web", &["side"])).unwrap();
        store.record_image(&record("side", &[])).unwrap();
        let e = check(&store, "side", &[], Some("web")).unwrap_err();
        assert!(e.to_string().contains("cycle, side -> web -> side"), "{e}");
        check(&store, "side", &[], None).unwrap();
        assert!(check(&store, "side", &[dep("web")], None).is_err());
        assert!(check(&store, "web", &[dep("nowhere")], None)
            .unwrap_err()
            .to_string()
            .contains("no machine of nspawn's"));
    }
}
