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

```
nspawn pull docker.io/library/busybox
nspawn start busybox -- /bin/sleep infinity   # replace the entrypoint for this start
nspawn exec busybox -- /bin/sh -c 'uname -n'  # exit code is propagated
nspawn stop busybox
```

Detection can be forced with `--mode boot|app` on `pull` and `build`. Everything nspawn
decides for a machine ends up in `/etc/systemd/nspawn/<name>.nspawn`, which the stock
`systemd-nspawn@.service` template honours through `--settings=override`.

## How images are stored

`pull` fetches the manifest (multi-arch indexes are resolved for the host platform),
downloads every layer while verifying its sha256 digest, and assembles the image with one
of three backends. Layers live once under `/var/lib/nspawn/layers/` and are shared
between images. Nothing of ours is hidden below `/var/lib/machines`, so `machinectl clean`
stays safe.

| Backend | Requirements | What it creates |
|---|---|---|
| `mstack` | systemd 261 or newer with systemd-nsresourced and systemd-mountfsd installed | `<name>.mstack/` with `layer@N` symlinks and `rw/`, plus `/etc/systemd/nspawn/<name>.nspawn` setting `PrivateUsers=managed` (the stock template's `-U` is rejected by `--mstack=`); `start` activates `systemd-nsresourced.socket` and `systemd-mountfsd.socket` |
| `overlay` | any systemd with overlayfs | a `.mount` unit that overlays the layers with a writable upper directory, plus a drop-in so `systemd-nspawn@<name>.service` requires it |
| `flat` | nothing | the layers extracted into `/var/lib/machines/<name>` |

`--backend auto` (the default) picks the first one the host supports. mstack layers are
extracted into `/var/lib/nspawn/layers-foreign/` with their UIDs shifted into systemd's
foreign UID range (2147352576 and up): systemd-mountfsd maps that range into the managed
user namespace of the machine, while root-owned directories would be mounted without any
mapping and stay unwritable inside. The other backends keep the image's own IDs under
`/var/lib/nspawn/layers/`. Whiteouts of
multi-layer images are honoured (converted to overlayfs whiteouts for `overlay` and
`mstack`, applied directly for `flat`).

The old `pull-tar` path keeps working for hosts without this tool: every layer blob served
by the registry is a compressed tar, so `importctl pull-tar https://hub/v2/<repo>/blobs/<digest>`
imports the same image on any systemd version.

## Configuration

`/etc/nspawn/nspawn.toml`, overridden by the `NSPAWN_REGISTRY`, `NSPAWN_CA_CERT` and
`NSPAWN_CONFIG` environment variables and by the `--registry`, `--ca-cert` and `--config`
flags:

```toml
registry = "hub.nspawn.org"      # default registry for references without a host part
ca_cert = "/etc/zot/ca.crt"      # extra CA to trust (optional)
backend = "auto"                 # auto | overlay | flat | mstack
machines_dir = "/var/lib/machines"
state_dir = "/var/lib/nspawn"      # layers, records, writable directories of overlay machines
```

Image references follow the usual form `[registry/]repository[:tag|@digest]`; `fedora:44`
becomes the local image `fedora-44`, `debian` (tag `latest`) becomes `debian`.

## Requirements

- A host with systemd-nspawn and systemd-machined (any recent version; 259 and 261 are
  tested), overlayfs for the `overlay` backend and cgroup v2.
- `pull` and `images rm` need root because they write below `/var/lib/machines`,
  `/var/lib/nspawn` and `/etc/systemd/system`. Everything else goes through D-Bus and polkit.
- Images must boot systemd (the hub images do): `exec` and `shell` use
  `OpenMachineShell`, which needs D-Bus inside the machine.

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
