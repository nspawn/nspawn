//! The decisions on the attributes of the Compose Specification, in the order of the
//! schema's sections.

use super::Match::{Is, Prefix};
use super::Support::{Full, Ignored, Partial, Refused};
use super::{full, ignored, partly, refused, when, Attribute, Value};

/// What a machine of an image that boots cannot take: its init runs as it is.
const BOOTED: &str = "refused for an image that boots, whose init runs as it is";
const GPU_CAPABILITIES: &str =
    "they pick the vendor whose CDI spec is used, as docker picks its device driver";
const GPU_DRIVER: &str = "nvidia, amd or cdi, through the CDI specs of the host";
const GPU_OPTIONS: &str = "the CDI spec says what the devices need";

/// The words of the warnings on what is left to Swarm, which nspawn compose leaves as
/// well.
const SWARM_DEPLOY_MODE: &str = "deploy.mode is only honored in Swarm mode";
const SWARM_DEPLOY_LABELS: &str = "deploy.labels are only applied to Swarm services";
const SWARM_UPDATE: &str = "deploy.update_config only applies to rolling updates in Swarm mode";
const SWARM_ROLLBACK: &str = "deploy.rollback_config only applies to rolling updates in Swarm mode";
const SWARM_ENDPOINT: &str = "deploy.endpoint_mode only applies to Swarm's routing mesh";
const SWARM_PLACEMENT: &str = "deploy.placement is only honored by the Swarm scheduler";
const SWARM_CREDENTIAL: &str =
    "credential_spec is only supported when running Windows containers under Swarm";
const SWARM_CONFIG_LABELS: &str = "configs[].labels are only applied to Swarm config objects";
const SWARM_SECRET_OPTS: &str = "secrets[].driver_opts is only honored by Swarm secret drivers";
const SWARM_SECRET_LABELS: &str = "secrets[].labels are only applied to Swarm secret objects";
const SWARM_UID: &str = "uid is not supported outside Swarm mode and will be ignored";
const SWARM_GID: &str = "gid is not supported outside Swarm mode and will be ignored";
const SWARM_MODE: &str = "mode is not supported outside Swarm mode and will be ignored";
const SWARM_PORT_MODE: &str = "ports[].mode: host is only honored by the Swarm routing mesh";

pub static ATTRIBUTES: &[Attribute] = &[
    full("version"),
    full("name"),
    full("include"),
    partly("include.[].path", crate::compose::paths::REMOTE),
    full("include.[].env_file"),
    full("include.[].project_directory"),
    // Services.
    full("services"),
    full("services.*"),
    ignored(
        "services.*.annotations",
        "nspawn machines have no OCI runtime annotations",
    )
    .and_below(),
    full("services.*.attach"),
    ignored(
        "services.*.blkio_config",
        "block I/O limits are not set on nspawn machines",
    )
    .and_below(),
    partly(
        "services.*.build",
        "built by nspawn's builder, which runs its steps one after another for this host's platform",
    ),
    refused(
        "services.*.build.additional_contexts",
        "named contexts are not supported by nspawn's builder",
    )
    .and_below(),
    full("services.*.build.args"),
    full("services.*.build.args.*"),
    ignored(
        "services.*.build.cache_from",
        "the build cache is nspawn's own, on this host",
    ),
    ignored(
        "services.*.build.cache_to",
        "the build cache is nspawn's own, on this host",
    ),
    partly(
        "services.*.build.context",
        "a directory of this host; a remote context is refused",
    ),
    full("services.*.build.dockerfile"),
    full("services.*.build.dockerfile_inline"),
    partly(
        "services.*.build.entitlements",
        "network.host; security.insecure is refused",
    ),
    full("services.*.build.extra_hosts"),
    full("services.*.build.extra_hosts.*"),
    ignored(
        "services.*.build.isolation",
        "isolation is a setting of Windows containers",
    ),
    full("services.*.build.labels"),
    full("services.*.build.labels.*"),
    partly("services.*.build.network", "default, none or host"),
    full("services.*.build.no_cache"),
    full("services.*.build.no_cache_filter"),
    partly(
        "services.*.build.platforms",
        "this host's platform only; another is refused",
    ),
    refused(
        "services.*.build.privileged",
        "build steps run with the confinement of an app machine",
    ),
    ignored(
        "services.*.build.provenance",
        "nspawn's builder writes no attestations",
    ),
    full("services.*.build.pull"),
    ignored(
        "services.*.build.sbom",
        "nspawn's builder writes no attestations",
    ),
    partly(
        "services.*.build.secrets",
        "secrets from a file or the environment, which RUN --mount=type=secret takes",
    ),
    ignored(
        "services.*.build.secrets.[].gid",
        "the RUN --mount of the Dockerfile sets the owner and mode of a build secret",
    ),
    ignored(
        "services.*.build.secrets.[].mode",
        "the RUN --mount of the Dockerfile sets the owner and mode of a build secret",
    ),
    full("services.*.build.secrets.[].source"),
    full("services.*.build.secrets.[].target"),
    ignored(
        "services.*.build.secrets.[].uid",
        "the RUN --mount of the Dockerfile sets the owner and mode of a build secret",
    ),
    full("services.*.build.shm_size"),
    refused(
        "services.*.build.ssh",
        "SSH agent forwarding into a build is not supported",
    )
    .and_below(),
    full("services.*.build.tags"),
    full("services.*.build.target"),
    full("services.*.build.ulimits"),
    full("services.*.build.ulimits.*"),
    full("services.*.build.ulimits.*.hard"),
    full("services.*.build.ulimits.*.soft"),
    full("services.*.cap_add"),
    full("services.*.cap_drop"),
    full("services.*.cgroup"),
    ignored(
        "services.*.cgroup_parent",
        "machines live in the slice of systemd-nspawn's units",
    ),
    partly("services.*.command", BOOTED),
    full("services.*.configs"),
    full("services.*.configs.[].source"),
    full("services.*.configs.[].target"),
    ignored("services.*.configs.[].uid", SWARM_UID),
    ignored("services.*.configs.[].gid", SWARM_GID),
    ignored("services.*.configs.[].mode", SWARM_MODE),
    partly(
        "services.*.container_name",
        "the machine's name with each `_` written `-`, as machined takes none; the name as written stays a name on the service's networks",
    ),
    ignored(
        "services.*.cpu_count",
        "only cpus limits the CPU of a machine",
    ),
    ignored(
        "services.*.cpu_percent",
        "only cpus limits the CPU of a machine",
    ),
    ignored(
        "services.*.cpu_period",
        "only cpus limits the CPU of a machine",
    ),
    ignored(
        "services.*.cpu_quota",
        "only cpus limits the CPU of a machine",
    ),
    ignored(
        "services.*.cpu_rt_period",
        "machines get no real-time CPU time",
    ),
    ignored(
        "services.*.cpu_rt_runtime",
        "machines get no real-time CPU time",
    ),
    ignored(
        "services.*.cpu_shares",
        "only cpus limits the CPU of a machine",
    ),
    full("services.*.cpus"),
    ignored("services.*.cpuset", "machines run on every CPU"),
    ignored("services.*.credential_spec", SWARM_CREDENTIAL).and_below(),
    full("services.*.depends_on"),
    full("services.*.depends_on.*"),
    full("services.*.depends_on.*.condition"),
    full("services.*.depends_on.*.required"),
    full("services.*.depends_on.*.restart"),
    partly(
        "services.*.deploy",
        "replicas, resources.limits and restart_policy.condition are honored",
    ),
    ignored("services.*.deploy.endpoint_mode", SWARM_ENDPOINT),
    ignored("services.*.deploy.labels", SWARM_DEPLOY_LABELS).and_below(),
    ignored("services.*.deploy.mode", SWARM_DEPLOY_MODE),
    ignored("services.*.deploy.placement", SWARM_PLACEMENT).and_below(),
    full("services.*.deploy.replicas"),
    partly(
        "services.*.deploy.resources",
        "the limits and the devices reserved are honored",
    ),
    full("services.*.deploy.resources.limits"),
    full("services.*.deploy.resources.limits.cpus"),
    full("services.*.deploy.resources.limits.memory"),
    full("services.*.deploy.resources.limits.pids"),
    partly(
        "services.*.deploy.resources.reservations",
        "the devices are honored; nspawn reserves no CPU or memory",
    ),
    ignored(
        "services.*.deploy.resources.reservations.cpus",
        "nspawn reserves nothing for a machine",
    ),
    full("services.*.deploy.resources.reservations.devices"),
    partly(
        "services.*.deploy.resources.reservations.devices.[].capabilities",
        GPU_CAPABILITIES,
    ),
    full("services.*.deploy.resources.reservations.devices.[].count"),
    full("services.*.deploy.resources.reservations.devices.[].device_ids"),
    partly(
        "services.*.deploy.resources.reservations.devices.[].driver",
        GPU_DRIVER,
    ),
    ignored(
        "services.*.deploy.resources.reservations.devices.[].options",
        GPU_OPTIONS,
    )
    .and_below(),
    ignored(
        "services.*.deploy.resources.reservations.generic_resources",
        "nspawn reserves nothing for a machine",
    )
    .and_below(),
    ignored(
        "services.*.deploy.resources.reservations.memory",
        "nspawn reserves nothing for a machine",
    ),
    partly(
        "services.*.deploy.restart_policy",
        "the condition is honored",
    ),
    full("services.*.deploy.restart_policy.condition"),
    ignored(
        "services.*.deploy.restart_policy.delay",
        "systemd restarts a machine at once",
    ),
    ignored(
        "services.*.deploy.restart_policy.max_attempts",
        "systemd restarts a machine with no count",
    ),
    ignored(
        "services.*.deploy.restart_policy.window",
        "systemd restarts a machine at once",
    ),
    ignored("services.*.deploy.rollback_config", SWARM_ROLLBACK).and_below(),
    ignored("services.*.deploy.update_config", SWARM_UPDATE).and_below(),
    ignored(
        "services.*.develop",
        "it is for watch, which nspawn compose does not have",
    )
    .and_below(),
    ignored(
        "services.*.device_cgroup_rules",
        "a device reaches a machine through devices",
    ),
    full("services.*.devices"),
    full("services.*.devices.[].permissions"),
    partly(
        "services.*.devices.[].source",
        "a device node of the host, or a CDI device of the host's specs",
    ),
    full("services.*.devices.[].target"),
    full("services.*.dns"),
    ignored(
        "services.*.dns_opt",
        "the resolv.conf of a machine takes no options",
    ),
    full("services.*.dns_search"),
    ignored(
        "services.*.domainname",
        "a machine has a hostname and no domain name",
    ),
    partly("services.*.entrypoint", BOOTED),
    full("services.*.env_file"),
    full("services.*.env_file.[].format"),
    full("services.*.env_file.[].path"),
    full("services.*.env_file.[].required"),
    partly(
        "services.*.environment",
        "refused for an image that boots, whose init runs as it is; a value with a newline or another control character but a tab is refused, as the settings file that carries it is read a line at a time",
    )
    .and_below(),
    full("services.*.expose"),
    full("services.*.extends"),
    partly("services.*.extends.file", crate::compose::paths::REMOTE),
    full("services.*.extends.service"),
    ignored(
        "services.*.external_links",
        "a machine reaches others by their names on a shared network",
    ),
    full("services.*.extra_hosts").and_below(),
    partly(
        "services.*.gpus",
        "the devices of the GPU vendor's CDI spec, nvidia-ctk cdi generate writing NVIDIA's",
    ),
    partly("services.*.gpus.[].capabilities", GPU_CAPABILITIES),
    full("services.*.gpus.[].count"),
    full("services.*.gpus.[].device_ids"),
    partly("services.*.gpus.[].driver", GPU_DRIVER),
    ignored("services.*.gpus.[].options", GPU_OPTIONS).and_below(),
    ignored(
        "services.*.group_add",
        "a machine's program runs with the groups of its user",
    ),
    full("services.*.healthcheck"),
    full("services.*.healthcheck.disable"),
    full("services.*.healthcheck.interval"),
    full("services.*.healthcheck.retries"),
    full("services.*.healthcheck.start_interval"),
    full("services.*.healthcheck.start_period"),
    full("services.*.healthcheck.test"),
    full("services.*.healthcheck.timeout"),
    full("services.*.hostname"),
    full("services.*.image"),
    partly("services.*.init", BOOTED),
    ignored(
        "services.*.ipc",
        "a machine always has an IPC namespace of its own",
    ),
    ignored(
        "services.*.isolation",
        "isolation is for Windows containers",
    ),
    full("services.*.label_file"),
    full("services.*.labels").and_below(),
    partly(
        "services.*.links",
        "an alias of the linked service on the default network, which it is made anew to take, and a dependency, nothing more",
    ),
    partly(
        "services.*.logging",
        "the driver becomes nspawn's log driver",
    ),
    ignored(
        "services.*.logging.driver",
        "the driver is docker's; the machine logs to nspawn's local log",
    ),
    ignored(
        "services.*.logging.options",
        "nspawn's log drivers take no options",
    )
    .and_below(),
    ignored(
        "services.*.mac_address",
        "systemd-nspawn chooses the MAC address of a machine",
    ),
    full("services.*.mem_limit"),
    ignored(
        "services.*.mem_reservation",
        "nspawn reserves nothing for a machine",
    ),
    ignored(
        "services.*.mem_swappiness",
        "machines swap as the host does",
    ),
    full("services.*.memswap_limit"),
    refused(
        "services.*.models",
        "models need Docker Model Runner, which nspawn does not have",
    )
    .and_below(),
    full("services.*.network_mode"),
    full("services.*.networks"),
    full("services.*.networks.*"),
    full("services.*.networks.*.aliases"),
    ignored(
        "services.*.networks.*.driver_opts",
        "driver options are docker's; nspawn attaches a machine its own way",
    )
    .and_below(),
    partly(
        "services.*.networks.*.gw_priority",
        "the machine routes through the first network that is not internal, in the order priority gives",
    ),
    ignored(
        "services.*.networks.*.interface_name",
        "nspawn names the interfaces of a machine host0, host1 and on",
    ),
    full("services.*.networks.*.ipv4_address"),
    ignored(
        "services.*.networks.*.ipv6_address",
        "nspawn networks are IPv4 only",
    ),
    ignored(
        "services.*.networks.*.link_local_ips",
        "nspawn gives a machine no link-local address",
    ),
    ignored(
        "services.*.networks.*.mac_address",
        "systemd-nspawn chooses the MAC address of a machine",
    ),
    full("services.*.networks.*.priority"),
    ignored(
        "services.*.oom_kill_disable",
        "the kernel may kill a machine's processes when memory runs out",
    ),
    full("services.*.oom_score_adj"),
    ignored(
        "services.*.pid",
        "a machine always has a PID namespace of its own",
    ),
    full("services.*.pids_limit"),
    partly(
        "services.*.platform",
        "the host's architecture is pulled; another one is warned about",
    ),
    full("services.*.ports"),
    full("services.*.ports.[].app_protocol"),
    full("services.*.ports.[].host_ip"),
    full("services.*.ports.[].mode"),
    full("services.*.ports.[].name"),
    full("services.*.ports.[].protocol"),
    partly(
        "services.*.ports.[].published",
        "a range of host ports for one port of the machine is refused",
    ),
    full("services.*.ports.[].target"),
    full("services.*.post_start"),
    full("services.*.post_start.[].command"),
    full("services.*.post_start.[].environment").and_below(),
    partly(
        "services.*.post_start.[].privileged",
        "the hook has the capabilities of the machine",
    ),
    full("services.*.post_start.[].user"),
    full("services.*.post_start.[].working_dir"),
    refused(
        "services.*.pre_start",
        "pre_start containers are not supported; run the step in the service itself",
    )
    .and_below(),
    full("services.*.pre_stop"),
    full("services.*.pre_stop.[].command"),
    full("services.*.pre_stop.[].environment").and_below(),
    partly(
        "services.*.pre_stop.[].privileged",
        "the hook has the capabilities of the machine",
    ),
    full("services.*.pre_stop.[].user"),
    full("services.*.pre_stop.[].working_dir"),
    full("services.*.privileged"),
    full("services.*.profiles"),
    refused(
        "services.*.provider",
        "provider services are docker compose plugins, which nspawn does not run",
    )
    .and_below(),
    full("services.*.pull_policy"),
    ignored(
        "services.*.pull_refresh_after",
        "a refreshing pull policy pulls every time",
    ),
    full("services.*.read_only"),
    full("services.*.restart"),
    ignored("services.*.runtime", "machines run under systemd-nspawn"),
    full("services.*.scale"),
    full("services.*.secrets"),
    full("services.*.secrets.[].source"),
    full("services.*.secrets.[].target"),
    ignored("services.*.secrets.[].uid", SWARM_UID),
    ignored("services.*.secrets.[].gid", SWARM_GID),
    ignored("services.*.secrets.[].mode", SWARM_MODE),
    ignored(
        "services.*.security_opt",
        "nspawn sets the security of a machine itself",
    ),
    full("services.*.shm_size"),
    full("services.*.stdin_open"),
    partly("services.*.stop_grace_period", "in whole seconds"),
    full("services.*.stop_signal"),
    ignored(
        "services.*.storage_opt",
        "nspawn stores a machine as layers on the host's file system",
    ),
    partly(
        "services.*.sysctls",
        "net.* keys for a machine that does not boot; other keys are refused",
    )
    .and_below(),
    full("services.*.tmpfs"),
    full("services.*.tty"),
    full("services.*.ulimits").and_below(),
    partly(
        "services.*.use_api_socket",
        "nspawn's Docker API, which reads but does not change machines (a kill aside), and no registry credentials with it",
    ),
    partly("services.*.user", BOOTED),
    ignored(
        "services.*.userns_mode",
        "a machine always has a user namespace of its own",
    ),
    ignored(
        "services.*.uts",
        "a machine always has a UTS namespace of its own",
    ),
    full("services.*.volumes"),
    full("services.*.volumes.[].bind"),
    full("services.*.volumes.[].bind.create_host_path"),
    ignored(
        "services.*.volumes.[].bind.propagation",
        "a bind mount into a machine propagates nothing",
    ),
    ignored(
        "services.*.volumes.[].bind.recursive",
        "a bind mount into a machine is recursive",
    ),
    ignored(
        "services.*.volumes.[].bind.selinux",
        "nspawn does not relabel what it mounts into a machine",
    ),
    ignored(
        "services.*.volumes.[].consistency",
        "consistency is for Docker Desktop's file sharing",
    ),
    refused(
        "services.*.volumes.[].image",
        "an image is not mounted as a volume",
    )
    .and_below(),
    full("services.*.volumes.[].read_only"),
    full("services.*.volumes.[].source"),
    full("services.*.volumes.[].target"),
    full("services.*.volumes.[].tmpfs").and_below(),
    full("services.*.volumes.[].type"),
    full("services.*.volumes.[].volume"),
    ignored(
        "services.*.volumes.[].volume.labels",
        "nspawn volumes have no labels",
    )
    .and_below(),
    ignored(
        "services.*.volumes.[].volume.nocopy",
        "a new volume is filled from the image all the same",
    ),
    refused(
        "services.*.volumes.[].volume.subpath",
        "a volume is mounted whole",
    ),
    refused(
        "services.*.volumes_from",
        "a machine mounts the volumes of another one by naming them",
    ),
    partly("services.*.working_dir", BOOTED),
    // Jobs: the rest of their attributes are those of a service.
    full("jobs"),
    full("jobs.*"),
    full("jobs.*.triggers"),
    full("jobs.*.triggers.manual"),
    refused(
        "jobs.*.triggers.schedule",
        "scheduled jobs are not supported in this version",
    )
    .and_below(),
    refused(
        "models",
        "models need Docker Model Runner, which nspawn does not have",
    )
    .and_below(),
    // Networks.
    full("networks"),
    full("networks.*"),
    full("networks.*.attachable"),
    refused(
        "networks.*.driver",
        "nspawn networks are bridges: no other driver",
    ),
    ignored(
        "networks.*.driver_opts",
        "driver options are docker's bridge settings; nspawn makes its own bridge",
    )
    .and_below(),
    full("networks.*.enable_ipv4"),
    full("networks.*.enable_ipv6"),
    full("networks.*.external"),
    full("networks.*.external.name"),
    full("networks.*.internal"),
    partly(
        "networks.*.ipam",
        "the subnet of the first IPv4 entry of config",
    ),
    partly(
        "networks.*.ipam.config",
        "the first IPv4 entry; an IPv6 one is ignored, a second IPv4 one refused",
    ),
    ignored(
        "networks.*.ipam.config.[].aux_addresses",
        "nspawn keeps no addresses aside",
    )
    .and_below(),
    ignored(
        "networks.*.ipam.config.[].gateway",
        "the gateway is the first address of the subnet",
    ),
    ignored(
        "networks.*.ipam.config.[].ip_range",
        "machines take addresses from the whole subnet",
    ),
    full("networks.*.ipam.config.[].subnet"),
    refused(
        "networks.*.ipam.driver",
        "nspawn allocates the addresses itself: only the default driver",
    ),
    ignored(
        "networks.*.ipam.options",
        "IPAM options are those of docker's drivers",
    )
    .and_below(),
    full("networks.*.labels").and_below(),
    full("networks.*.name"),
    // Volumes.
    full("volumes"),
    full("volumes.*"),
    refused(
        "volumes.*.driver",
        "nspawn volumes are directories of the host: only the local driver",
    ),
    partly(
        "volumes.*.driver_opts",
        "type: none with o: bind (ro or rw beside it) and an absolute device binds that host directory, which down -v leaves; other options are refused",
    )
    .and_below(),
    full("volumes.*.external"),
    full("volumes.*.external.name"),
    ignored("volumes.*.labels", "nspawn volumes have no labels").and_below(),
    full("volumes.*.name"),
    // Secrets.
    full("secrets"),
    full("secrets.*"),
    refused(
        "secrets.*.driver",
        "Docker Compose does not support secrets.*.driver",
    ),
    ignored("secrets.*.driver_opts", SWARM_SECRET_OPTS).and_below(),
    full("secrets.*.environment"),
    refused(
        "secrets.*.external",
        "an external secret is a Swarm secret, which nspawn cannot read",
    )
    .and_below(),
    full("secrets.*.file"),
    ignored("secrets.*.labels", SWARM_SECRET_LABELS).and_below(),
    full("secrets.*.name"),
    refused(
        "secrets.*.template_driver",
        "Docker Compose does not support secrets.*.template_driver",
    ),
    // Configs.
    full("configs"),
    full("configs.*"),
    full("configs.*.content"),
    full("configs.*.environment"),
    refused(
        "configs.*.external",
        "an external config is a Swarm config, which nspawn cannot read",
    )
    .and_below(),
    full("configs.*.file"),
    ignored("configs.*.labels", SWARM_CONFIG_LABELS).and_below(),
    full("configs.*.name"),
    refused(
        "configs.*.template_driver",
        "Docker Compose does not support configs.*.template_driver",
    ),
];

pub static VALUES: &[Value] = &[
    when(
        "services.*.cgroup",
        Is("host"),
        Ignored("a machine always has a cgroup namespace of its own"),
    ),
    when(
        "services.*.logging.driver",
        Is("json-file"),
        Partial("nspawn's local log, which logs reads"),
    ),
    when(
        "services.*.logging.driver",
        Is("local"),
        Partial("nspawn's local log, which logs reads"),
    ),
    when(
        "services.*.logging.driver",
        Is("journald"),
        Partial("the system's journal"),
    ),
    when("services.*.logging.driver", Is("none"), Full),
    when(
        "services.*.ports.[].mode",
        Is("host"),
        Ignored(SWARM_PORT_MODE),
    ),
    when(
        "services.*.ports.[].protocol",
        Is("sctp"),
        Refused("nspawn publishes tcp and udp ports"),
    ),
    when(
        "services.*.pull_policy",
        Is("daily"),
        Partial("pulled every time; a pull of an unchanged image downloads nothing"),
    ),
    when(
        "services.*.pull_policy",
        Is("weekly"),
        Partial("pulled every time; a pull of an unchanged image downloads nothing"),
    ),
    when(
        "services.*.pull_policy",
        Prefix("every_"),
        Partial("pulled every time; a pull of an unchanged image downloads nothing"),
    ),
    when(
        "services.*.pull_policy",
        Is("refresh"),
        Partial("pulled every time; a pull of an unchanged image downloads nothing"),
    ),
    when(
        "services.*.restart",
        Prefix("on-failure:"),
        Partial("restarted on every failure: systemd keeps no count"),
    ),
    when(
        "services.*.volumes.[].type",
        Is("cluster"),
        Refused("volumes[].type: cluster (CSI) volumes are only supported in Swarm mode"),
    ),
    when(
        "services.*.volumes.[].type",
        Is("image"),
        Refused("an image is not mounted as a volume"),
    ),
    when(
        "services.*.volumes.[].type",
        Is("npipe"),
        Refused("named pipes are for Windows containers"),
    ),
    when("networks.*.driver", Is("bridge"), Full),
    when(
        "networks.*.enable_ipv4",
        Is("false"),
        Refused("nspawn networks always carry IPv4"),
    ),
    when(
        "networks.*.enable_ipv6",
        Is("true"),
        Ignored("nspawn networks are IPv4 only"),
    ),
    when("networks.*.ipam.driver", Is("default"), Full),
    when("volumes.*.driver", Is("local"), Full),
    // The neutral values of what is refused: the default driver is the empty one, and
    // false asks for nothing.
    when("networks.*.driver", Is(""), Full),
    when("networks.*.ipam.driver", Is(""), Full),
    when("volumes.*.driver", Is(""), Full),
    when("secrets.*.external", Is("false"), Full),
    when("configs.*.external", Is("false"), Full),
    when("services.*.build.privileged", Is("false"), Full),
];
