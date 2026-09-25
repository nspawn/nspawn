# nspawn

Docker-like management of [systemd-nspawn](https://www.freedesktop.org/software/systemd/man/latest/systemd-nspawn.html)
machines. Images come from an OCI registry (the hub), are stored as shared layers and are
started, inspected and stopped through the D-Bus APIs of systemd-machined and systemd
itself. `machinectl` and `importctl` are never called. The work is done by a service on
the system bus, `org.nspawn`, and the command line is its client, the way `machinectl`
and `systemctl` are clients of machined and systemd.

```
nspawn hub ls                       # repositories and tags on the hub
nspawn search fedora                # images on the hub and on Docker Hub, with their source
nspawn login docker.io -u me        # keep credentials for a registry (the hub by default)
nspawn pull fedora:44               # download and assemble an image
nspawn images ls                    # local images (all of them, not only ours)
nspawn start fedora-44              # boot it as a machine
nspawn create fedora-44 web2        # another machine from the same local image, docker create style
nspawn run -d nginx:1.27 --name web -p 8080:80   # make a machine and start it in the background
nspawn run -it --rm docker.io/library/alpine:3 sh   # like docker run: a terminal, removed at the end
nspawn start web -p 8080:80 -e KEY=v -v /srv/data:/data -v pgdata:/var/lib/pg   # docker-style flags
nspawn start web --label caddy=web.example   # labels for whoever reads them (inspect, ps --json)
nspawn start web --restart unless-stopped -m 512m --cpus 1   # restart policy and limits, like docker
nspawn update web -m 1g --restart always   # change them later, a running machine at once
nspawn ps                           # running machines: image, mode, command, uptime (-a adds stopped ones)
                                    # (containers only: the virtual machines machined also lists are left out)
nspawn inspect web                  # everything nspawn knows about a machine, as JSON
nspawn stats                        # CPU, memory, network and disk of running machines, like docker stats
nspawn exec fedora-44 -- /usr/bin/systemctl is-system-running
nspawn shell fedora-44
nspawn logs fedora-44               # console output; --inside reads the machine's own journal
nspawn events --filter name=web     # starts, exits, restarts, pulls and removals as they happen
nspawn cp ./nginx.conf web:/etc/nginx/   # copy files in or out, like docker cp
nspawn stop fedora-44
nspawn kill -s HUP web              # a signal for the program, like docker kill (SIGKILL by default)
nspawn rm -f web2                   # remove a machine, stopping it first (rm without -f is images rm)
nspawn images rm fedora-44          # also frees layers and blobs nobody references
nspawn volume ls                    # named volumes and the machines that use them (create, rm, prune)
nspawn network create backend       # a network of its own; --network backend on start, run, create

nspawn build -t team/app:1 ./app    # mkosi -t oci on ./app, imported as a local image
nspawn push team/app:1              # upload it to the hub (layers already there are skipped)
nspawn push app-1 --to team/app:2   # push a local image under another tag
```

`run` is `pull` (or `create` from a local image with the same reference, as docker's
`--pull missing` has it) followed by `start`, with the flags of both; `--pull always` asks
the registry every time and `--pull never` never. A name that is taken is refused unless
`--force`, which makes the machine anew. What follows the image replaces an app's
command, as with docker (`nspawn run -it alpine sh`; after `--` as well). Like `docker
run` it stays attached: the
machine's output follows until it ends (stdout and stderr together, a line at a time,
from the journal, so that `logs` shows it later too), and `run` exits with the program's
exit code, or 128 plus the signal it died of. Ctrl-C, SIGTERM, SIGHUP and SIGQUIT go to
the program; a third Ctrl-C within a second leaves it running and returns. `-i` gives the
program this standard input, `-t` a terminal (`-it` for a shell), `--rm` removes the
machine once it ends (named volumes stay; an image it had to pull is kept under its own
name, as docker keeps images, and the machine gets a name of its own unless `--name`
says one), and `-d` starts it in the background and
returns. A booted image shows its console until it powers off
(Ctrl-C powers it off); `run -it` on one waits for its boot, opens a root shell and powers
the machine off when the shell ends, with the shell's exit code. Closing the terminal of
`run -t` stops the machine, since nothing would read its terminal any more; `-d` and
`exec` are for machines that stay.

`build` runs `mkosi` in the given directory with `--format=oci`, so the same
`mkosi.conf` tree that works on its own works here; `--distribution`, `--release`,
`--profile` and anything after `--` are passed through. The result is stored like a pulled
image (blobs, manifest, assembled machine) and can be started right away or pushed.

## Machines and apps

Images that ship an init system (systemd) and whose entrypoint is that init are booted with
`--boot`, like `machinectl start` does, and `shell` opens machined's login session
there. Any other image, for example anything from Docker Hub, is an "app":
its entrypoint runs under nspawn's stub init, with the environment, working
directory, user and stop signal from the OCI config, and joins the bridge network like
any other machine (see Networking). The arguments given to `create` or `start` after `--`
replace the image's cmd and follow its entrypoint, exactly as with docker (`nspawn start
web -- nginx -T` still runs `/docker-entrypoint.sh` first); `--entrypoint PROGRAM` replaces
the entrypoint and `--entrypoint ""` drops it. Both are remembered, like the command of a
docker container; `start --image-command` goes back to the image's own. `-e VAR=value`
adds environment on top of the image's (`-e VAR` copies it from your shell), and
`-v SOURCE:TARGET[:ro]` mounts a host directory (which has to exist), or a named volume
that nspawn keeps under `/var/lib/nspawn/volumes/NAME` and creates on first use, into
any kind of machine; in machines that run
with private users the mount is idmapped, so root inside owns what it writes on the
host. Every booted machine with volumes gets a small unit mounted into it,
`nspawn-volumes.service`, that holds `local-fs.target` until all of them are mounted and
fails visibly otherwise, so services find their configuration and data in place whatever
the backend. On overlay and flat the volumes are there from the first instruction (they
come from the settings file); mstack machines get them attached from the host right after
their init starts, since systemd-nspawn cannot idmap binds under managed user namespaces,
which is what the unit waits for. `-e none` and `-v none` forget them.

Named volumes outlive the machines that use them, as in docker: removing a machine
keeps them and says so. `nspawn volume ls` lists them with the machines whose records
mount them, `volume create NAME` makes one ahead of its first use, and `volume rm` and
`volume prune` remove the ones no machine uses; a volume still named by a machine is
refused until that machine is started with other volumes (or `-v none`) or removed.

`--restart` takes docker's policies. `on-failure` restarts the machine when its program
or its init dies with an error, `always` whenever it ends and also at boot, and
`unless-stopped` like `always` until `nspawn stop`, which takes the boot start back
until the next `start`; restarts come after a second, then later and later, up to half a
minute, for as long as they keep failing, and `ps` shows such a machine as
`restarting`. `nspawn stop` always stops it, one waiting to be restarted included (an
`always` machine still starts at the next boot); `machinectl stop` does too but does not
take an `unless-stopped` machine off the boot list, and a `poweroff` from inside counts as
ending under `always`. `kill` works as docker's: SIGKILL, its default, stops the machine
for good; any other signal goes to the program (or the init of a booted machine), and
should it end the machine the policy decides, unless it was the machine's stop signal,
which counts as a stop. `-m/--memory`, `--cpus` and `--pids-limit`
bound the whole machine (its unit's MemoryMax=, CPUQuota= and TasksMax=), which is why
they cannot be seen from inside; as with docker, `--memory` also lets the machine use as
much swap again (MemorySwapMax=), and no more. Both are remembered like the ports and apply at the
next start; `--restart no` and a limit of 0 remove them. `update` changes them without a
start, like docker update: a running machine gets the new limits in its cgroup at once.
`stats` shows what each running machine uses against them, its unit's cgroup read every
second (`--no-stream` for one reading, `--json` for scripts). With a policy, `stop --no-wait`
of an app also lets the stub init send the program SIGTERM and SIGHUP, since nobody
stays to stop the unit later.

Healthchecks are docker's: an image's `HEALTHCHECK`, or `--health-cmd` with
`--health-interval`, `--health-timeout`, `--health-retries`, `--health-start-period` and
`--health-start-interval` on `run`, `start`, `create` and `update` (`--no-healthcheck`
turns the image's off), run a probe inside the machine at the interval; `ps` shows
`(healthy)`, `(unhealthy)` or `(health: starting)` next to the state, `inspect` the last
probes with their output, and `events` a `health_status` on every change. The probes run
from a unit of their own (`nspawn-health-NAME.service`) bound to the machine's, so they
go with it.

The other flags of `docker run` are there too, remembered like the rest: `--hostname`,
`-u/--user` and `-w/--workdir` (instead of the image's; the user is resolved from the
image's passwd and group files by a stand-in for getent that nspawn binds, since
busybox has no getent, musl's answers no initgroups and neither takes a uid the passwd
does not list, as docker does), `--cap-add`, `--cap-drop` (`--cap-drop
ALL --cap-add NET_BIND_SERVICE` keeps that one, as with docker) and `--privileged`,
`--read-only`, `--tmpfs PATH[:OPTIONS]`, `--shm-size`, `--device
HOST[:CONTAINER[:rwm]]`, `--dns` and `--dns-search`, `--add-host HOST:IP` (with
`host-gateway`), `--ulimit NAME=SOFT[:HARD]`, `--oom-score-adj`, `--stop-signal` and
`--stop-timeout` (what `stop` uses unless `-t` says otherwise), `--init` (accepted; the
stub init reaps anyway) and `--sysctl` (`net.*` keys, set in an app machine's network
namespace). Each becomes a line of the machine's settings file or of its unit; `inspect`
shows them all. A path the image declares as a volume and nothing is mounted over gets a
note at start: nspawn has no anonymous volumes, so what is written there goes with the
machine.

Secrets are docker's too, without a swarm: `nspawn secret create db-password` reads the
content from standard input (or `--file`) and keeps it encrypted with `systemd-creds`,
bound to the host's TPM2 where there is one and to its credential key otherwise;
`--secret db-password` on `run`, `start` or `create` gives the machine the plaintext at
`/run/secrets/db-password`, a read-only file of root's with mode 0444
(`NAME:TARGET:MODE:UID:GID` says otherwise), decrypted into a tmpfs of root's alone for
as long as the machine runs. `secret ls` shows who takes each one, `secret inspect` what
is known about it (never the content), and `secret rm` refuses one a machine still
takes. Not on mstack machines yet.

`events` is docker events: what happens to machines (`start`, `die` with its exit code,
`stop`, `restart`, `oom`, `fail`, `health_status`, and nspawn's own `pull`, `build`,
`create`, `push`, `kill`, `update`, `remove`), networks and volumes, as it happens or between `--since`
and `--until`, filtered by name, type, event or label, as text or `--json`. It reads
the journal, so machines started by `machinectl`, at boot or by a restart policy are
there too, and so is what happened while nobody watched.

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

Labels work as in docker. An image's own labels (`LABEL` in a Containerfile,
`OciLabels=` in a mkosi.conf) are read from its OCI config when it is pulled or built,
and `--label KEY=VALUE` on `start` or `create` adds labels of the machine's own on top,
remembered like the ports (`--label none` forgets them). nspawn itself does nothing
with them; `inspect` and `ps --json` show both the merged `labels` and the
`image_labels`, for tools such as a reverse proxy that configures itself from them.

`cp` copies files and directories between the host and a machine, running or not (a
stopped mstack machine excepted: its tree only exists while it runs), with docker cp's
rules: an existing directory receives the source under its own name, anything else is
the name of the copy, and `DIR/.` copies the contents of DIR. What goes in belongs to
root inside the machine, whatever user namespace it runs in; what comes out belongs to
whoever ran `cp`, because the command line writes it. Paths inside the machine are
resolved inside it, so a link there, absolute or not, never leads to the host; links
are copied as links, and devices, sockets and fifos are left out. The service and the
command line exchange a tar stream, as docker's API does. Where SELinux enforces, a host
directory mounted with `-v` keeps its own label, which the service may not be allowed to
write (docker needs `:z` for the same); named volumes are nspawn's and always work.

One image, as many machines as you like: `nspawn create SOURCE NAME` makes another
machine from an image that is already local, without touching the registry. It shares the
source's layers and gets a writable layer, an address, settings and ports of its own
(`-p`, `--network`); removing one never affects the others. A pulled image is a machine
too, so `nspawn rm NAME` and `nspawn images rm NAME` remove the same thing: the record,
the tree, the unit files and whatever layers nobody else uses. `rm --force` stops a
running machine first (SIGKILL, like `docker rm -f`) where both refuse otherwise. `pull` with `--name`
ends up the same way but resolves the manifest through the registry first.

Completions for bash, zsh and fish come with the packages; from a build of your own,
`nspawn completions bash > ~/.local/share/bash-completion/completions/nspawn` (or the
equivalent for your shell). `man nspawn` is the same reference the packages install.

`ps`, `machines ls`, `images ls`, `network ls` and `volume ls` take `--json` and print what the
service answered instead of a table, and `nspawn inspect NAME...` prints the whole
record of machines or images (running or not) as a JSON array, like docker inspect:
what a script or an agent reads. The keys are the ones of the D-Bus interface (see
`docs/DBUS.md`), so the command line and the bus never disagree.

## Registries and credentials

Every command talks to the `org.nspawn` service on the system bus, which decides
through polkit who may do what: `org.nspawn.inspect` for the commands that only look,
`org.nspawn.manage` for the rest, both for administrators by default. `sudo nspawn ...`
therefore works everywhere; to drive nspawn without a password, hand one of those
actions to a group with a polkit rule, as in `nspawn-wheel.rules` in the documentation
directory. Doing that makes the group administrators of the host, since a machine's
commands run as root and any host path can be mounted into one.

Registries are used anonymously until `nspawn login [REGISTRY] -u USER` (password asked on
the terminal, or `--password-stdin`) checks the credentials the way docker login does and
keeps them in `/etc/nspawn/auth.json`, mode 0600, in the auth.json format podman and skopeo
use. That file is the only one consulted: the service has no home of the caller's to
look into, so `docker login` or `podman login` do not carry over. Every operation
chooses the credentials of the registry it talks to, so the hub's never travel to
Docker Hub; `nspawn logout [REGISTRY]` forgets them. Anonymous pulls from Docker Hub are rate limited per address; logging in
lifts that.

## Networking

Booted machines join the `nspawn0` bridge, a docker0 style network that nspawn manages
itself, so it behaves the same whether the host runs systemd-networkd, NetworkManager or
nothing at all. On the first `start` nspawn creates the bridge with the first address of
the subnet (`10.99.0.0/24` by default; `bridge`, `subnet` and `dns` can be set in
`nspawn.toml`), enables forwarding and installs the nftables table `ip nspawn` with
masquerading. An existing interface with that name is only taken over when it is a bridge
nspawn made (or an empty one); `bridge = "docker0"` is refused rather than acted on. Each machine gets a fixed address from the subnet, remembered with the
image and handed to the systemd-networkd inside it through a `.network` file mounted at
`/run/systemd/network/10-host0.network`; the DNS servers are the host's upstream ones.
A generated `/etc/hosts` gives every machine the names of the other machines on the
bridge and `host.nspawn.internal` for the host, and hosts with systemd 258 or newer
resolve machine names themselves through machined. The bridge carries IPv4 only, so
neither the machines' `host0` nor the bridge get an IPv6 link-local address: a machine's
name leads to its bridge address, not to a `fe80::` one. `nspawn network inspect bridge`
shows the addresses and ports; `nspawn network up` creates the bridges without starting
anything.

More networks are made like docker's user-defined ones: `nspawn network create backend`
gives a bridge of its own (`nsbr-backend`) with the next free /24 of `network_pool`
(`10.99.0.0/16` unless set in `nspawn.toml`, never one the host already routes) or the
one given with `--subnet`, and `--network backend` on `start`, `run` or `create` puts a
machine on it. Machines of one network reach each other by name, and nothing of another
network, the default one included, reaches them; a port they publish is reachable from
every network through the host, as from the LAN. `--internal` makes a network with no
way out: its machines reach each other and the host, publish nothing and are reached by
nothing else. `--network` given several times joins several networks, the first one
primary (its gateway is the default route, unless it is internal): a proxy on `front` and
`back` reaches both, while `front` and `back` still do not reach each other.
`--network-alias NAME` (or `NETWORK=NAME`) gives a machine another name on a network, as
docker does; `--network none` gives it no interface but `lo`. `network ls` lists the
networks, `network rm` and `network prune` remove the ones no machine joins, bridge and
rules included; `network create --label KEY=VALUE` tags one.

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
own addresses and from 127.0.0.1, and it goes away when the machine stops. `-p
127.0.0.1:8080:80` publishes on one address of the host alone, for a reverse proxy in
front; `-p 8000-8010:8000-8010` publishes a range, one mapping per port. A port another
running machine publishes (on every address, or on that one), or one a service of the
host listens on, is refused. The list is remembered for the image; `-p none` forgets it. With firewalld running, the bridge is
bound to the trusted zone at runtime, which also lets published ports through; the
binding does not survive `firewall-cmd --reload`, the next `start` or `nspawn network up`
puts it back.

Hosts running docker (in its default iptables mode) or ufw have a FORWARD policy of
DROP; `start` then adds two rules to the DOCKER-USER chain, which docker reserves for
that, or to FORWARD itself: anything out of the bridge, and into the bridge only what was
published or belongs to a connection a machine opened. That needs the `iptables` command,
which those tools bring with them; where the rules are there and the command is not, or
where a hand-written nftables firewall drops forwarded traffic, `start` says so and the
exception has to be made by hand. Without it the machines reach nothing beyond the bridge
and published ports answer on the host alone.

`--network host` shares the host's network instead, and `--network veth` keeps the classic
systemd-nspawn setup: a virtual ethernet pair configured by systemd-networkd on the host
through `80-container-ve.network`. In that mode `start` activates systemd-networkd when
the host has no `.network` files of its own and refuses with an explanation otherwise,
and binds `ve-<name>` to firewalld's trusted zone while the machine runs.

## D-Bus

Everything above is a method on `org.nspawn.Manager` on the system bus, and
the command line is one client of it. `sudo nspawn daemon --install` makes the
bus start the service on demand; without it every command says so. Images,
machines, the network and credentials are methods; pulls, pushes, builds, creates,
removals, copies and volume removals come back as job objects with their output and
result; `exec` hands the command's terminal or pipes over the bus, and `cp` a tar
stream. See `docs/DBUS.md`.

## Requirements

- A host with systemd-nspawn and systemd-machined 255 or newer (255, 259 and 261 are
  tested; 252 cannot mount the generated files under `/run` of a machine), overlayfs for
  the `overlay` backend and cgroup v2.
- The service on the system bus: a package installs it, `sudo nspawn daemon --install`
  does the same for a binary built by hand. It runs as root and does everything below
  `/var/lib/machines`, `/var/lib/nspawn`, `/etc/systemd` and `/etc/nspawn`; who may call
  it is polkit's answer, so `sudo` works everywhere and a polkit rule lets a group do
  without it. polkit itself is needed for anyone but root to call at all.
- On Fedora or RHEL with SELinux enforcing, the `nspawn-selinux` package (the policy in
  `packaging/selinux`), which the RPM recommends: without its domain the service runs
  unconfined and the bus drops it when it passes a descriptor, so `exec`, `shell`,
  `logs` and `cp` fail. A binary installed by hand needs the label as well, which
  `daemon --install` points out.
- `shell` on a booted machine uses machined's login session, which needs D-Bus inside
  (the hub images have it); `exec` enters the namespaces and needs nothing inside.
- The bridge network needs `ip` and `nft` on the host (iproute2 and nftables), nothing
  else. `--network veth` needs systemd-networkd on the host.

## Development

See docs/HACKING.md for building, tests and the e2e suite, docs/ARCHITECTURE.md for
how it works, and CONTRIBUTING.md before sending changes.

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

## AI use disclosure

AI tools help write nspawn's code. Every change is reviewed by a person, comes with unit
tests, and is checked by the end-to-end suite, which runs the distribution packages on
virtual machines with different systemd versions and kernels: Ubuntu 24.04 (systemd 255,
Linux 6.8), Fedora 44 (systemd 259, Linux 7.1, SELinux enforcing) and Arch Linux
(systemd 261, Linux 7.2).
