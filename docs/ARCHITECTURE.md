# Architecture

nspawn is a single binary that talks to systemd (`org.freedesktop.systemd1`)
and systemd-machined (`org.freedesktop.machine1`) over D-Bus and to OCI
registries over HTTPS. There is no daemon: state lives in files, and the
machine units call nspawn back through drop-in hooks.

## Modules

| Module | Role |
|---|---|
| `cli.rs`, `commands/` | clap definitions and one file per command |
| `config.rs` | `/etc/nspawn/nspawn.toml`, environment and flags |
| `reference.rs` | image references, local names, machine name rules |
| `hub.rs`, `auth.rs`, `search.rs` | registry client, credentials, search |
| `layout.rs`, `oci.rs` | OCI image layout reader, image config, boot/app detection |
| `store.rs` | layers, blobs, records, manifests, gc, the store lock |
| `install.rs`, `backend.rs` | turning blobs into a machine (overlay, flat, mstack) |
| `settings.rs` | the `.nspawn` settings file and the unit hook drop-in |
| `bridge.rs`, `hostnet.rs` | the nspawn0 bridge, ports, firewalls; veth mode |
| `volume.rs`, `volmount.rs` | `-v` parsing; host-side mounts for mstack machines |
| `nsenter.rs`, `pty.rs` | exec through namespaces, terminal pumping |
| `systemd.rs` | typed D-Bus calls, job waiting |

## On disk

```
/var/lib/nspawn/
  layers/            root-owned extracted layers (overlay backend)
  layers-foreign/    layers shifted into the foreign UID range (mstack)
  blobs/             compressed blobs, kept for push
  images/NAME.json   the record: reference, backend, mode, network, address,
                     ports, entrypoint/cmd, env, volumes
  manifests/NAME.json raw manifest bytes (digest stays valid)
  machines/NAME/     overlay upper/work, host0.network, hosts, resolv.conf,
                     units/ for the volume wait unit
  volumes/NAME/      named volumes
  .lock              flock serialising commands that change the store
/var/lib/machines/NAME        the root machined boots (mount point or dir)
/var/lib/machines/NAME.mstack mstack layout (layer@N links, rw/)
/etc/systemd/nspawn/NAME.nspawn          generated settings, regenerated on start
/etc/systemd/system/systemd-nspawn@NAME.service.d/
  nspawn-overlay.conf   RequiresMountsFor= (overlay)
  nspawn-hooks.conf     ExecStartPre/Post, ExecStopPost calling nspawn
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
repeatedly until it is gone. `exec` joins the leader's namespaces (user
first, mount last), joins its cgroup, becomes the machine's root and then the
requested user, so capabilities are dropped.

## Machines and apps

An image with an init system whose entrypoint is that init is booted
(`Boot=yes`). Anything else is an app: its command runs as PID 2 under the
stub init (`ProcessTwo=yes`), with the OCI config's environment, working
directory, user and stop signal. Apps on the bridge run with
`PrivateUsers=no`, since a user namespace cannot join the network namespace
prepared on the host; app images are assembled with overlay even where mstack
exists.

Overlay machines run under a user namespace like the others, and no released
kernel lets an overlayfs mount be idmapped, so nspawn shifts the tree with a
recursive chown at the first start. The mount carries `metacopy=on` so that
this chown copies inodes rather than file contents into the upper directory
and the layers stay shared. Attributes in overlayfs's own namespace
(`trusted.overlay.*`, `user.overlay.*`) are dropped from every layer on
extraction, so an image cannot redirect a file or its data.

## Networking

The bridge (`nspawn0`, `10.99.0.0/24`) is created with `ip`; the nftables
table `ip nspawn` holds the DNAT map for published ports, masquerading,
hairpin masquerading and a guard so that `route_localnet` cannot expose the
host's loopback services. Booted machines get a fixed address through a
`.network` file mounted at `/run/systemd/network/10-host0.network`; app
machines get a namespace built beforehand (`ip netns`, veth, address, route)
referenced by `NamespacePath=`. `/etc/hosts` lists every machine on the bridge
and `host.nspawn.internal`. With firewalld the bridge is bound to the trusted
zone; with docker or ufw, accept rules go into DOCKER-USER or FORWARD.

`--network veth` keeps systemd-nspawn's own veth configured by systemd-networkd
on the host; `--network host` shares the host's network.

## Volumes

`-v SOURCE:TARGET[:ro]` becomes a `Bind=`/`BindReadOnly=` line, idmapped when
the machine runs with private users. mstack machines cannot idmap binds, so
their volumes are attached from the host by the publish hook (`open_tree`,
`mount_setattr` with the machine's user namespace, `move_mount`). Every
booted machine with volumes gets `nspawn-volumes.service`, which holds
`local-fs.target` until they are all mounted.
