//! The changes the Docker API makes to networks: create, connect, disconnect, remove
//! and prune, each through the service's method on the bus, as `nspawn network` asks for
//! them. A network made here keeps the name the API gave it beside its own name, which
//! the service derives when the given one is not a name a network of nspawn can take;
//! the API shows and looks it up by the given one.

use std::sync::Arc;

use bytes::Bytes;
use hyper::{Method, Response, StatusCode};
use serde_json::{json, Value};
use zbus::zvariant::Value as Bus;

use super::containers::{check_json, read_body, status_of, Answer, Sent};
use super::create::{bad, decode, Object, Opt, Opts, Refusal};
use super::filters::{Filters, Invalid};
use super::model::{Machine, NetworkView, Snapshot};
use super::util::network_id;
use super::{json_response, lookup_status, respond, Body, Query};
use crate::api::Context;
use crate::bridge::{Subnet, DEFAULT_NETWORK};
use crate::client::{self, Client, Options};
use crate::settings::Network;

/// A change of networks, by method and path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    /// POST /networks/create.
    Create,
    /// POST /networks/{id}/connect.
    Connect(String),
    /// POST /networks/{id}/disconnect.
    Disconnect(String),
    /// DELETE /networks/{id}.
    Remove(String),
    /// POST /networks/prune.
    Prune,
}

impl Call {
    pub fn of(method: &Method, parts: &[&str]) -> Option<Call> {
        let post = *method == Method::POST;
        match parts {
            ["networks", "create"] if post => Some(Call::Create),
            ["networks", "prune"] if post => Some(Call::Prune),
            ["networks", id, "connect"] if post => Some(Call::Connect(id.to_string())),
            ["networks", id, "disconnect"] if post => Some(Call::Disconnect(id.to_string())),
            ["networks", id] if *method == Method::DELETE => Some(Call::Remove(id.to_string())),
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

/// An error of the bus as it is: its status and its words.
fn as_is(e: &anyhow::Error) -> Answer {
    says(status_of(client::error_name(e)), format!("{e:#}"))
}

async fn connect_bus() -> Result<Client, Answer> {
    Client::connect()
        .await
        .map_err(|e| failed(format!("{e:#}")))
}

async fn snapshot(ctx: &Context) -> Result<Snapshot, Answer> {
    Snapshot::take(ctx)
        .await
        .map_err(|e| failed(format!("{e:#}")))
}

fn not_found(key: &str) -> Answer {
    says(StatusCode::NOT_FOUND, format!("network {key} not found"))
}

/// The body of a request that takes JSON, as the engine reads one: of type
/// application/json or none, and no body at all the zero value.
pub(super) async fn read_json(sent: Sent) -> Result<Value, Answer> {
    check_json(&sent.headers)?;
    let chunked = sent
        .headers
        .get_all(hyper::header::TRANSFER_ENCODING)
        .iter()
        .any(|v| {
            v.to_str()
                .is_ok_and(|v| v.to_ascii_lowercase().contains("chunked"))
        });
    let bytes = read_body(sent.body).await?;
    if bytes.is_empty() && !chunked {
        return Ok(json!({}));
    }
    Ok(decode(&bytes)?)
}

/// The networks the engine has of its own, which the API neither makes nor removes.
const PREDEFINED: &[&str] = &["bridge", "host", "none", "default"];

/// The driver options the engine reads itself, besides the driver's.
const ENABLE_IPV4: &str = "com.docker.network.enable_ipv4";
const ENABLE_IPV6: &str = "com.docker.network.enable_ipv6";

/// What a create asks for: the name the API gives the network, CreateNetwork's options
/// and what the answer warns of.
#[derive(Debug, Clone, PartialEq)]
pub struct Creation {
    pub name: String,
    pub opts: Opts,
    pub warnings: Vec<String>,
}

/// A boolean of the driver options, as Go's ParseBool reads it.
fn option_flag(
    options: &std::collections::BTreeMap<String, String>,
    key: &str,
) -> Result<Option<bool>, Refusal> {
    match options.get(key).map(String::as_str) {
        None => Ok(None),
        Some("1" | "t" | "T" | "TRUE" | "true" | "True") => Ok(Some(true)),
        Some("0" | "f" | "F" | "FALSE" | "false" | "False") => Ok(Some(false)),
        Some(_) => Err(bad(format!("driver-opt {key:?} is not a valid bool"))),
    }
}

/// The subnet of the IPAM configuration, IPv4 and one at most, with what nspawn cannot
/// make of the rest refused: its bridge takes the subnet's first address and its
/// machines get theirs from the whole subnet.
fn subnet_of(ipam: &Object, warnings: &mut Vec<String>) -> Result<Option<Subnet>, Refusal> {
    let driver = ipam.text("Driver")?;
    if !driver.is_empty() && driver != "default" {
        return Err(bad(format!(
            "IPAM driver {driver} is not supported: nspawn gives the addresses of its networks itself"
        )));
    }
    if !ipam.pairs("Options")?.is_empty() {
        warnings.push(
            "IPAM options are ignored: nspawn gives the addresses of its networks itself"
                .to_string(),
        );
    }
    let mut found = None;
    for config in ipam.objects("Config", "network.IPAMConfig")? {
        let text = config.text("Subnet")?;
        if text.contains(':') {
            return Err(bad(format!(
                "IPv6 subnet {text} is not supported: nspawn's networks carry IPv4 only"
            )));
        }
        let invalid = || {
            bad(format!(
                "invalid subnet {text}: invalid CIDR block notation"
            ))
        };
        let (address, prefix) = text.split_once('/').ok_or_else(invalid)?;
        let address: std::net::Ipv4Addr = address.parse().map_err(|_| invalid())?;
        if prefix.parse::<u8>().map_or(true, |p| p > 32) {
            return Err(invalid());
        }
        let subnet: Subnet = text.parse().map_err(|e| bad(format!("{e:#}")))?;
        if subnet.network != address {
            return Err(bad(format!("invalid subnet {text}: it should be {subnet}")));
        }
        if found.is_some() {
            return Err(bad(
                "more than one IPv4 subnet is not supported: a network of nspawn has one",
            ));
        }
        let range = config.text("IPRange")?;
        if !range.is_empty() && range != text && range != subnet.to_string() {
            return Err(bad(format!(
                "ip-range {range} is not supported: machines get addresses from the whole subnet of a network of nspawn"
            )));
        }
        let gateway = config.text("Gateway")?;
        if !gateway.is_empty() && gateway != subnet.gateway().to_string() {
            return Err(bad(format!(
                "gateway {gateway} is not supported: the bridge of a network of nspawn takes the subnet's first address, {}",
                subnet.gateway()
            )));
        }
        if !config.pairs("AuxiliaryAddresses")?.is_empty() {
            return Err(bad(
                "auxiliary addresses are not supported: nspawn gives the addresses of its networks itself",
            ));
        }
        found = Some(subnet);
    }
    Ok(found)
}

/// A body of POST /networks/create as CreateNetwork's options: a bridge of the host with
/// an IPv4 subnet, internal or not, with labels. What would make another kind of network
/// is refused; what changes nothing the network does (Attachable, CheckDuplicate) is
/// taken, and driver options are warned of. `version` is the API version the request
/// names, before which EnableIPv4 was not read.
pub fn creation(value: &Value, version: Option<(u32, u32)>) -> Result<Creation, Refusal> {
    let body = Object::of(value, "network.CreateRequest")?;
    let name = body.text("Name")?;
    if PREDEFINED.contains(&name.as_str()) {
        return Err(Refusal {
            status: StatusCode::FORBIDDEN,
            message: format!("operation is not permitted on predefined {name} network "),
        });
    }
    // A machine's network veth is a mode of its own, which a name would shadow.
    if name == "veth" {
        return Err(Refusal {
            status: StatusCode::BAD_REQUEST,
            message: "network name veth is reserved by nspawn".to_string(),
        });
    }
    let mut warnings = Vec::new();
    let driver = body.text("Driver")?;
    if !driver.is_empty() && driver != "bridge" {
        return Err(bad(format!(
            "driver {driver} is not supported: nspawn's networks are bridges of the host"
        )));
    }
    let scope = body.text("Scope")?;
    if !scope.is_empty() && scope != "local" {
        return Err(bad(format!(
            "scope {scope} is not supported: nspawn's networks are of this host"
        )));
    }
    let options = body.pairs("Options")?;
    let ipv4 = if version.is_none_or(|v| v >= (1, 48)) {
        body.maybe_flag("EnableIPv4")?
    } else {
        None
    };
    let ipv4 = match ipv4 {
        Some(flag) => Some(flag),
        None => option_flag(&options, ENABLE_IPV4)?,
    };
    if ipv4 == Some(false) {
        return Err(bad(
            "a network without IPv4 is not supported: nspawn's networks carry IPv4",
        ));
    }
    let ipv6 = match body.maybe_flag("EnableIPv6")? {
        Some(flag) => Some(flag),
        None => option_flag(&options, ENABLE_IPV6)?,
    };
    if ipv6 == Some(true) {
        return Err(bad(
            "IPv6 is not supported: nspawn's networks carry IPv4 only",
        ));
    }
    let ignored: Vec<&str> = options
        .keys()
        .map(String::as_str)
        .filter(|k| *k != ENABLE_IPV4 && *k != ENABLE_IPV6)
        .collect();
    if !ignored.is_empty() {
        warnings.push(format!(
            "driver options are ignored: {}",
            ignored.join(", ")
        ));
    }
    let subnet = subnet_of(&body.object("IPAM", "network.IPAM")?, &mut warnings)?;
    if body.flag("Ingress")? {
        return Err(bad(
            "ingress networks are not supported: nspawn has no swarm",
        ));
    }
    if body.flag("ConfigOnly")? {
        return Err(bad(
            "config-only networks are not supported by nspawn's Docker API",
        ));
    }
    if !body
        .object("ConfigFrom", "network.ConfigReference")?
        .text("Network")?
        .is_empty()
    {
        return Err(bad(
            "ConfigFrom is not supported: nspawn has no config-only networks",
        ));
    }
    let internal = body.flag("Internal")?;
    let labels = body.pairs("Labels")?;
    body.flag("Attachable")?;
    if name.trim().is_empty() {
        return Err(bad("invalid name: name is empty"));
    }
    let mut opts = Opts::from([("api_name", Opt::Text(name.clone()))]);
    if let Some(subnet) = subnet {
        opts.insert("subnet", Opt::Text(subnet.to_string()));
    }
    if internal {
        opts.insert("internal", Opt::Flag(true));
    }
    if !labels.is_empty() {
        opts.insert(
            "labels",
            Opt::Texts(labels.iter().map(|(k, v)| format!("{k}={v}")).collect()),
        );
    }
    Ok(Creation {
        name,
        opts,
        warnings,
    })
}

/// What a failed create answers.
pub fn create_failed(kind: Option<&str>, name: &str, message: &str) -> Answer {
    match kind {
        Some("Exists") => says(
            StatusCode::CONFLICT,
            format!("network with name {name} already exists"),
        ),
        kind => says(status_of(kind), message),
    }
}

/// The body of a create's answer.
pub fn created_body(made: &client::Dict, warnings: &[String]) -> Value {
    json!({
        "Id": network_id(&client::string(made, "name"), client::u64(made, "created")),
        "Warning": warnings.join("; "),
    })
}

/// POST /networks/create: CreateNetwork with the given name as the API's, the network's
/// own made of it when it is not one.
async fn create(sent: Sent) -> Served {
    let version = sent.version;
    let body = read_json(sent).await?;
    let creation = creation(&body, version)?;
    let client = connect_bus().await?;
    let made = client
        .manager
        .create_network("", super::create::to_bus(&creation.opts))
        .await
        .map_err(client::error)
        .map_err(|e| create_failed(client::error_name(&e), &creation.name, &format!("{e:#}")))?;
    Ok(json_response(
        StatusCode::CREATED,
        &created_body(&made, &creation.warnings),
    ))
}

/// The ConnectNetwork options an endpoint's settings ask for: aliases and an IPv4
/// address. A MAC address, driver options and a priority are left to nspawn, which
/// derives the first from the address.
pub fn endpoint_options(endpoint: &Object) -> Result<Opts, Refusal> {
    let mut opts = Opts::new();
    let aliases = endpoint.texts("Aliases")?;
    if !aliases.is_empty() {
        opts.insert("aliases", Opt::Texts(aliases));
    }
    let ipam = endpoint.object("IPAMConfig", "network.EndpointIPAMConfig")?;
    let address = ipam.text("IPv4Address")?;
    if !address.is_empty() {
        opts.insert("address", Opt::Text(address));
    }
    if !ipam.text("IPv6Address")?.is_empty() {
        return Err(bad(
            "IPAMConfig.IPv6Address is not supported: nspawn's networks carry IPv4 only",
        ));
    }
    if !ipam.texts("LinkLocalIPs")?.is_empty() {
        return Err(bad(
            "IPAMConfig.LinkLocalIPs is not supported: nspawn's networks carry no link-local addresses",
        ));
    }
    if !endpoint.texts("Links")?.is_empty() {
        return Err(bad(
            "links are not supported by a network connect of nspawn's Docker API: a container reaches another by its name or an alias on the network",
        ));
    }
    Ok(opts)
}

const HOST_CONFLICT: &str =
    "container cannot be disconnected from host network or connected to host network";
const SHARED_CONFLICT: &str =
    "container sharing network namespace with another container or host cannot be connected to any other network";
const NONE_CONFLICT: &str =
    "container cannot be connected to multiple networks with one of the networks in private (none) mode";

/// What the engine refuses of a connect before it tries: a container of the host's
/// network, of another's or of none joins nothing, and only those of the host's network
/// join it.
pub fn connect_refusal(machine: &Machine, network: &NetworkView) -> Option<Answer> {
    let record = &machine.record;
    let refuse = |text: &str| Some(says(StatusCode::BAD_REQUEST, text));
    if record.network == Network::Host {
        return refuse(HOST_CONFLICT);
    }
    if record.network_container.is_some() {
        return refuse(SHARED_CONFLICT);
    }
    if record.no_network {
        return refuse(NONE_CONFLICT);
    }
    match network.spec {
        Some(_) => None,
        None if network.name == "host" => refuse(
            "cannot connect container to host network - container must be created in host network mode",
        ),
        None => refuse(NONE_CONFLICT),
    }
}

/// What a failed connect answers.
pub fn connect_failed(
    kind: Option<&str>,
    machine: &Machine,
    network: &NetworkView,
    message: &str,
) -> Answer {
    match kind {
        Some("Exists") => says(
            StatusCode::FORBIDDEN,
            format!(
                "endpoint with name {} already exists in network {}",
                machine.api_name(),
                network.shown
            ),
        ),
        kind => says(status_of(kind), message),
    }
}

/// POST /networks/{id}/connect: the container joins the network, at once when it runs.
async fn connect(ctx: &Context, key: &str, sent: Sent) -> Served {
    let body = read_json(sent).await?;
    let request = Object::of(&body, "network.ConnectOptions")?;
    let container = request.text("Container")?;
    let opts = endpoint_options(&request.object("EndpointConfig", "network.EndpointSettings")?)?;
    let snapshot = snapshot(ctx).await?;
    let machine = snapshot
        .find(&container)
        .map_err(|e| says(lookup_status(&e), e))?;
    let network = snapshot.network(key).ok_or_else(|| not_found(key))?;
    if let Some(refusal) = connect_refusal(machine, &network) {
        return Err(refusal);
    }
    let client = connect_bus().await?;
    client
        .run_job_silent(
            || {
                client.manager.connect_network(
                    &network.name,
                    machine.name(),
                    super::create::to_bus(&opts),
                )
            },
            true,
        )
        .await
        .map_err(|e| {
            connect_failed(client::error_name(&e), machine, &network, &format!("{e:#}"))
        })?;
    Ok(respond(StatusCode::OK, "", Bytes::new()))
}

/// What the engine refuses of a disconnect from the host's network or none, which
/// nspawn's machines do not leave.
pub fn disconnect_refusal(network: &NetworkView) -> Option<Answer> {
    match network.spec {
        Some(_) => None,
        None if network.name == "host" => Some(says(
            StatusCode::BAD_REQUEST,
            "cannot disconnect container from host network - container was created in host network mode",
        )),
        None => Some(says(
            StatusCode::BAD_REQUEST,
            format!("cannot disconnect container from network {}: nspawn's machines on none have no network to leave", network.shown),
        )),
    }
}

/// POST /networks/{id}/disconnect: the container leaves the network, at once when it
/// runs. With Force a container or a membership that is not there is no error.
async fn disconnect(ctx: &Context, key: &str, sent: Sent) -> Served {
    let body = read_json(sent).await?;
    let request = Object::of(&body, "network.DisconnectOptions")?;
    let container = request.text("Container")?;
    let force = request.flag("Force")?;
    let snapshot = snapshot(ctx).await?;
    let machine = match snapshot.find(&container) {
        Ok(m) => m.name().to_string(),
        Err(_) if force => container.clone(),
        Err(e) => return Err(says(lookup_status(&e), e)),
    };
    let network = snapshot.network(key).ok_or_else(|| not_found(key))?;
    if let Some(refusal) = disconnect_refusal(&network) {
        return Err(refusal);
    }
    let client = connect_bus().await?;
    let mut options = Options::new();
    options.insert("force", Bus::from(force));
    client
        .run_job_silent(
            || {
                client
                    .manager
                    .disconnect_network(&network.name, &machine, options)
            },
            true,
        )
        .await
        .map_err(|e| as_is(&e))?;
    Ok(respond(StatusCode::OK, "", Bytes::new()))
}

/// Whether the API may remove `network`: none of the engine's own, nspawn's default
/// network among them.
pub fn removable(network: &NetworkView) -> Result<(), Answer> {
    if network.spec.is_none() || network.name == DEFAULT_NETWORK {
        return Err(says(
            StatusCode::FORBIDDEN,
            format!(
                "{} is a pre-defined network and cannot be removed",
                network.shown
            ),
        ));
    }
    Ok(())
}

/// The machines whose records name the network whose own name is `own`, by the names
/// the API knows them by.
fn members(snapshot: &Snapshot, own: &str) -> Vec<String> {
    snapshot
        .machines
        .iter()
        .filter(|m| crate::bridge::joins(&m.record, own))
        .map(|m| m.api_name().to_string())
        .collect()
}

/// What a failed removal answers: one in use names the containers that name it.
pub fn remove_failed(
    kind: Option<&str>,
    network: &NetworkView,
    members: &[String],
    message: &str,
) -> Answer {
    match kind {
        Some("InUse") => says(
            StatusCode::FORBIDDEN,
            format!(
                "error while removing network: network {} has active endpoints ({})",
                network.shown,
                members.join(", ")
            ),
        ),
        Some("NoSuchNetwork") => not_found(&network.shown),
        kind => says(status_of(kind), message),
    }
}

/// DELETE /networks/{id}: RemoveNetworks of that one.
async fn remove(ctx: &Context, key: &str) -> Served {
    let snapshot = snapshot(ctx).await?;
    let network = snapshot.network(key).ok_or_else(|| not_found(key))?;
    removable(&network)?;
    let client = connect_bus().await?;
    let names = [network.name.clone()];
    client
        .run_job_silent(|| client.manager.remove_networks(&names), true)
        .await
        .map_err(|e| {
            remove_failed(
                client::error_name(&e),
                &network,
                &members(&snapshot, &network.name),
                &format!("{e:#}"),
            )
        })?;
    Ok(respond(StatusCode::NO_CONTENT, "", Bytes::new()))
}

/// A prune's `until` as the bus takes it: those made before the second after it, the
/// engine's "made at it or before". It is a time or a duration before `now`, as the
/// engine reads it.
pub(super) fn prune_until(filters: &Filters, now: f64, offset: i64) -> Result<Option<u64>, Answer> {
    let bad = |text: String| says(StatusCode::BAD_REQUEST, text);
    let Some(values) = filters.values("until") else {
        return Ok(None);
    };
    if values.len() > 1 {
        return Err(bad("more than one until filter specified".to_string()));
    }
    let value = values.iter().next().map(String::as_str).unwrap_or_default();
    let stamp = crate::compose::gotime::get_timestamp(value, now, offset).map_err(bad)?;
    let seconds: i64 = stamp
        .split('.')
        .next()
        .unwrap_or_default()
        .parse()
        .map_err(|_| {
            bad(format!(
                "failed to parse value as time or duration: {}",
                crate::compose::gostr::quote(value)
            ))
        })?;
    Ok(Some(u64::try_from(seconds.saturating_add(1)).unwrap_or(0)))
}

/// The present time, and the local zone's offset then, as a prune reads `until` with.
pub(super) fn now_and_offset() -> (f64, i64) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    (now, crate::compose::gotime::local_offset(now as i64))
}

/// The networks a prune takes, by their own names: the ones made by `network create` or
/// the API that no machine names, made before `until`, matching every `label` and not
/// every `label!`.
pub fn prunable(
    networks: &[crate::api::network::NetworkSummary],
    filters: &Filters,
    until: Option<u64>,
) -> Vec<String> {
    networks
        .iter()
        .filter(|n| n.spec.name != DEFAULT_NETWORK && n.machines.is_empty())
        .filter(|n| until.is_none_or(|t| n.spec.created < t))
        .filter(|n| {
            filters.labels("label", &n.spec.labels)
                && !(filters.values("label!").is_some() && filters.labels("label!", &n.spec.labels))
        })
        .map(|n| n.spec.name.clone())
        .collect()
}

/// POST /networks/prune: RemoveNetworks of the networks the filters select. One that
/// could not go (a machine came to name it) stays, as the engine leaves it.
async fn prune(ctx: &Context, query: &Query) -> Served {
    let filters = Filters::parse(query.get("filters"))
        .map_err(|Invalid(text)| says(StatusCode::BAD_REQUEST, text))?;
    filters
        .only(&["label", "label!", "until"])
        .map_err(|Invalid(text)| says(StatusCode::BAD_REQUEST, text))?;
    let (now, offset) = now_and_offset();
    let until = prune_until(&filters, now, offset)?;
    let networks = crate::api::network::list_networks(ctx).map_err(|e| failed(format!("{e:#}")))?;
    let chosen = prunable(&networks, &filters, until);
    if chosen.is_empty() {
        return Ok(json_response(
            StatusCode::OK,
            &json!({ "NetworksDeleted": null }),
        ));
    }
    let client = connect_bus().await?;
    let outcome = client
        .run_job_silent(|| client.manager.remove_networks(&chosen), true)
        .await;
    let result = match outcome {
        Ok(result) => result,
        Err(e) => match client::removed_before(e) {
            Ok(result) => result,
            Err(e) => return Err(as_is(&e)),
        },
    };
    let removed: Vec<String> = client::strings(&result, "removed")
        .iter()
        .map(|own| {
            networks.iter().find(|n| n.spec.name == *own).map_or_else(
                || own.clone(),
                |n| super::model::shown_name(&n.spec).to_string(),
            )
        })
        .collect();
    Ok(json_response(
        StatusCode::OK,
        &json!({
            "NetworksDeleted": if removed.is_empty() { Value::Null } else { json!(removed) },
        }),
    ))
}

/// Serves a change of networks.
pub(super) async fn serve(
    ctx: &Arc<Context>,
    call: Call,
    query: &Query,
    sent: Sent,
) -> Response<Body> {
    let served = match call {
        Call::Create => create(sent).await,
        Call::Connect(key) => connect(ctx, &key, sent).await,
        Call::Disconnect(key) => disconnect(ctx, &key, sent).await,
        Call::Remove(key) => remove(ctx, &key).await,
        Call::Prune => prune(ctx, query).await,
    };
    served.unwrap_or_else(Answer::respond)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::NetSpec;

    fn created(body: Value) -> Result<Creation, (StatusCode, String)> {
        creation(&body, Some((1, 51))).map_err(|r| (r.status, r.message))
    }

    #[test]
    fn a_create_takes_a_bridge_with_its_subnet_labels_and_any_name() {
        // GitLab Runner's network of a build.
        let made = created(json!({
            "Name": "runner-abc-1-2-3",
            "Driver": "",
            "EnableIPv6": false,
            "Options": {},
            "Labels": { "com.gitlab.gitlab-runner.job.id": "3" },
            "IPAM": null,
        }))
        .unwrap();
        assert_eq!(made.name, "runner-abc-1-2-3");
        assert_eq!(
            made.opts,
            Opts::from([
                ("api_name", Opt::Text("runner-abc-1-2-3".into())),
                (
                    "labels",
                    Opt::Texts(vec!["com.gitlab.gitlab-runner.job.id=3".into()])
                ),
            ])
        );
        assert!(made.warnings.is_empty());
        // Testcontainers' network, attachable; the docker CLI's with a subnet.
        let made = created(json!({
            "Name": "reaper_default", "Driver": "bridge", "Attachable": true,
            "CheckDuplicate": true,
        }))
        .unwrap();
        assert_eq!(made.opts.len(), 1);
        let made = created(json!({
            "Name": "ci.net", "Driver": "bridge", "Internal": true, "Scope": "local",
            "IPAM": { "Driver": "default", "Options": {}, "Config": [
                { "Subnet": "10.77.0.0/24", "Gateway": "10.77.0.1", "IPRange": "10.77.0.0/24" },
            ]},
            "Options": { "com.docker.network.driver.mtu": "1400" },
        }))
        .unwrap();
        assert_eq!(made.opts["api_name"], Opt::Text("ci.net".into()));
        assert_eq!(made.opts["subnet"], Opt::Text("10.77.0.0/24".into()));
        assert_eq!(made.opts["internal"], Opt::Flag(true));
        assert_eq!(
            made.warnings,
            ["driver options are ignored: com.docker.network.driver.mtu"]
        );
        // EnableIPv4 is read from API 1.48 on.
        assert!(created(json!({ "Name": "n4", "EnableIPv4": false })).is_err());
        assert!(creation(&json!({ "Name": "n4", "EnableIPv4": false }), Some((1, 47))).is_ok());
        assert!(created(json!({ "Name": "n4", "EnableIPv4": true })).is_ok());
    }

    #[test]
    fn a_create_is_refused_where_nspawn_makes_another_network() {
        let status = |body: Value| created(body).unwrap_err().0;
        for name in ["bridge", "host", "none", "default"] {
            assert_eq!(
                created(json!({ "Name": name })).unwrap_err(),
                (
                    StatusCode::FORBIDDEN,
                    format!("operation is not permitted on predefined {name} network ")
                )
            );
        }
        assert_eq!(
            created(json!({ "Name": "veth" })).unwrap_err(),
            (
                StatusCode::BAD_REQUEST,
                "network name veth is reserved by nspawn".into()
            )
        );
        assert_eq!(
            created(json!({})).unwrap_err(),
            (
                StatusCode::BAD_REQUEST,
                "invalid name: name is empty".into()
            )
        );
        for body in [
            json!({ "Name": "n", "Driver": "macvlan" }),
            json!({ "Name": "n", "Scope": "swarm" }),
            json!({ "Name": "n", "EnableIPv6": true }),
            json!({ "Name": "n", "Options": { ENABLE_IPV6: "true" } }),
            json!({ "Name": "n", "Options": { ENABLE_IPV4: "false" } }),
            json!({ "Name": "n", "Options": { ENABLE_IPV4: "maybe" } }),
            json!({ "Name": "n", "Ingress": true }),
            json!({ "Name": "n", "ConfigOnly": true }),
            json!({ "Name": "n", "ConfigFrom": { "Network": "cfg" } }),
            json!({ "Name": "n", "IPAM": { "Driver": "dhcp" } }),
            json!({ "Name": "n", "IPAM": { "Config": [{ "Subnet": "fd00::/64" }] } }),
            json!({ "Name": "n", "IPAM": { "Config": [{ "Subnet": "10.1.0.0/24" }, { "Subnet": "10.2.0.0/24" }] } }),
            json!({ "Name": "n", "IPAM": { "Config": [{ "Subnet": "10.1.0.0/24", "Gateway": "10.1.0.254" }] } }),
            json!({ "Name": "n", "IPAM": { "Config": [{ "Subnet": "10.1.0.0/24", "IPRange": "10.1.0.128/25" }] } }),
            json!({ "Name": "n", "IPAM": { "Config": [{ "Subnet": "10.1.0.0/24", "AuxiliaryAddresses": { "a": "10.1.0.9" } }] } }),
            json!({ "Name": "n", "IPAM": { "Config": [{ "Gateway": "10.1.0.1" }] } }),
            json!({ "Name": "n", "Labels": ["a"] }),
        ] {
            assert_eq!(status(body.clone()), StatusCode::BAD_REQUEST, "{body}");
        }
        assert_eq!(
            created(json!({ "Name": "n", "IPAM": { "Config": [{ "Subnet": "10.1.0.5/24" }] } }))
                .unwrap_err()
                .1,
            "invalid subnet 10.1.0.5/24: it should be 10.1.0.0/24"
        );
        assert_eq!(
            created(json!({ "Name": "n", "IPAM": { "Config": [{ "Subnet": "nope" }] } }))
                .unwrap_err()
                .1,
            "invalid subnet nope: invalid CIDR block notation"
        );
    }

    #[test]
    fn a_create_answers_as_the_engine() {
        assert_eq!(
            create_failed(Some("Exists"), "ci.net", "network ci.net exists already"),
            says(
                StatusCode::CONFLICT,
                "network with name ci.net already exists"
            )
        );
        assert_eq!(
            create_failed(Some("Invalid"), "x", "bad name"),
            says(StatusCode::BAD_REQUEST, "bad name")
        );
        let made: client::Dict = [
            (
                "name",
                zbus::zvariant::OwnedValue::from(zbus::zvariant::Str::from("ci-net-0a1b2c3d")),
            ),
            ("created", zbus::zvariant::OwnedValue::from(5u64)),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let body = created_body(&made, &[]);
        // The ID the list and an inspect give it.
        assert_eq!(body["Id"], json!(network_id("ci-net-0a1b2c3d", 5)));
        assert_eq!(body["Warning"], "");
        let golden: Value = serde_json::from_str(r#"{"Id":"x","Warning":""}"#).unwrap();
        assert_eq!(
            crate::dockerapi::shapes::diff(&body, &golden),
            (vec![], vec![])
        );
    }

    fn spec(name: &str, api_name: Option<&str>, created: u64, labels: &[(&str, &str)]) -> NetSpec {
        let mut spec: NetSpec = serde_json::from_value(json!({
            "name": name, "interface": format!("nsbr-{name}"), "subnet": "10.99.1.0/24",
            "created": created,
        }))
        .unwrap();
        spec.api_name = api_name.map(str::to_string);
        spec.labels = labels
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        spec
    }

    #[test]
    fn a_prune_takes_unused_networks_by_time_and_label() {
        use crate::api::network::NetworkSummary;
        let summary = |spec: NetSpec, machines: &[&str]| NetworkSummary {
            spec,
            machines: machines.iter().map(|m| m.to_string()).collect(),
        };
        let mut default = spec(DEFAULT_NETWORK, None, 0, &[]);
        default.created = 0;
        let networks = [
            summary(default, &[]),
            summary(spec("front", None, 100, &[("tier", "web")]), &["web"]),
            summary(spec("old", None, 100, &[]), &[]),
            summary(
                spec("ci-net-0a1b2c3d", Some("ci.net"), 200, &[("tier", "ci")]),
                &[],
            ),
        ];
        let pruned = |text: &str, until: Option<u64>| {
            prunable(&networks, &Filters::parse(Some(text)).unwrap(), until)
        };
        assert_eq!(pruned("{}", None), ["old", "ci-net-0a1b2c3d"]);
        assert_eq!(pruned("{}", Some(150)), ["old"]);
        assert_eq!(pruned("{}", Some(201)), ["old", "ci-net-0a1b2c3d"]);
        assert_eq!(pruned(r#"{"label":["tier"]}"#, None), ["ci-net-0a1b2c3d"]);
        assert_eq!(pruned(r#"{"label!":["tier=ci"]}"#, None), ["old"]);
        let f = |text: &str| Filters::parse(Some(text)).unwrap();
        assert_eq!(
            prune_until(&f(r#"{"until":["10m"]}"#), 1_000_000.0, 0).unwrap(),
            Some(1_000_000 - 600 + 1)
        );
        assert_eq!(prune_until(&f("{}"), 1_000_000.0, 0).unwrap(), None);
        assert_eq!(
            prune_until(&f(r#"{"until":["1","2"]}"#), 1_000_000.0, 0).unwrap_err(),
            says(
                StatusCode::BAD_REQUEST,
                "more than one until filter specified"
            )
        );
        assert!(prune_until(&f(r#"{"until":["soon"]}"#), 1_000_000.0, 0).is_err());
    }

    fn view<'a>(spec: Option<&'a NetSpec>, name: &str) -> NetworkView<'a> {
        NetworkView {
            id: network_id(name, 0),
            name: name.to_string(),
            shown: spec
                .map_or(name, super::super::model::shown_name)
                .to_string(),
            spec,
        }
    }

    #[test]
    fn removals_spare_the_networks_of_their_own() {
        let default = spec(DEFAULT_NETWORK, None, 0, &[]);
        let ci = spec("ci-net-0a1b2c3d", Some("ci.net"), 5, &[]);
        for (spec, name) in [(None, "host"), (None, "none"), (Some(&default), "bridge")] {
            assert_eq!(
                removable(&view(spec, name)),
                Err(says(
                    StatusCode::FORBIDDEN,
                    format!("{name} is a pre-defined network and cannot be removed")
                ))
            );
        }
        let ci = view(Some(&ci), "ci-net-0a1b2c3d");
        assert_eq!(ci.shown, "ci.net");
        assert_eq!(removable(&ci), Ok(()));
        assert_eq!(
            remove_failed(Some("InUse"), &ci, &["build".into()], "in use"),
            says(
                StatusCode::FORBIDDEN,
                "error while removing network: network ci.net has active endpoints (build)"
            )
        );
        assert_eq!(
            remove_failed(Some("NoSuchNetwork"), &ci, &[], "gone"),
            says(StatusCode::NOT_FOUND, "network ci.net not found")
        );
    }

    #[test]
    fn endpoints_take_aliases_and_an_address() {
        let options =
            |body: Value| endpoint_options(&Object::of(&body, "network.EndpointSettings").unwrap());
        assert_eq!(
            options(json!({
                "Aliases": ["db", "pg"], "IPAMConfig": { "IPv4Address": "10.99.1.9" },
                "MacAddress": "02:42:ac:11:00:02", "DriverOpts": { "a": "b" }, "GwPriority": 1,
            }))
            .unwrap(),
            Opts::from([
                ("aliases", Opt::Texts(vec!["db".into(), "pg".into()])),
                ("address", Opt::Text("10.99.1.9".into())),
            ])
        );
        assert_eq!(options(Value::Null).unwrap(), Opts::new());
        for body in [
            json!({ "IPAMConfig": { "IPv6Address": "fd00::9" } }),
            json!({ "IPAMConfig": { "LinkLocalIPs": ["169.254.0.9"] } }),
            json!({ "Links": ["db:db"] }),
            json!({ "Aliases": "db" }),
        ] {
            assert_eq!(
                options(body.clone()).unwrap_err().status,
                StatusCode::BAD_REQUEST,
                "{body}"
            );
        }
    }

    fn machine(network: &str) -> Machine {
        let record: crate::store::ImageRecord = serde_json::from_value(json!({
            "name": "web", "reference": "docker.io/library/busybox:1.37",
            "manifest_digest": "sha256:aa", "layers": [], "backend": "overlay",
            "created": 1u64, "mode": "app", "network": network,
        }))
        .unwrap();
        Machine {
            id: super::super::util::container_id("web", 1),
            summary: crate::api::machines::MachineSummary {
                name: "web".into(),
                record: Some(record.clone()),
                state: "stopped".into(),
                started: None,
                leader: None,
                os: None,
                health: None,
                exit_code: None,
                finished: None,
                last_started: None,
            },
            record,
            finished_usec: None,
            restarts: 0,
        }
    }

    #[test]
    fn connects_are_refused_as_the_engine_refuses_them() {
        let ci = spec("ci-net-0a1b2c3d", Some("ci.net"), 5, &[]);
        let ci = view(Some(&ci), "ci-net-0a1b2c3d");
        let bridged = machine("bridge");
        assert_eq!(connect_refusal(&bridged, &ci), None);
        assert_eq!(
            connect_refusal(&bridged, &view(None, "host")),
            Some(says(
                StatusCode::BAD_REQUEST,
                "cannot connect container to host network - container must be created in host network mode"
            ))
        );
        assert_eq!(
            connect_refusal(&machine("host"), &ci),
            Some(says(StatusCode::BAD_REQUEST, HOST_CONFLICT))
        );
        let mut alone = machine("bridge");
        alone.record.no_network = true;
        assert_eq!(
            connect_refusal(&alone, &ci),
            Some(says(StatusCode::BAD_REQUEST, NONE_CONFLICT))
        );
        let mut sharing = machine("bridge");
        sharing.record.network_container = Some("db".into());
        assert_eq!(
            connect_refusal(&sharing, &ci),
            Some(says(StatusCode::BAD_REQUEST, SHARED_CONFLICT))
        );
        assert_eq!(
            connect_failed(
                Some("Exists"),
                &bridged,
                &ci,
                "endpoint with name web already exists in network ci-net-0a1b2c3d"
            ),
            says(
                StatusCode::FORBIDDEN,
                "endpoint with name web already exists in network ci.net"
            )
        );
        assert_eq!(
            connect_failed(
                Some("NoSuchMachine"),
                &bridged,
                &ci,
                "No such container: web"
            ),
            says(StatusCode::NOT_FOUND, "No such container: web")
        );
        assert_eq!(disconnect_refusal(&ci), None);
        assert_eq!(
            disconnect_refusal(&view(None, "host")).unwrap().0,
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn the_routes_of_changes() {
        fn parts(path: &str) -> Vec<&str> {
            path.split('/').collect()
        }
        assert_eq!(
            Call::of(&Method::POST, &parts("networks/create")),
            Some(Call::Create)
        );
        assert_eq!(
            Call::of(&Method::POST, &parts("networks/prune")),
            Some(Call::Prune)
        );
        assert_eq!(
            Call::of(&Method::POST, &parts("networks/ci.net/connect")),
            Some(Call::Connect("ci.net".into()))
        );
        assert_eq!(
            Call::of(&Method::POST, &parts("networks/ab12/disconnect")),
            Some(Call::Disconnect("ab12".into()))
        );
        assert_eq!(
            Call::of(&Method::DELETE, &parts("networks/ci.net")),
            Some(Call::Remove("ci.net".into()))
        );
        assert_eq!(Call::of(&Method::GET, &parts("networks/ci.net")), None);
        assert_eq!(Call::of(&Method::POST, &parts("networks/ci.net")), None);
    }
}

#[cfg(test)]
mod routes {
    use crate::dockerapi::access::Need;
    use crate::dockerapi::{classify, Route};
    use hyper::Method;

    /// Every change of volumes, networks and images asks org.nspawn.manage, as the
    /// methods behind them do; the image changes nspawn does not make stay "not
    /// supported".
    #[test]
    fn each_change_is_routed_and_asks_manage() {
        for (method, path) in [
            (Method::POST, "/volumes/create"),
            (Method::DELETE, "/volumes/data"),
            (Method::POST, "/volumes/prune"),
            (Method::POST, "/networks/create"),
            (Method::POST, "/networks/ci.net/connect"),
            (Method::POST, "/networks/ci.net/disconnect"),
            (Method::DELETE, "/networks/ci.net"),
            (Method::POST, "/networks/prune"),
            (Method::DELETE, "/images/nginx:latest"),
            (Method::POST, "/images/prune"),
        ] {
            let route = classify(&method, path);
            assert!(
                matches!(
                    route,
                    Route::VolumeChange(_) | Route::NetworkChange(_) | Route::ImageChange(_)
                ),
                "{method} {path}: {route:?}"
            );
            assert_eq!(route.need(), Need::Manage, "{method} {path}");
        }
        for (method, path) in [
            (Method::POST, "/images/load"),
            (Method::POST, "/images/nginx/tag"),
            (Method::POST, "/images/nginx/push"),
            (Method::POST, "/commit"),
            (Method::POST, "/build"),
            (Method::PUT, "/volumes/data"),
        ] {
            assert_eq!(classify(&method, path), Route::Change, "{method} {path}");
        }
        assert_eq!(
            classify(&Method::GET, "/networks/ci.net"),
            Route::Network("ci.net".into())
        );
        assert_eq!(
            classify(&Method::GET, "/volumes/data"),
            Route::Volume("data".into())
        );
    }
}
