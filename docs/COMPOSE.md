# nspawn compose

`nspawn compose` reads the files of docker compose as they are and runs them on
nspawn machines, networks, volumes and secrets.

## What is supported

Every attribute of the Compose Specification and every command and option of
docker compose has a row below, with what nspawn compose does with it:

- **yes**: as docker compose does.
- **partly**: honored, with the difference the notes give.
- **ignored**: read and left without effect, with the notes as the warning.
- **no**: refused, with the notes as the error.

The table is the one in `src/compose/support`, which the loader reads its
warnings and errors from. Its tests check it against the schema and the command
reference of the docker compose release it names, kept under
`src/compose/upstream` (`fetch.sh` there brings those of another release), and
against the command line itself: a command it says nspawn compose has is one,
and its options take docker compose's short forms. A command nspawn compose does
not have yet is one row; what it will do with that command's options is already
decided in `src/compose/support`.

## Where it differs on purpose

Beyond the rows that say so:

- Where docker compose follows the order of a Go map, which changes from run to
  run (the lines of `config --services`, `--volumes`, `--networks`, `--images`
  and `--variables`, and which of several errors comes first), nspawn compose
  keeps one order.
- Where docker compose crashes or never ends (an include that comes back to the
  file including it through one of its override files, input that makes it
  panic), nspawn compose gives an error.
- The options of the docker client that `docker-compose` passes on to it
  (`--debug`, `--host`, `--context`, `--log-level`, `--tls`...) are taken and
  left without effect: nspawn compose talks to no docker.
- `--help` prints nspawn compose's own help, and cobra's hidden `__complete`
  command is not there: the shell completions of nspawn come from
  `nspawn completions`.

<!-- The support table: src/compose/support makes it, and NSPAWN_UPDATE_DOCS=1 cargo test writes it here. -->
Decided on docker compose v5.6.0, whose Compose Specification is that of compose-go v2.16.1. An attribute of a job without a row of its own is the service attribute of the same name.

### Attributes

| Attribute | nspawn | Notes |
| --- | --- | --- |
| `configs` | yes |  |
| `configs.*` | yes |  |
| `configs.*.content` | yes |  |
| `configs.*.environment` | yes |  |
| `configs.*.external` and below | no | an external config is a Swarm config, which nspawn cannot read |
| `configs.*.file` | yes |  |
| `configs.*.labels` and below | ignored | configs[].labels are only applied to Swarm config objects |
| `configs.*.name` | yes |  |
| `configs.*.template_driver` | no | Docker Compose does not support configs.*.template_driver |
| `include` | yes |  |
| `include.[].env_file` | yes |  |
| `include.[].path` | partly | files on this host: a git or oci:// address is refused |
| `include.[].project_directory` | yes |  |
| `jobs` | yes |  |
| `jobs.*` | yes |  |
| `jobs.*.triggers` | yes |  |
| `jobs.*.triggers.manual` | yes |  |
| `jobs.*.triggers.schedule` and below | no | scheduled jobs are not supported in this version |
| `models` and below | no | models need Docker Model Runner, which nspawn does not have |
| `name` | yes |  |
| `networks` | yes |  |
| `networks.*` | yes |  |
| `networks.*.attachable` | yes |  |
| `networks.*.driver` | no | nspawn networks are bridges: no other driver |
| `networks.*.driver`: `bridge` | yes |  |
| `networks.*.driver_opts` and below | ignored | driver options are docker's bridge settings; nspawn makes its own bridge |
| `networks.*.enable_ipv4` | yes |  |
| `networks.*.enable_ipv4`: `false` | no | nspawn networks always carry IPv4 |
| `networks.*.enable_ipv6` | yes |  |
| `networks.*.enable_ipv6`: `true` | ignored | nspawn networks are IPv4 only |
| `networks.*.external` | yes |  |
| `networks.*.external.name` | yes |  |
| `networks.*.internal` | yes |  |
| `networks.*.ipam` | partly | the subnet of the first IPv4 entry of config |
| `networks.*.ipam.config` | partly | the first IPv4 entry; an IPv6 one is ignored, a second IPv4 one refused |
| `networks.*.ipam.config.[].aux_addresses` and below | ignored | nspawn keeps no addresses aside |
| `networks.*.ipam.config.[].gateway` | ignored | the gateway is the first address of the subnet |
| `networks.*.ipam.config.[].ip_range` | ignored | machines take addresses from the whole subnet |
| `networks.*.ipam.config.[].subnet` | yes |  |
| `networks.*.ipam.driver` | no | nspawn allocates the addresses itself: only the default driver |
| `networks.*.ipam.driver`: `default` | yes |  |
| `networks.*.ipam.options` and below | ignored | IPAM options are those of docker's drivers |
| `networks.*.labels` and below | yes |  |
| `networks.*.name` | yes |  |
| `secrets` | yes |  |
| `secrets.*` | yes |  |
| `secrets.*.driver` | no | Docker Compose does not support secrets.*.driver |
| `secrets.*.driver_opts` and below | ignored | secrets[].driver_opts is only honored by Swarm secret drivers |
| `secrets.*.environment` | yes |  |
| `secrets.*.external` and below | no | an external secret is a Swarm secret, which nspawn cannot read |
| `secrets.*.file` | yes |  |
| `secrets.*.labels` and below | ignored | secrets[].labels are only applied to Swarm secret objects |
| `secrets.*.name` | yes |  |
| `secrets.*.template_driver` | no | Docker Compose does not support secrets.*.template_driver |
| `services` | yes |  |
| `services.*` | yes |  |
| `services.*.annotations` and below | ignored | nspawn machines have no OCI runtime annotations |
| `services.*.attach` | yes |  |
| `services.*.blkio_config` and below | ignored | block I/O limits are not set on nspawn machines |
| `services.*.build` and below | ignored | nspawn runs published images and does not build Dockerfiles: the image is pulled, and a service without one is refused |
| `services.*.cap_add` | yes |  |
| `services.*.cap_drop` | yes |  |
| `services.*.cgroup` | yes |  |
| `services.*.cgroup`: `host` | ignored | a machine always has a cgroup namespace of its own |
| `services.*.cgroup_parent` | ignored | machines live in the slice of systemd-nspawn's units |
| `services.*.command` | partly | refused for an image that boots, whose init runs as it is |
| `services.*.configs` | yes |  |
| `services.*.configs.[].gid` | ignored | gid is not supported outside Swarm mode and will be ignored |
| `services.*.configs.[].mode` | ignored | mode is not supported outside Swarm mode and will be ignored |
| `services.*.configs.[].source` | yes |  |
| `services.*.configs.[].target` | yes |  |
| `services.*.configs.[].uid` | ignored | uid is not supported outside Swarm mode and will be ignored |
| `services.*.container_name` | partly | a machine name: letters, digits and hyphens, up to 64 (machined takes no `_`) |
| `services.*.cpu_count` | ignored | only cpus limits the CPU of a machine |
| `services.*.cpu_percent` | ignored | only cpus limits the CPU of a machine |
| `services.*.cpu_period` | ignored | only cpus limits the CPU of a machine |
| `services.*.cpu_quota` | ignored | only cpus limits the CPU of a machine |
| `services.*.cpu_rt_period` | ignored | machines get no real-time CPU time |
| `services.*.cpu_rt_runtime` | ignored | machines get no real-time CPU time |
| `services.*.cpu_shares` | ignored | only cpus limits the CPU of a machine |
| `services.*.cpus` | yes |  |
| `services.*.cpuset` | ignored | machines run on every CPU |
| `services.*.credential_spec` and below | ignored | credential_spec is only supported when running Windows containers under Swarm |
| `services.*.depends_on` | yes |  |
| `services.*.depends_on.*` | yes |  |
| `services.*.depends_on.*.condition` | yes |  |
| `services.*.depends_on.*.required` | yes |  |
| `services.*.depends_on.*.restart` | yes |  |
| `services.*.deploy` | partly | replicas, resources.limits and restart_policy.condition are honored |
| `services.*.deploy.endpoint_mode` | ignored | deploy.endpoint_mode only applies to Swarm's routing mesh |
| `services.*.deploy.labels` and below | ignored | deploy.labels are only applied to Swarm services |
| `services.*.deploy.mode` | ignored | deploy.mode is only honored in Swarm mode |
| `services.*.deploy.placement` and below | ignored | deploy.placement is only honored by the Swarm scheduler |
| `services.*.deploy.replicas` | yes |  |
| `services.*.deploy.resources` | partly | the limits are honored |
| `services.*.deploy.resources.limits` | yes |  |
| `services.*.deploy.resources.limits.cpus` | yes |  |
| `services.*.deploy.resources.limits.memory` | yes |  |
| `services.*.deploy.resources.limits.pids` | yes |  |
| `services.*.deploy.resources.reservations` and below | ignored | nspawn reserves nothing for a machine |
| `services.*.deploy.restart_policy` | partly | the condition is honored |
| `services.*.deploy.restart_policy.condition` | yes |  |
| `services.*.deploy.restart_policy.delay` | ignored | systemd restarts a machine at once |
| `services.*.deploy.restart_policy.max_attempts` | ignored | systemd restarts a machine with no count |
| `services.*.deploy.restart_policy.window` | ignored | systemd restarts a machine at once |
| `services.*.deploy.rollback_config` and below | ignored | deploy.rollback_config only applies to rolling updates in Swarm mode |
| `services.*.deploy.update_config` and below | ignored | deploy.update_config only applies to rolling updates in Swarm mode |
| `services.*.develop` and below | ignored | it is for watch, which nspawn compose does not have |
| `services.*.device_cgroup_rules` | ignored | a device reaches a machine through devices |
| `services.*.devices` | yes |  |
| `services.*.devices.[].permissions` | yes |  |
| `services.*.devices.[].source` | partly | a device node of the host: a CDI name is refused |
| `services.*.devices.[].target` | yes |  |
| `services.*.dns` | yes |  |
| `services.*.dns_opt` | ignored | the resolv.conf of a machine takes no options |
| `services.*.dns_search` | yes |  |
| `services.*.domainname` | ignored | a machine has a hostname and no domain name |
| `services.*.entrypoint` | partly | refused for an image that boots, whose init runs as it is |
| `services.*.env_file` | yes |  |
| `services.*.env_file.[].format` | yes |  |
| `services.*.env_file.[].path` | yes |  |
| `services.*.env_file.[].required` | yes |  |
| `services.*.environment` and below | partly | refused for an image that boots, whose init runs as it is |
| `services.*.expose` | yes |  |
| `services.*.extends` | yes |  |
| `services.*.extends.file` | yes |  |
| `services.*.extends.service` | yes |  |
| `services.*.external_links` | ignored | a machine reaches others by their names on a shared network |
| `services.*.extra_hosts` and below | yes |  |
| `services.*.gpus` and below | ignored | nspawn hands no GPU to a machine; devices can |
| `services.*.group_add` | ignored | a machine's program runs with the groups of its user |
| `services.*.healthcheck` | yes |  |
| `services.*.healthcheck.disable` | yes |  |
| `services.*.healthcheck.interval` | yes |  |
| `services.*.healthcheck.retries` | yes |  |
| `services.*.healthcheck.start_interval` | yes |  |
| `services.*.healthcheck.start_period` | yes |  |
| `services.*.healthcheck.test` | yes |  |
| `services.*.healthcheck.timeout` | yes |  |
| `services.*.hostname` | yes |  |
| `services.*.image` | yes |  |
| `services.*.init` | partly | nspawn's stub init reaps orphans whether it is set or not |
| `services.*.ipc` | ignored | a machine always has an IPC namespace of its own |
| `services.*.isolation` | ignored | isolation is for Windows containers |
| `services.*.label_file` | yes |  |
| `services.*.labels` and below | yes |  |
| `services.*.links` | partly | an alias on the default network and a dependency, nothing more |
| `services.*.logging` | partly | the driver becomes nspawn's log driver |
| `services.*.logging.driver` | ignored | the driver is docker's; the machine logs to nspawn's local log |
| `services.*.logging.driver`: `json-file` | partly | nspawn's local log, which logs reads |
| `services.*.logging.driver`: `local` | partly | nspawn's local log, which logs reads |
| `services.*.logging.driver`: `journald` | partly | the system's journal |
| `services.*.logging.driver`: `none` | yes |  |
| `services.*.logging.options` and below | ignored | nspawn's log drivers take no options |
| `services.*.mac_address` | ignored | systemd-nspawn chooses the MAC address of a machine |
| `services.*.mem_limit` | yes |  |
| `services.*.mem_reservation` | ignored | nspawn reserves nothing for a machine |
| `services.*.mem_swappiness` | ignored | machines swap as the host does |
| `services.*.memswap_limit` | yes |  |
| `services.*.models` and below | no | models need Docker Model Runner, which nspawn does not have |
| `services.*.network_mode` | yes |  |
| `services.*.networks` | yes |  |
| `services.*.networks.*` | yes |  |
| `services.*.networks.*.aliases` | yes |  |
| `services.*.networks.*.driver_opts` and below | ignored | driver options are docker's; nspawn attaches a machine its own way |
| `services.*.networks.*.gw_priority` | yes |  |
| `services.*.networks.*.interface_name` | ignored | nspawn names the interfaces of a machine host0, host1 and on |
| `services.*.networks.*.ipv4_address` | no | a machine takes the next free address of a network; a fixed one is not supported |
| `services.*.networks.*.ipv6_address` | ignored | nspawn networks are IPv4 only |
| `services.*.networks.*.link_local_ips` | ignored | nspawn gives a machine no link-local address |
| `services.*.networks.*.mac_address` | ignored | systemd-nspawn chooses the MAC address of a machine |
| `services.*.networks.*.priority` | yes |  |
| `services.*.oom_kill_disable` | ignored | the kernel may kill a machine's processes when memory runs out |
| `services.*.oom_score_adj` | yes |  |
| `services.*.pid` | ignored | a machine always has a PID namespace of its own |
| `services.*.pids_limit` | yes |  |
| `services.*.platform` | partly | the host's architecture is pulled; another one is warned about |
| `services.*.ports` | yes |  |
| `services.*.ports.[].app_protocol` | yes |  |
| `services.*.ports.[].host_ip` | partly | an IPv4 address |
| `services.*.ports.[].mode` | yes |  |
| `services.*.ports.[].mode`: `host` | ignored | ports[].mode: host is only honored by the Swarm routing mesh |
| `services.*.ports.[].name` | yes |  |
| `services.*.ports.[].protocol` | yes |  |
| `services.*.ports.[].protocol`: `sctp` | no | nspawn publishes tcp and udp ports |
| `services.*.ports.[].published` | partly | a range of host ports for one port of the machine is refused |
| `services.*.ports.[].target` | yes |  |
| `services.*.post_start` | yes |  |
| `services.*.post_start.[].command` | yes |  |
| `services.*.post_start.[].environment` and below | yes |  |
| `services.*.post_start.[].privileged` | partly | the hook has the capabilities of the machine |
| `services.*.post_start.[].user` | yes |  |
| `services.*.post_start.[].working_dir` | yes |  |
| `services.*.pre_start` and below | no | pre_start containers are not supported; run the step in the service itself |
| `services.*.pre_stop` | yes |  |
| `services.*.pre_stop.[].command` | yes |  |
| `services.*.pre_stop.[].environment` and below | yes |  |
| `services.*.pre_stop.[].privileged` | partly | the hook has the capabilities of the machine |
| `services.*.pre_stop.[].user` | yes |  |
| `services.*.pre_stop.[].working_dir` | yes |  |
| `services.*.privileged` | yes |  |
| `services.*.profiles` | yes |  |
| `services.*.provider` and below | no | provider services are docker compose plugins, which nspawn does not run |
| `services.*.pull_policy` | yes |  |
| `services.*.pull_policy`: `build` | no | nspawn runs published images and does not build Dockerfiles |
| `services.*.pull_policy`: `daily` | partly | pulled every time; a pull of an unchanged image downloads nothing |
| `services.*.pull_policy`: `weekly` | partly | pulled every time; a pull of an unchanged image downloads nothing |
| `services.*.pull_policy`: `every_...` | partly | pulled every time; a pull of an unchanged image downloads nothing |
| `services.*.pull_policy`: `refresh` | partly | pulled every time; a pull of an unchanged image downloads nothing |
| `services.*.pull_refresh_after` | ignored | a refreshing pull policy pulls every time |
| `services.*.read_only` | yes |  |
| `services.*.restart` | yes |  |
| `services.*.restart`: `on-failure:...` | partly | restarted on every failure: systemd keeps no count |
| `services.*.runtime` | ignored | machines run under systemd-nspawn |
| `services.*.scale` | yes |  |
| `services.*.secrets` | yes |  |
| `services.*.secrets.[].gid` | ignored | gid is not supported outside Swarm mode and will be ignored |
| `services.*.secrets.[].mode` | ignored | mode is not supported outside Swarm mode and will be ignored |
| `services.*.secrets.[].source` | yes |  |
| `services.*.secrets.[].target` | yes |  |
| `services.*.secrets.[].uid` | ignored | uid is not supported outside Swarm mode and will be ignored |
| `services.*.security_opt` | ignored | nspawn sets the security of a machine itself |
| `services.*.shm_size` | yes |  |
| `services.*.stdin_open` | yes |  |
| `services.*.stop_grace_period` | partly | in whole seconds |
| `services.*.stop_signal` | yes |  |
| `services.*.storage_opt` | ignored | nspawn stores a machine as layers on the host's file system |
| `services.*.sysctls` and below | partly | net.* keys for a machine that does not boot; other keys are refused |
| `services.*.tmpfs` | yes |  |
| `services.*.tty` | yes |  |
| `services.*.ulimits` and below | yes |  |
| `services.*.use_api_socket` | no | there is no Docker API socket to hand to a machine |
| `services.*.user` | partly | refused for an image that boots, whose init runs as it is |
| `services.*.userns_mode` | ignored | a machine always has a user namespace of its own |
| `services.*.uts` | ignored | a machine always has a UTS namespace of its own |
| `services.*.volumes` | yes |  |
| `services.*.volumes.[].bind` | yes |  |
| `services.*.volumes.[].bind.create_host_path` | yes |  |
| `services.*.volumes.[].bind.propagation` | ignored | a bind mount into a machine propagates nothing |
| `services.*.volumes.[].bind.recursive` | ignored | a bind mount into a machine is recursive |
| `services.*.volumes.[].bind.selinux` | ignored | nspawn does not relabel what it mounts into a machine |
| `services.*.volumes.[].consistency` | ignored | consistency is for Docker Desktop's file sharing |
| `services.*.volumes.[].image` and below | no | an image is not mounted as a volume |
| `services.*.volumes.[].read_only` | yes |  |
| `services.*.volumes.[].source` | yes |  |
| `services.*.volumes.[].target` | yes |  |
| `services.*.volumes.[].tmpfs` and below | yes |  |
| `services.*.volumes.[].type` | yes |  |
| `services.*.volumes.[].type`: `cluster` | no | volumes[].type: cluster (CSI) volumes are only supported in Swarm mode |
| `services.*.volumes.[].type`: `image` | no | an image is not mounted as a volume |
| `services.*.volumes.[].type`: `npipe` | no | named pipes are for Windows containers |
| `services.*.volumes.[].volume` | yes |  |
| `services.*.volumes.[].volume.labels` and below | ignored | nspawn volumes have no labels |
| `services.*.volumes.[].volume.nocopy` | ignored | a new volume is filled from the image all the same |
| `services.*.volumes.[].volume.subpath` | no | a volume is mounted whole |
| `services.*.volumes_from` | no | a machine mounts the volumes of another one by naming them |
| `services.*.working_dir` | partly | refused for an image that boots, whose init runs as it is |
| `version` | yes |  |
| `volumes` | yes |  |
| `volumes.*` | yes |  |
| `volumes.*.driver` | no | nspawn volumes are directories of the host: only the local driver |
| `volumes.*.driver`: `local` | yes |  |
| `volumes.*.driver_opts` and below | partly | type: none with o: bind mounts the device directory of the host; other options are refused |
| `volumes.*.external` | yes |  |
| `volumes.*.external.name` | yes |  |
| `volumes.*.labels` and below | ignored | nspawn volumes have no labels |
| `volumes.*.name` | yes |  |

### Commands

| Command | nspawn | Notes |
| --- | --- | --- |
| `docker compose --all-resources` | yes |  |
| `docker compose --ansi` | yes |  |
| `docker compose --compatibility` | partly | config takes it as docker compose does; machine names join their parts with `-`, as machined takes no `_` |
| `docker compose --dry-run` | yes |  |
| `docker compose --env-file` | yes |  |
| `docker compose --file` | partly | files on this host: a git or oci:// address is refused |
| `docker compose --insecure-registry` | ignored | nspawn.toml says how each registry is reached |
| `docker compose --no-ansi` | yes |  |
| `docker compose --parallel` | yes |  |
| `docker compose --profile` | yes |  |
| `docker compose --progress` | yes |  |
| `docker compose --project-directory` | yes |  |
| `docker compose --project-name` | yes |  |
| `docker compose --verbose` | ignored | nspawn compose has no debug output |
| `docker compose --version` | partly | --short and json give the docker compose release it matches; the plain form names nspawn's version too |
| `docker compose --workdir` | yes |  |
| `alpha` | no | alpha is not supported by nspawn compose |
| `attach` | no | attach is not supported by nspawn compose; logs --follow shows a service's output |
| `bridge` | no | bridge is not supported by nspawn compose |
| `build` | no | nspawn runs published images and does not build Dockerfiles |
| `commit` | no | commit is not supported by nspawn compose |
| `config` | yes |  |
| `config --environment` | yes |  |
| `config --format` | yes |  |
| `config --hash` | yes |  |
| `config --images` | yes |  |
| `config --lock-image-digests` | no | images are not pinned to their digests yet |
| `config --models` | yes |  |
| `config --networks` | yes |  |
| `config --no-consistency` | yes |  |
| `config --no-env-resolution` | yes |  |
| `config --no-interpolate` | yes |  |
| `config --no-normalize` | yes |  |
| `config --no-path-resolution` | yes |  |
| `config --output` | yes |  |
| `config --profiles` | yes |  |
| `config --quiet` | yes |  |
| `config --resolve-image-digests` | no | images are not pinned to their digests yet |
| `config --services` | yes |  |
| `config --variables` | yes |  |
| `config --volumes` | yes |  |
| `cp` | no | not in nspawn compose yet |
| `create` | no | not in nspawn compose yet |
| `down` | no | not in nspawn compose yet |
| `events` | no | not in nspawn compose yet |
| `exec` | no | not in nspawn compose yet |
| `export` | no | export is not supported by nspawn compose |
| `images` | no | not in nspawn compose yet |
| `kill` | no | not in nspawn compose yet |
| `logs` | no | not in nspawn compose yet |
| `ls` | no | not in nspawn compose yet |
| `pause` | no | not in nspawn compose yet |
| `port` | no | not in nspawn compose yet |
| `ps` | no | not in nspawn compose yet |
| `publish` | no | publish is not supported by nspawn compose |
| `pull` | no | not in nspawn compose yet |
| `push` | no | push is not supported by nspawn compose, which builds no image |
| `restart` | no | not in nspawn compose yet |
| `rm` | no | not in nspawn compose yet |
| `run` | no | not in nspawn compose yet |
| `scale` | no | not in nspawn compose yet |
| `start` | no | not in nspawn compose yet |
| `stats` | no | not in nspawn compose yet |
| `stop` | no | not in nspawn compose yet |
| `top` | no | not in nspawn compose yet |
| `unpause` | no | not in nspawn compose yet |
| `up` | no | not in nspawn compose yet |
| `version` | partly | --short and json give the docker compose release it matches; the plain form names nspawn's version too |
| `version --format` | yes |  |
| `version --short` | yes |  |
| `volumes` | no | not in nspawn compose yet |
| `wait` | no | not in nspawn compose yet |
| `watch` | no | watch is not supported by nspawn compose |
<!-- End of the support table. -->
