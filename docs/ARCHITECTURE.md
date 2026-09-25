# Architecture

nspawn is a single binary that talks to systemd (`org.freedesktop.systemd1`)
and systemd-machined (`org.freedesktop.machine1`) over D-Bus and to OCI
registries over HTTPS. The same binary serves `org.nspawn` on the system bus,
which the command line is a client of; the bus starts that service on demand
and it exits again when idle, so nothing runs between commands. State lives in
files, and the machine units call nspawn back through drop-in hooks.

## Modules

| Module | Role |
|---|---|
| `api/` | the library: typed operations on images, machines and the network; nothing here prints, progress goes through a `Report` and results come back as values |
| `cli.rs`, `commands/` | clap definitions and the terminal side: argument conversion into calls on the service, tables, prompts, the commands that own the terminal (exec, shell, logs) and the host's end of cp |
| `client/` | the proxies for `org.nspawn`, how its errors read, how a job is followed |
| `daemon/` | the D-Bus service `org.nspawn`: the Manager interface, jobs, processes, dictionaries, the files that make the bus start it |
| `packaging/` | the unit, bus and SELinux policy files the packages ship, and the Fedora spec |
| `config.rs` | `/etc/nspawn/nspawn.toml`, environment and flags |
| `reference.rs` | image references, local names, machine name rules |
| `hub.rs`, `auth.rs`, `search.rs` | registry client, credentials, search |
| `layout.rs`, `oci.rs` | OCI image layout reader, image config, boot/app detection |
| `store.rs` | layers, blobs, records, manifests, gc, the store lock |
| `install.rs`, `backend.rs` | turning blobs into a machine (overlay, flat, mstack) |
| `settings.rs` | the `.nspawn` settings file and the unit hook drop-in |
| `policy.rs` | restart policies and resource limits (`--restart`, `-m`, `--cpus`, `--pids-limit`), written into the hook drop-in |
| `health.rs` | healthchecks: the image's or the flags', the probe runner (`health-run`, a transient unit bound to the machine's), its verdict under `/run/nspawn/health` |
| `api/secrets.rs` | secrets: systemd-creds around them, their files under the state directory, decrypted into a root-only tmpfs for a running machine and bind-mounted read-only |
| `tuning.rs` | docker's other per-container flags (hostname, user, capabilities, tmpfs, devices, dns, ulimits, signals, sysctls): parsing, and the settings and unit lines they become |
| `getent.rs` | the getent stand-in bound into an app that runs as a user |
| `bridge.rs`, `hostnet.rs` | the nspawn0 bridge, ports, firewalls; veth mode |
| `volume.rs`, `volmount.rs` | `-v` parsing; host-side mounts for mstack machines |
| `nsenter.rs`, `pty.rs` | exec through namespaces, terminal pumping |
| `api/copy.rs` | cp: tar streams packed and unpacked relative to directory descriptors, paths resolved with openat2 inside the machine's root |
| `systemd.rs` | typed D-Bus calls, job waiting |
| `journal.rs`, `api/events.rs` | structured journal entries for nspawn's own events; the journal read back as events |

## On disk

```
/var/lib/nspawn/     0711, and 0700 for everything below but machines/: the
                     layers hold the images' setuid programs and device
                     nodes, the records the machines' environment
  layers/            root-owned extracted layers (overlay backend)
  layers-foreign/    layers shifted into the foreign UID range (mstack)
  blobs/             compressed blobs, kept for push; .hold-* lists the blobs
                     of a pull in flight, which the collector leaves alone
  images/NAME.json   the record: reference, backend, mode, network (and
                     network_name for a user-defined one), address,
                     extra_networks with their addresses, aliases,
                     no_network, ports, entrypoint/cmd, env, volumes,
                     labels, restart policy, limits
  manifests/NAME.json raw manifest bytes (digest stays valid)
  machines/NAME/     overlay upper/work, host0.network (host1.. for the
                     other networks), hosts, resolv.conf, getent (the
                     stand-in bound into an app that runs as a user),
                     hostname (a booted machine's --hostname),
                     units/ for the volume wait unit, exit-on-next when
                     `kill` sent the stop signal, last-signal (what `kill`
                     or `stop` sent the current run); 0700 with a writable
                     layer, 0711 otherwise (an mstack machine binds its
                     files from inside its user namespace)
  volumes/NAME/      named volumes
  secrets/NAME.cred  secrets encrypted with systemd-creds, NAME.json next to
                     each (created, size, labels); decrypted for a machine into
                     /run/nspawn/secrets/MACHINE while it runs
  networks/NAME.json user-defined networks: interface, subnet, internal,
                     labels
  starting/NAME      a start in progress, until the machine is registered:
                     nothing removes or replaces the image meanwhile
  .lock              flock serialising commands that change the store
/var/lib/machines/NAME        the root machined boots (mount point or dir)
/var/lib/machines/NAME.mstack mstack layout (layer@N links, rw/)
/etc/systemd/nspawn/NAME.nspawn          generated settings, regenerated on start;
                                         0600, the -e variables are in it
/etc/systemd/system/systemd-nspawn@NAME.service.d/
  nspawn-overlay.conf   RequiresMountsFor= (overlay)
  nspawn-hooks.conf     ExecStartPre/Post, ExecStopPost calling nspawn,
                        LogRateLimitIntervalSec=0, Restart= and
                        MemoryMax=/MemorySwapMax=/CPUQuota=/TasksMax= when set,
                        for an app ExecStart= behind `nspawn attach-exec`
/etc/systemd/system/machines.target.wants/systemd-nspawn@NAME.service
                        --restart always or unless-stopped: started at boot
/etc/nspawn/auth.json  registry credentials, 0600
/run/nspawn/attach/NAME.sock  where an attached run waits for its machine's
                        systemd-nspawn (0700 directory)
```

Records are written through a temporary file and a rename. Garbage
collection refuses to run when a record cannot be read.

## Lifecycle

`start` takes the store lock, applies the flags to the record, runs `prepare`
(checks, bridge, address, generated files, settings, hooks), releases the
lock, starts `systemd-nspawn@NAME.service` and waits for machined to register
the machine. The unit's hooks repeat the preparation (`network prepare`),
publish ports and attach mstack volumes once it runs (`network publish`) and
release everything however it ends (`network release`), so `machinectl start`,
enabled units and programs that exit on their own behave the same. When the
machine has a healthcheck, `network publish` also starts
`nspawn-health-NAME.service`, a transient unit with `BindsTo=` the machine's
that runs `nspawn health-run NAME`: the test through `nsenter` at its interval
(the start interval during the start period), docker's rule for the verdict
(a success is healthy, `retries` failures in a row are unhealthy, failures
during the start period of a machine never healthy do not count), the verdict
and the last five probes in `/run/nspawn/health/NAME.json`, which `ps` and
`inspect` read, and a `health_status` event on every change; `network release`
removes the file, the unit goes with the machine's.

`stop` sends the image's stop signal to the program of an app machine and
SIGKILLs the cgroup after `--timeout`; a booted machine gets SIGRTMIN+4
repeatedly until it is gone. With a restart policy it also queues a stop job
for the unit, which is what keeps systemd from restarting it: before the first
signal for booted machines, right after the SIGKILL of `--force` (machined
refuses to kill a machine it is already closing), and for an app right after
its image's stop signal (after its time to act on it too, when `stop` waits),
so that the stub init does not add its SIGTERM and SIGHUP while the program
handles its own signal. Should the program end in between, the stop job also cancels
the restart systemd has scheduled. A machine machined no
longer lists gets the stop job too, which ends a pending restart and waits for
the release hook of its last run. `kill` with SIGKILL is `stop --force`; any
other signal goes to the program of an app or the init of a booted machine and
leaves the policy to decide should the machine end, except the machine's own
stop signal, which docker counts as a stop: `kill` then leaves `exit-on-next`
in the machine's directory, and the release hook, finding it, queues the stop
job while the unit is still winding down. `prepare` removes a mark an earlier
run left. The policy and the limits go into the hooks
drop-in, since the settings file has no keys for them; `always` and
`unless-stopped` enable the unit the way `machinectl enable` does, `stop`
disables an `unless-stopped` one and removing a machine disables it. `update`
rewrites the record and the drop-in and reloads: systemd applies a running
unit's changed limits to its cgroup at a daemon-reload (255 to 261 alike), and
reads Restart= again for its next ending. SetUnitProperties is not used: with
`runtime` it leaves copies under `/run/systemd/system.control` that would win
over the drop-in until the next boot. `stats` reads the same cgroup (the unit's `ControlGroup`, which holds
systemd-nspawn and the whole machine: `cpu.stat`, `memory.current` and
`memory.stat`, `memory.max`, `pids.current`, `io.stat`) and the machine's
interfaces through `/proc/LEADER/net/dev`; the service returns counters with a
monotonic time and the client makes rates of two samples. `exec` joins the leader's namespaces (user
first, mount last), joins its cgroup, becomes the machine's root and then the
requested user, so capabilities are dropped.

## Machines and apps

An image with an init system whose entrypoint is that init is booted
(`Boot=yes`). Anything else is an app: its command runs under the stub init
(`ProcessTwo=yes`), with the OCI config's environment, working directory,
user and stop signal. systemd-nspawn resolves the user with `getent passwd`
and `getent initgroups` run inside the machine, which busybox lacks, musl's
getent (alpine) cannot answer and glibc's refuses for a uid the passwd does
not list; an app that runs as a user gets `getent.rs`'s stand-in, a shell
script bound over `/usr/bin/getent` that answers those two from the image's
passwd and group files as docker reads them and hands anything else to the
image's own getent, bound at `/run/nspawn/getent`. Apps on the bridge run with
`PrivateUsers=no`, since a user namespace cannot join the network namespace
prepared on the host; app images are assembled with overlay even where mstack
exists. The namespace is named with `NamespacePath=` in the settings file on
systemd 259 or newer; before that the key does not exist, and the unit's
`ExecStart=` gets `--network-namespace-path=` instead (minus the options that
conflict). An app's `ExecStart=` is rewritten in the hooks drop-in on every
systemd version anyway: the argv systemd has loaded runs behind `nspawn
attach-exec NAME --` (`src/attach.rs`), which execs it as it is unless an
attached `run` waits for the machine on `/run/nspawn/attach/NAME.sock`; then it
receives the run's terminal or input there and adds `--console=interactive` or
`--console=pipe`, the console mode being something systemd-nspawn only takes
on its command line. The exec keeps the PID, so `Type=notify`, signals and the
exit status are systemd-nspawn's; and since `nspawn_t` executes `bin_t`
programs as `unconfined_service_t`, systemd-nspawn runs in the domain it had.

Overlay machines run under a user namespace like the others, and no released
kernel lets an overlayfs mount be idmapped, so nspawn shifts the tree with a
recursive chown at the first start. The mount carries `metacopy=on` so that
this chown copies inodes rather than file contents into the upper directory
and the layers stay shared. Attributes in overlayfs's own namespace
(`trusted.overlay.*`, `user.overlay.*`) are dropped from every layer on
extraction, so an image cannot redirect a file or its data.

## The bus service

`nspawn daemon` serves `org.nspawn` (`docs/DBUS.md`). The bus starts it
through `nspawn.service` when a client calls, and it exits after a minute
without a call, a job or a command running. Every method is a thin
conversion around an `api` function: options come as `a{sv}` and are read by
name and type (an unknown key is an error), results go back as dictionaries
with the command line's spellings. Pull, push, build, create, the removals
(images, machines, volumes) and cp run as jobs:
the method returns `/org/nspawn/job/N` at once, the job's report events
become `JobOutput` signals and the object's `Output`, the progress of its
downloads and uploads `JobProgress` signals, and `JobRemoved` says how it
ended. machined's `MachineNew` and `MachineRemoved` are relayed as
`MachineStarted` and `MachineStopped` for the machines nspawn installed.

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

The command line is a client of that service, so one code path does the
work: `commands/` converts arguments into calls and prints what comes back,
follows jobs line by line (with a bar for each transfer when standard error is
a terminal), attaches the terminal to the descriptors `Exec`,
`Shell` and `Logs` hand over, and packs or unpacks the tar streams of `CopyTo`
and `CopyFrom`. `run` is the one command made of several calls, as docker's
is: `PullImage` or `CreateMachine`, then `StartMachine` for `-d`, and
`RunMachine` otherwise. `RunMachine` subscribes to systemd's unit signals and
watches the unit's `PropertiesChanged` from before the start: the main
process's `ExecMainCode` and `ExecMainStatus` as the signal carries them are
the end (a later read could miss them once a restart began; a reboot from
inside, 133, is not an end). The output is the journal's, from a cursor taken
before the start (`_SYSTEMD_UNIT`, `_TRANSPORT=stdout`, leaving out what the
hooks write, whose `_COMM` is `nspawn`), copied line by line into the client's
pipe until it has been quiet for a moment after the end. The exit code is the
program's; systemd-nspawn exits 255 whatever signal killed an app's program and
1 after a SIGKILL of the whole machine on systemd 259 and newer, so the signal
nspawn itself sent (Ctrl-C through the process object, `kill` and `stop`
through `machines/NAME/last-signal`) makes it 128 plus that signal. With `-t`
the service keeps a copy of the terminal's master: should the client go away,
it drains the terminal and stops the machine the normal way. `--rm` sets
`remove_on_exit` in the record, and the hooks drop-in then resets
`RestartForceExitStatus=`; the release hook starts a transient unit
`nspawn-rm-NAME-<invocation>` that runs `nspawn remove-after-exit`, which waits
for the machine's unit to be down after that invocation and removes the
machine. Where that unit cannot start (at shutdown) the service removes such
machines when it starts, and before every `RunMachine`. The registry and CA certificate the
command line was given travel as options of each call, so `--registry`,
`--ca-cert` and the environment keep their meaning. Only the service itself,
`daemon --install` and the unit hooks (`network prepare`, `publish`,
`release`, which must not depend on the service while a machine starts) run
the library in the command line's own process.

## Networking

The default bridge (`nspawn0`, `10.99.0.0/24`) and those of user-defined
networks (`nsbr-NAME`, a /24 of `network_pool` each) are created with `ip`;
the nftables table `ip nspawn` holds the DNAT maps for published ports (`ports` for
those on every address of the host, `addr_ports` for those on one), and for
every network masquerading (not for internal ones), hairpin masquerading and a
guard so that `route_localnet` cannot expose the host's loopback services. Its
forward chain keeps the networks apart: an internal network forwards nothing,
connections to published ports cross (DNAT), anything else from one bridge to
another is dropped, which holds whatever firewalld or iptables accept. The
table is always written for all networks at once, so bringing one bridge up
never drops another's rules. A machine's record says its network kind
(`network`) and, for a user-defined network, its name in `network_name`; the
other bridge networks it joins are `extra_networks`, each with the machine's
address there, its extra names on each network are `aliases`, and `--network
none` is `no_network` (`Private=yes`). All three are separate fields, so an
older nspawn reads such a record as a machine of its primary network alone.
The primary network is the first `--network`; the default route goes through
the first network that is not internal (`bridge::gateway_index`). Booted
machines get a fixed address on each network through `.network` files mounted
at `/run/systemd/network/10-host0.network`, `11-host1.network`.., host0 on the
primary bridge through `Bridge=` and the others through `VirtualEthernetExtra=`,
whose host ends (`vb1-NAME`..) the publish hook puts on their bridges; app
machines get a namespace built beforehand (`ip netns`, one veth per network,
addresses, route) referenced by `NamespacePath=`. Under managed user
namespaces (mstack) nspawn has systemd-nsresourced create the veths and does
not put their host ends on the bridges, so the publish hook does
(`bridge::adopt_managed_veth`, which finds the peer of each `hostN` through the
machine's sysfs). `/etc/hosts` lists, for each network the machine joins, every
member with its address there, its name and its aliases on that network (an
alias several members share names the first of them by name), and
`host.nspawn.internal`, the gateway of the machine's gateway network. With firewalld the
bridges are bound to the trusted zone; with docker or ufw, accept rules go into
DOCKER-USER or FORWARD. `network rm` undoes all of it for its bridge. The
bridge is IPv4 only: it gets `addrgenmode none` and no `fe80::` address,
host0 gets `LinkLocalAddressing=no` in its
`.network` file or `addrgenmode none` in an app's namespace, so machined never
hands out a link-local address under a machine's name.

`--network veth` keeps systemd-nspawn's own veth configured by systemd-networkd
on the host; `--network host` shares the host's network.

## Volumes

`-v SOURCE:TARGET[:ro]` becomes a `Bind=`/`BindReadOnly=` line, idmapped when
the machine runs with private users. mstack machines cannot idmap binds, so
their volumes are attached from the host by the publish hook (`open_tree`,
`mount_setattr` with the machine's user namespace, `move_mount`). Every
booted machine with volumes gets `nspawn-volumes.service`, which holds
`local-fs.target` until they are all mounted. Named volumes are plain directories
under `volumes/`; `api/volumes.rs` lists them with the records that name
them and removes only the ones no record names, reading the records
strictly so that one it cannot read never makes a volume look unused.
