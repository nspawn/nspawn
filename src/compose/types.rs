//! compose-go's structs of the model (types/types.go and its neighbours), field by
//! field in their order, with their YAML and JSON names: what `config` writes and in
//! what order comes from here.

use super::model::{Custom, Def, Field, Json, Member, Ty};

/// `yaml:"key,omitempty" json:"key,omitempty"`, the common case.
const fn f(key: &'static str, ty: Ty) -> Member {
    Member::F(Field {
        key,
        ty,
        omit: true,
        json: Json::Key {
            name: key,
            omitempty: true,
            omitzero: false,
        },
    })
}

/// `yaml:"key" json:"key"`: written even when zero.
const fn always(key: &'static str, ty: Ty) -> Member {
    Member::F(Field {
        key,
        ty,
        omit: false,
        json: Json::Key {
            name: key,
            omitempty: false,
            omitzero: false,
        },
    })
}

/// `yaml:"key,omitempty" json:"key"`: left out of YAML when zero, written to JSON.
const fn json_always(key: &'static str, ty: Ty) -> Member {
    Member::F(Field {
        key,
        ty,
        omit: true,
        json: Json::Key {
            name: key,
            omitempty: false,
            omitzero: false,
        },
    })
}

/// `yaml:"key,omitempty" json:"key,omitzero"`: an opt-out, true unless written.
const fn opt_out(key: &'static str) -> Member {
    Member::F(Field {
        key,
        ty: Ty::OptOut,
        omit: true,
        json: Json::Key {
            name: key,
            omitempty: false,
            omitzero: true,
        },
    })
}

/// `yaml:"key,omitempty" json:"-"`.
const fn yaml_only(key: &'static str, ty: Ty) -> Member {
    Member::F(Field {
        key,
        ty,
        omit: true,
        json: Json::Skip,
    })
}

/// A field without tags: YAML lowercases its name, JSON keeps it.
const fn untagged(key: &'static str, go: &'static str, ty: Ty) -> Member {
    Member::F(Field {
        key,
        ty,
        omit: false,
        json: Json::Key {
            name: go,
            omitempty: false,
            omitzero: false,
        },
    })
}

const fn def(members: &'static [Member]) -> Def {
    Def {
        members,
        custom: Custom::None,
    }
}

use Member::{Embed, Extensions};
use Ty::{
    Bool, DeviceCount, Duration, FileMode, Float32, HealthCheckTest, HostsList, Int, Int64, Labels,
    Mapping, MappingWithEquals, MultiOptions, NanoCpus, Options, Ptr, ShellCommand, Slice,
    SshConfig, Str, StringList, StringOrNumberList, Struct, Uint16, Uint32, Uint64, Ulimit,
    UnitBytes,
};

const STRINGS: Ty = Slice(&Str);

pub static CONTAINER_SPEC: Def = def(&[
    f("annotations", Mapping),
    f("blkio_config", Ptr(&Struct(&BLKIO_CONFIG))),
    f("cap_add", STRINGS),
    f("cap_drop", STRINGS),
    f("cgroup_parent", Str),
    f("cgroup", Str),
    f("cpu_count", Int64),
    f("cpu_percent", Float32),
    f("cpu_period", Int64),
    f("cpu_quota", Int64),
    f("cpu_rt_period", Int64),
    f("cpu_rt_runtime", Int64),
    f("cpus", Float32),
    f("cpuset", Str),
    f("cpu_shares", Int64),
    json_always("command", ShellCommand),
    f("configs", Slice(&Struct(&FILE_REFERENCE))),
    f("credential_spec", Ptr(&Struct(&CREDENTIAL_SPEC))),
    f("device_cgroup_rules", STRINGS),
    f("devices", Slice(&Struct(&DEVICE_MAPPING))),
    f("dns", StringList),
    f("dns_opt", STRINGS),
    f("dns_search", StringList),
    f("domainname", Str),
    json_always("entrypoint", ShellCommand),
    f("environment", MappingWithEquals),
    f("env_file", Slice(&Struct(&ENV_FILE))),
    f("extra_hosts", HostsList),
    f("group_add", STRINGS),
    f("gpus", Slice(&Struct(&DEVICE_REQUEST))),
    f("hostname", Str),
    f("image", Str),
    f("init", Ptr(&Bool)),
    f("ipc", Str),
    f("isolation", Str),
    f("labels", Labels),
    f("label_file", STRINGS),
    f("logging", Ptr(&Struct(&LOGGING))),
    f("log_driver", Str),
    f("log_opt", Ty::Map(&Str)),
    f("mem_limit", UnitBytes),
    f("mem_reservation", UnitBytes),
    f("memswap_limit", UnitBytes),
    f("mem_swappiness", UnitBytes),
    f("mac_address", Str),
    f("models", Ty::Map(&Ptr(&Struct(&SERVICE_MODEL)))),
    f("network_mode", Str),
    f("networks", Ty::Map(&Ptr(&Struct(&SERVICE_NETWORK)))),
    f("oom_kill_disable", Bool),
    f("oom_score_adj", Int64),
    f("pid", Str),
    f("pids_limit", Int64),
    f("platform", Str),
    f("privileged", Bool),
    f("pull_policy", Str),
    f("pull_refresh_after", Str),
    f("read_only", Bool),
    f("runtime", Str),
    f("secrets", Slice(&Struct(&FILE_REFERENCE))),
    f("security_opt", STRINGS),
    f("shm_size", UnitBytes),
    f("stop_grace_period", Ptr(&Duration)),
    f("stop_signal", Str),
    f("storage_opt", Ty::Map(&Str)),
    f("sysctls", Mapping),
    f("tmpfs", StringList),
    f("ulimits", Ty::Map(&Ptr(&Ulimit))),
    f("use_api_socket", Bool),
    f("user", Str),
    f("userns_mode", Str),
    f("uts", Str),
    f("volume_driver", Str),
    f("volumes", Slice(&Struct(&SERVICE_VOLUME))),
    f("volumes_from", STRINGS),
    f("working_dir", Str),
]);

pub static WORKLOAD_SPEC: Def = def(&[
    f("build", Ptr(&Struct(&BUILD))),
    f("depends_on", Ty::Map(&Struct(&SERVICE_DEPENDENCY))),
    f("dockerfile", Str),
    f("expose", StringOrNumberList),
    f("healthcheck", Ptr(&Struct(&HEALTHCHECK))),
    f("ports", Slice(&Struct(&SERVICE_PORT))),
    f("stdin_open", Bool),
    f("tty", Bool),
]);

pub static SERVICE: Def = Def {
    members: &[
        yaml_only("name", Str),
        f("profiles", STRINGS),
        f("deploy", Ptr(&Struct(&DEPLOY))),
        f("develop", Ptr(&Struct(&DEVELOP))),
        f("restart", Str),
        f("scale", Ptr(&Int)),
        f("attach", Ptr(&Bool)),
        f("container_name", Str),
        f("provider", Ptr(&Struct(&PROVIDER))),
        f("extends", Ptr(&Struct(&EXTENDS))),
        f("external_links", STRINGS),
        f("links", STRINGS),
        f("net", Str),
        f("pre_start", Slice(&Struct(&PRE_START_HOOK))),
        f("post_start", Slice(&Struct(&SERVICE_HOOK))),
        f("pre_stop", Slice(&Struct(&SERVICE_HOOK))),
        Embed(&CONTAINER_SPEC),
        Embed(&WORKLOAD_SPEC),
        Extensions,
    ],
    custom: Custom::Service,
};

/// IncludeConfig, which an `include` entry decodes into.
static INCLUDE: Def = def(&[
    f("path", StringList),
    f("project_directory", Str),
    f("env_file", StringList),
]);

/// What `include` decodes into: []IncludeConfig.
pub static INCLUDES: Ty = Slice(&Struct(&INCLUDE));

pub static JOB: Def = def(&[
    yaml_only("name", Str),
    f("profiles", STRINGS),
    f("triggers", Ptr(&Struct(&TRIGGERS))),
    Embed(&CONTAINER_SPEC),
    Embed(&WORKLOAD_SPEC),
    Extensions,
]);

static TRIGGERS: Def = def(&[
    f("manual", Ptr(&Bool)),
    f("schedule", Slice(&Struct(&SCHEDULE))),
    Extensions,
]);

static SCHEDULE: Def = def(&[
    f("cron", Str),
    f("timezone", Str),
    f("concurrency", Str),
    f("missed_fires", Str),
    Extensions,
]);

static PROVIDER: Def = def(&[f("type", Str), f("options", MultiOptions), Extensions]);

static PRE_START_HOOK: Def = def(&[Embed(&CONTAINER_SPEC), f("per_replica", Bool), Extensions]);

static SERVICE_HOOK: Def = def(&[
    json_always("command", ShellCommand),
    f("user", Str),
    f("privileged", Bool),
    f("working_dir", Str),
    f("environment", MappingWithEquals),
    Extensions,
]);

static DEVELOP: Def = def(&[f("watch", Slice(&Struct(&WATCH_TRIGGER))), Extensions]);

static WATCH_TRIGGER: Def = def(&[
    always("path", Str),
    always("action", Str),
    f("target", Str),
    f("exec", Struct(&SERVICE_HOOK)),
    f("include", STRINGS),
    f("ignore", STRINGS),
    f("initial_sync", Bool),
    Extensions,
]);

static BUILD: Def = def(&[
    f("context", Str),
    f("dockerfile", Str),
    f("dockerfile_inline", Str),
    f("entitlements", STRINGS),
    f("args", MappingWithEquals),
    f("provenance", Str),
    f("sbom", Str),
    f("ssh", SshConfig),
    f("labels", Labels),
    f("cache_from", StringList),
    f("cache_to", StringList),
    f("no_cache", Bool),
    f("no_cache_filter", StringList),
    f("additional_contexts", Mapping),
    f("pull", Bool),
    f("extra_hosts", HostsList),
    f("isolation", Str),
    f("network", Str),
    f("target", Str),
    f("secrets", Slice(&Struct(&FILE_REFERENCE))),
    f("shm_size", UnitBytes),
    f("tags", StringList),
    f("ulimits", Ty::Map(&Ptr(&Ulimit))),
    f("platforms", StringList),
    f("privileged", Bool),
    Extensions,
]);

static HEALTHCHECK: Def = def(&[
    f("test", HealthCheckTest),
    f("timeout", Ptr(&Duration)),
    f("interval", Ptr(&Duration)),
    f("retries", Ptr(&Uint64)),
    f("start_period", Ptr(&Duration)),
    f("start_interval", Ptr(&Duration)),
    f("disable", Bool),
    Extensions,
]);

static DEVICE_REQUEST: Def = def(&[
    f("capabilities", STRINGS),
    f("driver", Str),
    f("count", DeviceCount),
    f("device_ids", STRINGS),
    f("options", Mapping),
]);

static BLKIO_CONFIG: Def = def(&[
    f("weight", Uint16),
    f("weight_device", Slice(&Struct(&WEIGHT_DEVICE))),
    f("device_read_bps", Slice(&Struct(&THROTTLE_DEVICE))),
    f("device_read_iops", Slice(&Struct(&THROTTLE_DEVICE))),
    f("device_write_bps", Slice(&Struct(&THROTTLE_DEVICE))),
    f("device_write_iops", Slice(&Struct(&THROTTLE_DEVICE))),
    Extensions,
]);

static WEIGHT_DEVICE: Def = def(&[
    untagged("path", "Path", Str),
    untagged("weight", "Weight", Uint16),
    Extensions,
]);

static THROTTLE_DEVICE: Def = def(&[
    untagged("path", "Path", Str),
    untagged("rate", "Rate", UnitBytes),
    Extensions,
]);

static DEVICE_MAPPING: Def = def(&[
    f("source", Str),
    f("target", Str),
    f("permissions", Str),
    Extensions,
]);

static LOGGING: Def = def(&[f("driver", Str), f("options", Options), Extensions]);

static DEPLOY: Def = def(&[
    f("mode", Str),
    f("replicas", Ptr(&Int)),
    f("labels", Labels),
    f("update_config", Ptr(&Struct(&UPDATE_CONFIG))),
    f("rollback_config", Ptr(&Struct(&UPDATE_CONFIG))),
    f("resources", Struct(&RESOURCES)),
    f("restart_policy", Ptr(&Struct(&RESTART_POLICY))),
    f("placement", Struct(&PLACEMENT)),
    f("endpoint_mode", Str),
    Extensions,
]);

static UPDATE_CONFIG: Def = def(&[
    f("parallelism", Ptr(&Uint64)),
    f("delay", Duration),
    f("failure_action", Str),
    f("monitor", Duration),
    f("max_failure_ratio", Float32),
    f("order", Str),
    Extensions,
]);

static RESOURCES: Def = def(&[
    f("limits", Ptr(&Struct(&RESOURCE))),
    f("reservations", Ptr(&Struct(&RESOURCE))),
    Extensions,
]);

static RESOURCE: Def = def(&[
    f("cpus", NanoCpus),
    f("memory", UnitBytes),
    f("pids", Int64),
    f("devices", Slice(&Struct(&DEVICE_REQUEST))),
    f("generic_resources", Slice(&Struct(&GENERIC_RESOURCE))),
    Extensions,
]);

static GENERIC_RESOURCE: Def = def(&[
    f("discrete_resource_spec", Ptr(&Struct(&DISCRETE_RESOURCE))),
    Extensions,
]);

static DISCRETE_RESOURCE: Def = def(&[always("kind", Str), always("value", Int64), Extensions]);

static RESTART_POLICY: Def = def(&[
    f("condition", Str),
    f("delay", Ptr(&Duration)),
    f("max_attempts", Ptr(&Uint64)),
    f("window", Ptr(&Duration)),
    Extensions,
]);

static PLACEMENT: Def = def(&[
    f("constraints", STRINGS),
    f("preferences", Slice(&Struct(&PLACEMENT_PREFERENCES))),
    f("max_replicas_per_node", Uint64),
    Extensions,
]);

static PLACEMENT_PREFERENCES: Def = def(&[f("spread", Str), Extensions]);

static SERVICE_NETWORK: Def = def(&[
    f("aliases", STRINGS),
    f("driver_opts", Options),
    f("gw_priority", Int),
    f("interface_name", Str),
    f("ipv4_address", Str),
    f("ipv6_address", Str),
    f("link_local_ips", STRINGS),
    f("mac_address", Str),
    f("priority", Int),
    Extensions,
]);

pub static SERVICE_PORT: Def = def(&[
    f("name", Str),
    f("mode", Str),
    f("host_ip", Str),
    f("target", Uint32),
    f("published", Str),
    f("protocol", Str),
    f("app_protocol", Str),
    Extensions,
]);

pub static SERVICE_VOLUME: Def = def(&[
    f("type", Str),
    f("source", Str),
    f("target", Str),
    f("read_only", Bool),
    f("consistency", Str),
    f("bind", Ptr(&Struct(&VOLUME_BIND))),
    f("volume", Ptr(&Struct(&VOLUME_VOLUME))),
    f("tmpfs", Ptr(&Struct(&VOLUME_TMPFS))),
    f("image", Ptr(&Struct(&VOLUME_IMAGE))),
    Extensions,
]);

static VOLUME_BIND: Def = def(&[
    f("selinux", Str),
    f("propagation", Str),
    opt_out("create_host_path"),
    f("recursive", Str),
    Extensions,
]);

static VOLUME_VOLUME: Def = def(&[
    f("labels", Mapping),
    f("nocopy", Bool),
    f("subpath", Str),
    Extensions,
]);

static VOLUME_TMPFS: Def = def(&[f("size", UnitBytes), f("mode", Uint32), Extensions]);

static VOLUME_IMAGE: Def = def(&[f("subpath", Str), Extensions]);

static FILE_REFERENCE: Def = def(&[
    f("source", Str),
    f("target", Str),
    f("uid", Str),
    f("gid", Str),
    f("mode", Ptr(&FileMode)),
    Extensions,
]);

static CREDENTIAL_SPEC: Def = def(&[
    f("config", Str),
    f("file", Str),
    f("registry", Str),
    Extensions,
]);

static SERVICE_DEPENDENCY: Def = def(&[
    f("condition", Str),
    f("restart", Bool),
    Extensions,
    always("required", Bool),
]);

static ENV_FILE: Def = def(&[f("path", Str), opt_out("required"), f("format", Str)]);

static EXTENDS: Def = def(&[f("file", Str), f("service", Str)]);

static SERVICE_MODEL: Def = def(&[f("endpoint_var", Str), f("model_var", Str), Extensions]);

pub static NETWORK: Def = def(&[
    f("name", Str),
    f("driver", Str),
    f("driver_opts", Options),
    f("ipam", Struct(&IPAM)),
    f("external", Bool),
    f("internal", Bool),
    f("attachable", Bool),
    f("labels", Labels),
    f("enable_ipv4", Ptr(&Bool)),
    f("enable_ipv6", Ptr(&Bool)),
    Extensions,
]);

static IPAM: Def = def(&[
    f("driver", Str),
    f("config", Slice(&Ptr(&Struct(&IPAM_POOL)))),
    f("options", Options),
    Extensions,
]);

/// IPAMPool's extensions are `yaml:",inline"` without a name mapstructure fills:
/// they never hold anything.
static IPAM_POOL: Def = def(&[
    f("subnet", Str),
    f("gateway", Str),
    f("ip_range", Str),
    f("aux_addresses", Mapping),
]);

pub static VOLUME: Def = def(&[
    f("name", Str),
    f("driver", Str),
    f("driver_opts", Options),
    f("external", Bool),
    f("labels", Labels),
    Extensions,
]);

const FILE_OBJECT: &[Member] = &[
    f("name", Str),
    f("file", Str),
    f("environment", Str),
    f("content", Str),
    f("external", Bool),
    f("labels", Labels),
    f("driver", Str),
    f("driver_opts", Ty::Map(&Str)),
    f("template_driver", Str),
    Extensions,
];

pub static SECRET: Def = Def {
    members: FILE_OBJECT,
    custom: Custom::Secret,
};

pub static CONFIG: Def = Def {
    members: FILE_OBJECT,
    custom: Custom::Config,
};

pub static MODEL: Def = def(&[
    f("name", Str),
    f("model", Str),
    f("context_size", Int),
    f("runtime_flags", STRINGS),
    Extensions,
]);

/// The project as written by YAML; JSON writes a map of its own.
pub static PROJECT: Def = def(&[
    f("name", Str),
    always("services", Ty::Map(&Struct(&SERVICE))),
    f("jobs", Ty::Map(&Struct(&JOB))),
    f("networks", Ty::Map(&Struct(&NETWORK))),
    f("volumes", Ty::Map(&Struct(&VOLUME))),
    f("secrets", Ty::Map(&Struct(&SECRET))),
    f("configs", Ty::Map(&Struct(&CONFIG))),
    f("models", Ty::Map(&Struct(&MODEL))),
    Extensions,
]);
