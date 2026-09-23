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
| `bridge.rs`, `hostnet.rs` | the nspawn0 bridge, ports, firewalls; veth mode |
| `volume.rs`, `volmount.rs` | `-v` parsing; host-side mounts for mstack machines |
| `nsenter.rs`, `pty.rs` | exec through namespaces, terminal pumping |
| `api/copy.rs` | cp: tar streams packed and unpacked relative to directory descriptors, paths resolved with openat2 inside the machine's root |
| `systemd.rs` | typed D-Bus calls, job waiting |

## On disk

```
/var/lib/nspawn/
  layers/            root-owned extracted layers (overlay backend)
  layers-foreign/    layers shifted into the foreign UID range (mstack)
  blobs/             compressed blobs, kept for push; .hold-* lists the blobs
                     of a pull in flight, which the collector leaves alone
  images/NAME.json   the record: reference, backend, mode, network, address,
                     ports, entrypoint/cmd, env, volumes, labels,
                     restart policy, limits
  manifests/NAME.json raw manifest bytes (digest stays valid)
  machines/NAME/     overlay upper/work, host0.network, hosts, resolv.conf,
                     units/ for the volume wait unit
  volumes/NAME/      named volumes
  starting/NAME      a start in progress, until the machine is registered:
                     nothing removes or replaces the image meanwhile
  .lock              flock serialising commands that change the store
/var/lib/machines/NAME        the root machined boots (mount point or dir)
/var/lib/machines/NAME.mstack mstack layout (layer@N links, rw/)
/etc/systemd/nspawn/NAME.nspawn          generated settings, regenerated on start
/etc/systemd/system/systemd-nspawn@NAME.service.d/
  nspawn-overlay.conf   RequiresMountsFor= (overlay)
  nspawn-hooks.conf     ExecStartPre/Post, ExecStopPost calling nspawn,
                        Restart= and MemoryMax=/MemorySwapMax=/CPUQuota=/TasksMax= when set
/etc/systemd/system/machines.target.wants/systemd-nspawn@NAME.service
                        --restart always or unless-stopped: started at boot
/etc/nspawn/auth.json  registry credentials, 0600
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
enabled units and programs that exit on their own behave the same.

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
the release hook of its last run. The policy and the limits go into the hooks
drop-in, since the settings file has no keys for them; `always` and
`unless-stopped` enable the unit the way `machinectl enable` does, `stop`
disables an `unless-stopped` one and removing a machine disables it. Limits
could be applied to a running unit with SetUnitProperties; for now they apply
at the next start, and Restart= could not be anyway. `exec` joins the leader's namespaces (user
first, mount last), joins its cgroup, becomes the machine's root and then the
requested user, so capabilities are dropped.

## Machines and apps

An image with an init system whose entrypoint is that init is booted
(`Boot=yes`). Anything else is an app: its command runs as PID 2 under the
stub init (`ProcessTwo=yes`), with the OCI config's environment, working
directory, user and stop signal. Apps on the bridge run with
`PrivateUsers=no`, since a user namespace cannot join the network namespace
prepared on the host; app images are assembled with overlay even where mstack
exists. The namespace is named with `NamespacePath=` in the settings file on
systemd 259 or newer; before that the key does not exist, and the hooks
drop-in rewrites the unit's `ExecStart=` with `--network-namespace-path=`
instead (the argv systemd has loaded, minus the options that conflict).

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

The command line is a client of that service, so one code path does the
work: `commands/` converts arguments into calls and prints what comes back,
follows jobs line by line (with a bar for each transfer when standard error is
a terminal), attaches the terminal to the descriptors `Exec`,
`Shell` and `Logs` hand over, and packs or unpacks the tar streams of `CopyTo`
and `CopyFrom`. The registry and CA certificate the
command line was given travel as options of each call, so `--registry`,
`--ca-cert` and the environment keep their meaning. Only the service itself,
`daemon --install` and the unit hooks (`network prepare`, `publish`,
`release`, which must not depend on the service while a machine starts) run
the library in the command line's own process.

## Networking

The bridge (`nspawn0`, `10.99.0.0/24`) is created with `ip`; the nftables
table `ip nspawn` holds the DNAT map for published ports, masquerading,
hairpin masquerading and a guard so that `route_localnet` cannot expose the
host's loopback services. Booted machines get a fixed address through a
`.network` file mounted at `/run/systemd/network/10-host0.network`; app
machines get a namespace built beforehand (`ip netns`, veth, address, route)
referenced by `NamespacePath=`. Under managed user namespaces (mstack) nspawn
has systemd-nsresourced create the veth and does not put its host end on the
bridge, so the publish hook does (`bridge::adopt_managed_veth`, which finds the
peer of the machine's host0 through its sysfs). `/etc/hosts` lists every machine on the bridge
and `host.nspawn.internal`. With firewalld the bridge is bound to the trusted
zone; with docker or ufw, accept rules go into DOCKER-USER or FORWARD. The
bridge is IPv4 only: it gets `addrgenmode none` (and loses the `fe80::`
address an earlier version left), host0 gets `LinkLocalAddressing=no` in its
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
