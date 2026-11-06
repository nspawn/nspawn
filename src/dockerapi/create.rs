//! POST /containers/create: a request of the Docker Engine API as CreateMachine's
//! options. Each field is taken (as the option that does the same), taken and ignored
//! (what concerns the client or another platform alone), warned of (accepted, does
//! nothing, said in the answer's Warnings) or refused (400 with the reason): what would
//! change what the container does.

use std::collections::{BTreeMap, BTreeSet};

use hyper::StatusCode;
use serde_json::{Map, Value};

/// A value of an option of the bus.
#[derive(Debug, Clone, PartialEq)]
pub enum Opt {
    Text(String),
    Texts(Vec<String>),
    Flag(bool),
    Unsigned(u64),
    Signed(i64),
    Float(f64),
}

/// A method's options, by name.
pub type Opts = BTreeMap<&'static str, Opt>;

/// The options as the bus carries them.
pub fn to_bus(opts: &Opts) -> crate::client::Options<'_> {
    use zbus::zvariant::Value as Bus;
    opts.iter()
        .map(|(key, value)| {
            let value = match value {
                Opt::Text(text) => Bus::from(text.as_str()),
                Opt::Texts(list) => Bus::from(list.clone()),
                Opt::Flag(flag) => Bus::from(*flag),
                Opt::Unsigned(n) => Bus::from(*n),
                Opt::Signed(n) => Bus::from(*n),
                Opt::Float(n) => Bus::from(*n),
            };
            (*key, value)
        })
        .collect()
}

/// Why a request is not served: its status and the words of the answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub status: StatusCode,
    pub message: String,
}

pub fn bad(message: impl Into<String>) -> Refusal {
    Refusal {
        status: StatusCode::BAD_REQUEST,
        message: message.into(),
    }
}

const EMPTY_CONFIG: &str = "Config cannot be empty in order to create a container";

/// The kind of a JSON value, as Go's decoder names it in its errors.
fn kind(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(_) => "bool".to_string(),
        Value::Number(n) => format!("number {n}"),
        Value::String(_) => "string".to_string(),
        Value::Array(_) => "array".to_string(),
        Value::Object(_) => "object".to_string(),
    }
}

/// A request body as Go's decoder reads one value of it: an empty body is EOF, a
/// truncated one an unexpected EOF, and more after the value is refused.
pub fn decode(body: &[u8]) -> Result<Value, Refusal> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Err(bad("invalid JSON: got EOF while reading request body"));
    }
    let mut values = serde_json::Deserializer::from_slice(body).into_iter::<Value>();
    let value = match values.next() {
        Some(Ok(value)) => value,
        Some(Err(e)) if e.is_eof() => return Err(bad("invalid JSON: unexpected EOF")),
        Some(Err(e)) => return Err(bad(format!("invalid JSON: {e}"))),
        None => return Err(bad("invalid JSON: got EOF while reading request body")),
    };
    if !body[values.byte_offset()..]
        .iter()
        .all(u8::is_ascii_whitespace)
    {
        return Err(bad("unexpected content after JSON"));
    }
    Ok(value)
}

/// An object of the request read as Go reads a struct: a key matched without regard to
/// case, null as the zero value, a value of another type an error.
#[derive(Clone, Copy)]
pub struct Object<'a> {
    map: Option<&'a Map<String, Value>>,
    owner: &'static str,
}

type Read<T> = Result<T, Refusal>;

impl<'a> Object<'a> {
    pub fn of(value: &'a Value, owner: &'static str) -> Read<Object<'a>> {
        match value {
            Value::Object(map) => Ok(Object {
                map: Some(map),
                owner,
            }),
            Value::Null => Ok(Object { map: None, owner }),
            other => Err(bad(format!(
                "invalid JSON: json: cannot unmarshal {} into Go value of type {owner}",
                kind(other)
            ))),
        }
    }

    /// Whether `key` is there at all, null or not.
    fn present(&self, key: &str) -> bool {
        self.map
            .is_some_and(|m| m.keys().any(|k| k.eq_ignore_ascii_case(key)))
    }

    fn get(&self, key: &str) -> Option<&'a Value> {
        let map = self.map?;
        map.get(key)
            .or_else(|| {
                map.iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(key))
                    .map(|(_, v)| v)
            })
            .filter(|v| !v.is_null())
    }

    fn wrong(&self, key: &str, value: &Value, go_type: &str) -> Refusal {
        bad(format!(
            "invalid JSON: json: cannot unmarshal {} into Go struct field {}.{key} of type {go_type}",
            kind(value),
            self.owner
        ))
    }

    pub fn text(&self, key: &str) -> Read<String> {
        match self.get(key) {
            None => Ok(String::new()),
            Some(Value::String(s)) => Ok(s.clone()),
            Some(other) => Err(self.wrong(key, other, "string")),
        }
    }

    pub fn flag(&self, key: &str) -> Read<bool> {
        Ok(self.maybe_flag(key)?.unwrap_or(false))
    }

    pub fn maybe_flag(&self, key: &str) -> Read<Option<bool>> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(*b)),
            Some(other) => Err(self.wrong(key, other, "bool")),
        }
    }

    pub fn int(&self, key: &str) -> Read<i64> {
        Ok(self.maybe_int(key)?.unwrap_or(0))
    }

    pub fn maybe_int(&self, key: &str) -> Read<Option<i64>> {
        match self.get(key) {
            None => Ok(None),
            Some(v @ Value::Number(n)) => n
                .as_i64()
                .map(Some)
                .ok_or_else(|| self.wrong(key, v, "int64")),
            Some(other) => Err(self.wrong(key, other, "int64")),
        }
    }

    pub fn texts(&self, key: &str) -> Read<Vec<String>> {
        match self.get(key) {
            None => Ok(Vec::new()),
            Some(v @ Value::Array(items)) => items
                .iter()
                .map(|i| match i {
                    Value::String(s) => Ok(s.clone()),
                    Value::Null => Ok(String::new()),
                    _ => Err(self.wrong(key, v, "[]string")),
                })
                .collect(),
            Some(other) => Err(self.wrong(key, other, "[]string")),
        }
    }

    /// A strslice: a list of strings, or one string as a list of one; None for null.
    pub fn words(&self, key: &str) -> Read<Option<Vec<String>>> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(vec![s.clone()])),
            Some(Value::Array(_)) => self.texts(key).map(Some),
            Some(other) => Err(self.wrong(key, other, "strslice.StrSlice")),
        }
    }

    /// A map of strings to strings.
    pub fn pairs(&self, key: &str) -> Read<BTreeMap<String, String>> {
        match self.get(key) {
            None => Ok(BTreeMap::new()),
            Some(v @ Value::Object(map)) => map
                .iter()
                .map(|(k, value)| match value {
                    Value::String(s) => Ok((k.clone(), s.clone())),
                    Value::Null => Ok((k.clone(), String::new())),
                    _ => Err(self.wrong(key, v, "map[string]string")),
                })
                .collect(),
            Some(other) => Err(self.wrong(key, other, "map[string]string")),
        }
    }

    /// The keys of a map that stands for a set (`{"80/tcp": {}}`).
    pub fn set(&self, key: &str) -> Read<Vec<String>> {
        match self.get(key) {
            None => Ok(Vec::new()),
            Some(Value::Object(map)) => Ok(map.keys().cloned().collect()),
            Some(other) => Err(self.wrong(key, other, "map[string]struct {}")),
        }
    }

    pub fn object(&self, key: &str, owner: &'static str) -> Read<Object<'a>> {
        match self.get(key) {
            None => Ok(Object { map: None, owner }),
            Some(Value::Object(map)) => Ok(Object {
                map: Some(map),
                owner,
            }),
            Some(other) => Err(self.wrong(key, other, owner)),
        }
    }

    /// The objects of a list, each null one left out.
    pub fn objects(&self, key: &str, owner: &'static str) -> Read<Vec<Object<'a>>> {
        match self.get(key) {
            None => Ok(Vec::new()),
            Some(v @ Value::Array(items)) => items
                .iter()
                .filter(|i| !i.is_null())
                .map(|i| match i {
                    Value::Object(map) => Ok(Object {
                        map: Some(map),
                        owner,
                    }),
                    _ => Err(self.wrong(key, v, owner)),
                })
                .collect(),
            Some(other) => Err(self.wrong(key, other, owner)),
        }
    }

    /// The entries of a map of objects, by key.
    pub fn entries(&self, key: &str, owner: &'static str) -> Read<Vec<(String, Object<'a>)>> {
        match self.get(key) {
            None => Ok(Vec::new()),
            Some(Value::Object(map)) => map
                .iter()
                .map(|(k, v)| Object::of(v, owner).map(|o| (k.clone(), o)))
                .collect(),
            Some(other) => Err(self.wrong(key, other, owner)),
        }
    }

    /// A non-empty value of `key`, of any type: what a field nspawn has no use for
    /// holds when it says something.
    fn set_at(&self, key: &str) -> bool {
        match self.get(key) {
            None => false,
            Some(Value::Bool(b)) => *b,
            Some(Value::Number(n)) => n.as_f64() != Some(0.0),
            Some(Value::String(s)) => !s.is_empty(),
            Some(Value::Array(a)) => !a.is_empty(),
            Some(Value::Object(o)) => !o.is_empty(),
            Some(Value::Null) => false,
        }
    }
}

/// A container the request names, as the host has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Linked {
    /// Its machine's name.
    pub machine: String,
    /// Its address on its primary network, once it has one.
    pub address: Option<std::net::Ipv4Addr>,
}

/// What a translation looks up: containers, networks and host paths.
pub trait World {
    /// A container by its ID, name or ID prefix.
    fn container(&self, key: &str) -> Option<Linked>;
    /// A network's name, from its name, ID or ID prefix.
    fn network(&self, key: &str) -> Option<String>;
    /// Whether a host path exists.
    fn exists(&self, path: &str) -> bool;
}

/// A create as the bus is asked for it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    /// Config.Image, as the client wrote it.
    pub image: String,
    pub opts: Opts,
    pub warnings: Vec<String>,
    /// The image's Cmd is the command: the request empties the entrypoint and gives
    /// none.
    pub image_cmd: bool,
    /// A network named that does not exist: 404 once the image is found.
    pub missing_network: Option<String>,
    /// A container named that does not exist (network mode, links): 404 likewise.
    pub missing_container: Option<String>,
}

impl Plan {
    fn warn(&mut self, text: impl Into<String>) {
        self.warnings.push(text.into());
    }

    fn texts(&mut self, key: &'static str, list: Vec<String>) {
        if !list.is_empty() {
            self.opts.insert(key, Opt::Texts(list));
        }
    }

    fn text(&mut self, key: &'static str, text: String) {
        if !text.is_empty() {
            self.opts.insert(key, Opt::Text(text));
        }
    }

    fn flag(&mut self, key: &'static str, on: bool) {
        if on {
            self.opts.insert(key, Opt::Flag(true));
        }
    }
}

/// The fields of Config, which a request carries at its top level: none of them is no
/// Config.
const CONFIG_FIELDS: &[&str] = &[
    "Hostname",
    "Domainname",
    "User",
    "AttachStdin",
    "AttachStdout",
    "AttachStderr",
    "ExposedPorts",
    "Tty",
    "OpenStdin",
    "StdinOnce",
    "Env",
    "Cmd",
    "Healthcheck",
    "ArgsEscaped",
    "Image",
    "Volumes",
    "WorkingDir",
    "Entrypoint",
    "NetworkDisabled",
    "MacAddress",
    "OnBuild",
    "Labels",
    "StopSignal",
    "StopTimeout",
    "Shell",
];

/// The shortest healthcheck interval the engine takes, in nanoseconds.
const MINIMUM_DURATION: i64 = 1_000_000;

/// CpuPeriod when only CpuQuota is given, in microseconds.
const DEFAULT_CPU_PERIOD: i64 = 100_000;

/// The request as CreateMachine's options, checked in the order the engine checks
/// them: its Config, then its HostConfig, then its resources.
pub fn translate(body: &Value, world: &dyn World) -> Result<Plan, Refusal> {
    let config = Object::of(body, "container.CreateRequest")?;
    if body.is_null() || !CONFIG_FIELDS.iter().any(|f| config.present(f)) {
        return Err(bad(EMPTY_CONFIG));
    }
    let config = Object {
        owner: "Config",
        ..config
    };
    let host = config.object("HostConfig", "HostConfig")?;
    let networking = config.object("NetworkingConfig", "NetworkingConfig")?;
    let mut plan = Plan {
        image: config.text("Image")?,
        ..Plan::default()
    };
    read_config(&config, &mut plan)?;
    check_host(&host)?;
    let targets = read_mounts(&host, world, &mut plan)?;
    for path in config.set("Volumes")? {
        if !targets.contains(path.trim_end_matches('/')) {
            plan.warn(format!(
                "Volumes {path} is ignored: what the container writes there stays in its own layer"
            ));
        }
    }
    read_host(&host, &config, &mut plan)?;
    read_network(&host, &config, &networking, world, &mut plan)?;
    read_resources(&host, &mut plan)?;
    Ok(plan)
}

fn read_config(config: &Object, plan: &mut Plan) -> Result<(), Refusal> {
    let working_dir = config.text("WorkingDir")?;
    if !working_dir.is_empty() && !working_dir.starts_with('/') {
        return Err(bad(format!(
            "the working directory '{working_dir}' is invalid, it needs to be an absolute path"
        )));
    }
    let stop_signal = config.text("StopSignal")?;
    if !stop_signal.is_empty() {
        super::parse_signal(&stop_signal).map_err(bad)?;
    }
    let mut env = Vec::new();
    let mut unset = Vec::new();
    for value in config.texts("Env")? {
        if value.is_empty() || value.starts_with('=') {
            return Err(bad(format!("invalid environment variable: {value}")));
        }
        // A name alone takes the image's variable away.
        if value.contains('=') {
            env.push(value);
        } else {
            unset.push(value);
        }
    }
    let health = config.object("Healthcheck", "HealthConfig")?;
    let duration = |field: &str| -> Read<i64> {
        let value = health.int(field)?;
        if value != 0 && value < MINIMUM_DURATION {
            return Err(bad(format!(
                "{field} in Healthcheck cannot be less than 1ms"
            )));
        }
        Ok(value)
    };
    let interval = duration("Interval")?;
    let timeout = duration("Timeout")?;
    let retries = health.int("Retries")?;
    if retries < 0 {
        return Err(bad("Retries in Healthcheck cannot be negative"));
    }
    let start_period = duration("StartPeriod")?;
    let start_interval = duration("StartInterval")?;
    let test = health.texts("Test")?;
    if test.first().map(String::as_str) == Some("NONE") {
        plan.flag("no_healthcheck", true);
    } else {
        plan.texts("health_test", test);
        for (option, nanoseconds) in [
            ("health_interval", interval),
            ("health_timeout", timeout),
            ("health_start_period", start_period),
            ("health_start_interval", start_interval),
        ] {
            if nanoseconds > 0 {
                plan.opts
                    .insert(option, Opt::Unsigned(nanoseconds as u64 / 1000));
            }
        }
        if retries > 0 {
            plan.opts
                .insert("health_retries", Opt::Unsigned(retries as u64));
        }
    }
    plan.text("hostname", config.text("Hostname")?);
    if !config.text("Domainname")?.is_empty() {
        plan.warn("Domainname is ignored: nspawn sets no domain name");
    }
    plan.text("user", config.text("User")?);
    for field in ["AttachStdin", "AttachStdout", "AttachStderr", "ArgsEscaped"] {
        config.flag(field)?;
    }
    plan.flag("tty", config.flag("Tty")?);
    plan.flag("interactive", config.flag("OpenStdin")?);
    plan.flag("stdin_once", config.flag("StdinOnce")?);
    plan.texts("env", env);
    plan.texts("unset_env", unset);
    let cmd = config.words("Cmd")?.filter(|c| !c.is_empty());
    match config.words("Entrypoint")? {
        None => {
            if let Some(cmd) = cmd {
                plan.texts("command", cmd);
            }
        }
        Some(entrypoint) => {
            // An empty list takes the image's Cmd when the request has none, as the
            // engine merges them; [""] is none as well, and takes nothing of the image.
            let image_cmd = entrypoint.is_empty();
            let entrypoint = if entrypoint == [""] {
                Vec::new()
            } else {
                entrypoint
            };
            plan.opts.insert("entrypoint", Opt::Texts(entrypoint));
            match cmd {
                Some(cmd) => plan.texts("command", cmd),
                None => plan.image_cmd = image_cmd,
            }
        }
    }
    plan.text("working_dir", working_dir);
    if !config.text("MacAddress")?.is_empty() {
        plan.warn(
            "MacAddress is ignored: nspawn derives a container's MAC address from its IP address",
        );
    }
    config.texts("OnBuild")?;
    config.words("Shell")?;
    let labels: Vec<String> = config
        .pairs("Labels")?
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    plan.texts("label", labels);
    plan.text("stop_signal", stop_signal);
    if let Some(timeout) = config.maybe_int("StopTimeout")? {
        let seconds = u64::try_from(timeout).unwrap_or(crate::policy::NO_STOP_TIMEOUT);
        plan.opts.insert("stop_timeout", Opt::Unsigned(seconds));
    }
    Ok(())
}

/// The restart policy's text for the bus, after the engine's checks.
pub fn restart_text(policy: &Object) -> Result<Option<String>, Refusal> {
    let name = policy.text("Name")?;
    let count = policy.int("MaximumRetryCount")?;
    match name.as_str() {
        "" => Ok(None),
        "no" | "always" | "unless-stopped" => {
            if count != 0 {
                let mut msg = "invalid restart policy: maximum retry count can only be used with 'on-failure'".to_string();
                if count < 0 {
                    msg.push_str(" and cannot be negative");
                }
                return Err(bad(msg));
            }
            Ok(Some(name))
        }
        "on-failure" => match count {
            n if n < 0 => Err(bad(
                "invalid restart policy: maximum retry count cannot be negative",
            )),
            0 => Ok(Some(name)),
            n => Ok(Some(format!("on-failure:{n}"))),
        },
        other => Err(bad(format!(
            "invalid restart policy: unknown policy '{other}'; use one of 'no', 'always', 'on-failure', or 'unless-stopped'"
        ))),
    }
}

/// The warning a restart policy with a count of retries gets: nspawn keeps no count.
pub fn retry_count_warning(restart: &str) -> Option<&'static str> {
    restart.starts_with("on-failure:").then_some(
        "RestartPolicy.MaximumRetryCount is ignored: the container is restarted after every failure",
    )
}

/// What the engine refuses in a HostConfig before it looks at the image.
fn check_host(host: &Object) -> Result<(), Refusal> {
    let policy = host.object("RestartPolicy", "RestartPolicy")?;
    let restart = restart_text(&policy);
    if host.flag("AutoRemove")? && !matches!(policy.text("Name")?.as_str(), "" | "no") {
        return Err(bad(
            "can't create 'AutoRemove' container with restart policy",
        ));
    }
    restart?;
    let isolation = host.text("Isolation")?;
    if !matches!(isolation.as_str(), "" | "default") {
        return Err(bad(format!("invalid isolation '{isolation}' on linux")));
    }
    if host.int("NanoCpus")? > 0 {
        if host.int("CpuPeriod")? > 0 {
            return Err(bad(
                "Conflicting options: Nano CPUs and CPU Period cannot both be set",
            ));
        }
        if host.int("CpuQuota")? > 0 {
            return Err(bad(
                "Conflicting options: Nano CPUs and CPU Quota cannot both be set",
            ));
        }
    }
    Ok(())
}

/// One field of a `--mount` value, quoted when it holds a comma or a quote.
fn csv(field: String) -> String {
    if field.contains([',', '"']) {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field
    }
}

/// Binds and Mounts as volume and tmpfs options; the targets they mount.
fn read_mounts(
    host: &Object,
    world: &dyn World,
    plan: &mut Plan,
) -> Result<BTreeSet<String>, Refusal> {
    let mut volumes = Vec::new();
    let mut tmpfs = Vec::new();
    let mut targets = BTreeSet::new();
    for bind in host.texts("Binds")? {
        // A missing directory of the host is made when the container starts.
        if let Some(target) = bind.split(':').nth(1) {
            targets.insert(target.trim_end_matches('/').to_string());
        }
        volumes.push(bind);
    }
    for (path, options) in host.pairs("Tmpfs")? {
        targets.insert(path.trim_end_matches('/').to_string());
        tmpfs.push(if options.is_empty() {
            path
        } else {
            format!("{path}:{options}")
        });
    }
    for mount in host.objects("Mounts", "mount.Mount")? {
        let kind = mount.text("Type")?.to_lowercase();
        let source = mount.text("Source")?;
        let target = mount.text("Target")?;
        let mut fields = vec![format!("type={kind}")];
        if !source.is_empty() {
            fields.push(format!("source={source}"));
        }
        if !target.is_empty() {
            fields.push(format!("target={target}"));
        }
        if mount.flag("ReadOnly")? {
            fields.push("readonly".to_string());
        }
        let bind = mount.object("BindOptions", "mount.BindOptions")?;
        let volume = mount.object("VolumeOptions", "mount.VolumeOptions")?;
        let memory = mount.object("TmpfsOptions", "mount.TmpfsOptions")?;
        for field in [
            "NonRecursive",
            "ReadOnlyNonRecursive",
            "ReadOnlyForceRecursive",
        ] {
            if bind.flag(field)? {
                return Err(bad(format!(
                    "BindOptions.{field} is not supported: a bind into an nspawn machine is recursive"
                )));
            }
        }
        let propagation = bind.text("Propagation")?;
        if !propagation.is_empty() {
            fields.push(format!("bind-propagation={propagation}"));
        }
        let create_source = bind.flag("CreateMountpoint")?;
        if volume.flag("NoCopy")? {
            fields.push("volume-nocopy".to_string());
        }
        let subpath = volume.text("Subpath")?;
        if !subpath.is_empty() {
            fields.push(format!("volume-subpath={subpath}"));
        }
        let driver = volume.object("DriverConfig", "mount.Driver")?;
        let driver_name = driver.text("Name")?;
        if !driver_name.is_empty() {
            fields.push(format!("volume-driver={driver_name}"));
        }
        if !driver.pairs("Options")?.is_empty() {
            return Err(bad(
                "VolumeOptions.DriverConfig.Options is not supported: nspawn volumes are directories of the host",
            ));
        }
        let size = memory.int("SizeBytes")?;
        if size > 0 {
            fields.push(format!("tmpfs-size={size}"));
        }
        let mode = memory.int("Mode")?;
        if mode > 0 {
            fields.push(format!("tmpfs-mode={mode:o}"));
        }
        if memory.set_at("Options") {
            plan.warn("TmpfsOptions.Options are ignored: a tmpfs takes its size and mode");
        }
        if kind == "volume" && source.is_empty() {
            targets.insert(target.trim_end_matches('/').to_string());
            plan.warn(format!(
                "Mounts {target} is ignored: an anonymous volume; what the container writes there stays in its own layer"
            ));
            continue;
        }
        if kind == "bind"
            && source.starts_with('/')
            && !crate::volume::is_api_socket_path(&source)
            && !world.exists(&source)
        {
            // One it asks to have made is made when the container starts.
            if !create_source {
                return Err(bad(format!(
                    "invalid mount config for type \"bind\": bind source path does not exist: {source}"
                )));
            }
        }
        if kind == "bind" && !source.starts_with('/') && !source.is_empty() {
            return Err(bad(format!(
                "invalid mount config for type \"bind\": invalid mount path: '{source}' mount path must be absolute"
            )));
        }
        let text = fields.into_iter().map(csv).collect::<Vec<_>>().join(",");
        match crate::mountopt::parse(&text).map_err(|e| bad(format!("{e:#}")))? {
            crate::mountopt::Mount::Volume(v) => volumes.push(v),
            crate::mountopt::Mount::Tmpfs(t) => tmpfs.push(t),
        }
        targets.insert(target.trim_end_matches('/').to_string());
    }
    plan.texts("volume", volumes);
    plan.texts("tmpfs", tmpfs);
    Ok(targets)
}

/// The HostConfig apart from networks and resources.
fn read_host(host: &Object, config: &Object, plan: &mut Plan) -> Result<(), Refusal> {
    // Settings that would change what the container does.
    for (field, allowed, why) in [
        (
            "PidMode",
            &[""][..],
            "a container has its own process namespace",
        ),
        (
            "IpcMode",
            &["", "private", "shareable"][..],
            "a container has its own IPC namespace",
        ),
        ("UTSMode", &[""][..], "a container has its own hostname"),
        (
            "UsernsMode",
            &[""][..],
            "nspawn picks a container's user namespace",
        ),
        (
            "CgroupnsMode",
            &["", "private"][..],
            "a container has its own cgroup namespace",
        ),
        ("Cgroup", &[""][..], "a container has a cgroup of its own"),
        (
            "CgroupParent",
            &[""][..],
            "a container's cgroup is its systemd unit's",
        ),
        (
            "VolumeDriver",
            &["", "local"][..],
            "nspawn volumes are directories of the host",
        ),
    ] {
        let value = host.text(field)?;
        if !allowed.contains(&value.as_str()) {
            return Err(bad(format!("{field} {value} is not supported: {why}")));
        }
    }
    match host.text("Runtime")?.as_str() {
        "" => {}
        "runc" => plan.warn("Runtime runc is ignored: containers run under systemd-nspawn"),
        other => {
            return Err(bad(format!(
                "Runtime {other} is not supported: containers run under systemd-nspawn"
            )))
        }
    }
    if !host.texts("VolumesFrom")?.is_empty() {
        return Err(bad(
            "VolumesFrom is not supported: name the volumes the container mounts",
        ));
    }
    if !host.texts("GroupAdd")?.is_empty() {
        return Err(bad(
            "GroupAdd is not supported: supplementary groups would change what the program may open",
        ));
    }
    for field in ["IOMaximumIOps", "IOMaximumBandwidth"] {
        if host.int(field)? != 0 {
            return Err(bad(format!("{field} is not supported on linux")));
        }
    }
    host.text("ContainerIDFile")?;
    let log = host.object("LogConfig", "LogConfig")?;
    match log.text("Type")?.as_str() {
        "" | "json-file" | "local" => {}
        "journald" => plan.text("log_driver", "journal".to_string()),
        "none" => plan.text("log_driver", "none".to_string()),
        other => plan.warn(format!(
            "LogConfig.Type {other} is ignored: nspawn keeps a container's output in the journal"
        )),
    }
    if !log.pairs("Config")?.is_empty() {
        plan.warn("LogConfig.Config is ignored: the journal keeps a container's output");
    }
    let policy = host.object("RestartPolicy", "RestartPolicy")?;
    if let Some(restart) = restart_text(&policy)?.filter(|r| r != "no") {
        if let Some(warning) = retry_count_warning(&restart) {
            plan.warn(warning);
        }
        plan.text("restart", restart);
    }
    plan.flag("remove", host.flag("AutoRemove")?);
    plan.texts("cap_add", host.words("CapAdd")?.unwrap_or_default());
    plan.texts("cap_drop", host.words("CapDrop")?.unwrap_or_default());
    plan.texts("dns", host.texts("Dns")?);
    if !host.texts("DnsOptions")?.is_empty() {
        plan.warn("DnsOptions are ignored: nspawn writes no resolver options");
    }
    plan.texts("dns_search", host.texts("DnsSearch")?);
    plan.texts("extra_hosts", host.texts("ExtraHosts")?);
    let oom = host.int("OomScoreAdj")?;
    if oom != 0 {
        plan.opts.insert("oom_score_adj", Opt::Signed(oom));
    }
    plan.flag("privileged", host.flag("Privileged")?);
    plan.flag("read_only", host.flag("ReadonlyRootfs")?);
    if !host.texts("SecurityOpt")?.is_empty() {
        plan.warn(
            "SecurityOpt is ignored: nspawn applies its own seccomp filter and SELinux context",
        );
    }
    if !host.pairs("StorageOpt")?.is_empty() {
        plan.warn("StorageOpt is ignored: a container's layer has no quota");
    }
    if !host.pairs("Annotations")?.is_empty() {
        plan.warn("Annotations are ignored: no OCI runtime is there to take them");
    }
    let shm = host.int("ShmSize")?;
    if shm > 0 {
        plan.opts.insert("shm_size", Opt::Unsigned(shm as u64));
    }
    let sysctls: Vec<String> = host
        .pairs("Sysctls")?
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    plan.texts("sysctls", sysctls);
    if host.maybe_flag("Init")? == Some(true) {
        plan.flag("init", true);
    }
    for field in ["MaskedPaths", "ReadonlyPaths"] {
        if !host.texts(field)?.is_empty() {
            plan.warn(format!(
                "{field} are ignored: nspawn masks and protects its own set of paths"
            ));
        }
    }
    // Ports: those bound, and with PublishAllPorts each one exposed, the image's too.
    let mut publish = Vec::new();
    let bound: Vec<(String, Vec<Object>)> = match host.get("PortBindings") {
        None => Vec::new(),
        Some(Value::Object(map)) => map
            .iter()
            .map(|(port, list)| {
                let list = match list {
                    Value::Null => Ok(Vec::new()),
                    Value::Array(items) => items
                        .iter()
                        .filter(|i| !i.is_null())
                        .map(|i| Object::of(i, "nat.PortBinding"))
                        .collect(),
                    other => Err(host.wrong("PortBindings", other, "[]nat.PortBinding")),
                };
                list.map(|l| (port.clone(), l))
            })
            .collect::<Result<_, _>>()?,
        Some(other) => return Err(host.wrong("PortBindings", other, "nat.PortMap")),
    };
    for (port, list) in &bound {
        for binding in list {
            publish.push(publish_spec(
                &binding.text("HostIp")?,
                &binding.text("HostPort")?,
                port,
            ));
        }
    }
    let publish_all = host.flag("PublishAllPorts")?;
    if publish_all {
        for port in config.set("ExposedPorts")? {
            if !bound.iter().any(|(p, _)| same_port(p, &port)) {
                publish.push(publish_spec("", "", &port));
            }
        }
    }
    plan.texts("publish", publish);
    plan.flag("publish_all", publish_all);
    Ok(())
}

/// A port of the API (`80/tcp`, `53/udp`, `80`) with its protocol.
fn port_key(port: &str) -> String {
    match port.split_once('/') {
        Some((number, protocol)) => format!("{number}/{}", protocol.to_lowercase()),
        None => format!("{port}/tcp"),
    }
}

fn same_port(a: &str, b: &str) -> bool {
    port_key(a) == port_key(b)
}

/// A binding as -p writes it: [IP:][HOST:]CONTAINER/PROTOCOL, an IPv6 address in
/// brackets; no host port is a free one.
pub fn publish_spec(host_ip: &str, host_port: &str, port: &str) -> String {
    let container = port_key(port);
    let ip = if host_ip.contains(':') && !host_ip.starts_with('[') {
        format!("[{host_ip}]")
    } else {
        host_ip.to_string()
    };
    match (ip.is_empty(), host_port.is_empty()) {
        (true, true) => container,
        (true, false) => format!("{host_port}:{container}"),
        (false, _) => format!("{ip}:{host_port}:{container}"),
    }
}

/// NetworkMode and the endpoints: the networks, the first one primary, with the
/// aliases, addresses and links each one gives.
fn read_network(
    host: &Object,
    config: &Object,
    networking: &Object,
    world: &dyn World,
    plan: &mut Plan,
) -> Result<(), Refusal> {
    let mode = host.text("NetworkMode")?;
    check_network_mode(&mode, host, config)?;
    let mode = if config.flag("NetworkDisabled")? {
        "none".to_string()
    } else if mode.is_empty() || mode == "default" {
        crate::bridge::DEFAULT_NETWORK.to_string()
    } else {
        mode
    };
    let resolve = |key: &str, plan: &mut Plan| -> Option<String> {
        match key {
            "bridge" | "host" | "none" => Some(key.to_string()),
            _ => {
                if let Some(owner) = key.strip_prefix("container:") {
                    return match world.container(owner) {
                        Some(linked) => Some(format!("container:{}", linked.machine)),
                        None => {
                            plan.missing_container
                                .get_or_insert_with(|| owner.to_string());
                            None
                        }
                    };
                }
                world.network(key)
            }
        }
    };
    let primary = match resolve(&mode, plan) {
        Some(name) => name,
        None => {
            if plan.missing_container.is_none() {
                plan.missing_network.get_or_insert_with(|| mode.clone());
            }
            mode.clone()
        }
    };
    let mut others: Vec<(i64, String)> = Vec::new();
    let mut aliases = Vec::new();
    let mut addresses = Vec::new();
    let mut links = host.texts("Links")?;
    for (key, endpoint) in networking.entries("EndpointsConfig", "network.EndpointSettings")? {
        let net = if key == "default" {
            primary.clone()
        } else {
            match resolve(&key, plan) {
                Some(net) => net,
                None => {
                    let id = endpoint.text("NetworkID")?;
                    match (!id.is_empty()).then(|| world.network(&id)).flatten() {
                        Some(net) => net,
                        None => {
                            plan.missing_network.get_or_insert_with(|| key.clone());
                            key.clone()
                        }
                    }
                }
            }
        };
        if net != primary && !others.iter().any(|(_, n)| *n == net) {
            others.push((endpoint.int("GwPriority")?, net.clone()));
        }
        for alias in endpoint.texts("Aliases")? {
            aliases.push(format!("{net}={alias}"));
        }
        let ipam = endpoint.object("IPAMConfig", "network.EndpointIPAMConfig")?;
        let ipv4 = ipam.text("IPv4Address")?;
        if !ipv4.is_empty() {
            addresses.push(format!("{net}={ipv4}"));
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
        if !endpoint.text("MacAddress")?.is_empty() {
            plan.warn(format!(
                "EndpointsConfig {key}: MacAddress is ignored: nspawn derives a container's MAC address from its IP address"
            ));
        }
        if !endpoint.pairs("DriverOpts")?.is_empty() {
            plan.warn(format!(
                "EndpointsConfig {key}: DriverOpts are ignored: nspawn's networks take no driver options"
            ));
        }
        links.extend(endpoint.texts("Links")?);
    }
    // The highest priority first after the primary, as the engine picks its gateway.
    others.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut networks = vec![primary];
    networks.extend(others.into_iter().map(|(_, n)| n));
    plan.opts.insert("networks", Opt::Texts(networks));
    plan.texts("aliases", aliases);
    plan.texts("addresses", addresses);
    // A link's alias names the linked container's address; its own name resolves
    // already on a network they share.
    let mut hosts = Vec::new();
    for link in links {
        let (name, alias) = link
            .split_once(':')
            .unwrap_or((link.as_str(), link.as_str()));
        let name = name.trim_start_matches('/');
        let alias = alias.rsplit('/').next().unwrap_or(alias);
        let Some(linked) = world.container(name) else {
            plan.missing_container
                .get_or_insert_with(|| name.to_string());
            continue;
        };
        if alias == linked.machine {
            continue;
        }
        match linked.address {
            Some(address) => hosts.push(format!("{alias}:{address}")),
            None => plan.warn(format!(
                "Links {name}:{alias} is ignored: {name} has no address yet; start it first"
            )),
        }
    }
    if let Some(Opt::Texts(given)) = plan.opts.remove("extra_hosts") {
        hosts.splice(0..0, given);
    }
    plan.texts("extra_hosts", hosts);
    // The host's network and none have no ports of their own to publish.
    if let Some(Opt::Texts(networks)) = plan.opts.get("networks") {
        if matches!(networks.first().map(String::as_str), Some("host" | "none")) {
            if networks[0] == "host" && host.get("PortBindings").is_some_and(non_empty) {
                plan.warn("Published ports are discarded when using host network mode");
            }
            plan.opts.remove("publish");
            plan.opts.remove("publish_all");
        }
    }
    Ok(())
}

/// A JSON value that holds something: a list or object with an entry.
fn non_empty(value: &Value) -> bool {
    match value {
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
        _ => false,
    }
}

/// What the engine refuses of a network mode: a container's network with settings of
/// a network of its own, and the host's with links.
fn check_network_mode(mode: &str, host: &Object, config: &Object) -> Result<(), Refusal> {
    if let Some(owner) = mode.strip_prefix("container:") {
        let conflict = |what: &str| bad(format!("conflicting options: {what}"));
        if owner.is_empty() {
            return Err(bad(
                "invalid network mode: invalid container format container:<name|id>",
            ));
        }
        if !config.text("Hostname")?.is_empty() {
            return Err(conflict("hostname and the network mode"));
        }
        if !host.texts("Links")?.is_empty() {
            return Err(conflict(
                "container type network can't be used with links. This would result in undefined behavior",
            ));
        }
        if !host.texts("Dns")?.is_empty() {
            return Err(conflict("dns and the network mode"));
        }
        if !host.texts("ExtraHosts")?.is_empty() {
            return Err(conflict("custom host-to-IP mapping and the network mode"));
        }
        if host.get("PortBindings").is_some_and(non_empty) || host.flag("PublishAllPorts")? {
            return Err(conflict(
                "port publishing and the container type network mode",
            ));
        }
        if config.get("ExposedPorts").is_some_and(non_empty) {
            return Err(conflict(
                "port exposing and the container type network mode",
            ));
        }
    }
    if mode == "host" && !host.texts("Links")?.is_empty() {
        return Err(bad(
            "conflicting options: host type networking can't be used with links. This would result in undefined behavior",
        ));
    }
    Ok(())
}

/// The resources: limits taken, the rest warned of.
fn read_resources(host: &Object, plan: &mut Plan) -> Result<(), Refusal> {
    let (opts, warnings) = resources(host, true)?;
    plan.opts.extend(opts);
    plan.warnings.extend(warnings);
    Ok(())
}

/// Resources as the bus takes them: memory, memory_swap, cpus, pids_limit, and at a
/// create the devices and ulimits too; a warning for each other one that is set.
pub fn resources(host: &Object, create: bool) -> Result<(Opts, Vec<String>), Refusal> {
    let mut opts = Opts::new();
    let mut warnings = Vec::new();
    let memory = host.int("Memory")?;
    if memory > 0 {
        opts.insert("memory", Opt::Unsigned(memory as u64));
    }
    let swap = host.int("MemorySwap")?;
    if swap != 0 {
        opts.insert("memory_swap", Opt::Signed(swap));
    }
    let nano = host.int("NanoCpus")?;
    let quota = host.int("CpuQuota")?;
    let period = host.int("CpuPeriod")?;
    if nano > 0 {
        opts.insert("cpus", Opt::Float(nano as f64 / 1e9));
    } else if quota > 0 {
        let period = if period > 0 {
            period
        } else {
            DEFAULT_CPU_PERIOD
        };
        opts.insert("cpus", Opt::Float(quota as f64 / period as f64));
    }
    if let Some(pids) = host.maybe_int("PidsLimit")? {
        // 0 and below are no limit.
        if pids > 0 || !create {
            opts.insert("pids_limit", Opt::Unsigned(pids.max(0) as u64));
        }
    }
    let ignored: &[(&str, &str)] = &[
        ("CpuShares", "nspawn sets no CPU weight"),
        ("BlkioWeight", "nspawn sets no I/O limits"),
        ("BlkioWeightDevice", "nspawn sets no I/O limits"),
        ("BlkioDeviceReadBps", "nspawn sets no I/O limits"),
        ("BlkioDeviceWriteBps", "nspawn sets no I/O limits"),
        ("BlkioDeviceReadIOps", "nspawn sets no I/O limits"),
        ("BlkioDeviceWriteIOps", "nspawn sets no I/O limits"),
        ("CpuRealtimePeriod", "nspawn sets no realtime scheduling"),
        ("CpuRealtimeRuntime", "nspawn sets no realtime scheduling"),
        ("CpusetCpus", "nspawn pins no CPUs"),
        ("CpusetMems", "nspawn pins no memory nodes"),
        ("DeviceCgroupRules", "devices are given by Devices"),
        ("MemoryReservation", "nspawn sets no soft memory limit"),
    ];
    for (field, why) in ignored {
        if host.set_at(field) {
            warnings.push(format!("{field} is ignored: {why}"));
        }
    }
    if quota <= 0 && period > 0 && nano <= 0 {
        warnings.push("CpuPeriod is ignored: without CpuQuota it limits nothing".to_string());
    }
    if let Some(swappiness) = host.maybe_int("MemorySwappiness")? {
        if swappiness != -1 {
            warnings.push(
                "MemorySwappiness is ignored: nspawn leaves the kernel's swappiness".to_string(),
            );
        }
    }
    if host.maybe_flag("OomKillDisable")? == Some(true) {
        warnings.push(
            "OomKillDisable is ignored: a container over its memory limit is stopped".to_string(),
        );
    }
    if !create {
        for field in ["Devices", "DeviceRequests", "Ulimits"] {
            if host.set_at(field) {
                warnings.push(format!("{field} is ignored: an update sets limits only"));
            }
        }
        return Ok((opts, warnings));
    }
    let mut devices = Vec::new();
    for device in host.objects("Devices", "container.DeviceMapping")? {
        let on_host = device.text("PathOnHost")?;
        let inside = device.text("PathInContainer")?;
        let permissions = device.text("CgroupPermissions")?;
        let inside = if inside.is_empty() {
            on_host.clone()
        } else {
            inside
        };
        let permissions = if permissions.is_empty() {
            "rwm".to_string()
        } else {
            permissions
        };
        devices.push(format!("{on_host}:{inside}:{permissions}"));
    }
    let mut gpus = Vec::new();
    for request in host.objects("DeviceRequests", "container.DeviceRequest")? {
        let driver = request.text("Driver")?;
        let ids = request.texts("DeviceIDs")?;
        if driver == "cdi" {
            devices.extend(ids);
            continue;
        }
        let mut capabilities = Vec::new();
        if let Some(Value::Array(sets)) = request.get("Capabilities") {
            for set in sets {
                for word in set.as_array().into_iter().flatten() {
                    if let Some(word) = word.as_str().filter(|w| *w != "gpu") {
                        if !capabilities.iter().any(|c| c == word) {
                            capabilities.push(word.to_string());
                        }
                    }
                }
            }
        }
        let gpu = crate::cdi::GpuRequest {
            driver,
            count: if ids.is_empty() {
                request.int("Count")?
            } else {
                0
            },
            device_ids: ids,
            capabilities,
            options: request.pairs("Options")?,
        };
        gpus.push(gpu.to_string());
    }
    if !devices.is_empty() {
        opts.insert("devices", Opt::Texts(devices));
    }
    if !gpus.is_empty() {
        opts.insert("gpus", Opt::Texts(gpus));
    }
    let mut ulimits = Vec::new();
    for limit in host.objects("Ulimits", "container.Ulimit")? {
        ulimits.push(format!(
            "{}={}:{}",
            limit.text("Name")?,
            limit.int("Soft")?,
            limit.int("Hard")?
        ));
    }
    if !ulimits.is_empty() {
        opts.insert("ulimits", Opt::Texts(ulimits));
    }
    Ok((opts, warnings))
}

/// The engine's rule for a container's name: [a-zA-Z0-9][a-zA-Z0-9_.-]+, after a slash
/// or not.
pub fn valid_name(name: &str) -> bool {
    let name = name.strip_prefix('/').unwrap_or(name);
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && name.len() > 1
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// A name for a container made without one: the last part of its image's repository,
/// then `-` and eight hex digits of `random`, a machine name as it is.
pub fn generated_name(image: &str, random: u32) -> String {
    let repository = image
        .split('@')
        .next()
        .unwrap_or(image)
        .rsplit('/')
        .next()
        .unwrap_or(image);
    // The tag, unless what follows the colon is a port of a registry.
    let repository = repository.split(':').next().unwrap_or(repository);
    let mut base = String::new();
    for c in repository.chars() {
        if c.is_ascii_alphanumeric() {
            base.push(c.to_ascii_lowercase());
        } else if !base.ends_with('-') {
            base.push('-');
        }
    }
    let base = base.trim_matches('-');
    let base = base[..base.len().min(40)].trim_matches('-');
    // An image named by its ID has no name to give.
    let hex_id = base.len() >= 12 && base.bytes().all(|b| b.is_ascii_hexdigit());
    let base = if base.is_empty() || base == "sha256" || hex_id {
        "container"
    } else {
        base
    };
    format!("{base}-{random:08x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Fake {
        paths: Vec<&'static str>,
    }

    impl World for Fake {
        fn container(&self, key: &str) -> Option<Linked> {
            match key {
                "db" | "db-id" => Some(Linked {
                    machine: "db".into(),
                    address: Some("10.99.0.5".parse().unwrap()),
                }),
                "my_cache" => Some(Linked {
                    machine: "my-cache-1a2b3c4d".into(),
                    address: None,
                }),
                _ => None,
            }
        }

        fn network(&self, key: &str) -> Option<String> {
            match key {
                "front" | "f00d" => Some("front".into()),
                "back" => Some("back".into()),
                _ => None,
            }
        }

        fn exists(&self, path: &str) -> bool {
            self.paths.contains(&path)
        }
    }

    fn world() -> Fake {
        Fake {
            paths: vec!["/srv", "/data"],
        }
    }

    fn plan(body: Value) -> Plan {
        translate(&body, &world()).unwrap()
    }

    fn refused(body: Value) -> String {
        let r = translate(&body, &world()).unwrap_err();
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.message);
        r.message
    }

    fn texts(words: &[&str]) -> Opt {
        Opt::Texts(words.iter().map(|w| w.to_string()).collect())
    }

    /// The options of `plan` read by the parsers the service reads them with: what the
    /// translation writes, CreateMachine takes.
    fn service_takes(plan: &Plan) {
        let list = |key: &str| match plan.opts.get(key) {
            Some(Opt::Texts(list)) => list.clone(),
            None => Vec::new(),
            Some(other) => panic!("{key}: {other:?}"),
        };
        let text = |key: &str| match plan.opts.get(key) {
            Some(Opt::Text(text)) => Some(text.clone()),
            None => None,
            Some(other) => panic!("{key}: {other:?}"),
        };
        let unsigned = |key: &str| match plan.opts.get(key) {
            Some(Opt::Unsigned(n)) => Some(*n),
            None => None,
            Some(other) => panic!("{key}: {other:?}"),
        };
        let flag = |key: &str| match plan.opts.get(key) {
            Some(Opt::Flag(b)) => Some(*b),
            None => None,
            Some(other) => panic!("{key}: {other:?}"),
        };
        crate::bridge::parse_publish(&list("publish")).unwrap();
        crate::volume::parse_volumes(&list("volume")).unwrap();
        crate::volume::parse_env(&list("env")).unwrap();
        crate::volume::parse_labels(&list("label")).unwrap();
        if let Some(restart) = text("restart") {
            crate::policy::parse_restart(&restart).unwrap();
        }
        crate::api::network::choices(&list("networks")).unwrap();
        let health = crate::health::Overrides {
            test: Some(list("health_test")).filter(|t| !t.is_empty()),
            interval: unsigned("health_interval"),
            timeout: unsigned("health_timeout"),
            start_period: unsigned("health_start_period"),
            start_interval: unsigned("health_start_interval"),
            retries: unsigned("health_retries").map(|n| n as u32),
            disable: flag("no_healthcheck").unwrap_or(false),
            ..Default::default()
        };
        health.apply(None).unwrap();
        let tuning = crate::tuning::Overrides {
            hostname: text("hostname"),
            user: text("user"),
            working_dir: text("working_dir"),
            cap_add: list("cap_add"),
            cap_drop: list("cap_drop"),
            privileged: flag("privileged"),
            read_only: flag("read_only"),
            tmpfs: list("tmpfs"),
            shm_size: unsigned("shm_size"),
            devices: list("devices"),
            gpus: list("gpus"),
            dns: list("dns"),
            dns_search: list("dns_search"),
            extra_hosts: list("extra_hosts"),
            ulimits: list("ulimits"),
            oom_score_adj: match plan.opts.get("oom_score_adj") {
                Some(Opt::Signed(n)) => Some(*n as i32),
                _ => None,
            },
            stop_signal: text("stop_signal"),
            stop_timeout: unsigned("stop_timeout"),
            init: flag("init"),
            sysctls: list("sysctls"),
            log_driver: text("log_driver"),
            ..Default::default()
        };
        tuning.apply(&mut crate::tuning::Tuning::default()).unwrap();
    }

    #[test]
    fn the_service_takes_what_is_translated() {
        let p = plan(json!({
            "Image": "x", "Hostname": "web", "User": "1000:1000", "WorkingDir": "/app",
            "Env": ["A=1"], "Labels": { "a": "b" }, "StopSignal": "SIGQUIT", "StopTimeout": -1,
            "ExposedPorts": { "53/udp": {} },
            "Healthcheck": { "Test": ["CMD", "true"], "Interval": 1_000_000_000i64, "Retries": 2 },
            "HostConfig": {
                "Binds": ["/srv:/srv:ro,Z", "data:/data"],
                "Mounts": [{ "Type": "tmpfs", "Target": "/t", "TmpfsOptions": { "SizeBytes": 1048576 } }],
                "PortBindings": {
                    "80/tcp": [{ "HostIp": "127.0.0.1", "HostPort": "" }, { "HostIp": "::1", "HostPort": "8081" }],
                    "90/tcp": [{ "HostPort": "9000-9010" }],
                },
                "PublishAllPorts": true,
                "RestartPolicy": { "Name": "on-failure", "MaximumRetryCount": 5 },
                "CapAdd": ["CAP_NET_ADMIN"], "CapDrop": ["MKNOD"], "Dns": ["9.9.9.9"],
                "DnsSearch": ["lan"], "ExtraHosts": ["h:10.0.0.1", "g:host-gateway"],
                "Tmpfs": { "/run": "rw,size=1m" }, "Sysctls": { "net.core.somaxconn": "1024" },
                "Ulimits": [{ "Name": "nofile", "Soft": 1024, "Hard": -1 }],
                "Devices": [{ "PathOnHost": "/dev/null", "PathInContainer": "/dev/n", "CgroupPermissions": "rw" }],
                "DeviceRequests": [{ "Driver": "nvidia", "DeviceIDs": ["0", "1"], "Capabilities": [["gpu", "compute"]] }],
                "OomScoreAdj": -5, "ShmSize": 1048576, "Init": true, "Privileged": true,
                "ReadonlyRootfs": true, "LogConfig": { "Type": "none" },
            },
            "NetworkingConfig": { "EndpointsConfig": { "front": { "Aliases": ["w"] } } },
        }));
        service_takes(&p);
        assert_eq!(p.opts["networks"], texts(&["bridge", "front"]));
        service_takes(&plan(
            json!({ "Image": "x", "Healthcheck": { "Test": ["NONE"] } }),
        ));
        service_takes(&plan(
            json!({ "Image": "x", "HostConfig": { "NetworkMode": "container:db" } }),
        ));
        service_takes(&plan(
            json!({ "Image": "x", "HostConfig": { "NetworkMode": "host" } }),
        ));
        // Every option the translation writes is one of CreateMachine's.
        let known = [
            "addresses",
            "aliases",
            "api_name",
            "cap_add",
            "cap_drop",
            "command",
            "cpus",
            "devices",
            "dns",
            "dns_search",
            "entrypoint",
            "env",
            "extra_hosts",
            "gpus",
            "health_interval",
            "health_retries",
            "health_start_interval",
            "health_start_period",
            "health_test",
            "health_timeout",
            "hostname",
            "init",
            "interactive",
            "label",
            "log_driver",
            "memory",
            "memory_swap",
            "networks",
            "no_healthcheck",
            "oom_score_adj",
            "pids_limit",
            "privileged",
            "publish",
            "publish_all",
            "read_only",
            "remove",
            "restart",
            "shm_size",
            "stdin_once",
            "stop_signal",
            "stop_timeout",
            "sysctls",
            "tmpfs",
            "tty",
            "ulimits",
            "unset_env",
            "user",
            "volume",
            "working_dir",
        ];
        for key in p.opts.keys() {
            assert!(known.contains(key), "{key}");
        }
    }

    #[test]
    fn what_the_cli_sends_for_run_is_taken() {
        // docker run -d --name life -l e2e=lifecycle -e A=1 -p 8080:80 busybox sleep 3600,
        // with the defaults the command line always sends.
        let p = plan(serde_json::from_str(r#"{
            "Hostname": "", "Domainname": "", "User": "", "AttachStdin": false,
            "AttachStdout": false, "AttachStderr": false,
            "ExposedPorts": { "80/tcp": {} }, "Tty": false, "OpenStdin": false,
            "StdinOnce": false, "Env": ["A=1"], "Cmd": ["sleep", "3600"],
            "Image": "busybox:1.37", "Volumes": {}, "WorkingDir": "", "Entrypoint": null,
            "OnBuild": null, "Labels": { "e2e": "lifecycle" },
            "HostConfig": {
                "Binds": null, "ContainerIDFile": "", "LogConfig": { "Type": "", "Config": {} },
                "NetworkMode": "default", "PortBindings": { "80/tcp": [{ "HostIp": "", "HostPort": "8080" }] },
                "RestartPolicy": { "Name": "no", "MaximumRetryCount": 0 }, "AutoRemove": false,
                "VolumeDriver": "", "VolumesFrom": null, "ConsoleSize": [0, 0], "CapAdd": null,
                "CapDrop": null, "CgroupnsMode": "", "Dns": [], "DnsOptions": [], "DnsSearch": [],
                "ExtraHosts": null, "GroupAdd": null, "IpcMode": "", "Cgroup": "", "Links": null,
                "OomScoreAdj": 0, "PidMode": "", "Privileged": false, "PublishAllPorts": false,
                "ReadonlyRootfs": false, "SecurityOpt": null, "UTSMode": "", "UsernsMode": "",
                "ShmSize": 0, "Isolation": "", "CpuShares": 0, "Memory": 0, "NanoCpus": 0,
                "CgroupParent": "", "BlkioWeight": 0, "BlkioWeightDevice": [],
                "BlkioDeviceReadBps": [], "BlkioDeviceWriteBps": [], "BlkioDeviceReadIOps": [],
                "BlkioDeviceWriteIOps": [], "CpuPeriod": 0, "CpuQuota": 0,
                "CpuRealtimePeriod": 0, "CpuRealtimeRuntime": 0, "CpusetCpus": "",
                "CpusetMems": "", "Devices": [], "DeviceCgroupRules": null,
                "DeviceRequests": null, "MemoryReservation": 0, "MemorySwap": 0,
                "MemorySwappiness": -1, "OomKillDisable": false, "PidsLimit": 0,
                "Ulimits": [], "CpuCount": 0, "CpuPercent": 0, "IOMaximumIOps": 0,
                "IOMaximumBandwidth": 0, "MaskedPaths": null, "ReadonlyPaths": null
            },
            "NetworkingConfig": { "EndpointsConfig": { "default": { "GwPriority": 0 } } }
        }"#).unwrap());
        assert!(p.warnings.is_empty(), "{:?}", p.warnings);
        assert_eq!(p.image, "busybox:1.37");
        let expected = Opts::from([
            ("command", texts(&["sleep", "3600"])),
            ("env", texts(&["A=1"])),
            ("label", texts(&["e2e=lifecycle"])),
            ("networks", texts(&["bridge"])),
            ("publish", texts(&["8080:80/tcp"])),
        ]);
        assert_eq!(p.opts, expected);
    }

    #[test]
    fn the_terminal_input_and_removal_are_kept() {
        // docker create -it --rm busybox sh
        let p = plan(json!({
            "Image": "busybox:1.37", "Cmd": ["sh"], "Tty": true, "OpenStdin": true,
            "StdinOnce": true, "AttachStdin": true,
            "HostConfig": { "AutoRemove": true, "RestartPolicy": { "Name": "" } },
        }));
        for key in ["tty", "interactive", "stdin_once", "remove"] {
            assert_eq!(p.opts.get(key), Some(&Opt::Flag(true)), "{key}");
        }
        assert!(!p.opts.contains_key("restart"));
        assert_eq!(
            refused(json!({
                "Image": "busybox:1.37",
                "HostConfig": { "AutoRemove": true, "RestartPolicy": { "Name": "always" } },
            })),
            "can't create 'AutoRemove' container with restart policy"
        );
    }

    #[test]
    fn entrypoint_and_cmd_as_the_engine_merges_them() {
        let opts = |body: Value| plan(body).opts;
        // Neither: the image's own.
        let p = plan(json!({ "Image": "x" }));
        assert!(!p.opts.contains_key("entrypoint") && !p.opts.contains_key("command"));
        // A string is a list of one.
        assert_eq!(
            opts(json!({ "Image": "x", "Entrypoint": "/bin/sh", "Cmd": "-c" }))["entrypoint"],
            texts(&["/bin/sh"])
        );
        // An entrypoint without Cmd runs alone.
        let p = plan(json!({ "Image": "x", "Entrypoint": ["tail", "-f", "/dev/null"] }));
        assert_eq!(p.opts["entrypoint"], texts(&["tail", "-f", "/dev/null"]));
        assert!(!p.opts.contains_key("command") && !p.image_cmd);
        // [] and [""] are none: Cmd alone; without one, [] runs the image's Cmd and
        // [""] nothing, which the service refuses.
        for empty in [json!([]), json!([""])] {
            let p = plan(json!({ "Image": "x", "Entrypoint": empty, "Cmd": ["echo", "hi"] }));
            assert_eq!(p.opts["entrypoint"], texts(&[]));
            assert_eq!(p.opts["command"], texts(&["echo", "hi"]));
        }
        assert!(plan(json!({ "Image": "x", "Entrypoint": [] })).image_cmd);
        let p = plan(json!({ "Image": "x", "Entrypoint": [""] }));
        assert!(!p.image_cmd);
        assert_eq!(p.opts["entrypoint"], texts(&[]));
    }

    #[test]
    fn a_bare_name_in_env_takes_the_image_s_variable_away() {
        let p = plan(json!({ "Image": "x", "Env": ["A=1", "PATH", "B="] }));
        assert_eq!(p.opts["env"], texts(&["A=1", "B="]));
        assert_eq!(p.opts["unset_env"], texts(&["PATH"]));
        assert_eq!(
            refused(json!({ "Image": "x", "Env": ["=x"] })),
            "invalid environment variable: =x"
        );
    }

    #[test]
    fn the_restart_policy_keeps_its_count() {
        let restart = |policy: Value| {
            plan(json!({ "Image": "x", "HostConfig": { "RestartPolicy": policy } }))
                .opts
                .get("restart")
                .cloned()
        };
        assert_eq!(
            restart(json!({ "Name": "on-failure", "MaximumRetryCount": 3 })),
            Some(Opt::Text("on-failure:3".into()))
        );
        let p = plan(
            json!({ "Image": "x", "HostConfig": { "RestartPolicy": { "Name": "on-failure", "MaximumRetryCount": 3 } } }),
        );
        assert_eq!(p.warnings, [retry_count_warning("on-failure:3").unwrap()]);
        assert_eq!(retry_count_warning("on-failure"), None);
        assert_eq!(
            restart(json!({ "Name": "unless-stopped" })),
            Some(Opt::Text("unless-stopped".into()))
        );
        assert_eq!(restart(json!({ "Name": "no" })), None);
        assert_eq!(restart(json!({ "Name": "" })), None);
        assert!(refused(json!({ "Image": "x", "HostConfig": { "RestartPolicy": { "Name": "always", "MaximumRetryCount": 2 } } }))
            .contains("maximum retry count can only be used with 'on-failure'"));
        assert!(refused(
            json!({ "Image": "x", "HostConfig": { "RestartPolicy": { "Name": "sometimes" } } })
        )
        .starts_with("invalid restart policy: unknown policy 'sometimes'"));
    }

    #[test]
    fn ports_follow_the_network_mode_as_the_engine_takes_them() {
        let ports = json!({ "80/tcp": [{ "HostIp": "", "HostPort": "8080" }] });
        let p = plan(
            json!({ "Image": "x", "HostConfig": { "NetworkMode": "host", "PortBindings": ports } }),
        );
        assert!(!p.opts.contains_key("publish"), "{:?}", p.opts);
        assert_eq!(
            p.warnings,
            ["Published ports are discarded when using host network mode"]
        );
        let p = plan(
            json!({ "Image": "x", "HostConfig": { "NetworkMode": "none", "PortBindings": ports, "PublishAllPorts": true } }),
        );
        assert!(!p.opts.contains_key("publish") && !p.opts.contains_key("publish_all"));
        assert!(p.warnings.is_empty());
        let p = plan(
            json!({ "Image": "x", "NetworkDisabled": true, "HostConfig": { "PortBindings": ports } }),
        );
        assert!(!p.opts.contains_key("publish"));
        for (body, message) in [
            (
                json!({ "Image": "x", "HostConfig": { "NetworkMode": "container:db", "PortBindings": ports } }),
                "conflicting options: port publishing and the container type network mode",
            ),
            (
                json!({ "Image": "x", "ExposedPorts": { "80/tcp": {} }, "HostConfig": { "NetworkMode": "container:db" } }),
                "conflicting options: port exposing and the container type network mode",
            ),
            (
                json!({ "Image": "x", "Hostname": "h", "HostConfig": { "NetworkMode": "container:db" } }),
                "conflicting options: hostname and the network mode",
            ),
            (
                json!({ "Image": "x", "HostConfig": { "NetworkMode": "container:db", "Dns": ["1.1.1.1"] } }),
                "conflicting options: dns and the network mode",
            ),
            (
                json!({ "Image": "x", "HostConfig": { "NetworkMode": "container:" } }),
                "invalid network mode: invalid container format container:<name|id>",
            ),
            (
                json!({ "Image": "x", "HostConfig": { "NetworkMode": "host", "Links": ["db:db"] } }),
                "conflicting options: host type networking can't be used with links. This would result in undefined behavior",
            ),
        ] {
            assert_eq!(refused(body), message);
        }
    }

    #[test]
    fn missing_bind_sources_are_taken_and_bind_mounts_refused() {
        let p = plan(json!({
            "Image": "x",
            "HostConfig": { "Binds": [
                "/srv:/srv:ro", "/new/dir:/d", "data:/data",
                "/var/run/docker.sock:/var/run/docker.sock", "/new/dir:/again",
            ] },
        }));
        assert_eq!(
            p.opts["volume"],
            texts(&[
                "/srv:/srv:ro",
                "/new/dir:/d",
                "data:/data",
                "/var/run/docker.sock:/var/run/docker.sock",
                "/new/dir:/again",
            ])
        );
        assert_eq!(
            refused(json!({ "Image": "x", "HostConfig": { "Mounts": [
                { "Type": "bind", "Source": "/nope", "Target": "/n" },
            ] } })),
            "invalid mount config for type \"bind\": bind source path does not exist: /nope"
        );
        // Unless it asks to have it made.
        let p = plan(json!({ "Image": "x", "HostConfig": { "Mounts": [
            { "Type": "bind", "Source": "/nope", "Target": "/n", "ReadOnly": true,
              "BindOptions": { "CreateMountpoint": true } },
            { "Type": "volume", "Source": "cache", "Target": "/cache" },
            { "Type": "tmpfs", "Target": "/tmp", "TmpfsOptions": { "SizeBytes": 65536, "Mode": 1023 } },
            { "Type": "volume", "Target": "/anonymous" },
        ] } }));
        assert_eq!(p.opts["volume"], texts(&["/nope:/n:ro", "cache:/cache"]));
        assert_eq!(p.opts["tmpfs"], texts(&["/tmp:size=65536,mode=1777"]));
        assert_eq!(p.warnings.len(), 1, "{:?}", p.warnings);
        assert!(p.warnings[0].starts_with("Mounts /anonymous is ignored"));
        assert!(refused(json!({ "Image": "x", "HostConfig": { "Mounts": [
            { "Type": "npipe", "Source": "x", "Target": "/x" },
        ] } }))
        .contains("npipe"));
    }

    #[test]
    fn volumes_without_a_mount_are_warned_of() {
        let p = plan(json!({
            "Image": "x", "Volumes": { "/data": {}, "/cache": {} },
            "HostConfig": { "Binds": ["/data:/data"] },
        }));
        assert_eq!(
            p.warnings,
            ["Volumes /cache is ignored: what the container writes there stays in its own layer"]
        );
    }

    #[test]
    fn networks_aliases_addresses_and_links() {
        let p = plan(json!({
            "Image": "x",
            "HostConfig": { "NetworkMode": "front", "Links": ["db:database", "/db:/web/db"] },
            "NetworkingConfig": { "EndpointsConfig": {
                "front": { "Aliases": ["www"], "IPAMConfig": { "IPv4Address": "10.99.1.9" } },
                "back": { "GwPriority": 1 },
                "bridge": { "GwPriority": 2, "MacAddress": "02:42:ac:11:00:02" },
            } },
        }));
        assert_eq!(p.opts["networks"], texts(&["front", "bridge", "back"]));
        assert_eq!(p.opts["aliases"], texts(&["front=www"]));
        assert_eq!(p.opts["addresses"], texts(&["front=10.99.1.9"]));
        assert_eq!(p.opts["extra_hosts"], texts(&["database:10.99.0.5"]));
        assert_eq!(p.warnings.len(), 1);
        // The default endpoint is the network mode's; tc-go names the bridge by its ID.
        let p = plan(json!({
            "Image": "x", "HostConfig": { "NetworkMode": "bridge" },
            "NetworkingConfig": { "EndpointsConfig": { "default": { "Aliases": ["a"] } } },
        }));
        assert_eq!(p.opts["networks"], texts(&["bridge"]));
        assert_eq!(p.opts["aliases"], texts(&["bridge=a"]));
        let p = plan(json!({
            "Image": "x", "HostConfig": { "NetworkMode": "f00d" },
            "NetworkingConfig": { "EndpointsConfig": { "nope": { "NetworkID": "f00d" } } },
        }));
        assert_eq!(p.opts["networks"], texts(&["front"]));
        assert_eq!(p.missing_network, None);
        // Another container's namespace, by its API name; none at all.
        let p =
            plan(json!({ "Image": "x", "HostConfig": { "NetworkMode": "container:my_cache" } }));
        assert_eq!(p.opts["networks"], texts(&["container:my-cache-1a2b3c4d"]));
        let p = plan(json!({ "Image": "x", "NetworkDisabled": true }));
        assert_eq!(p.opts["networks"], texts(&["none"]));
        // What does not exist is found missing once the image is.
        let p = plan(json!({ "Image": "x", "HostConfig": { "NetworkMode": "nope" } }));
        assert_eq!(p.missing_network.as_deref(), Some("nope"));
        let p = plan(json!({ "Image": "x", "HostConfig": { "NetworkMode": "container:gone" } }));
        assert_eq!(p.missing_container.as_deref(), Some("gone"));
        assert_eq!(p.missing_network, None);
        assert!(refused(
            json!({ "Image": "x", "NetworkingConfig": { "EndpointsConfig": {
            "front": { "IPAMConfig": { "IPv6Address": "fd00::2" } },
        } } })
        )
        .contains("IPv4 only"));
    }

    #[test]
    fn ports_bound_and_published_all() {
        let p = plan(json!({
            "Image": "x", "ExposedPorts": { "80/tcp": {}, "53/udp": {}, "9000/tcp": {} },
            "HostConfig": {
                "PublishAllPorts": true,
                "PortBindings": {
                    "80/tcp": [{ "HostIp": "127.0.0.1", "HostPort": "" }, { "HostIp": "::1", "HostPort": "8081" }],
                    "9000/tcp": [{ "HostIp": "", "HostPort": "0" }],
                },
            },
        }));
        assert_eq!(
            p.opts["publish"],
            texts(&[
                "127.0.0.1::80/tcp",
                "[::1]:8081:80/tcp",
                "0:9000/tcp",
                "53/udp",
            ])
        );
        assert_eq!(p.opts["publish_all"], Opt::Flag(true));
        assert_eq!(publish_spec("", "", "80"), "80/tcp");
    }

    #[test]
    fn resources_limits_and_devices() {
        let p = plan(json!({ "Image": "x", "HostConfig": {
            "Memory": 67108864, "MemorySwap": -1, "NanoCpus": 1500000000, "PidsLimit": 64,
            "Ulimits": [{ "Name": "nofile", "Soft": 1024, "Hard": 2048 }],
            "Devices": [{ "PathOnHost": "/dev/fuse", "PathInContainer": "", "CgroupPermissions": "" }],
            "DeviceRequests": [
                { "Driver": "nvidia", "Count": -1, "Capabilities": [["gpu"]] },
                { "Driver": "cdi", "DeviceIDs": ["vendor.com/class=x"] },
            ],
            "CpuShares": 512, "MemoryReservation": 1024, "MemorySwappiness": -1,
            "OomKillDisable": false, "OomScoreAdj": 100, "ShmSize": 1048576, "Init": true,
            "Privileged": true, "ReadonlyRootfs": true, "Tmpfs": { "/run": "size=1m" },
            "Sysctls": { "net.ipv4.ip_forward": "1" }, "CapAdd": ["CAP_NET_ADMIN"],
            "Dns": ["1.1.1.1"], "DnsSearch": ["lan"], "ExtraHosts": ["host.docker.internal:host-gateway"],
            "LogConfig": { "Type": "journald" },
        } }));
        let o = &p.opts;
        assert_eq!(o["memory"], Opt::Unsigned(67108864));
        assert_eq!(o["memory_swap"], Opt::Signed(-1));
        assert_eq!(o["cpus"], Opt::Float(1.5));
        assert_eq!(o["pids_limit"], Opt::Unsigned(64));
        assert_eq!(o["ulimits"], texts(&["nofile=1024:2048"]));
        assert_eq!(
            o["devices"],
            texts(&["/dev/fuse:/dev/fuse:rwm", "vendor.com/class=x"])
        );
        let all = crate::cdi::GpuRequest {
            driver: "nvidia".into(),
            count: -1,
            ..Default::default()
        };
        assert_eq!(o["gpus"], Opt::Texts(vec![all.to_string()]));
        assert_eq!(o["oom_score_adj"], Opt::Signed(100));
        assert_eq!(o["shm_size"], Opt::Unsigned(1048576));
        assert_eq!(o["init"], Opt::Flag(true));
        assert_eq!(o["privileged"], Opt::Flag(true));
        assert_eq!(o["read_only"], Opt::Flag(true));
        assert_eq!(o["tmpfs"], texts(&["/run:size=1m"]));
        assert_eq!(o["sysctls"], texts(&["net.ipv4.ip_forward=1"]));
        assert_eq!(o["cap_add"], texts(&["CAP_NET_ADMIN"]));
        assert_eq!(o["dns"], texts(&["1.1.1.1"]));
        assert_eq!(o["dns_search"], texts(&["lan"]));
        assert_eq!(
            o["extra_hosts"],
            texts(&["host.docker.internal:host-gateway"])
        );
        assert_eq!(o["log_driver"], Opt::Text("journal".into()));
        assert_eq!(
            p.warnings,
            [
                "CpuShares is ignored: nspawn sets no CPU weight",
                "MemoryReservation is ignored: nspawn sets no soft memory limit",
            ]
        );
        // A quota over the default period.
        let p = plan(json!({ "Image": "x", "HostConfig": { "CpuQuota": 50000 } }));
        assert_eq!(p.opts["cpus"], Opt::Float(0.5));
        assert_eq!(
            refused(json!({ "Image": "x", "HostConfig": { "NanoCpus": 1, "CpuQuota": 50000 } })),
            "Conflicting options: Nano CPUs and CPU Quota cannot both be set"
        );
    }

    #[test]
    fn what_would_change_the_container_is_refused() {
        for (host, field) in [
            (json!({ "PidMode": "host" }), "PidMode"),
            (json!({ "IpcMode": "host" }), "IpcMode"),
            (json!({ "UTSMode": "host" }), "UTSMode"),
            (json!({ "UsernsMode": "host" }), "UsernsMode"),
            (json!({ "CgroupnsMode": "host" }), "CgroupnsMode"),
            (json!({ "CgroupParent": "/x" }), "CgroupParent"),
            (json!({ "Runtime": "kata" }), "Runtime"),
            (json!({ "VolumeDriver": "nfs" }), "VolumeDriver"),
            (json!({ "VolumesFrom": ["db"] }), "VolumesFrom"),
            (json!({ "GroupAdd": ["audio"] }), "GroupAdd"),
            (json!({ "IOMaximumIOps": 5 }), "IOMaximumIOps"),
        ] {
            let text = refused(json!({ "Image": "x", "HostConfig": host }));
            assert!(text.starts_with(field), "{text}");
        }
        assert_eq!(
            refused(json!({ "Image": "x", "HostConfig": { "Isolation": "hyperv" } })),
            "invalid isolation 'hyperv' on linux"
        );
        // Their defaults are taken.
        plan(json!({ "Image": "x", "HostConfig": {
            "IpcMode": "shareable", "CgroupnsMode": "private", "VolumeDriver": "local",
            "Isolation": "default", "Runtime": "",
        } }));
    }

    #[test]
    fn warnings_for_what_does_nothing() {
        let p = plan(json!({
            "Image": "x", "Domainname": "example.org", "MacAddress": "02:42:ac:11:00:02",
            "HostConfig": {
                "Runtime": "runc", "SecurityOpt": ["label=disable"], "DnsOptions": ["ndots:2"],
                "LogConfig": { "Type": "syslog", "Config": { "tag": "x" } },
                "MemorySwappiness": 10, "OomKillDisable": true, "CpusetCpus": "0-1",
                "MaskedPaths": ["/proc/kcore"],
            },
        }));
        assert_eq!(p.warnings.len(), 11, "{:#?}", p.warnings);
        assert!(!p.opts.contains_key("log_driver"));
    }

    #[test]
    fn healthchecks_in_microseconds() {
        let p = plan(json!({ "Image": "x", "Healthcheck": {
            "Test": ["CMD-SHELL", "true"], "Interval": 2_000_000_000i64, "Retries": 3,
        } }));
        assert_eq!(p.opts["health_test"], texts(&["CMD-SHELL", "true"]));
        assert_eq!(p.opts["health_interval"], Opt::Unsigned(2_000_000));
        assert_eq!(p.opts["health_retries"], Opt::Unsigned(3));
        let p = plan(json!({ "Image": "x", "Healthcheck": { "Test": ["NONE"] } }));
        assert_eq!(p.opts["no_healthcheck"], Opt::Flag(true));
        assert_eq!(
            refused(json!({ "Image": "x", "Healthcheck": { "Interval": 10 } })),
            "Interval in Healthcheck cannot be less than 1ms"
        );
    }

    #[test]
    fn bodies_are_read_as_the_engine_reads_them() {
        let golden = |name: &str| -> String {
            let text = std::fs::read_to_string(format!(
                "{}/tests/dockerapi/golden/{name}.body",
                env!("CARGO_MANIFEST_DIR")
            ))
            .unwrap();
            serde_json::from_str::<Value>(&text).unwrap()["message"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(
            decode(br#"{"Image":"busybox:1.37""#).unwrap_err().message,
            golden("create-badjson")
        );
        assert_eq!(
            decode(b"  ").unwrap_err().message,
            "invalid JSON: got EOF while reading request body"
        );
        assert_eq!(
            decode(b"{} {}").unwrap_err().message,
            "unexpected content after JSON"
        );
        assert!(decode(b"{x}")
            .unwrap_err()
            .message
            .starts_with("invalid JSON: "));
        assert_eq!(
            translate(&json!({ "HostConfig": {} }), &world())
                .unwrap_err()
                .message,
            EMPTY_CONFIG
        );
        assert_eq!(
            translate(&Value::Null, &world()).unwrap_err().message,
            EMPTY_CONFIG
        );
        assert_eq!(
            refused(json!({ "Image": "x", "Cmd": 5 })),
            "invalid JSON: json: cannot unmarshal number 5 into Go struct field Config.Cmd of type strslice.StrSlice"
        );
        assert_eq!(
            refused(json!({ "Image": "x", "WorkingDir": "app" })),
            "the working directory 'app' is invalid, it needs to be an absolute path"
        );
        // Keys of any case, as Go matches them.
        assert_eq!(plan(json!({ "image": "x" })).image, "x");
        assert_eq!(
            golden("create-autoremove-restart"),
            refused(json!({
                "Image": "busybox:1.37",
                "HostConfig": { "AutoRemove": true, "RestartPolicy": { "Name": "always" } },
            }))
        );
    }

    #[test]
    fn names_as_the_engine_takes_them() {
        let long = "x".repeat(200);
        for good in ["life", "/life", "e2e_dk_b", "a1", "A.b-c_d", long.as_str()] {
            assert!(valid_name(good), "{good}");
        }
        for bad in ["", "a", "/", "-life", "_x", ".x", "a b", "a/b", "\u{e4}1"] {
            assert!(!valid_name(bad), "{bad}");
        }
        assert_eq!(generated_name("busybox:1.37", 0xab), "busybox-000000ab");
        assert_eq!(
            generated_name("ghcr.io/o/My_App@sha256:abc", 1),
            "my-app-00000001"
        );
        assert_eq!(generated_name("registry:5000/team/web", 2), "web-00000002");
        assert_eq!(
            generated_name(&format!("sha256:{}", "a".repeat(64)), 3),
            "container-00000003"
        );
        assert_eq!(generated_name("30ecbe150909", 4), "container-00000004");
        for name in ["busybox-000000ab", "my-app-00000001", "container-00000003"] {
            crate::reference::validate_machine_name(name).unwrap();
        }
    }
}
