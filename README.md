# nspawn

Docker-like management of [systemd-nspawn](https://www.freedesktop.org/software/systemd/man/latest/systemd-nspawn.html)
machines. Images come from an OCI registry (the hub), are stored as shared layers and are
started, inspected and stopped through the D-Bus APIs of systemd-machined and systemd
itself; `machinectl` and `importctl` are never called. The work is done by a service on
the system bus, `org.nspawn`, and the command line is its client, the way `machinectl`
and `systemctl` are clients of machined and systemd.

```
nspawn search fedora                # images on the hub and on Docker Hub, with their source
nspawn pull fedora:44               # verify its signature, download and assemble it
nspawn start fedora-44              # boot it as a machine
nspawn run -d nginx:1.27 --name web -p 8080:80   # an app from Docker Hub, detached
nspawn run -it --rm docker.io/library/alpine:3 sh   # a terminal, removed at the end
nspawn start web -e KEY=v -v pgdata:/var/lib/pg --restart always -m 512m   # docker's flags
nspawn create fedora-44 web2        # another machine from the same local image
nspawn ps                           # running machines (-a adds stopped ones)
nspawn exec web -- nginx -T
nspawn shell fedora-44
nspawn logs web                     # console output; --inside reads the machine's journal
nspawn events                       # starts, exits, restarts, pulls and removals, live
nspawn stop web
nspawn rm -f web2                   # remove a machine, stopping it first
nspawn images rm fedora-44          # also frees layers and blobs nobody references
nspawn network create backend       # a network of its own: --network backend on start
nspawn build -t team/app:1 ./app    # mkosi -t oci on ./app, imported as a local image
nspawn push team/app:1              # upload it to the hub, layers already there skipped
```

`nspawn --help` lists the rest: `login`, `hub ls`, `inspect`, `update`, `stats`, `top`,
`cp`, `kill`, `restart`, `pause`, `volume`, `secret`, `network`, and `completions` for
the shells; `man nspawn` is the same reference the packages install.

## What it does

- **Two kinds of machine.** An image that ships systemd boots like `machinectl start`
  does. Any other image, for example anything from Docker Hub, runs as an app: its
  entrypoint under a stub init, with the environment, user, working directory and
  stop signal of its OCI config. Both take the same flags and join the same networks.
- **docker's flags and semantics.** `run`, `create` and `start` take `-e`, `-v`, `-p`,
  `--restart`, `-m`, `--cpus`, `--pids-limit`, the `--health-*` flags, `--secret`,
  `--label`, `--hostname`, `-u`, `-w`, `--cap-add`, `--cap-drop`, `--privileged`,
  `--read-only`, `--tmpfs`, `--device`, `--dns`, `--add-host`, `--ulimit`,
  `--stop-signal` and the rest, remembered per machine; `update` changes the limits,
  the policy and the healthcheck of a machine, running or not; `exec`, `logs`,
  `events`, `cp`, `kill`, `pause`, `top`, `stats` and `inspect` behave as docker's.
  Named volumes are seeded from the image and outlive the machines that use them;
  secrets are kept encrypted with systemd-creds and handed to a machine as files.
- **One image, many machines.** A pulled image is a machine; `create` makes more from
  it, sharing its layers, each with settings, ports and an address of its own.
- **Signed images.** Every image on hub.nspawn.org is signed by its build workflow,
  with the project's key and keyless through Sigstore; `pull` verifies one of the two
  before downloading anything, refuses an image without a valid signature and
  remembers who signed it. `--no-verify` skips the check for one command.
- **A network of its own.** Machines join the `nspawn0` bridge (docker0 style, managed
  with nftables, fixed addresses, names in `/etc/hosts`); `network create` adds
  isolated networks with aliases, `-p` publishes ports, and `--network host`, `veth`,
  `none` and `container:NAME` cover the other cases. Every machine's unit calls nspawn
  around its life, so `machinectl start`, a unit at boot and a restart policy get the
  same network.
- **Everything through the bus.** Every command is a method of `org.nspawn` on the
  system bus and polkit decides who may call; `inspect` and `--json` print what the
  service answered, with the keys of the D-Bus interface.

[docs/USAGE.md](docs/USAGE.md) has all of this in detail, [docs/DBUS.md](docs/DBUS.md)
the bus interface, and [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) how it is built.

## Requirements

- A host with systemd-nspawn and systemd-machined 255 or newer (255, 259 and 262 are
  tested), Linux 6.5 or newer with overlayfs, and cgroup v2. The `mstack` backend
  (systemd 261 or newer with systemd-nsresourced and systemd-mountfsd) is experimental
  and only used when asked for with `--backend mstack`.
- The service on the system bus: a package installs it, `sudo nspawn daemon --install`
  does the same for a binary built by hand. It runs as root; who may call it is polkit's
  answer (`org.nspawn.inspect`, `org.nspawn.manage`, administrators by default), so
  `sudo` works everywhere and a polkit rule lets a group do without it.
- On Fedora or RHEL with SELinux enforcing, the `nspawn-selinux` package: without its
  domain the bus drops the service when it passes a descriptor, so `exec`, `shell`,
  `logs` and `cp` fail. A binary installed by hand needs the label as well, which
  `daemon --install` points out.
- `ip` and `nft` on the host (iproute2 and nftables). `--network veth` needs
  systemd-networkd; `shell` on a booted machine needs D-Bus inside it, `exec` nothing.

## Development

[docs/HACKING.md](docs/HACKING.md) covers building, the checks and the end-to-end
suite, which runs as root on a disposable VM; [CONTRIBUTING.md](CONTRIBUTING.md) says
what a change must come with.

## AI use disclosure

AI tools help write nspawn's code. Every change is reviewed by a person, comes with unit
tests, and is checked by the end-to-end suite, which runs the distribution packages on
virtual machines with different systemd versions and kernels: Ubuntu 24.04 (systemd 255,
Linux 6.8), Fedora 44 (systemd 259, Linux 7.1, SELinux enforcing) and Arch Linux
(systemd 262, Linux 7.2).
