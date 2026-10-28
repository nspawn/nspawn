# The D-Bus interface

nspawn serves `org.nspawn` on the system bus, and the command line is a
client of it: everything `nspawn` does is a method here, so another client
sees exactly what the command line sees. machined stays an implementation
detail: a client never has to talk to it, though every running machine carries
its machined object path (`machine_path`) for whoever wants that view.

## Getting it on the bus

```
sudo nspawn daemon --install
```

writes these files, the units among them carrying the path of the binary that
ran the command, and reloads systemd and the bus:

| File | Role |
|---|---|
| `/etc/dbus-1/system.d/org.nspawn.conf` | bus policy: root may own `org.nspawn`, everyone may call it and receive its signals (those of a job or a command are sent to the client that started it) |
| `/usr/share/polkit-1/actions/org.nspawn.policy` | the two actions polkit authorizes callers for |
| `/usr/share/dbus-1/system-services/org.nspawn.service` | bus activation: `SystemdService=nspawn.service` |
| `/etc/systemd/system/nspawn.service` | `Type=dbus` unit running `nspawn daemon` |
| `/etc/systemd/system/nspawn-api@.socket`, `nspawn-api@.service` | a machine's Docker API, activated on demand |
| `/etc/systemd/system/nspawn-auto-update.service`, `.timer` | `nspawn auto-update`, its timer written but not enabled |
| `/usr/lib/systemd/journald@nspawn.conf` | the size of the journal namespace machines write their output to (1 GiB) |

Nothing runs until a client calls `org.nspawn`; the bus then starts the unit,
and the service exits again after a minute without a call, a job or a command
running (`nspawn daemon --idle-exit`). The service reads
`/etc/nspawn/nspawn.toml`; `nspawn --config FILE daemon --install` puts
another file on the unit's command line.

`PullImage`, `CreateMachine`, `PushImage`, `BuildImage`, `BuildDockerfile`,
`SearchImages`, `ListRepositories`, `ListTags`, `ImageDigest` and `Login` take
`registry` and `ca_cert` options that override that configuration for one
call; `AutoUpdate` always uses the service's. The command line sends each one
it was given (flag, environment or its own configuration file) and leaves the
service's alone otherwise. The signature policy of a registry is the service's
own, from the `[registries."host"]` tables of its file (the hub's is built in):
a client chooses the registry, never the policy.

## Who may call what

Every user may call; who may do what is polkit's answer. `ListImages`,
`GetImage`, `Generate`, `ListMachines`, `GetMachine`, `MachineStats`,
`MachineProcesses`, `Events`, `ListNetworks`, `GetNetwork`, `ListVolumes`,
`ListSecrets`, `GetSecret`, `DiskUsage` and `BuildCacheUsage` ask for
`org.nspawn.inspect`. Every other method asks for `org.nspawn.manage`: those
that change something, run something inside a machine, or use the registry
credentials kept here, so `Logs`, `SearchImages`, `ListRepositories`,
`ListTags` and `ImageDigest` among them. What inspect shows includes every
machine's `-e` values, and `BuildCacheUsage` the build steps, a `--build-arg`
value written in a `RUN` among them.

Both actions are for administrators by default, so `sudo nspawn ...` works and
a desktop or `pkttyagent` session is asked for a password. polkit asks only
when the call carries the ALLOW_INTERACTIVE_AUTHORIZATION flag, which nspawn's
command line sets, as busctl and systemctl do; a call without it that would
need a password is refused at once. Root is allowed without asking, which is
also how the service keeps working where polkit is not installed: there,
nobody else can call it. A refusal comes back as
`org.nspawn.Error.NotAuthorized`.

Not everything goes to polkit: anyone may read the Manager's properties, a job
or process object answers the user who started it and root (anyone else gets
`AccessDenied`), and `org.nspawn.Names` hands out names alone. The signals of a
job or a command (`JobOutput`, `JobProgress`, `JobRemoved`, `Exited` and the
`PropertiesChanged` of their objects) go to the client that started it and to
nobody else; `ImageAdded`, `ImageRemoved`, `MachineStarted` and
`MachineStopped` reach everyone listening.

An administrator hands either action to a group with a rule of their own,
which is how to drive nspawn without a password. The packages ship one as an
example in their documentation directory
(`packaging/polkit/nspawn-wheel.rules`):

```
polkit.addRule(function (action, subject) {
    if (action.id.startsWith("org.nspawn.") && subject.isInGroup("wheel")) {
        return polkit.Result.YES;
    }
});
```

Copy it to `/etc/polkit-1/rules.d/50-nspawn.rules`. Everyone in that group can
run commands as root inside a machine and mount any host path into one, so the
rule makes them administrators of the host.

## SELinux

Where SELinux is enforcing (Fedora, RHEL) the service needs a domain of its
own, `nspawn_t`, as machined and the container runtimes have: the base policy
lets no domain use the pipes of an unconfined service, so the bus drops the
service as soon as it passes a descriptor (`Exec`, `Shell`, `RunMachine`,
`Logs`, `Events`, `CopyFrom`, `CopyTo`), and the client sees a disconnect. The
policy lives in `packaging/selinux` and the `nspawn-selinux` package loads it.
`--install` warns when SELinux is enabled and the module is not loaded, and,
with the module loaded, when the binary is not labelled `nspawn_exec_t`, which
a build installed by hand is not. Commands run inside a machine take the
machine's own context.

## org.nspawn.Manager at /org/nspawn

### Conventions

Results are dictionaries (`a{sv}`) whose keys are the names `nspawn inspect`
and `--json` print. Options are dictionaries too, and a key nothing expects is
an error, so a typo never passes as a default. An unsigned option takes `t`,
`u`, `q` or `y`; a signed one (`memory_swap`, `oom_score_adj`) any integer
type; `cpus` takes `d` or any integer. A list option given as `["none"]`
forgets what the machine had, and an empty string forgets a string option
(`hostname`, `user`, `working_dir`, `stop_signal`). Times are unix seconds
unless a key says otherwise, durations of a healthcheck microseconds. Errors
come back as `org.nspawn.Error.Failed` with the same message the command line
prints.

Properties: `Version` (the command line compares it with its own and warns when
they differ, since the service keeps running the binary it started with),
`Registry` (the configured registry), `Bridge` and `Subnet` (the default
network's bridge and subnet), `Jobs` and `Processes` (`ao`: every job and every
command still running, and the last hundred that ended; older ones are gone
with their objects).

### Records

`GetImage`, `ListMachines` and `GetMachine` return a record with these keys:

| Key | Type | Meaning |
|---|---|---|
| `name`, `reference`, `digest` | s | the local name, the reference pulled or built, the manifest digest |
| `backend`, `origin`, `mode` | s | overlay, flat or mstack; pull, build or create; boot or app |
| `created` | t | when this record was made here, by a pull, a build or a create; with the name, which machine of the name it is, as events carry it |
| `signed_by`, `signed_at` | s, t | who signed the image and when, as the pull verified it; empty and 0 without a verified signature |
| `network` | s | bridge, veth, host, none, `container:NAME` or the primary network's name |
| `networks` | as | every bridge network joined, the primary one first |
| `address`, `addresses` | s, a{ss} | the address on the primary network, and by network |
| `aliases` | as | `NETWORK=NAME` |
| `ports`, `volumes`, `secrets` | as | `[IP:]HOST->CONTAINER/PROTO` (an IPv6 IP in brackets; no IP for every address); `SOURCE:TARGET[:ro]`; `NAME:TARGET:MODE:UID:GID` |
| `env`, `image_env` | as | the machine's variables, the image's |
| `entrypoint`, `cmd`, `command`, `image_cmd` | as | the entrypoint (the one given, else the image's) and the arguments after it (the ones given, none after an entrypoint given alone, else the image's cmd), both together, the image's own cmd |
| `working_dir`, `user`, `stop_signal` | s | the flag's when given, the image's otherwise |
| `labels`, `image_labels` | a{ss} | the image's with the machine's on top, the image's alone |
| `restart` | s | no, on-failure, always or unless-stopped |
| `memory`, `pids_limit` | t | bytes, processes; 0 for none |
| `memory_swap` | x | memory and swap together, -1 without a bound on swap, 0 for as much again as memory |
| `cpus` | d | CPUs, 0 for none |
| `hostname`, `timezone`, `log_driver` | s | `""` for the defaults (the machine's name, systemd-nspawn's auto); local, journal or none |
| `cap_add`, `cap_drop`, `tmpfs`, `dns`, `dns_search`, `interfaces`, `gpus` | as | as the flags take them |
| `extra_hosts` | as | `HOST:IP` |
| `devices` | as | `HOST:CONTAINER:PERMISSIONS`, then the CDI device names |
| `privileged`, `read_only` | b | `--privileged`, `--read-only` |
| `init`, `pid1` | b | `--init` given; the program of an app runs as PID 1 (its image carries s6-overlay, or `--init=false`) |
| `shm_size` | t | bytes, 0 for the default |
| `stop_timeout` | t | seconds, 10 when not given, 18446744073709551615 for none |
| `oom_score_adj` | x | -1000 to 1000 |
| `ulimits`, `sysctls` | a{ss} | NAME to `SOFT:HARD`; KEY to VALUE |
| `depends_on` | as | `NAME:CONDITION` or `NAME:CONDITION:optional`, the condition started, healthy or completed |
| `image_volumes` | as | what the image declares as volumes |
| `healthcheck` | a{sv} | test (as), interval, timeout, start_period, start_interval (t, microseconds, 0 for the default: 30 s, 30 s, none and 5 s), retries (u, 0 for 3); empty without one |

A machine of `ListMachines` and `GetMachine` adds:

| Key | Type | Meaning |
|---|---|---|
| `state` | s | machined's (opening, running, closing), paused for a running machine whose cgroup is frozen, or, for a machine machined does not list, restarting, starting, closing or stopped |
| `started`, `finished` | t | when the last run started and ended, 0 when unknown |
| `leader`, `os` | t, s | the leader's PID on the host, the machine's OS, while it runs |
| `machine_path` | s | machined's object path, `""` when it does not run |
| `bindings` | as | where its ports are published, the host's side first: `0.0.0.0:8080->80/tcp`, and `[::]:8080->80/tcp` on a host with IPv6 |
| `exit_code` | i | the last run's exit code (the program's, or 128 plus the signal it died of), on a stopped or restarting machine |
| `health`, `health_failing_streak`, `health_log` | s, u, as | for a running machine with a healthcheck: starting, healthy or unhealthy; the failures in a row; the last probes as `END EXIT_CODE OUTPUT` |

### Machine options

`StartMachine`, `RunMachine` and `CreateMachine` take these options; what is
given is remembered for the machine.

| Option | Type | Meaning |
|---|---|---|
| `network`, `networks` | s, as | bridge, veth, host, none, `container:NAME` or a network made with `CreateNetwork`; several bridge networks, the first one primary |
| `aliases` | as | `NAME` on the primary network or `NETWORK=NAME` |
| `addresses` | as | `IP` on the primary network or `NETWORK=IP`: fixed addresses on networks made with `CreateNetwork` |
| `publish` | as | `[IP:][HOST:]CONTAINER[/udp]` (an IPv6 IP in brackets: `[::1]:8080:80`; `0.0.0.0` every IPv4 address alone, `[::]` every IPv6 one, no IP both) or `published=HOST,target=CONTAINER[,protocol=udp][,host_ip=IP]`; no host port or 0 takes a free one, a host range with one container port the first free one in it, ranges of equal length one mapping per port |
| `entrypoint` | s | replaces the image's and drops its cmd; `""` runs the command alone |
| `command` | as | replaces an app's cmd |
| `image_command` | b | forgets the remembered entrypoint and command and runs the image's own |
| `env`, `volume`, `label` | as | `VAR=value` or `VAR`; `SOURCE:TARGET[:ro]`; `KEY=VALUE` on top of the image's labels |
| `unset_env` | as | `CreateMachine` only: variables of the image's the machine goes without |
| `restart` | s | no, on-failure[:N], always or unless-stopped |
| `memory`, `pids_limit`, `memory_swap`, `cpus` | t, t, x, d | as in the record; 0 removes a limit |
| `health_cmd`, `health_test` | s, as | the probe through the image's shell, or the whole test (`["CMD", ARGV...]` runs without a shell) |
| `health_interval`, `health_timeout`, `health_start_period`, `health_start_interval` | t | microseconds |
| `health_retries`, `no_healthcheck` | u, b | |
| `hostname`, `user`, `working_dir`, `stop_signal` | s | `user` is `USER[:GROUP]` |
| `timezone` | s | auto, off, copy, bind, symlink or delete, systemd-nspawn's `Timezone=`; auto forgets it |
| `log_driver` | s | local (the default) writes to nspawn's own journal namespace, journal to the system journal, none drops the output, though an attached `RunMachine` still passes it straight through |
| `cap_add`, `cap_drop`, `tmpfs`, `dns`, `dns_search`, `interfaces` | as | as the flags take them |
| `devices` | as | `HOST[:CONTAINER[:PERMISSIONS]]` or a CDI device name, `VENDOR/CLASS=NAME` |
| `gpus` | as | as `--gpus` takes them: all, a count, `device=IDS`, `driver=`, `capabilities=` |
| `extra_hosts`, `ulimits`, `sysctls` | as | `HOST:IP`; `NAME=SOFT[:HARD]`; `KEY=VALUE` |
| `secrets` | as | `NAME[:TARGET[:MODE[:UID:GID]]]` |
| `depends_on` | as | `NAME[:CONDITION][:optional]`, other machines systemd starts first |
| `privileged`, `read_only` | b | |
| `init` | b | true puts the program of an app under the stub init, false makes it PID 1; without it the image decides |
| `shm_size`, `stop_timeout` | t | bytes; seconds |
| `oom_score_adj` | x | |

`StartMachine` adds `wait` (b, default true: wait for a booted machine's init)
and `remove` (b, `run -d --rm`: removed once it ends). `CreateMachine` takes
no `image_command`, and adds `backend` and `force`, and `registry` and
`ca_cert` for a source it has to pull.

### Images

| Method | Like | Notes |
|---|---|---|
| `ListImages() -> aa{sv}` | `images ls` | name, kind, size, read_only (b, machined's read-only flag), and for the images nspawn installed backend, origin, reference, signed_by, image_only (b: a record pulled or built that never ran as a machine, which pulls and prunes may replace), created (t: when the image was made, as its config says; 0 when it says nothing), pulled (t: when it was pulled or built here, the record's created) |
| `GetImage(s name) -> a{sv}` | | the record (see Records) |
| `PullImage(s reference, a{sv} options) -> o` | `pull` | options name, backend (auto, overlay, flat, mstack), mode (auto, boot, app), force, verify (b, default true: the signature check the registry's policy asks for; `--no-verify` sends false), registry, ca_cert; a job |
| `CreateMachine(s source, s name, a{sv} options) -> o` | `create` | the machine options (see Machine options); a job |
| `PushImage(s image, a{sv} options) -> o` | `push` | options to, registry, ca_cert; a job |
| `BuildImage(s directory, s tag, a{sv} options) -> o` | mkosi `build` | options name, distribution, release, profile, backend, mode, force, keep_output, mkosi_args, registry, ca_cert; a job whose output includes mkosi's |
| `BuildDockerfile(h context, a{sv} options) -> o` | Dockerfile `build`, `compose build` | `context` a tar stream of the build context; options dockerfile (ay, required), tags (as), build_args (as, KEY=VALUE), target (s), no_cache (b), no_cache_filter (as, stages), pull (b), labels (as, KEY=VALUE), network (s: default, none or host), add_hosts (as, HOST:IP or HOST=IP), allow (as: network.host), secrets (a{say}, by id), shm_size (t, bytes), ulimits (as, NAME=SOFT[:HARD]), verify (b, default true), backend, mode, force, registry, ca_cert; a job whose output is the steps' progress; the build stops when its caller leaves the bus |
| `PruneBuildCache(a{sv} options) -> o` | `builder prune` | options all (b: what images take too), until (t: entries last used before), keep_storage (t, bytes: the least recently used go until what the cache holds of its own fits); a job |
| `BuildCacheUsage() -> aa{sv}` | `builder du` | what the build cache holds, most recently used first: id, kind (layer, base or mount), description, size (t), used (t), shared (b, an image takes it too) |
| `PruneImages(a{sv} options) -> o` | `images prune` | option all (b): every image (a record pulled or built and never started) no machine was made from; without it nothing qualifies, as nspawn's store keeps no dangling images; labels (as, KEY or KEY=VALUE) and until (t) narrow it; a job |
| `DiskUsage() -> a{sv}` | `system df` | images, containers (machines), local_volumes and build_cache, each `a{st}` of total, active, size and reclaimable (bytes); image_rows (`aa{sv}`: name, reference, created, size, containers), container_rows (name, reference, size, running, made: made from an image by create or run, which system prune removes once stopped), volume_rows (name, links, size) |
| `RemoveImages(as names) -> o` | `images rm` | a job: every name is tried, and it fails at the end when one could not be removed; a name with nothing behind it fails with `no machine or image named NAME` |
| `RemoveMachines(as names, a{sv} options) -> o` | `rm` | `RemoveImages` with the option force, which stops a running machine first (SIGKILL) instead of refusing it |

### Registries

| Method | Like | Notes |
|---|---|---|
| `SearchImages(s term, s source, u limit, a{sv} options) -> (aa{sv}, as)` | `search` | source `""` (both), "hub" or "dockerhub", limit per source; the hits (source, name, description, stars (t), official (b)) and the notes (a source that could not be reached); options registry, ca_cert |
| `ListRepositories(s filter, b with_tags, a{sv} options) -> aa{sv}` | `hub ls` | name, tags (as, empty without `with_tags`); options registry, ca_cert |
| `ListTags(s repository, a{sv} options) -> as` | `hub tags` | options registry, ca_cert |
| `ImageDigest(s reference, a{sv} options) -> s` | `compose config --resolve-image-digests` | the digest the reference names on its registry now (an index's for a tag of several platforms), asked without pulling it, with the credentials of `login`; options registry, ca_cert |
| `Login(s registry, s username, s password, a{sv} options) -> a{sv}` | `login` | `""` for the configured registry; options registry (the one `""` stands for), ca_cert; registry, username, asked (b, whether the registry asked for credentials) |
| `Logout(s registry) -> b` | `logout` | true when credentials were stored and removed |

### Machines

Containers only: the virtual machines machined also registers are left out of
the listings, and naming one to a method that takes a machine fails with an
error that says so.

| Method | Like | Notes |
|---|---|---|
| `ListMachines(b all) -> aa{sv}` | `ps`, `ps -a` | the machines that run (and, with `all`, every machine nspawn keeps a record of; a restarting one is listed without it too), each a record and its machine keys (see Records) |
| `GetMachine(s name) -> a{sv}` | `inspect` | one machine as `ListMachines` has it, running or not; fails for a name that neither runs nor has a record |
| `StartMachine(s name, a{sv} options) -> (s, as)` | `start` | the machine options (see Machine options); "started", "ended" (the program returned before the machine registered) or "restarting" (it ended and its restart policy brings it back), and the notes made on the way |
| `RunMachine(s name, a{sv} options, a{sh} fds) -> (a{sh}, o, as)` | `run` without `-d` | starts, attached, a machine that `PullImage` or `CreateMachine` made; options those of `StartMachine` (wait defaults to false, remove is `--rm`), tty (b), rows and cols (t, 24 and 80), term (s, xterm); descriptor stdin for the program's input without tty. An app gets a pseudo terminal with tty, its master under "tty"; otherwise the machine's output, a line at a time, under "stdout". Returns the streams, a process object with the run's exit status (the program's, or 128 plus the signal it died of, 137 after SIGKILL), and the start's notes. A transient unit removes a `--rm` machine once the machine's unit is down |
| `StopMachine(s name, a{sv} options) -> (s, as)` | `stop` | options force, wait (default true), timeout (seconds; the machine's stop_timeout by default, else 10; a day at most, or 18446744073709551615 to wait for as long as it takes, as `stop -t -1` asks); "stopped" or "was-not-running", and the notes (a program that had to be killed) |
| `KillMachine(s name, a{sv} options) -> as` | `kill` | option signal (a name such as KILL, SIGHUP or RTMIN+3, or a number; SIGKILL by default). SIGKILL is `StopMachine` with force; other signals go to an app's program (waited for a few seconds right after a start) or to a booted machine's init. A signal that stops the machine keeps its restart policy from bringing it back: SIGKILL, any signal to an app without a stop signal of its own, an app's own stop signal, or SIGRTMIN+3 and SIGRTMIN+4 (halt, poweroff) to a booted init; other signals leave the policy in charge. Fails for a machine that does not run, except SIGKILL, which also ends one between two runs of its restart policy or still waiting to start. Returns the notes |
| `PauseMachine(s name)`, `UnpauseMachine(s name)` | `pause`, `unpause` | the machine's cgroup frozen and thawed; fail for a machine that does not run |
| `UpdateMachine(s name, a{sv} options) -> b` | `update` | options restart, memory, memory_swap, cpus, pids_limit, no_healthcheck and the health_* ones, as for `StartMachine` (0 removes a limit, absent keeps it); a running machine gets the limits at once and its probes start over; true when it was running |
| `MachineStats(as names) -> aa{sv}` | `stats` | one sample of each named machine that runs (every running container when none is named; names that do not run are left out): name, time_usec (CLOCK_MONOTONIC), cpu_usec, memory (memory.current without inactive_file), memory_limit (the host's memory without a limit), pids, io_read, io_write, net_rx, net_tx; a counter that cannot be read is left out, the network ones on the host's network; rates come from two samples |
| `MachineProcesses(s name) -> aa{sv}` | `top` | the processes in the machine's PID namespace, from its cgroup: pid (u), user (s, the uid as the machine sees it, root for 0), time (s, CPU time HH:MM:SS), command (s) |
| `Generate(as names, a{sv} options) -> s` | `generate` | a shell script that makes the named machines again, every machine of nspawn's when none is named: `network create` for the networks they join, then `run -d` with every flag a machine keeps (`pull` for an image without flags of its own), each after the machines it depends on or takes its network from, which come along; what the flags cannot carry (secrets, host directories, images built on this host, a healthcheck in exec form) as comments at the top; options: none (any key is an error) |
| `AutoUpdate(as names, a{sv} options) -> o` | `auto-update` | the machines labelled org.nspawn.auto-update=registry (or io.containers.autoupdate=registry), or the ones named, are made anew from the image their reference names now at its registry, with every option they had, started when they ran, and put back when they do not come up (healthy, with a healthcheck). Option dry_run (b) only looks. A job; each machine's outcome is an `auto-update` event |
| `Exec(s machine, as argv, s user, a{sv} options) -> (a{sh}, o)` | `exec` | user `""` for the machine's own (its --user, else its image's USER; root without either, and on a booted machine); options tty (default true), rows and cols (t, 24 and 80), env (as, the caller's `TERM=` among them; xterm otherwise on a terminal), workdir (s, instead of the machine's), detach (b: no streams come back, the output is discarded and the command runs on); returns the streams ("tty", or "stdin", "stdout", "stderr") and a process object |
| `Shell(s machine, s user, a{sv} options) -> (h, s)` | `shell` | user `""` for root; the login session machined offers for a booted machine: its pseudo terminal and the terminal's path; options env, as for `Exec`; apps get `Exec` of a shell with a tty instead |
| `CopyFrom(s machine, s path, a{sv} options) -> (h, o)` | `cp NAME:PATH ...` | a tar stream of `path` on the pipe, owners as the machine sees them, and a job with the outcome; a running machine, or a stopped overlay or flat one; options: none |
| `CopyTo(s machine, s path, h stream, a{sv} options) -> o` | `cp ... NAME:PATH` | the tar stream read from `stream` unpacked at `path` by `cp`'s rules (see USAGE.md), everything root's inside; option contents (b: the source was `DIR/.`); a job |
| `Events(a{sv} options) -> (a{sh}, o)` | `events` | options since, until (journalctl's time syntax; without until it follows, and until needs since), filters (as: name= or machine=, type=, event= or action=, label=KEY or KEY=VALUE); under "stdout" one JSON event per line (see Events), journalctl's complaints under "stderr", and a process object for its end; a user other than root holds 64 streams at once at most |
| `Logs(s machine, a{sv} options) -> (a{sh}, o)` | `logs` | options follow, lines, since, until (journalctl's time syntax; with until nothing is followed), timestamps, all, inside, json (b: journalctl's JSON entries, one a line, for a client that writes each line's time itself); journalctl's "stdout" and "stderr" and a process object for its exit status. Lines from before the machine was created (a removed machine of the same name) are left out; a local machine's lines come from nspawn's journal namespace and the system's journal interleaved; journalctl is stopped once nobody reads its output. A machine whose log_driver is none is refused unless inside is set |

`StartMachine` waits for a booted machine's init and `StopMachine` for the
machine to be gone, which can take longer than a client's default timeout
(`busctl --timeout=120`).

`nspawn exec` is a client of `Exec`: it asks for a pseudo terminal when run
from one and for pipes otherwise, pumps them, and takes the exit status from
the process object. The terminal is allocated inside the machine, on its own
devpts, so `tty` and everything that opens its terminal by name work there.

### Volumes

| Method | Like | Notes |
|---|---|---|
| `ListVolumes() -> aa{sv}` | `volume ls` | name, path, used_by (the machines whose records mount it), created |
| `CreateVolume(s name) -> s` | `volume create` | the volume's path; one that exists already is not an error |
| `RemoveVolumes(as names) -> o` | `volume rm` | a job: every name is tried, and it fails at the end when one was in use, unknown or not a volume |
| `PruneVolumes() -> o` | `volume prune` | a job removing every volume no machine uses |

### Secrets

| Method | Like | Notes |
|---|---|---|
| `ListSecrets() -> aa{sv}` | `secret ls` | name, created, size (t, bytes of the plaintext), labels (a{ss}), used_by (as, the machines whose records take it); never the content |
| `GetSecret(s name) -> a{sv}` | `secret inspect` | one secret as `ListSecrets` has it |
| `CreateSecret(s name, ay content, a{sv} options)` | `secret create` | the content, encrypted for this host with systemd-creds; options labels (as, KEY=VALUE), replace (b: the content of a secret of that name replaced, which the machines that name it read at their next start) and replace_if_changed (b: replaced only when its content or labels differ, the service comparing them); a name in use is refused otherwise |
| `RemoveSecrets(as names) -> o` | `secret rm` | a job: every name is tried, and it fails at the end when one was in use or unknown |

### Networks

| Method | Like | Notes |
|---|---|---|
| `ListNetworks() -> aa{sv}` | `network ls` | every network, the default one ("bridge") first: name, interface, subnet, gateway, internal, created, labels (a{ss}), machines (as, every machine joining it) |
| `GetNetwork(s name) -> (a{sv}, aa{sv})` | `network inspect` | the network as above without machines, and its machines (name, address on this network, aliases (as) on it, ports, running) |
| `CreateNetwork(s name, a{sv} options) -> a{sv}` | `network create` | options subnet (CIDR; the next free /24 of network_pool otherwise), internal (b), labels (as, KEY=VALUE); the network as `GetNetwork`'s first value has it, plus notes (as) |
| `RemoveNetworks(as names) -> o` | `network rm` | a job; a network in use, unknown or the default one is refused without stopping the others |
| `PruneNetworks() -> o` | `network prune` | a job removing every user-defined network no machine names; it fails at the end when one could not go |
| `ConnectNetwork(s network, s machine, a{sv} options) -> o` | `network connect` | the machine joins the network: at once when it runs, from its next start as any network it joins; options aliases (as), address (s, a fixed IPv4 address on a network made with `CreateNetwork`); a job |
| `DisconnectNetwork(s network, s machine, a{sv} options) -> o` | `network disconnect` | the machine leaves the network, at once when it runs; option force (b: a machine or a membership that is not there is no error); a job |
| `NetworkUp() -> a{sv}` | `network up` | every network's bridge comes up; the default network (bridge, subnet, gateway, host_name), networks (as, every network) and notes |

### Signals

| Signal | When |
|---|---|
| `JobOutput(o job, s kind, s line)` | a job said a line: kind "line" (progress, a result) or "note" (a remark); lines queued back to back go out in one signal, joined by newlines, up to 64 KiB |
| `JobProgress(o job, s item, t done, t total)` | how far a blob transfer of a job got (a pull, a push, a build fetching a base image, an auto-update): `done` bytes of `total` (0 when unknown) of blob `item` (its short digest); when the transfer starts, a few times a second, and when it ends. Not kept in `Output` |
| `JobRemoved(o job, s result)` | a job ended, "done" or "failed" |
| `ImageAdded(s name)` | after `PullImage`, `CreateMachine`, `BuildImage`, and `BuildDockerfile` (one per tag) |
| `ImageRemoved(s name)` | after `RemoveImages`, `RemoveMachines` and `PruneImages`; a `--rm` machine removed by its transient unit sends none |
| `MachineStarted(s name)`, `MachineStopped(s name)` | machined's own events, for the machines nspawn installed |

### Events

`Events` hands out one JSON object per line, the shape `nspawn events --json`
prints (`Event` in `src/api/events.rs`):

```json
{"time": "2026-09-25T10:20:16.502186Z", "time_usec": 1790331616502186,
 "type": "machine", "action": "health_status", "name": "web",
 "attributes": {"created": "1790331500", "image": "docker.io/library/busybox:latest", "status": "healthy"},
 "labels": {"role": "web"}}
```

`attributes` are the same for every action of a type, plus what the action
adds; they travel with the journal entry, so a `remove` says as much as a
`create` once the record is gone. systemd's actions take `image`, `created` and
the labels from the machine's record, and lack them once it is removed.
`labels` are the machine's while it has a record, and empty otherwise. The
entries are PID 1's about `systemd-nspawn@` units and nspawn's own (message ID
`b0b60147942247cab22cc49510006a0b`), both only from root.

| Type | Actions | Attributes of every action | The action's own |
|---|---|---|---|
| `machine` | `start`, `die`, `stop`, `restart`, `oom`, `fail` (systemd's, for every `systemd-nspawn@` unit), `pull`, `build`, `create`, `push`, `kill`, `update`, `pause`, `unpause`, `health_status`, `auto-update`, `remove` (nspawn's) | `image` (the record's reference), `created` (which machine of the name it was) | `die`: `code` (`exited`, `killed`, `dumped`), `exit_code` (that run's exit code, the program's or 128 plus its signal: from the signal nspawn sent to it while the record exists, from the status systemd logged once it is gone), `signal`; `restart`: `restarts`; `fail`: `result`; `pull`: `reference`, `signed_by` (when a signature was verified); `build`: `reference`; `create`: `from`, `reference`; `push`: `reference` (the destination); `kill`: `signal`; `health_status`: `status`; `auto-update`: `result`, `from`, `to` |
| `network` | `create`, `remove`, `connect`, `disconnect` | `subnet`, `interface`, `internal`, `created` | `connect`, `disconnect`: `container`, `container_created` |
| `volume` | `create`, `remove` | `path` | |
| `secret` | `create`, `remove` | `size` (bytes of the plaintext) | |

## org.nspawn.Names at /org/nspawn

Names only, for any user and without polkit, as machined hands out the names
of its machines and images. Shell completion uses them: it runs as the user,
under `sudo` too, and cannot answer a password prompt. Secret names are not
served.

| Method | Returns |
|---|---|
| `Machines(b all) -> as` | the machines that run, as `ps` lists them; with `all`, every machine nspawn keeps a record of too, as `ps -a` |
| `Images() -> as` | the images, as `images ls` lists them: what `start` takes |
| `References() -> as` | the references of the local images: what `run` makes a machine from without a pull |
| `Networks() -> as` | the networks, `bridge` among them |
| `Volumes() -> as` | the named volumes |

Each list is sorted, every name once.

## org.nspawn.Process at /org/nspawn/process/N

What `Exec`, `Logs`, `Events` or `RunMachine` started. Properties: `Machine`,
`Argv`, `Pid` (on the host), `State` (running, exited) and `ExitStatus` (128
plus the signal when it died of one). `Signal(i signal)` signals through a
pidfd, so it never reaches a reused PID; for a `RunMachine` it goes to the
app's program, whichever process runs it then, and for a booted machine any
signal is a poweroff request; it fails once the process has exited.
`Exited(i status)` is sent after `State` changed. The Manager's `Processes`
property lists them.

## org.nspawn.Job at /org/nspawn/job/N

A job is returned by the long operations and keeps what happened. Properties:

- `Kind`: pull, create, push, build, rm, cp, volume-rm, volume-prune, secret-rm,
  network-rm, network-connect, network-disconnect, network-prune, image-prune,
  builder-prune or auto-update.
- `Target`: the image name, reference or tag it works on, `MACHINE:PATH` for cp,
  the machine for network-connect and network-disconnect, the names joined by
  spaces for a removal, empty for the prunes and auto-update.
- `State`: running, done or failed. A removal or prune that failed for some
  names is failed with its `Result` still listing what it removed.
- `Output`: its last thousand lines, at most 1 MiB of them; lines that come
  faster than they go out as `JobOutput` are dropped, and a note says how many.
- `Error`: the message of a failed job.
- `Result`, a dictionary:

| Kind | Result |
|---|---|
| pull | name, reference, mode, signed_by |
| build (mkosi) | name, reference, mode, output |
| build (Dockerfile) | id (the config digest), manifest_digest, names (as, an image per tag) |
| push | name, destination, url |
| create | name, mode |
| rm, volume-rm, volume-prune, secret-rm, network-rm, network-prune | removed (as) |
| image-prune, builder-prune | removed (as), reclaimed (t, bytes) |
| auto-update | machines (aa{ss}: name, image, policy, updated (false, pending, true, failed, rolled back or skipped), reason, from, to) |
| cp | entries, bytes |
| network-connect, network-disconnect | empty |

The service does not go idle before a job or a process has announced its end.

## Example

```
job=$(busctl --system call org.nspawn /org/nspawn org.nspawn.Manager \
    PullImage 'sa{sv}' fedora:44 2 name s web force b true | awk '{print $2}' | tr -d '"')
until [ "$(busctl --system get-property org.nspawn "$job" org.nspawn.Job State)" != 's "running"' ]; do
    sleep 1
done
busctl --system get-property org.nspawn "$job" org.nspawn.Job State Output
busctl --system --timeout=120 call org.nspawn /org/nspawn org.nspawn.Manager \
    StartMachine 'sa{sv}' web 1 publish as 1 8080:80
busctl --system call org.nspawn /org/nspawn org.nspawn.Manager ListMachines b false
busctl --system monitor org.nspawn
```
