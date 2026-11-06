//! Named volumes as the Docker API shows them: the local driver's, kept under the
//! store's volumes directory. A create, a removal and a prune go through the service's
//! methods on the bus, as `nspawn volume` asks for them.

use std::collections::BTreeMap;
use std::sync::Arc;

use hyper::{Method, Response, StatusCode};
use serde_json::{json, Value};

use super::containers::{status_of, Answer, Sent};
use super::create::{bad, Object, Opt, Opts, Refusal};
use super::filters::{Filters, Invalid};
use super::{json_response, respond, Body, Query};
use crate::api::volumes::VolumeInfo;
use crate::api::Context;
use crate::client::{self, Client};

/// The label the engine marks a volume made without a name with: nspawn keeps the mark
/// as the volume's `anonymous`, and the API shows it as this label.
pub const ANONYMOUS: &str = "com.docker.volume.anonymous";

/// The keys GET /volumes takes.
const FILTERS: &[&str] = &["dangling", "driver", "label", "name"];

/// The options a local volume of the engine takes, which mount something there.
const LOCAL_OPTIONS: &[&str] = &["type", "device", "o", "size"];

/// A change of volumes, by method and path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    /// POST /volumes/create.
    Create,
    /// DELETE /volumes/{name}.
    Remove(String),
    /// POST /volumes/prune.
    Prune,
}

impl Call {
    pub fn of(method: &Method, parts: &[&str]) -> Option<Call> {
        match parts {
            ["volumes", "create"] if *method == Method::POST => Some(Call::Create),
            ["volumes", "prune"] if *method == Method::POST => Some(Call::Prune),
            ["volumes", name] if *method == Method::DELETE => Some(Call::Remove(name.to_string())),
            _ => None,
        }
    }
}

type Served = Result<Response<Body>, Answer>;

fn says(status: StatusCode, text: impl Into<String>) -> Answer {
    Answer(status, Some(text.into()))
}

fn failed(e: impl std::fmt::Display) -> Answer {
    says(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

async fn connect() -> Result<Client, Answer> {
    Client::connect()
        .await
        .map_err(|e| failed(format!("{e:#}")))
}

/// A volume's labels as the API shows them: the anonymous mark among them.
pub fn labels(volume: &VolumeInfo) -> BTreeMap<String, String> {
    let mut labels = volume.labels.clone();
    if volume.anonymous {
        labels.insert(ANONYMOUS.to_string(), String::new());
    }
    labels
}

/// The volumes `filters` selects, as the engine reads them.
pub fn select<'a>(
    volumes: &'a [VolumeInfo],
    filters: &Filters,
) -> Result<Vec<&'a VolumeInfo>, Invalid> {
    filters.only(FILTERS)?;
    if let Some(values) = filters.values("dangling") {
        if let Some(bad) = values
            .iter()
            .find(|v| !matches!(v.as_str(), "true" | "false" | "1" | "0"))
        {
            return Err(Invalid(format!("invalid filter 'dangling={bad}'")));
        }
    }
    Ok(volumes
        .iter()
        .filter(|v| match filters.values("dangling") {
            Some(values) => {
                let want = values.iter().any(|x| x == "true" || x == "1");
                v.used_by.is_empty() == want
            }
            None => true,
        })
        .filter(|_| filters.values("driver").is_none() || filters.matches("driver", "local"))
        .filter(|v| filters.values("name").is_none() || filters.matches("name", &v.name))
        .filter(|v| filters.labels("label", &labels(v)))
        .collect())
}

/// A volume, as GET /volumes/{name} and the list show it.
pub fn json(volume: &VolumeInfo) -> Value {
    let labels = labels(volume);
    json!({
        "CreatedAt": super::util::rfc3339_nano(volume.created * 1_000_000, false),
        "Driver": "local",
        "Labels": if labels.is_empty() { Value::Null } else { json!(labels) },
        "Mountpoint": volume.path.to_string_lossy(),
        "Name": volume.name,
        "Options": null,
        "Scope": "local",
    })
}

/// A volume as the service gives it back.
fn from_dict(dict: &client::Dict) -> VolumeInfo {
    let labels = dict
        .get("labels")
        .map(|v| client::to_json(v))
        .and_then(|v| v.as_object().cloned())
        .map(|map| {
            map.into_iter()
                .map(|(k, v)| (k, v.as_str().unwrap_or_default().to_string()))
                .collect()
        })
        .unwrap_or_default();
    VolumeInfo {
        name: client::string(dict, "name"),
        path: client::string(dict, "path").into(),
        used_by: client::strings(dict, "used_by"),
        created: client::u64(dict, "created"),
        labels,
        anonymous: client::bool(dict, "anonymous"),
    }
}

/// What a create asks for: its name (empty for one the service names), and
/// CreateVolume's options.
#[derive(Debug, Clone, PartialEq)]
pub struct Creation {
    pub name: String,
    pub opts: Opts,
}

/// The engine's names of local volumes: two characters at least, the first a letter or
/// a digit.
fn check_name(name: &str) -> Result<(), Refusal> {
    let refused = |why: String| bad(format!("create {name}: {why}"));
    if name.len() == 1 {
        return Err(refused(
            "volume name is too short, names should be at least two alphanumeric characters"
                .to_string(),
        ));
    }
    let mut chars = name.chars();
    let first = chars.next().is_some_and(|c| c.is_ascii_alphanumeric());
    if !first || !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')) {
        return Err(refused(format!(
            "{name:?} includes invalid characters for a local volume name, only \"[a-zA-Z0-9][a-zA-Z0-9_.-]\" are allowed. If you intended to pass a host directory, use absolute path"
        )));
    }
    Ok(())
}

/// Whether `body` has `key`, in any case, with a value that is not null.
fn given(body: &Value, key: &str) -> bool {
    body.as_object().is_some_and(|map| {
        map.iter()
            .any(|(k, v)| k.eq_ignore_ascii_case(key) && !v.is_null())
    })
}

/// A body of POST /volumes/create as CreateVolume's arguments: the local driver alone,
/// without options, as a volume of nspawn is a directory of the host. The anonymous
/// mark comes with a volume made without a name, or with the engine's label for it.
pub fn creation(value: &Value) -> Result<Creation, Refusal> {
    let body = Object::of(value, "volume.CreateOptions")?;
    let name = body.text("Name")?;
    let driver = body.text("Driver")?;
    let options = body.pairs("DriverOpts")?;
    let mut labels = body.pairs("Labels")?;
    body.object("ClusterVolumeSpec", "volume.ClusterVolumeSpec")?;
    if given(value, "ClusterVolumeSpec") {
        return Err(Refusal {
            status: StatusCode::NOT_IMPLEMENTED,
            message: "cluster volumes are not supported by nspawn's Docker API".to_string(),
        });
    }
    if !driver.is_empty() && driver != "local" {
        return Err(Refusal {
            status: StatusCode::NOT_FOUND,
            message: format!(
                "create {name}: error looking up volume plugin {driver}: plugin \"{driver}\" not found"
            ),
        });
    }
    if !name.is_empty() {
        check_name(&name)?;
    }
    if let Some(key) = options
        .keys()
        .find(|k| !LOCAL_OPTIONS.contains(&k.as_str()))
    {
        return Err(bad(format!("create {name}: invalid option: {key:?}")));
    }
    if !options.is_empty() {
        return Err(bad(format!(
            "create {name}: driver options are not supported: nspawn volumes are directories of the host"
        )));
    }
    let anonymous = name.is_empty() || labels.remove(ANONYMOUS).is_some();
    let mut opts = Opts::new();
    // KEY=VALUE each, also for an empty value: the service reads a lone "none" as no
    // labels at all.
    let labels: Vec<String> = labels.iter().map(|(k, v)| format!("{k}={v}")).collect();
    if !labels.is_empty() {
        opts.insert("labels", Opt::Texts(labels));
    }
    if anonymous {
        opts.insert("anonymous", Opt::Flag(true));
    }
    Ok(Creation { name, opts })
}

/// POST /volumes/create: the volume as the service has it once made, or as it was.
async fn create(sent: Sent) -> Served {
    let body = super::networks::read_json(sent).await?;
    let creation = creation(&body)?;
    let client = connect().await?;
    let dict = client
        .manager
        .create_volume(&creation.name, super::create::to_bus(&creation.opts))
        .await
        .map_err(client::error)
        .map_err(|e| {
            let message = format!("{e:#}");
            says(
                status_of(client::error_name(&e)),
                format!("create {}: {message}", creation.name),
            )
        })?;
    Ok(json_response(StatusCode::CREATED, &json(&from_dict(&dict))))
}

/// What a failed removal answers: one that is not there is no error with force, as the
/// engine forgets a volume it does not have; one in use names the containers that use
/// it.
pub fn remove_failed(
    kind: Option<&str>,
    name: &str,
    force: bool,
    users: &[String],
    message: &str,
) -> Option<Answer> {
    match kind {
        Some("NoSuchVolume") if force => None,
        Some("NoSuchVolume") | Some("Invalid") => Some(says(
            StatusCode::NOT_FOUND,
            format!("get {name}: no such volume"),
        )),
        Some("InUse") => Some(says(
            StatusCode::CONFLICT,
            format!("remove {name}: volume is in use - [{}]", users.join(", ")),
        )),
        kind => Some(says(status_of(kind), format!("remove {name}: {message}"))),
    }
}

/// The container IDs of the machines that use `name`.
fn users_of(ctx: &Context, name: &str) -> Vec<String> {
    let Ok(volumes) = crate::api::volumes::list(&ctx.store) else {
        return Vec::new();
    };
    volumes
        .iter()
        .filter(|v| v.name == name)
        .flat_map(|v| v.used_by.iter())
        .filter_map(|machine| ctx.store.load_image(machine).ok().flatten())
        .map(|record| super::util::container_id(&record.name, record.created))
        .collect()
}

/// DELETE /volumes/{name}: RemoveVolumes of that one.
async fn remove(ctx: &Context, name: &str, query: &Query) -> Served {
    let force = query.flag("force", false);
    let client = connect().await?;
    let names = [name.to_string()];
    let removal = client
        .run_job_silent(|| client.manager.remove_volumes(&names), true)
        .await;
    if let Err(e) = removal {
        let kind = client::error_name(&e);
        let users = if kind == Some("InUse") {
            users_of(ctx, name)
        } else {
            Vec::new()
        };
        if let Some(answer) = remove_failed(kind, name, force, &users, &format!("{e:#}")) {
            return Err(answer);
        }
    }
    Ok(respond(StatusCode::NO_CONTENT, "", bytes::Bytes::new()))
}

/// The filters of POST /volumes/prune: `all` (one boolean, as Go's ParseBool reads it),
/// `label` and `label!`. Without `all` only the volumes the API made without a name go;
/// clients of API versions before 1.42 get every unused one, as they were promised.
pub fn prune_all(filters: &Filters, version: Option<(u32, u32)>) -> Result<bool, Answer> {
    let refuse = |text: String| says(StatusCode::BAD_REQUEST, text);
    filters
        .only(&["label", "label!", "all"])
        .map_err(|Invalid(text)| refuse(text))?;
    let all = match filters.values("all") {
        None => false,
        Some(values) if values.len() > 1 => {
            let listed: Vec<&str> = values.iter().map(String::as_str).collect();
            return Err(refuse(format!(
                "invalid filter 'all=[{}]': only one value is expected",
                listed.join(" ")
            )));
        }
        Some(values) => {
            let value = values.iter().next().map(String::as_str).unwrap_or_default();
            match value {
                "1" | "t" | "T" | "TRUE" | "true" | "True" => true,
                "0" | "f" | "F" | "FALSE" | "false" | "False" => false,
                _ => {
                    return Err(refuse(format!(
                        "invalid filter 'all': strconv.ParseBool: parsing {}: invalid syntax",
                        crate::compose::gostr::quote(value)
                    )))
                }
            }
        }
    };
    Ok(all || version.is_some_and(|v| v < (1, 42)))
}

/// The volumes a prune takes: unused, anonymous unless `all`, matching every `label`
/// and not every `label!`.
pub fn prunable<'a>(volumes: &'a [VolumeInfo], filters: &Filters, all: bool) -> Vec<&'a str> {
    volumes
        .iter()
        .filter(|v| v.used_by.is_empty() && (all || v.anonymous))
        .filter(|v| {
            let labels = labels(v);
            filters.labels("label", &labels)
                && !(filters.values("label!").is_some() && filters.labels("label!", &labels))
        })
        .map(|v| v.name.as_str())
        .collect()
}

/// POST /volumes/prune: RemoveVolumes of the volumes the filters select, measured
/// before they go. One that could not go (a machine came to use it) stays, as the
/// engine leaves it.
async fn prune(ctx: &Context, query: &Query, version: Option<(u32, u32)>) -> Served {
    let filters = Filters::parse(query.get("filters"))
        .map_err(|Invalid(text)| says(StatusCode::BAD_REQUEST, text))?;
    let all = prune_all(&filters, version)?;
    let volumes = crate::api::volumes::list(&ctx.store).map_err(|e| failed(format!("{e:#}")))?;
    let chosen = prunable(&volumes, &filters, all);
    if chosen.is_empty() {
        return Ok(json_response(
            StatusCode::OK,
            &json!({ "VolumesDeleted": [], "SpaceReclaimed": 0 }),
        ));
    }
    let sizes: BTreeMap<&str, u64> = chosen
        .iter()
        .filter_map(|name| volumes.iter().find(|v| v.name == *name))
        .map(|v| {
            let size = super::images::walking(|| {
                crate::api::disk::tree_size(&v.path, &mut Default::default())
            });
            (v.name.as_str(), size)
        })
        .collect();
    let names: Vec<String> = chosen.iter().map(|n| n.to_string()).collect();
    let client = connect().await?;
    let outcome = client
        .run_job_silent(|| client.manager.remove_volumes(&names), true)
        .await;
    let result = match outcome {
        Ok(result) => result,
        Err(e) => match client::removed_before(e) {
            Ok(result) => result,
            Err(e) => return Err(says(status_of(client::error_name(&e)), format!("{e:#}"))),
        },
    };
    let removed = client::strings(&result, "removed");
    let reclaimed: u64 = removed
        .iter()
        .filter_map(|name| sizes.get(name.as_str()))
        .sum();
    Ok(json_response(
        StatusCode::OK,
        &json!({ "VolumesDeleted": removed, "SpaceReclaimed": reclaimed }),
    ))
}

/// Serves a change of volumes.
pub(super) async fn serve(
    ctx: &Arc<Context>,
    call: Call,
    query: &Query,
    sent: Sent,
) -> Response<Body> {
    let version = sent.version;
    let served = match call {
        Call::Create => create(sent).await,
        Call::Remove(name) => remove(ctx, &name, query).await,
        Call::Prune => prune(ctx, query, version).await,
    };
    served.unwrap_or_else(Answer::respond)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn volume(name: &str, used_by: &[&str]) -> VolumeInfo {
        VolumeInfo {
            name: name.to_string(),
            path: PathBuf::from(format!("/var/lib/nspawn/volumes/{name}")),
            used_by: used_by.iter().map(|u| u.to_string()).collect(),
            created: 1_700_000_000,
            ..Default::default()
        }
    }

    fn labelled(name: &str, pairs: &[(&str, &str)], anonymous: bool) -> VolumeInfo {
        VolumeInfo {
            labels: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            anonymous,
            ..volume(name, &[])
        }
    }

    #[test]
    fn volumes_are_selected_as_the_engine_filters_them() {
        let volumes = [
            volume("data", &["web"]),
            volume("cache", &[]),
            labelled("kept", &[("tier", "data")], false),
            labelled("loose", &[], true),
        ];
        let names = |text: &str| -> Vec<String> {
            let filters = Filters::parse(Some(text)).unwrap();
            select(&volumes, &filters)
                .unwrap()
                .iter()
                .map(|v| v.name.clone())
                .collect()
        };
        assert_eq!(names("{}"), ["data", "cache", "kept", "loose"]);
        assert_eq!(
            names(r#"{"dangling":["true"]}"#),
            ["cache", "kept", "loose"]
        );
        assert_eq!(names(r#"{"dangling":["false"]}"#), ["data"]);
        assert_eq!(names(r#"{"name":["ach"]}"#), ["cache"]);
        assert_eq!(names(r#"{"driver":["local"]}"#).len(), 4);
        assert!(names(r#"{"driver":["nfs"]}"#).is_empty());
        assert!(names(r#"{"label":["a"]}"#).is_empty());
        assert_eq!(names(r#"{"label":["tier"]}"#), ["kept"]);
        assert_eq!(names(r#"{"label":["tier=data"]}"#), ["kept"]);
        assert!(names(r#"{"label":["tier=web"]}"#).is_empty());
        assert_eq!(names(&format!(r#"{{"label":["{ANONYMOUS}"]}}"#)), ["loose"]);
        let bad = Filters::parse(Some(r#"{"nope":["x"]}"#)).unwrap();
        assert!(select(&volumes, &bad).is_err());
    }

    #[test]
    fn shapes_against_docker() {
        use crate::dockerapi::shapes::{diff, golden};
        let docker = golden(include_str!(
            "../../tests/dockerapi/golden/volumes-dangling.body"
        ));
        let ours = json!({ "Volumes": [json(&volume("loose", &[]))], "Warnings": null });
        assert_eq!(diff(&ours, &docker), (vec![], vec![]), "list");
        let docker = golden(include_str!(
            "../../tests/dockerapi/golden/volume-kept.body"
        ));
        let ours = json(&labelled(
            "kept",
            &[("e2e", "yes"), ("tier", "data")],
            false,
        ));
        assert_eq!(diff(&ours, &docker), (vec![], vec![]), "labelled");
        assert_eq!(ours["Labels"], docker["Labels"]);
        let docker = golden(include_str!(
            "../../tests/dockerapi/golden/volume-missing.body"
        ));
        assert_eq!(docker["message"], "get nope: no such volume");
    }

    #[test]
    fn a_volume_is_the_local_driver_s() {
        let v = json(&volume("data", &[]));
        assert_eq!(v["Driver"], "local");
        assert_eq!(v["Scope"], "local");
        assert_eq!(v["Mountpoint"], "/var/lib/nspawn/volumes/data");
        assert_eq!(v["Name"], "data");
        assert_eq!(v["Labels"], Value::Null);
        // The mark of one made without a name reads as the engine's label.
        let v = json(&labelled("0123", &[("a", "b")], true));
        assert_eq!(v["Labels"], json!({ "a": "b", ANONYMOUS: "" }));
    }

    fn creation_of(body: Value) -> Result<Creation, Refusal> {
        creation(&body)
    }

    #[test]
    fn a_create_takes_a_name_and_labels_of_the_local_driver() {
        let made = creation_of(json!({
            "Name": "runner-abc-cache-0123456789abcdef",
            "Driver": "local",
            "Labels": { "com.gitlab.gitlab-runner.type": "cache", "empty": "" },
        }))
        .unwrap();
        assert_eq!(made.name, "runner-abc-cache-0123456789abcdef");
        assert_eq!(
            made.opts,
            Opts::from([(
                "labels",
                Opt::Texts(vec![
                    "com.gitlab.gitlab-runner.type=cache".into(),
                    "empty=".into()
                ])
            )])
        );
        // No name: the service names it, and the mark comes with it.
        let made = creation_of(json!({})).unwrap();
        assert_eq!(made.name, "");
        assert_eq!(made.opts, Opts::from([("anonymous", Opt::Flag(true))]));
        let made = creation_of(Value::Null).unwrap();
        assert_eq!(made.opts["anonymous"], Opt::Flag(true));
        // The engine's label for the mark is the mark.
        let made = creation_of(json!({ "Name": "tmp1", "Labels": { ANONYMOUS: "" } })).unwrap();
        assert_eq!(made.opts, Opts::from([("anonymous", Opt::Flag(true))]));
        // Go's decoder: any case of a key, null as nothing.
        let made = creation_of(json!({ "name": "lower", "Labels": null })).unwrap();
        assert_eq!(made.name, "lower");
        assert!(made.opts.is_empty());
    }

    #[test]
    fn a_create_is_refused_in_the_engine_s_words() {
        let refused = |body: Value| {
            let r = creation_of(body).unwrap_err();
            (r.status, r.message)
        };
        assert_eq!(
            refused(json!({ "Name": "x" })),
            (
                StatusCode::BAD_REQUEST,
                "create x: volume name is too short, names should be at least two alphanumeric characters".to_string()
            )
        );
        for name in ["_data", "-data", ".data", "a/b", "a b"] {
            let (status, message) = refused(json!({ "Name": name }));
            assert_eq!(status, StatusCode::BAD_REQUEST, "{name}");
            assert!(
                message.starts_with(&format!(
                    "create {name}: \"{name}\" includes invalid characters for a local volume name"
                )),
                "{message}"
            );
        }
        assert_eq!(
            refused(json!({ "Name": "data", "Driver": "nfs" })),
            (
                StatusCode::NOT_FOUND,
                "create data: error looking up volume plugin nfs: plugin \"nfs\" not found"
                    .to_string()
            )
        );
        assert_eq!(
            refused(json!({ "Name": "data", "DriverOpts": { "nope": "1" } })),
            (
                StatusCode::BAD_REQUEST,
                "create data: invalid option: \"nope\"".to_string()
            )
        );
        let (status, message) =
            refused(json!({ "Name": "data", "DriverOpts": { "type": "nfs", "device": ":/x" } }));
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(message.contains("not supported"), "{message}");
        let (status, message) =
            refused(json!({ "Name": "data", "ClusterVolumeSpec": { "Group": "g" } }));
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
        assert!(message.contains("not supported"), "{message}");
        let (status, message) = refused(json!({ "Name": 1 }));
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            message.contains("volume.CreateOptions.Name of type string"),
            "{message}"
        );
    }

    #[test]
    fn a_removal_answers_as_the_engine() {
        let ids = vec!["ab".repeat(32)];
        assert_eq!(
            remove_failed(
                Some("NoSuchVolume"),
                "gone",
                false,
                &[],
                "no volume named gone"
            ),
            Some(says(StatusCode::NOT_FOUND, "get gone: no such volume"))
        );
        // Force forgets one that is not there.
        assert_eq!(
            remove_failed(
                Some("NoSuchVolume"),
                "gone",
                true,
                &[],
                "no volume named gone"
            ),
            None
        );
        assert_eq!(
            remove_failed(
                Some("InUse"),
                "data",
                true,
                &ids,
                "volume data is in use by web"
            ),
            Some(says(
                StatusCode::CONFLICT,
                format!("remove data: volume is in use - [{}]", ids[0])
            ))
        );
        assert_eq!(
            remove_failed(None, "data", false, &[], "boom"),
            Some(says(StatusCode::INTERNAL_SERVER_ERROR, "remove data: boom"))
        );
    }

    #[test]
    fn a_prune_takes_the_anonymous_ones_unless_all() {
        let volumes = [
            volume("used", &["web"]),
            labelled("named", &[("keep", "yes")], false),
            labelled("loose", &[], true),
            labelled("tagged", &[("tier", "x")], true),
        ];
        let pruned = |text: &str, version: Option<(u32, u32)>| -> Result<Vec<&str>, Answer> {
            let filters = Filters::parse(Some(text)).unwrap();
            let all = prune_all(&filters, version)?;
            Ok(prunable(&volumes, &filters, all))
        };
        let latest = Some((1, 51));
        assert_eq!(pruned("{}", latest).unwrap(), ["loose", "tagged"]);
        assert_eq!(pruned("{}", None).unwrap(), ["loose", "tagged"]);
        assert_eq!(
            pruned(r#"{"all":["true"]}"#, latest).unwrap(),
            ["named", "loose", "tagged"]
        );
        // Clients of API versions before 1.42 prune every unused one.
        assert_eq!(
            pruned("{}", Some((1, 41))).unwrap(),
            ["named", "loose", "tagged"]
        );
        assert_eq!(
            pruned(r#"{"all":["0"]}"#, latest).unwrap(),
            ["loose", "tagged"]
        );
        assert_eq!(pruned(r#"{"label":["tier"]}"#, latest).unwrap(), ["tagged"]);
        assert_eq!(
            pruned(r#"{"label!":["tier=x"]}"#, latest).unwrap(),
            ["loose"]
        );
        assert_eq!(
            pruned(r#"{"all":["1"],"label!":["keep"]}"#, latest).unwrap(),
            ["loose", "tagged"]
        );
        for (text, message) in [
            (r#"{"dangling":["true"]}"#, "invalid filter 'dangling'"),
            (
                r#"{"all":["1","true"]}"#,
                "invalid filter 'all=[1 true]': only one value is expected",
            ),
            (
                r#"{"all":["yes"]}"#,
                "invalid filter 'all': strconv.ParseBool: parsing \"yes\": invalid syntax",
            ),
        ] {
            assert_eq!(
                pruned(text, latest).unwrap_err(),
                says(StatusCode::BAD_REQUEST, message),
                "{text}"
            );
        }
    }

    #[test]
    fn the_routes_of_changes() {
        fn parts(path: &str) -> Vec<&str> {
            path.split('/').collect()
        }
        assert_eq!(
            Call::of(&Method::POST, &parts("volumes/create")),
            Some(Call::Create)
        );
        assert_eq!(
            Call::of(&Method::POST, &parts("volumes/prune")),
            Some(Call::Prune)
        );
        assert_eq!(
            Call::of(&Method::DELETE, &parts("volumes/data")),
            Some(Call::Remove("data".into()))
        );
        assert_eq!(Call::of(&Method::GET, &parts("volumes/data")), None);
        assert_eq!(Call::of(&Method::PUT, &parts("volumes/data")), None);
        assert_eq!(Call::of(&Method::POST, &parts("volumes")), None);
    }
}
