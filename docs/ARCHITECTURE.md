# Architecture

nspawn is a single binary that talks to systemd (`org.freedesktop.systemd1`)
and systemd-machined (`org.freedesktop.machine1`) over D-Bus and to OCI
registries over HTTPS. The same binary serves `org.nspawn` on the system bus,
which the command line is a client of; the bus starts that service on demand
and it exits again when idle, so the service does not run between commands.
State lives in files, and the machine units call nspawn back through drop-in
hooks.

## Modules

| Module | Role |
|---|---|
| `api/` | the library: typed operations on images, machines and the network; progress goes through a `Report` and results come back as values, and nothing here prints but the unit hooks' messages, which go to the journal |
| `cli.rs`, `commands/` | clap definitions and the terminal side: arguments turned into calls on the service, tables, prompts, the commands that own the terminal (`run`, `exec`, `shell`, `logs`) and the host's end of `cp` |
| `client/` | the proxies for `org.nspawn`, how its errors read, how a job is followed |
| `daemon/` | the D-Bus service `org.nspawn`: the Manager interface, the Names interface (names alone, for every user), who may call what (`polkit.rs`), jobs, processes, dictionaries (`values.rs`), the files that make the bus start it (`install.rs`) |
| `completion.rs` | shell completion: the script a shell sources calls `nspawn` back with `NSPAWN_COMPLETE` set, and the names of machines, images, networks and volumes come from `org.nspawn.Names` at each TAB (`api/names.rs`) |
| `config.rs` | `/etc/nspawn/nspawn.toml` (the signature policies per registry among it), environment and flags |
| `reference.rs` | image references, local names, machine name rules |
| `hub.rs`, `auth.rs`, `search.rs` | registry client, credentials, search |
| `verify.rs` | image signatures: the policy per registry, the Sigstore bundles among an image's referrers, checked with sigstore-verify |
| `layout.rs`, `oci.rs` | OCI image layout reader, image config, boot or app detection |
| `store.rs` | layers, blobs, records, manifests, gc, the store lock |
| `install.rs`, `backend.rs` | turning blobs into a machine (overlay by default, flat without overlayfs, mstack only by name: experimental) |
| `settings.rs` | the `.nspawn` settings file and the unit hook drop-in |
| `attach.rs` | `attach-exec`, a machine's `ExecStart=`: the terminal, input or output an attached run hands over on its socket, or the stream into the journal namespace |
| `policy.rs` | restart policies and resource limits (`--restart`, `-m`, `--cpus`, `--pids-limit`), written into the hook drop-in |
| `health.rs` | healthchecks: the image's or the flags', the probe runner (`health-run`, a transient unit with `Requisite=` the machine's), its verdict under `/run/nspawn/health` |
| `tuning.rs` | the other per-machine flags (hostname, user, capabilities, tmpfs, devices, dns, ulimits, signals, sysctls, timezone), `--interface` and `--depends-on`: parsing, and the settings and unit lines they become |
| `getent.rs` | the getent stand-in bound into an app that runs as a user: a shell script, or a program for an image without a shell |
| `bridge.rs`, `hostnet.rs` | the nspawn0 bridge and user-defined networks, ports, firewalls; veth mode |
| `portproxy.rs` | `port-proxy`, the IPv6 side of published ports: a transient unit per machine that listens on the host's IPv6 addresses and relays TCP and UDP to the machine |
| `volume.rs`, `volmount.rs` | `-v` parsing; host-side mounts for mstack machines |
| `nsenter.rs`, `pty.rs` | exec through namespaces, terminal pumping |
| `systemd.rs` | typed D-Bus calls, job waiting |
| `unitname.rs` | `systemd-escape --path`: mount and device unit names |
| `journal.rs`, `api/events.rs` | structured journal entries for nspawn's own events; the journal read back as events |
| `cdi.rs` | CDI specs read strictly (no unknown field, the `cdiVersion` a spec's contents need, `/var/run/cdi` over `/etc/cdi`, a device two specs of one directory name refused), `--gpus` parsed (all, a count, `device=`, `driver=`, `capabilities=`) and mapped to a vendor's devices, the plan a start applies (nodes, binds, variables) and the hooks run before it in a private mount namespace (`nsenter::in_hook_root`) |
| `kvfile.rs`, `mountopt.rs` | `--env-file` and `--label-file` (KEY=VALUE files without quoting or expansion, shared with compose's raw `env_file`), `--mount` read as CSV and turned into `-v` and `--tmpfs` |
| `gotemplate.rs` | Go's text/template over JSON values, with the functions `json`, `join`, `split`, `lower`, `upper`, `title`, `pad` and `truncate` and the `table` layout: `ps --format`, `inspect -f` |
| `output.rs`, `columns.rs` | tables and sizes; the columns text takes where `--format` cuts it or lines it up (East Asian Width where it is cut; per grapheme cluster where a table lines it up, its characters' widths added up to at most two) |
| `api/secrets.rs` | secrets: systemd-creds around them, their files under the state directory, decrypted into a root-only tmpfs for a running machine and bind-mounted read-only |
| `api/generate.rs` | `generate`: the `network create` and `run -d` commands that make machines again from their records, in the order of their dependencies, quoted for a shell |
| `api/dependencies.rs` | `--depends-on`: the checks (machines of nspawn's, no cycle), the hook that waits for each condition (`await-dependencies`), the verdict on a dependency as its unit and healthcheck show it |
| `api/copy.rs` | cp: tar streams packed and unpacked relative to directory descriptors, paths resolved with openat2 inside the machine's root, tried again when a `..` crossed a rename or mount on the host (EAGAIN) |
| `api/disk.rs`, `commands/system.rs` | what is an image and what a machine (a record pulled or built and never started is an image only), sizes by kind for `system df`, `images prune`, and `system prune` on top of the removals and prunes of the service |
| `api/recreate.rs`, `api/autoupdate.rs` | a machine made anew from a newer image with its options, and put back from its rollback when the new one does not come up; `auto-update`: candidates by label, the registry asked once per reference, dependents restarted |
| `dockerapi/` | the read-only Docker Engine API (`api-serve`), served on a machine's own socket: routing and versions, containers and networks in docker's shapes, filters, events, logs, stats; the one write is a kill through the service |
| `build/dockerfile/`, `build/dispatch/` | ports of BuildKit's frontend: the parser, the shell lexer, instructions and flags; stages, build arguments, the steps with their history lines and the image's settings |
| `build/plan.rs` | the stages and steps of a build, worked out before a step runs |
| `build/ignore.rs`, `walk.rs`, `copy.rs`, `untar.rs` | the build context: `.dockerignore` patterns and their `!` exceptions, the walk of a context in lexical order without following symlinks, COPY and ADD with every path resolved inside its root, the context's tar and ADD's archives unpacked with every name resolved inside their destination |
| `build/layer.rs` | a step's layer: what its overlay upper directory changed over the layers below, as an OCI tar (deletions as `.wh.` whiteouts, file capabilities as PAX records), and its gzip blob |
| `build/root.rs`, `sandbox.rs` | the overlay mount of a step; the systemd-nspawn of a RUN and the mount points it leaves |
| `build/cache.rs`, `image.rs`, `exec.rs` | the build cache; the config and manifest; the steps run in order, the layers committed into the store and the images installed |
| `commands/build.rs`, `compose/engine/build.rs` | `build` of a Dockerfile on the client: the builder picked, the context packed as the user with its ignore file, secrets read; compose's `build` sections, image names and labels |
| `compose/` | `nspawn compose`, a client of the service like the rest of the command line: docker compose's command line read (`cobra.rs`: global flags before the command, `=false` on a bool flag, the last of a repeated flag winning), the files loaded (the YAML parser and constructor, interpolation, merging, include and extends, the model), `config`, the support table (`support/`) |
| `compose/engine/` | a project's services made machines through the bus: a service turned into the options of `CreateMachine`, up's convergence, the commands on a project's machines, found by docker compose's labels |
| `packaging/` | what the packages ship (the units, the bus files, the polkit actions and an example rule, `journald@nspawn.conf`, the SELinux module) and the Arch, Debian and Fedora packaging |

## On disk

```
/var/lib/nspawn/     0711, and 0700 for every directory below but machines/:
                     the layers hold the images' setuid programs and device
                     nodes, the records the machines' environment
  layers/            root-owned extracted layers (overlay backend); .diffids/
                     the digest each layer's archive was found to have, which
                     the image's config names (checked at extraction, and
                     from the blob for a layer extracted without a record),
                     with the format of the extraction; .empty, the lower
                     directory of an image without layers
  layers-foreign/    layers shifted into the foreign UID range (mstack)
  blobs/             compressed blobs, kept for push; .hold-* lists the blobs
                     of a pull, push or build in flight, which the collector
                     leaves alone; the service removes every hold left when
                     it starts, since only it makes them
  .holds             flock between the holds and the collector
  images/NAME.json   the record: reference, manifest_digest, index_digest
                     (the index a tag named at the pull), layers, created,
                     origin (pull, build or create), built, backend, mode,
                     pid1, started (its first start), run (the image
                     config's command, environment and the rest), network
                     (and network_name for a user-defined one), address,
                     extra_networks with their addresses, aliases,
                     fixed_addresses (the networks whose address was asked
                     for), no_network, network_container, ports, entrypoint
                     and cmd, env, volumes, labels, restart policy, limits,
                     healthcheck (the --health-* flags), tuning (the other
                     per-machine flags), remove_on_exit (--rm), signed_by and
                     signed_at when the pull verified a signature
  manifests/NAME.json raw manifest bytes (digest stays valid)
  machines/NAME/     overlay upper/work, host0.network (host1.. for the
                     other networks), hosts, resolv.conf, getent (the
                     stand-in bound into an app that runs as a user),
                     hostname (a booted machine's --hostname),
                     docker/containers/ID/hostname (bound at /etc/hostname
                     of a machine that takes the Docker API), units/ for the
                     volume wait unit, cdi/nodes/ (CDI device nodes the host
                     lacks), cdi/bundle/ (the config.json CDI hooks read the
                     root from), exit-on-next (a kill that counts as a stop),
                     last-signal (what kill or stop sent the current run);
                     0700 with a writable layer, 0711 otherwise (an mstack
                     machine binds its files from inside its user namespace)
  volumes/NAME/      named volumes
  secrets/NAME.cred  secrets encrypted with systemd-creds, NAME.json next to
                     each (created, size, labels); decrypted for a machine
                     into /run/nspawn/secrets/MACHINE while it runs
  networks/NAME.json user-defined networks: interface, subnet, internal,
                     labels
  starting/NAME      a start in progress, until the machine is registered:
                     nothing removes or replaces the image meanwhile; the
                     service removes every marker left when it starts
  claims/NAME        the host ports of a machine whose ExecStartPre hook
                     passed, until its ExecStartPost or ExecStopPost: the
                     next unit's check sees them before machined does; a
                     claim, like a start marker, holds for the boot that
                     made it
  starts-kept-since  unix seconds from which records note their first start:
                     a record created earlier without started counts as a
                     machine when flat, and when its writable layer is not
                     empty otherwise
  rollback/NAME/     auto-update's copy of a machine's previous record,
                     manifest and upper/work, until its new image is up;
                     its layers and blobs stay meanwhile
  builds/df-ID/      a Dockerfile build while it runs: the context, the
                     upper and work of each step, its network namespace's
                     address; removed at its end, and at the service's start
                     for builds a service that went away left
  builds/mkosi-ID/   an mkosi build's output, removed at its end unless
                     --keep-output keeps it, and at the service's start
                     while it has the marker an unfinished build leaves
  cache/build/       the build cache: entries/ (a step's layer by its key,
                     with the layers it was made on), bases/ (the base
                     images builds pulled), mounts/ (RUN --mount=type=cache);
                     its layers and blobs stay until builder prune. A key
                     comes from the step and the keys before it, not from
                     the layers they made, so when another build runs a
                     shared step again the entries after it stay valid
  cache/mkosi/       mkosi's own cache
  cache/mkosi-workspace/ID/  an mkosi build's workspace, removed at its end
                     and at the service's start
  .lock              flock serialising commands that change the store
/var/lib/machines/NAME        the root machined boots (mount point or dir)
/var/lib/machines/.nspawn-staging-NAME   a flat machine's layers applied
                     before they replace the machine of the name
/var/lib/machines/NAME.mstack mstack layout (layer@N links, rw/)
/etc/systemd/nspawn/NAME.nspawn   generated settings, regenerated on start;
                     0600, the -e variables are in it
/etc/systemd/system/var-lib-machines-NAME.mount   the overlay root:
                     lowerdir+= per layer, upperdir, workdir, metacopy=on
/etc/systemd/system/systemd-nspawn@NAME.service.d/
  nspawn-overlay.conf  RequiresMountsFor= (overlay)
  nspawn-hooks.conf  ExecStartPre= (await-dependencies with --depends-on,
                     then network prepare), ExecStartPost= and ExecStopPost=
                     calling nspawn, LogRateLimitIntervalSec=0; when set:
                     Restart= with its delays and StartLimitIntervalSec=0,
                     RestartForceExitStatus= (--rm), MemoryMax=,
                     MemorySwapMax=, CPUQuota=, TasksMax=, DeviceAllow=,
                     TimeoutStartSec=infinity, StandardOutput=null; ExecStart=
                     behind nspawn attach-exec for an app, and for a booted
                     machine with --log-driver local; Wants= and After= the
                     units of --depends-on, the device units of --interface,
                     systemd-journald@nspawn.socket and the Docker API socket
/etc/systemd/system/machines.target.wants/systemd-nspawn@NAME.service
                     --restart always or unless-stopped: started at boot
/run/systemd/system.control/systemd-nspawn@NAME.service.d/50-DeviceAllow.conf
                     DeviceAllow= of CDI nodes made again with other numbers,
                     for the running machine
/etc/nspawn/auth.json         registry credentials, 0600
/run/nspawn/service.lock      flock the service holds while it runs: a second
                     one exits at once, as it would take the first one's
                     builds, sockets and updates
/run/nspawn/attach/NAME.sock  where an attached run waits for its machine's
                     systemd-nspawn (0700 directory)
/run/nspawn/api/NAME.sock     a machine's Docker API, opened by
                     nspawn-api@NAME.socket (0600, 0700 directory) and bound
                     where the machine mounts docker's socket
/run/nspawn/health/NAME.json  the verdict and the last probes of a healthcheck
/run/nspawn/firewalld/NAME    the interfaces of a veth machine bound to
                     firewalld's trusted zone, for the release hook to unbind
/run/netns/nspawn-NAME        the network namespace of an app, or the one it
                     shares
```

Records are written through a temporary file and a rename. Garbage
collection refuses to run when a record cannot be read.

## Lifecycle

### Start and the unit hooks

`start` takes the store lock, applies the flags to the record, runs `prepare`
(checks, bridge, address, generated files, settings, hooks), releases the
lock, starts `systemd-nspawn@NAME.service` and waits for machined to register
the machine. The unit's hooks repeat the preparation (`network prepare`),
publish ports and attach mstack volumes once it runs (`network publish`) and
release everything however it ends (`network release`), so `machinectl start`,
enabled units and programs that exit on their own behave the same.

When the machine has a healthcheck, `network publish` also starts
`nspawn-health-NAME.service`, which runs `nspawn health-run NAME`. Its
`Requisite=` and `After=` on the machine's unit keep it from starting the
machine, and it ends once the machine is gone. It runs the test through
`nsenter` at its interval, at the start interval while a machine still starting
is in its start period. A success is healthy, `retries` failures in a row
are unhealthy, and failures during the start period of a machine never
healthy do not count. The verdict and the last
five probes go to `/run/nspawn/health/NAME.json`, which `ps` and `inspect`
read; each change is a `health_status` event. `network release` removes the
file.

### Dependencies

A machine with `--depends-on` has its order kept by systemd: its drop-in wants
each other machine's unit and is ordered after it (`Wants=`, so that stopping
one leaves it alone). Its first ExecStartPre, `nspawn await-dependencies NAME`,
comes before `network prepare` and its lock, and waits in turn for each
condition: a start that went through, a healthy verdict in
`/run/nspawn/health`, or a program that ended with 0. It fails the start when a
dependency fails, or writes a warning for an optional one.

A machine depended on as `completed` is only ordered before, not wanted, since
`Wants=` would run it again at every start. A run of it that ended well counts:
systemd's `Result=` for a run since the host booted (ExecMainStatus may be an
earlier run's when this one failed before its program started), the exit code
the journal kept otherwise. Without such a run the hook queues a start of it
and waits for that run.

Conditions that wait (`healthy`, `completed`) also set
`TimeoutStartSec=infinity`, and `start` waits for the job without its time
limit when one of them is anywhere down the chain, since systemd holds each
start in its queue until the one before it is through; when the job fails,
`start` gives the dependency's failure as the reason. A machine on the network
of one it depends on skips attaching that network in the pass `start` makes
ahead of the unit, and its hook attaches it once systemd has started the owner.
The checks at `create` and `start` refuse a machine that is not nspawn's, the
machine itself and a cycle, `--network container:` counting as a dependency;
`rm` removes a machine before the ones it depends on and keeps a dependency
whose dependents stay.

### Stop, kill and restart policies

`stop` sends the image's stop signal to the program of an app machine and
SIGKILLs the cgroup after `--timeout`; a booted machine gets SIGRTMIN+4
repeatedly until it is gone. With a restart policy it also queues a stop job
for the unit, which is what keeps systemd from restarting it. For a booted
machine the stop job goes in before the first signal; with `--force`, right
after the SIGKILL (machined refuses to kill a machine it is already closing);
for an app, after the stop signal: at once with `--no-wait`, after the timeout
and its SIGKILL otherwise, so that the stub init does not add its SIGTERM and
SIGHUP while the program handles its own signal. Should the program end in
between, the stop job also cancels the restart systemd has scheduled. A machine
machined no longer lists gets the stop job too, which ends a pending restart
and waits for the release hook of its last run.

`kill` with SIGKILL is `stop --force`. Any other signal goes to an app's
program (the stub init's child, or the leader when the program is PID 1, once
the leader no longer bears systemd-nspawn's name, which it keeps until it
executes the program) or to a booted machine's init. The signal counts as a
stop when it is any signal to an app without a stop signal of its own, an
app's own stop signal, or a halt or poweroff request to an init; after any
other the policy applies, should the machine end. A kill that counts
as a stop, of a machine with a restart policy, leaves `exit-on-next` in the
machine's directory, and the release hook, finding it, queues the stop job
while the unit is still winding down. `prepare` removes a mark an earlier run
left.

The policy and the limits go into the hooks drop-in, since the settings file
has no keys for them; `always` and `unless-stopped` enable the unit together
with `machines.target`, which pulls it in at boot, `stop` disables an
`unless-stopped` one and removing a machine disables it (`machines.target`
stays enabled). `update` rewrites the record and the drop-in and reloads:
systemd applies a running unit's changed limits to its cgroup at a
daemon-reload (255 to 262 alike), and reads `Restart=` again for its next
ending. `update` does not call SetUnitProperties: with `runtime` it leaves
copies under `/run/systemd/system.control` that would win over the drop-in
until the next boot. The one runtime property nspawn sets is `DeviceAllow=` for
CDI nodes the hook makes again with other numbers, which the drop-in grants
only from the next start; `rm` removes its `50-DeviceAllow.conf`.

### stats and exec

`stats` reads the unit's cgroup (its `ControlGroup`, which holds
systemd-nspawn and the whole machine: `cpu.stat`, `memory.current` and
`memory.stat`, `memory.max`, `pids.current`, `io.stat`) and the machine's
interfaces through `/proc/LEADER/net/dev`; the service returns counters with a
monotonic time and the client makes rates of two samples.

`exec` joins the leader's namespaces (user first, mount last), joins its
cgroup, becomes the machine's root and then the requested user, so
capabilities are dropped. It first waits until the leader is past
systemd-nspawn's setup, since systemd-nspawn reports the machine started once
its child has mounted the file systems and before it confines itself (the
leader keeps the name `systemd-nspawn` until the stub init renames itself or the
machine's init runs). It then takes the leader's bounding set and resource
limits; the set loses `CAP_SYS_BOOT` when the machine shares the host's user
namespace and is not privileged, since the command enters without the
machine's seccomp filter and its kexec rule. The limits come last, once only
the three streams are left, so that a low open-files limit does not refuse the
passwd file or SELinux's exec attribute on the way.

## Machines and apps

### Booted or app

An image with an init system is booted (`Boot=yes`) when its command (the
entrypoint and the cmd) is empty or starts with that init: a path where
systemd-nspawn looks for it (`/sbin/init`, `/usr/lib/systemd/systemd`...),
`systemd` at any path, or the bare `init` or `systemd`. So an `/init` of the
image's own (s6-overlay's) is not mistaken for it in a base that also has
busybox's `/sbin/init`. Anything else is an app: its command runs under the
stub init (`ProcessTwo=yes`), with the OCI config's environment, working
directory, user and stop signal. An argument with a newline, which the settings
file cannot carry, goes in encoded: `Parameters=` then runs the image's `/bin/sh`
with a fixed script that turns each argument back with printf's `%b` and execs
the program with them, which keeps the shell's PID. An image without `/bin/sh`
cannot take such an argument.

An image that carries s6-overlay (found at install, across the layers, and kept
as `pid1` in the record) has its program run as PID 1 instead (`Boot=no` alone,
and `KillSignal=` its stop signal, which systemd-nspawn hands to PID 1 when the
unit stops), since s6's `/init` refuses anything else; `--init=false` and
`--init` choose either way for any app. The program is then the leader itself
for `kill` and `stop`, and a PID 1 killed by a signal ends systemd-nspawn with 1
rather than the stub's 255.

### Users

systemd-nspawn resolves the user with `getent passwd` and `getent initgroups`
run inside the machine, which busybox lacks, musl's getent (alpine) cannot
answer and glibc's refuses for a uid the passwd does not list. An app that runs
as a user gets `getent.rs`'s stand-in, a shell script bound over
`/usr/bin/getent` that answers those two from the image's passwd and group files
(a uid they do not list with gid 0 and no home, a group asked for as the
primary and only one) and hands anything else to the image's own getent, bound
at `/run/nspawn/getent`. An image without `/bin/sh` gets a program instead: a
few hundred bytes of x86_64 or aarch64 code without a libc, into which nspawn
writes the two answers it reads from the same files ahead of the start, and
which hands anything else on the same way. A root without `/usr` (an image
built from scratch) gets an empty one: systemd-nspawn runs a program only in a
root that has it.

### Apps on a bridge

Apps on a bridge, or in another machine's network namespace, run with
`PrivateUsers=no`, since a user namespace cannot join a network namespace made
on the host. With no user namespace, systemd-nspawn's capability set would leave
`CAP_SYS_ADMIN` to root in the app, and its seccomp filter allows `@mount`, so
root could remount `/proc/sys` writable and reach the host through
`kernel.core_pattern`. Such an app gets `tuning::DEFAULT_CAPABILITIES`
instead, the least that lets images run as written plus `CAP_SYS_BOOT`:
`Tuning::exec_lines` writes the difference as `DropCapability=` (keeping
`CAP_NET_ADMIN` with `--interface`) and adds
`SystemCallFilter=~kexec_load kexec_file_load`, since in its own PID namespace
reboot(2) only signals the machine's init while kexec_load would load a kernel
into the host. An app image is assembled with overlay even when mstack is asked
for.

The namespace is named with `NamespacePath=` in the settings file on systemd
259 or newer; before that the key does not exist, and the unit's `ExecStart=`
gets `--network-namespace-path=` instead (minus the options that conflict).
`Private=yes` (`--private-network`) goes with it: a namespace path alone does
not count as a private network to systemd-nspawn, which then mounts the
machine's `/sys` before joining the namespace, so that `/sys/class/net` lists
the host's interfaces; with it, sysfs is mounted inside the namespace.

### ExecStart and the output

An app's `ExecStart=` is rewritten in the hooks drop-in on every systemd
version: the argv systemd has loaded runs behind `nspawn attach-exec NAME --`
(`attach.rs`), which execs it as it is unless an attached `run` waits for the
machine on `/run/nspawn/attach/NAME.sock`. Then it receives the run's terminal
or input there and adds `--console=interactive` or `--console=pipe`, the
console mode being something systemd-nspawn only takes on its command line.

With `--log-driver local`, the default, the drop-in rewrites a booted machine's
`ExecStart=` the same way, adds `--journal-namespace nspawn` to attach-exec, and
makes the unit want `systemd-journald@nspawn.socket`. attach-exec opens a stream
to `/run/systemd/journal.nspawn/stdout` with journald's stream header, as
`sd_journal_stream_fd_with_namespace()` does, and makes it the stdout of
systemd-nspawn, whose relay carries the machine's output; journald files the
lines under the unit from the stream's cgroup. Its stderr stays the unit's, in
the system's journal, with the hooks' messages.

`logs` reads the namespace and the system's journal interleaved
(`--namespace=+nspawn`), from the machine's creation on, since the unit's lines
include those of removed machines of the name. A `--since` before the creation,
or none, becomes the creation, and an `--until` before it shows nothing
(`--lines=0`); journalctl itself compares the times, so any syntax it takes
works. An attached run follows the namespace.

A machine with `--log-driver none` has `StandardOutput=null` (and
`StandardError=journal`, for systemd-nspawn's and the hooks' messages) in the
drop-in; an attached run of it hands a pipe over instead, which becomes the
program's stdout and stderr. Should the caller go, a transient unit
`nspawn-drain-NAME.service` reads and drops what comes until the machine ends
(the service itself when systemd refuses that unit), so that the program never
blocks on it.

The exec keeps the PID, so `Type=notify`, signals and the exit status are
systemd-nspawn's; and since `nspawn_t` executes `bin_t` programs as
`unconfined_service_t`, systemd-nspawn runs in the domain it had.

### Ownership

A booted overlay machine, and an app off the bridge, runs in the user namespace
the template's `-U` picks. No released kernel lets an overlayfs mount be
idmapped, so systemd-nspawn shifts the tree with a recursive chown at the first
start. The mount carries `metacopy=on` so that this chown copies inodes rather
than file contents into the upper directory and the layers stay shared.
Attributes in overlayfs's own namespace (`trusted.overlay.*`, `user.overlay.*`)
are dropped from every layer on extraction, and so is every other `trusted.*`
attribute, so an image cannot redirect a file or its data.

A layer is applied entry by entry: each lands where its name resolves inside
the tree, a symlink's absolute target starting again at the tree's root and `..`
never going above it, and whatever is done to the entry is done there. A name from the root down (`/ko-app/crane`, as ko writes them) is one
inside the layer. A layer's archive is read to its end, and its digest has to be
the one the image's config gives it. A layer found without a record of its format is
kept as it is; `images rm` of the images that use it and a pull again extract it
anew.

## The bus service

`nspawn daemon` serves `org.nspawn` (`docs/DBUS.md`). The bus starts it through
`nspawn.service` when a client calls, and it exits after a minute without a
call, a job or a command running. Each method asks polkit about its caller, as
machined does: `org.nspawn.inspect` for the ones that only look,
`org.nspawn.manage` for the rest, root without asking; `org.nspawn.Names` asks
nothing. Every method is a thin conversion around an `api` function: options
come as `a{sv}` and are read by name and type (an unknown key is an error), and
results go back as dictionaries with the command line's spellings.

Pull, push, build, create, cp, auto-update, network connect and disconnect, and
every removal and prune (images, machines, volumes, networks, secrets, the
build cache) run as jobs: the method returns `/org/nspawn/job/N` at once, the
job's report events become `JobOutput` signals and the object's `Output`, the
progress of its downloads and uploads `JobProgress` signals, and `JobRemoved`
says how it ended.

`Hub::resolve` picks an index's entry itself. The host's platform gives an order of the
architectures and variants it runs, best first (`platform_vector`), and the entry of the
best of them is taken, the first of equals. Without one, the entries that name no
platform are tried in the index's order: the first whose config, read whole and checked
against its digest, gives a platform the host runs. Nothing else of the image has come
down by then.

A pull first checks the image's signatures where its registry has a policy (the
hub's is built in: the project's key or the build workflow's keyless identity,
one of them required). The Sigstore bundles among the referrers of the
manifest, or of the index the reference named, are verified offline with the
log entry and the timestamp they carry, against the trusted root
sigstore-verify embeds or the policy's `trusted_root`; the pull ends there when
none verifies, unless the policy says `required = false`, which only notes it.
It then fetches the blobs the store lacks three at a time, each verified
against its digest as it streams and written next to its final name until
then; a download that ends any other way takes its part file with it.

machined's `MachineNew` and `MachineRemoved` are relayed as `MachineStarted`
and `MachineStopped` for the machines nspawn installed.

`Events` keeps no history of its own: systemd logs every start, end, restart
and stop of a unit with a message ID of its own, and nspawn logs what it does
(pulls, removals, kills) the same way, as structured entries under message ID
`b0b60147942247cab22cc49510006a0b` with `NSPAWN_TYPE`, `NSPAWN_ACTION`,
`NSPAWN_NAME` and `NSPAWN_ATTR_*` fields. The service runs `journalctl
--output=json` with field matches only (PID 1's entries, nspawn's own, both
from uid 0, fields journald sets itself), maps them to events, adds the image
and labels of the machine's record and writes JSON lines into the client's
pipe. A clean exit is not logged as such, only as the unit's success, so a
success without an exit logged in the same invocation is `die` with code 0.

## The command line

The command line is a client of that service, so one code path does the work:
`commands/` turns arguments into calls and prints what comes back, follows jobs
line by line (with a bar for each transfer under way when standard error is a
terminal, as the progress signals name the blob), attaches the terminal to the
descriptors `Exec`, `Shell`, `RunMachine`, `Logs` and `Events` hand over, and
packs or unpacks the tar streams of `CopyTo` and `CopyFrom`. The registry and
CA certificate the command line was given travel as options of the calls that
reach a registry, so `--registry`, `--ca-cert` and the environment keep their
meaning.

`run` makes several calls: `PullImage` (then `CreateMachine` from it with
`--rm`, so the image stays) or `CreateMachine` from a local image, then
`StartMachine` for `-d` and `RunMachine` otherwise. `RunMachine`
subscribes to systemd's unit signals and watches the unit's
`PropertiesChanged` from before the start: the run ends with the
`ExecMainCode` and `ExecMainStatus` that signal carries, since a later read
could miss them once a restart began; a reboot from inside (133) that systemd
restarts is no end.

The output is the journal's, from a cursor taken before the start
(`_SYSTEMD_UNIT`, `_TRANSPORT=stdout`): journald's `nspawn` namespace with
`--log-driver local`, where only the machine writes, or the system's journal
with `--log-driver journal`, leaving out the hooks' lines (`_COMM` is
`nspawn`). It is copied line by line into the client's pipe until it has been
quiet for a moment after the end. The exit code is the program's;
systemd-nspawn exits 255 whatever signal killed an app's program and 1 after a
SIGKILL of the whole machine on systemd 259 and newer, so the signal nspawn
itself sent (Ctrl-C through the process object, `kill` and `stop` through
`machines/NAME/last-signal`) makes it 128 plus that signal. With `-t` the
service keeps a copy of the terminal's master: should the client go away, it
drains the terminal and stops the machine the normal way.

`--rm` sets `remove_on_exit` in the record, and the hooks drop-in then resets
`RestartForceExitStatus=`. The release hook starts a transient unit
`nspawn-rm-NAME-XXXXXXXX.service` (the first eight characters of the run's
invocation ID) that runs `nspawn remove-after-exit`, which waits for the
machine's unit to be down after that invocation and removes the machine. Where
that unit cannot start (at shutdown) the service removes such machines when it
starts, and before every `RunMachine`.

Only the service itself, `daemon --install`, the unit hooks
(`await-dependencies`, `network prepare`, `publish`, `release`) and what
systemd runs for a machine (`attach-exec`, `health-run`, `remove-after-exit`,
`api-serve`) run the library in their own process; none of them may depend on
the service while a machine starts.

## Networking

The default bridge (`nspawn0`, `10.99.0.0/24`) and those of user-defined
networks (`nsbr-NAME`, the `--subnet` given or the next free /24 of
`network_pool`) are created with `ip`. The nftables table `ip nspawn` holds the
DNAT maps for published ports (`ports` for those on every IPv4 address of the host,
`addr_ports` for those on one), and for every network masquerading (not for
internal ones), hairpin masquerading and a guard so that `route_localnet`
cannot expose the host's loopback services. Its forward chain keeps the
networks apart: an internal network forwards nothing, connections to published
ports cross (DNAT), anything else from one bridge to another is dropped, which
holds whatever firewalld or iptables accept. The table is always written for
all networks at once, so bringing one bridge up never drops another's rules.

The machines are IPv4-only and nftables cannot translate IPv6 into IPv4, so the IPv6
side of a published port is a proxy in user space. A port given no IP has both sides,
`0.0.0.0` the IPv4 one alone, `::` or an IPv6 address the IPv6 one alone
(`PortMap::ipv4_side`, `ipv6_side`). `network publish` starts
`nspawn-proxy-NAME.service`, a transient unit with `Requisite=` and `After=` the
machine's (`portproxy::start`, the health runner's pattern), `Type=notify`, its
capabilities bounded to `CAP_NET_BIND_SERVICE`. It runs `nspawn port-proxy NAME`,
which reads the record, binds each IPv6 listener for IPv6 alone (`IPV6_V6ONLY`, so the
IPv4 port stays the DNAT's), waits up to 30 s for an address still tentative at boot,
says `READY=1`, and relays: TCP with a copy each way per connection, UDP with a
connected socket per client towards the machine and the replies sent from the address
the client wrote to (`IPV6_PKTINFO`), a flow forgotten after 90 s without traffic. It
ends with the machine's run; `release` queues its stop, `prepare` stops one left over
before the port checks, which test-bind the IPv6 side too, and `network disconnect` of
the primary network starts it again on the new address. `start` says so when the proxy
is not up a few seconds after the machine (or a booted machine's init) is.

A machine's record says its network kind (`network`) and, for a user-defined
network, its name in `network_name`; the other bridge networks it joins are
`extra_networks`, each with the machine's address there, its extra names on
each network are `aliases`, and `--network none` is `no_network`
(`Private=yes`). The primary network is the first `--network`; the default
route goes through the first network that is not internal
(`bridge::gateway_index`).

- Booted machines get a fixed address on each network through `.network` files
  mounted at `/run/systemd/network/10-host0.network`, `11-host1.network`..,
  with the DNS servers and search domains on the one of the default route.
  host0 reaches the primary bridge through `Bridge=` and the others come
  through `VirtualEthernetExtra=`, whose host ends (`vb1-NAME`..) the publish
  hook puts on their bridges.
- App machines get a namespace built beforehand (`ip netns`, one veth per
  network, addresses, route, `net.ipv4.ping_group_range=0 2147483647` and
  `net.ipv4.ip_unprivileged_port_start=0`, then the `--sysctl` values), named
  by `NamespacePath=`.
- A machine with `--network container:NAME` (`network_container` in its
  record, the other network fields empty) gets no address of its own: nspawn
  binds its namespace name to the network namespace of NAME's leader itself
  (the mount `ip netns attach` would make, which under SELinux `ip` may not
  reach for a leader in a user namespace), so the same `NamespacePath=`
  applies; it binds NAME's generated hosts and resolv.conf, and `release` drops
  the name while the namespace stays NAME's.
- Each veth nspawn makes (an app's, and those of a live `network connect`)
  takes its inside MAC from its address (`02:42:` and the four octets), so a
  restart keeps it and the host's neighbour cache stays right.
- Under managed user namespaces (mstack) systemd-nsresourced creates a booted
  machine's veths and systemd-nspawn does not apply `Bridge=`, so the publish
  hook puts each host end on its bridge (`bridge::adopt_managed_veth`, which
  finds the peer of each `hostN` through the machine's sysfs).

`/etc/hosts` lists, for each network the machine joins, every member with its
address there, its name and its aliases on that network (an alias several
members share goes to the one whose name sorts first), and
`host.nspawn.internal`, the gateway of the machine's gateway network. With
firewalld the bridges are bound to the trusted zone, and so is a veth machine's
interface while it runs; with docker or ufw, accept rules go into DOCKER-USER
or FORWARD, before either has set FORWARD's policy when it is installed and not
up yet. `network rm` undoes all of it for its bridge. The bridge is IPv4
only: it gets `addrgenmode none` and no `fe80::` address, and host0 gets
`LinkLocalAddressing=no` in its `.network` file or `addrgenmode none` in an
app's namespace, so machined never hands out a link-local address under a
machine's name.

`--network veth` keeps systemd-nspawn's own veth configured by systemd-networkd
on the host; `--network host` shares the host's network.

### Interfaces

`--interface` (the record's `tuning.interfaces`) moves host interfaces into the
machine. Where systemd-nspawn makes the namespace (booted machines, any
`--network none` machine) they are `Interface=` lines of the settings file,
next to `Bridge=` or `VirtualEthernet=yes` (any `[Network]` key replaces the
template's command line, so the veth is asked for again), and systemd-nspawn
moves them in and back at exit.

An app on the bridge joins a namespace nspawn made (`NamespacePath=` takes no
`Interface=` beside it), so the ExecStartPre hook moves them in itself as the
last thing it does (`bridge::move_interface`: `ip link set` for ethernet, `iw
phy set netns` for wireless, whose phy moves whole). `release`, `stop`, the pass
`start` makes ahead of the unit, and `images rm` give them back
(`return_interface`, run inside the namespace with `ip -n` or `ip netns exec`,
the only place a moved phy is reachable from) before the namespace is deleted.
The pass `start` makes ahead of the unit checks but does not move, so that the
device units the unit wants (`Wants=` and `After=`
`sys-subsystem-net-devices-*.device`, for a USB adapter at boot) are still
active when its start job is queued.

The checks (`machines::check_interfaces`) run in both passes before the record
is written: another running machine naming the interface, the interface on the
host (with a 3 s grace for one a dead namespace has not given back yet),
nspawn's own bridges and veth ends, a port of a bridge or bond, the default
route (a warning), `iw` for a wireless one through the hooks and systemd 256
for one through `Interface=`.

## Volumes

`-v SOURCE:TARGET[:ro]` becomes a `Bind=` or `BindReadOnly=` line, idmapped
when the machine runs with private users. mstack machines cannot idmap binds,
so their volumes are attached from the host by the publish hook (`open_tree`,
`mount_setattr` with the machine's user namespace, `move_mount`). Every booted
machine with volumes gets `nspawn-volumes.service`, which holds
`local-fs.target` until they are all mounted. A host path must exist.

Named volumes are plain directories under `volumes/`. One is made at its first
use and filled from what the image has at its target, owners included; one made
ahead and still empty is filled at a machine's first start, unless another
running machine has it. `api/volumes.rs` lists them with the records that name
them and removes only the ones no record names, reading the records strictly so
that one it cannot read never makes a volume look unused.
