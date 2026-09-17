# nspawn

Docker-like management of [systemd-nspawn](https://www.freedesktop.org/software/systemd/man/latest/systemd-nspawn.html)
machines. Images come from an OCI registry (the hub), are stored as shared layers and are
started, inspected and stopped through the D-Bus APIs of systemd-machined and systemd
itself. `machinectl` and `importctl` are never called.

```
nspawn hub ls                       # repositories and tags on the hub
nspawn pull fedora:44               # download and assemble an image
nspawn images ls                    # local images (all of them, not only ours)
nspawn start fedora-44              # boot it as a machine
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
directory, user and stop signal from the OCI config, and shares the host's network
(`--network veth` switches to a virtual ethernet pair). `exec` and `shell` then enter the
machine's namespaces directly, so no D-Bus is needed inside, and `stop` sends the image's
stop signal to every process before terminating the machine after `--timeout` seconds.

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

Ports are published like docker: `nspawn start web -p 8080:80 -p 5353:53/udp`. Each one
is a DNAT entry in the same nftables table, reachable from other hosts, from the host's
own addresses and from 127.0.0.1, and it goes away when the machine stops. The list is
remembered for the image; `-p none` forgets it. With firewalld running, the bridge is
bound to the trusted zone at runtime, which also lets published ports through.

`--network host` shares the host's network instead (the default for app images, which
have no systemd-networkd to configure `host0`), and `--network veth` keeps the classic
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
