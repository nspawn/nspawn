# nspawn

Docker-like management of [systemd-nspawn](https://www.freedesktop.org/software/systemd/man/latest/systemd-nspawn.html)
machines. Images come from an OCI registry (the hub), are stored as shared layers and are
started, inspected and stopped through the D-Bus APIs of systemd-machined and systemd
itself. `machinectl` and `importctl` are never called.

```
nspawn hub ls                       # repositories and tags on the hub
nspawn search fedora                # images on the hub and on Docker Hub, with their source
nspawn login docker.io -u me        # keep credentials for a registry (the hub by default)
nspawn pull fedora:44               # download and assemble an image
nspawn images ls                    # local images (all of them, not only ours)
nspawn start fedora-44              # boot it as a machine
nspawn create fedora-44 web2        # another machine from the same local image, docker create style
nspawn start web -p 8080:80 -e KEY=v -v /srv/data:/data -v pgdata:/var/lib/pg   # docker-style flags
nspawn ps                           # running machines: image, mode, command, uptime (-a adds stopped ones)
nspawn exec fedora-44 -- /usr/bin/systemctl is-system-running
nspawn shell fedora-44
nspawn logs fedora-44               # console output; --inside reads the machine's own journal
nspawn stop fedora-44
nspawn images rm fedora-44          # also frees layers and blobs nobody references

nspawn build -t team/app:1 ./app    # mkosi -t oci on ./app, imported as a local image
nspawn push team/app:1              # upload it to the hub (layers already there are skipped)
nspawn push app-1 --to team/app:2   # push a local image under another tag
```

`build` runs `mkosi` in the given directory with `--format=oci`, so the same
`mkosi.conf` tree that works on its own works here; `--distribution`, `--release`,
`--profile` and anything after `--` are passed through. The result is stored like a pulled
image (blobs, manifest, assembled machine) and can be started right away or pushed.

## Machines and apps

Images that ship an init system (systemd) and whose entrypoint is that init are booted with
`--boot`, like `machinectl start` does; `exec` and `shell` go through machined's
`OpenMachineShell`. Any other image, for example anything from Docker Hub, is an "app":
its entrypoint runs as PID 2 under nspawn's stub init, with the environment, working
directory, user and stop signal from the OCI config, and joins the bridge network like
any other machine (see Networking). The arguments given to `create` or `start` after `--`
replace the image's cmd and follow its entrypoint, exactly as with docker (`nspawn start
web -- nginx -T` still runs `/docker-entrypoint.sh` first); `--entrypoint PROGRAM` replaces
the entrypoint and `--entrypoint ""` drops it. Both are remembered, like the command of a
docker container; `start --image-command` goes back to the image's own. `-e VAR=value`
adds environment on top of the image's (`-e VAR` copies it from your shell), and
`-v SOURCE:TARGET[:ro]` mounts a host directory, or a named volume that nspawn keeps
under `/var/lib/nspawn/volumes/NAME`, into any kind of machine; in machines that run
with private users the mount is idmapped, so root inside owns what it writes on the
host. Every booted machine with volumes gets a small unit mounted into it,
`nspawn-volumes.service`, that holds `local-fs.target` until all of them are mounted and
fails visibly otherwise, so services find their configuration and data in place whatever
the backend. On overlay and flat the volumes are there from the first instruction (they
come from the settings file); mstack machines get them attached from the host right after
their init starts, since systemd-nspawn cannot idmap binds under managed user namespaces,
which is what the unit waits for. `-e none` and `-v none` forget them.

`exec` enters the machine's namespaces for both kinds of machine, like docker exec: the
exit code comes back, the image's environment applies and nothing is needed inside (no
D-Bus, no PAM); `shell` opens machined's login session on booted machines and a plain
shell on apps. `stop` sends the image's stop signal to the program of an app machine and
SIGKILLs it after `--timeout` seconds, or asks a booted machine to power off; `--force`
kills at once. Stopping a machine that already ended is not an error: it only drops what
the machine left behind.

Every machine's unit gets a drop-in that calls nspawn around its life (ExecStartPre,
ExecStartPost, ExecStopPost), so `machinectl start`, an enabled unit at boot, a program
that exits on its own or a crash all prepare and release the network the same way as
`nspawn start` and `nspawn stop`. The drop-in names the nspawn binary that wrote it, so
install nspawn where a system service may run it (`/usr/bin` or `/usr/local/bin`; on
SELinux hosts a binary below a home directory is refused with "Permission denied").

One image, as many machines as you like: `nspawn create SOURCE NAME` makes another
machine from an image that is already local, without touching the registry. It shares the
source's layers and gets a writable layer, an address, settings and ports of its own
(`-p`, `--network`); `images rm` of one never affects the others. `pull` with `--name`
ends up the same way but resolves the manifest through the registry first.

## Registries and credentials

Registries are used anonymously until `nspawn login [REGISTRY] -u USER` (password asked on
the terminal, or `--password-stdin`) checks the credentials the way docker login does and
keeps them in `/etc/nspawn/auth.json`, mode 0600, in the auth.json format podman and skopeo
use. Credentials left by `docker login` or `podman login` on the host (also those of the
user behind sudo) are picked up as well. Every operation chooses the credentials of the
registry it talks to, so the hub's never travel to Docker Hub; `nspawn logout [REGISTRY]`
forgets them. Anonymous pulls from Docker Hub are rate limited per address; logging in
lifts that.

## Networking

Booted machines join the `nspawn0` bridge, a docker0 style network that nspawn manages
itself, so it behaves the same whether the host runs systemd-networkd, NetworkManager or
nothing at all. On the first `start` nspawn creates the bridge with the first address of
the subnet (`10.99.0.0/24` by default; `bridge`, `subnet` and `dns` can be set in
`nspawn.toml`), enables forwarding and installs the nftables table `ip nspawn` with
masquerading. Each machine gets a fixed address from the subnet, remembered with the
image and handed to the systemd-networkd inside it through a `.network` file mounted at
`/run/systemd/network/10-host0.network`; the DNS servers are the host's upstream ones.
A generated `/etc/hosts` gives every machine the names of the other machines on the
bridge and `host.nspawn.internal` for the host, and hosts with systemd 258 or newer
resolve machine names themselves through machined. `nspawn network ls` shows the
addresses and ports; `nspawn network up` creates the bridge without starting anything.

App images have nothing inside to configure `host0`, so for them nspawn builds the
network namespace before the process starts (`ip netns`, a veth pair on the bridge, the
address and the default route) and hands it to systemd-nspawn with `NamespacePath=`,
together with a generated `/etc/resolv.conf`. The process finds its network ready from
the first instruction, as in docker, and the namespace goes away with `stop`. One
consequence: a user namespace cannot join a network namespace that belongs to the host,
so app machines on the bridge run without one (`PrivateUsers=no`), which is also docker's
default; capabilities, seccomp and the other namespaces still apply. For the same reason
app images are assembled with the overlay backend even where mstack is available.

Ports are published like docker: `nspawn start web -p 8080:80 -p 5353:53/udp`. Each one
is a DNAT entry in the same nftables table, reachable from other hosts, from the host's
own addresses and from 127.0.0.1, and it goes away when the machine stops. A port another
running machine publishes, or one a service of the host listens on, is refused. The list
is remembered for the image; `-p none` forgets it. With firewalld running, the bridge is
bound to the trusted zone at runtime, which also lets published ports through; the
binding does not survive `firewall-cmd --reload`, the next `start` or `nspawn network up`
puts it back.

Hosts running docker (in its default iptables mode) or ufw have a FORWARD policy of
DROP; `start` then adds two rules to the DOCKER-USER chain, which docker reserves for
that, or to FORWARD itself: anything out of the bridge, and into the bridge only what was
published or belongs to a connection a machine opened. A hand-written nftables firewall
with a drop policy on forward needs the same exception by hand.

`--network host` shares the host's network instead, and `--network veth` keeps the classic
systemd-nspawn setup: a virtual ethernet pair configured by systemd-networkd on the host
through `80-container-ve.network`. In that mode `start` activates systemd-networkd when
the host has no `.network` files of its own and refuses with an explanation otherwise,
and binds `ve-<name>` to firewalld's trusted zone while the machine runs.

## Requirements

- A host with systemd-nspawn and systemd-machined (any recent version; 259 and 261 are
  tested), overlayfs for the `overlay` backend and cgroup v2.
- `pull` and `images rm` need root because they write below `/var/lib/machines`,
  `/var/lib/nspawn` and `/etc/systemd/system`. Everything else goes through D-Bus and polkit.
- Booted machines use `OpenMachineShell` for `exec` and `shell`, which needs D-Bus inside
  the machine (the hub images have it); app images are entered through their namespaces.
- The bridge network needs `ip` and `nft` on the host (iproute2 and nftables), nothing
  else. `--network veth` needs systemd-networkd on the host.

## Development

```
cargo test                                   # unit tests
cargo clippy --all-targets -- -D warnings
cargo build --release
NSPAWN=./target/release/nspawn sudo -E tests/e2e.sh   # end-to-end against a registry and machined
```

`tests/e2e.sh` pulls an image with both the `overlay` and the `flat` backend, boots it,
runs commands inside through the PTY, stops it, removes it, checks that layers are shared
and garbage collected, and (when mkosi is installed) builds `tests/build-context`, pushes
it, pulls it back and pushes it again under another tag. It expects `NSPAWN_REGISTRY` (and `NSPAWN_CA_CERT` for a private
CA) to point at a registry that serves the image given in `IMAGE` (default `fedora:44`).
