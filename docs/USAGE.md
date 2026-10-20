# Using nspawn

The commands in the detail the README leaves out: what `run`, `start` and the rest
do, which docker flags they take and where they differ, how machines get their
network, and where credentials live. `nspawn COMMAND --help` and `man nspawn` list
every flag; [DBUS.md](DBUS.md) has the bus interface the commands call, and
[ARCHITECTURE.md](ARCHITECTURE.md) how it is all built.

The documentation on [nspawn.org](https://nspawn.org/docs/) covers the same ground as
guides: [getting started](https://nspawn.org/docs/getting-started/),
[images](https://nspawn.org/docs/images/), [machines](https://nspawn.org/docs/machines/),
[networking](https://nspawn.org/docs/networking/),
[configuration](https://nspawn.org/docs/configuration/),
[building images](https://nspawn.org/docs/building/), the
[command reference](https://nspawn.org/docs/reference/) and a
[FAQ](https://nspawn.org/docs/faq/).

## run and build

`run` is `pull` (or `create` from a local image with the same reference, as docker's
`--pull missing` has it) followed by `start`, with the flags of both; `--pull always` asks
the registry every time and `--pull never` never. A name that is taken is refused unless
`--force`, which makes the machine anew. What follows the image replaces an app's
command, as with docker (`nspawn run -it alpine sh`; after `--` as well). Like `docker
run` it stays attached: the
machine's output follows until it ends (stdout and stderr together, a line at a time,
from nspawn's journal, so that `logs` shows it later too; with `--log-driver none`
straight from the program, and nothing is kept), and `run` exits with the program's
exit code, or 128 plus the signal it died of. Ctrl-C, SIGTERM, SIGHUP and SIGQUIT go to
the program; a third Ctrl-C within a second leaves it running and returns. `-i` gives the
program this standard input, `-t` a terminal (`-it` for a shell), `--rm` removes the
machine once it ends (named volumes stay; an image it had to pull is kept under its own
name when that name is free, as docker keeps images, and the machine gets a name of its
own unless `--name` says one), and `-d` starts it in the background and
returns. A booted image shows its console until it powers off
(Ctrl-C powers it off); `run -it` on one waits for its boot, opens a root shell and powers
the machine off when the shell ends, with the shell's exit code. Closing the terminal of
`run -t` stops the machine, since nothing would read its terminal any more; `-d` and
`exec` are for machines that stay.

`pull` fetches the layers the store lacks three at a time, as docker does, and says
of each blob whether it was already present, is downloading or has been downloaded;
on a terminal a bar follows every transfer under way. An image of hub.nspawn.org is
verified before any layer comes down (see [Signed images](#signed-images)): `pull`
says who signed it, refuses it without a valid signature, and `--no-verify` skips the
check for that command, on `run` and `create` too.

`build` runs `mkosi` in the given directory with `--format=oci`, so the same
`mkosi.conf` tree that works on its own works here; `--distribution`, `--release`,
`--profile` and anything after `--` are passed through. The result is stored like a pulled
image (blobs, manifest, assembled machine) and can be started right away or pushed.

## Machines and apps

Images that ship an init system (systemd) and whose entrypoint is that init (or none) are
booted with `--boot`, like `machinectl start` does, and `shell` opens machined's login
session there. Any other image, for example anything from Docker Hub, is an "app":
its entrypoint runs under nspawn's stub init, with the environment, working
directory, user and stop signal from the OCI config, and joins the bridge network like
any other machine (see Networking). An image whose own `/init` supervises it, as the
s6-overlay of linuxserver.io's images, Home Assistant's and others, runs as an app with
that program as PID 1 of the machine, which s6 insists on, as docker runs every
program; `--init=false` asks the same of any app, `--init` puts the program back under
the stub init. A program that is PID 1 ignores the signals it has no handler for, as
under docker, so `stop` kills it after its timeout when it does not handle its stop
signal. The arguments given to `create` or `start` after `--`
replace the image's cmd and follow its entrypoint, exactly as with docker (`nspawn start
web -- nginx -T` still runs `/docker-entrypoint.sh` first); `--entrypoint PROGRAM` replaces
the entrypoint and `--entrypoint ""` drops it. Both are remembered, like the command of a
docker container; `start --image-command` goes back to the image's own. `-e VAR=value`
adds environment on top of the image's (`-e VAR` copies it from your shell), and
`-v SOURCE:TARGET[:ro]` mounts a host directory (which has to exist), or a named volume
that nspawn keeps under `/var/lib/nspawn/volumes/NAME` and creates on first use with
what the image has at that path, owner and files included, as docker seeds one, into
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

### Restart policies, limits and stats

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
which counts as a stop. Right after a start, `kill` waits a few seconds at most for the
program to be running, so that the signal is not taken by systemd-nspawn's child, which
would end the machine before the program ran. `-m/--memory`, `--cpus` and `--pids-limit`
bound the whole machine (its unit's MemoryMax=, CPUQuota= and TasksMax=), which is why
they cannot be seen from inside; as with docker, `--memory` also lets the machine use as
much swap again (MemorySwapMax=), and no more, unless `--memory-swap` gives memory and
swap together: equal to `--memory` for no swap, larger for more, `-1` for swap without a
bound (it needs `--memory`, and `0` goes back to the default). They are remembered like the ports and apply at the
next start; `--restart no` and a limit of 0 remove them. `update` changes them without a
start, like docker update: a running machine gets the new limits in its cgroup at once.
`stats` shows what each running machine uses against them, its unit's cgroup read every
second (`--no-stream` for one reading, `--json` for scripts). With a policy, `stop --no-wait`
of an app also lets the stub init send the program SIGTERM and SIGHUP, since nobody
stays to stop the unit later.

### Healthchecks

Healthchecks are docker's: an image's `HEALTHCHECK`, or `--health-cmd` with
`--health-interval`, `--health-timeout`, `--health-retries`, `--health-start-period` and
`--health-start-interval` on `run`, `start`, `create` and `update` (`--no-healthcheck`
turns the image's off), run a probe inside the machine at the interval; `ps` shows
`(healthy)`, `(unhealthy)` or `(health: starting)` next to the state, `inspect` the last
probes with their output, and `events` a `health_status` on every change. The probes run
from a unit of their own (`nspawn-health-NAME.service`) bound to the machine's, so they
go with it.

### Dependencies

`--depends-on NAME[:CONDITION][:optional]` on `run`, `create` and `start` is compose's
`depends_on` for one machine: the other machine is started first, and this one waits for
what the condition asks of it. `started`, the default, asks that its start went through;
`healthy` that its healthcheck says healthy (it needs one, and an unhealthy verdict fails
the start); `completed` that its program ended with 0, for a machine that prepares
something and exits, such as a migration, which then runs again at every start of the
machines that depend on it, as with `docker compose up`. With `:optional` a failure of
the other machine is a warning and this one starts anyway. The flag is repeatable and
remembered, `--depends-on none` forgets them; the machines named must be nspawn's, and a
cycle is refused.

systemd does the work: the machine's unit wants the other one's and is ordered after it,
and its first hook waits for the condition, so `machinectl start`, a start at boot and a
restart policy follow the same order as `nspawn start`, which says which machines it
started first. A start that waits for `healthy` or `completed`, directly or through the
machines its dependencies depend on, has no time limit, since it lasts as long as the
other machine takes; `nspawn stop` ends it. A machine on another one's network
(`--network container:NAME`) that also depends on it gets it started first, at boot as
by `nspawn start`. Stopping a machine leaves the ones that depend on it running, as
compose does, and `rm` refuses a machine others depend on unless they go in the same
command, and then only once they are gone.

### The other flags of docker run

The other flags of `docker run` are there too, remembered like the rest: `--hostname`,
`-u/--user USER[:GROUP]` and `-w/--workdir` (instead of the image's; the user is
resolved from the image's passwd and group files by a stand-in for getent that nspawn
binds, since busybox has no getent, musl's answers no initgroups and neither takes a
uid the passwd does not list, as docker does; an image without a shell (distroless,
scratch) gets a small program in its place, so `-u 1000:1000` runs there too, and one
without `/usr`, which systemd-nspawn wants, gets an empty one; a group, a name of the image's group
file or a number, becomes the primary and only group, as with docker, and a name the
image lacks is refused), `--cap-add`, `--cap-drop` (on top of the default set: docker's
and SYS_BOOT for an app on a bridge network, which runs without a user namespace,
systemd-nspawn's for a machine in one; `--cap-drop ALL --cap-add NET_BIND_SERVICE` keeps that one, as
with docker) and `--privileged`,
`--read-only`, `--tmpfs PATH[:OPTIONS]`, `--shm-size`, `--device
HOST[:CONTAINER[:rwm]]` (a node, or a directory such as `/dev/dri`, whose nodes are
allowed one by one), `--dns` and `--dns-search`, `--add-host HOST:IP` or `HOST=IP` (with
`host-gateway`; the second form for IPv6 addresses), `--ulimit NAME=SOFT[:HARD]` (numbers or `unlimited`), `--oom-score-adj`, `--stop-signal` and
`--stop-timeout` (what `stop` uses unless `-t` says otherwise), `--timezone MODE`
(systemd-nspawn's `Timezone=`: its default, `auto`, points `/etc/localtime` at the host's
zone at every start, and `off` leaves the machine's own, so a zone set inside with
`timedatectl` survives a restart; an app also takes `-e TZ=`), `--log-driver DRIVER`
(see "Where the output goes" below), `--init` and `--init=false` (the program under
the stub init, which reaps orphans and turns a poweroff into SIGTERM, or as PID 1; see
"Machines and apps"), `--sysctl` (`net.*` keys, set in an app machine's network
namespace) and `--interface IFACE`, which has no docker counterpart: a network
interface of the host moved into the machine while it runs (see "Physical
interfaces" under Networking). A `--tmpfs` that lands on `/run` (`/var/run` in most images) is left out
with a note: `/run` is a tmpfs of every machine already. `--hostname` reaches a booted
machine as its `/etc/hostname`. Each becomes a line of the machine's settings file or of its unit; `inspect`
shows them all. A path inside the machine, the target of a volume or a secret, a
`--tmpfs` or a `--device` path, must be plain: a `.` or `..` component is refused,
since it would land elsewhere once mounted. A path the image declares as a volume and nothing is mounted over gets a
note at start: nspawn has no anonymous volumes, so what is written there goes with the
machine.

### Secrets

Secrets are docker's too, without a swarm: `nspawn secret create db-password` reads the
content from standard input (or `--file`) and keeps it encrypted with `systemd-creds`,
bound to the host's TPM2 where there is one and to its credential key otherwise;
`--secret db-password` on `run`, `start` or `create` gives the machine the plaintext at
`/run/secrets/db-password`, a read-only file of root's with mode 0444
(`NAME:TARGET:MODE:UID:GID` says otherwise), decrypted into a tmpfs of root's alone for
as long as the machine runs. `secret ls` shows who takes each one, `secret inspect` what
is known about it (never the content), and `secret rm` refuses one a machine still
takes. Not on mstack machines yet.

### Events

`events` is docker events: what happens to machines (`start`, `die` with its exit code,
`stop`, `restart`, `oom`, `fail`, `health_status`, and nspawn's own `pull`, `build`,
`create`, `push`, `kill`, `update`, `pause`, `unpause`, `remove`), networks, volumes and
secrets, as it happens or between `--since` and `--until`, filtered by name, type, event
or label, as text or `--json`. Every event of a kind carries the same metadata whatever
the action, so a `remove` says as much as a `create`: `image=` on a machine's (and its
labels while it has a record), `subnet=`, `interface=` and `internal=` on a network's,
`path=` on a volume's, `size=` on a secret's. It reads
the journal, so machines started by `machinectl`, at boot or by a restart policy are
there too, and so is what happened while nobody watched.

### Where the output goes

What a machine prints goes, by default (`--log-driver local`), to a journal of
nspawn's own: journald's `nspawn` namespace, kept apart from the system's journal, so a
program that writes a lot never floods the host's logs. `logs` and an attached `run`
read it, with `-f`, `-n`, `--since` and timestamps as before, and
`journalctl --namespace=nspawn` shows it all. As with docker, `logs` shows what this
machine wrote, from its creation on: a removed machine of the same name, whose lines
are still in the journal, does not show, even with an earlier `--since`. The namespace holds 1 GiB at most, the
oldest lines going first (`/usr/lib/systemd/journald@nspawn.conf`, which
`/etc/systemd/journald@nspawn.conf` or a drop-in overrides); the limit is the
namespace's, shared by every machine, and a removed machine's lines stay until they age
out. Machines of earlier versions write there from their next start, and `logs` still
shows what they wrote to the system's journal before. `--log-driver journal` keeps a
machine in the system's journal as before, and `--log-driver none` drops the output:
`logs` refuses such a machine, an attached `run` of an app still shows the output,
straight from the program, and a booted machine's console goes nowhere, so it runs
with `-d`. Like docker's, the driver is a per-machine setting, remembered.

### exec, shell, stop and the rest

`exec` enters the machine's namespaces for both kinds of machine, like docker exec: the
exit code comes back, the image's environment applies and nothing is needed inside (no
D-Bus, no PAM). The command runs with the machine's capabilities and resource limits,
like its own processes, except CAP_SYS_BOOT in an app that shares the host's user
namespace (unless `--privileged`): the machine's processes keep it beside a seccomp
filter that keeps kexec out, which the command does not carry; right after a start it waits, a few seconds at most, until
systemd-nspawn has finished confining the machine. It attaches a terminal when standard input and output are one
(`-t` and `-T` decide otherwise), so redirected output is byte-exact, and `-u USER[:GROUP]` takes
names or numbers of the image's passwd and group files, with the home of the passwd
entry and the user's supplementary groups unless a group is given, which is then the
only one; a uid the passwd lacks runs with group 0 and `/` as home, as with docker and
as the machine's own program does. `shell` opens machined's login session on booted machines and a plain
shell on apps. `stop` sends the image's stop signal to the program of an app machine and
SIGKILLs it after `--timeout` seconds, or asks a booted machine to power off; `--force`
kills at once. `restart` is a stop and a start with the remembered options, `pause` and
`unpause` freeze and thaw a machine's cgroup like docker pause (`ps` shows it paused),
`top` lists a machine's processes, and `inspect` of a stopped machine shows the exit code
of its last run. Stopping a machine that already ended is not an error: it only drops what
the machine left behind.

### The unit hooks

Every machine's unit gets a drop-in that calls nspawn around its life (ExecStartPre,
ExecStartPost, ExecStopPost), so `machinectl start`, an enabled unit at boot, a program
that exits on its own or a crash all prepare and release the network the same way as
`nspawn start` and `nspawn stop`. The drop-in names the nspawn binary that wrote it, so
install nspawn where a system service may run it (`/usr/bin` or `/usr/local/bin`; on
SELinux hosts a binary below a home directory is refused with "Permission denied").

### Labels

Labels work as in docker. An image's own labels (`LABEL` in a Containerfile,
`OciLabels=` in a mkosi.conf) are read from its OCI config when it is pulled or built,
and `--label KEY=VALUE` on `start` or `create` adds labels of the machine's own on top,
remembered like the ports (`--label none` forgets them). nspawn itself does nothing
with them; `inspect` and `ps --json` show both the merged `labels` and the
`image_labels`, for tools such as a reverse proxy that configures itself from them.

### cp

`cp` copies files and directories between the host and a machine, running or not (a
stopped mstack machine excepted: its tree only exists while it runs), with docker cp's
rules: an existing directory receives the source under its own name, anything else is
the name of the copy, and `DIR/.` copies the contents of DIR. What goes in belongs to
root inside the machine, whatever user namespace it runs in; what comes out belongs to
whoever ran `cp`, because the command line writes it. Paths inside the machine are
resolved inside it, so a link there, absolute or not, never leads to the host; links
are copied as links, devices, sockets and fifos are left out, and so are the kernel's
file systems mounted inside a running machine (`/proc`, `/sys` and the like). The service and the
command line exchange a tar stream, as docker's API does. Where SELinux enforces, a host
directory mounted with `-v` keeps its own label, which the service may not be allowed to
write (docker needs `:z` for the same); named volumes are nspawn's and always work.

### One image, many machines

One image, as many machines as you like: `nspawn create SOURCE NAME` makes another
machine from an image that is already local, without touching the registry (a reference
that is not local is pulled first, under its own name and with the same signature check,
as `run` does). It shares the
source's layers and gets a writable layer, an address, settings and ports of its own
(`-p`, `--network`); removing one never affects the others. A pulled image is a machine
too, so `nspawn rm NAME` and `nspawn images rm NAME` remove the same thing: the record,
the tree, the unit files and whatever layers nobody else uses. `rm --force` stops a
running machine first (SIGKILL, like `docker rm -f`) where both refuse otherwise. A
name with nothing behind it (no record, no image machined knows, no leftovers of a
failed install) is refused with `no machine or image named NAME`; the other names of
the same command are still tried. `pull` with `--name`
ends up the same way but resolves the manifest through the registry first.

### The same machines on another host

`nspawn generate [NAME...]` prints the commands that make machines again, on another
host or on this one after a reinstall: a shell script with `network create` for the
networks they join, then `run -d` for each machine with every flag it keeps (networks
and aliases, ports, entrypoint and command, environment, volumes, labels, restart
policy, limits, healthcheck, docker's other flags and `--depends-on`), each after the
machines it depends on or takes its network from, which come along when they are not
named; an image without flags of its own is pulled again instead, unless another
machine takes its network, which needs it running. Each `run -d` pulls its machine on
its own (`--pull always`, blobs already there are not downloaded again), so that a
machine never takes the network kind, mode or backend of another one made from the
same image. Without names it covers every machine nspawn has. Run on the other host, the script starts the machines
as they were brought up here. What a flag cannot carry is said in comments at the top:
a secret is encrypted for this host, so `nspawn secret create` it there first; a host
directory a machine mounts has to exist there; an image built here has to be pushed
first. The contents of named volumes and of a machine's writable layer, and its
address on a network, stay behind: volumes are seeded again from the image, and the
addresses are handed out anew.

### Completions, the man page and JSON

Completions for bash, zsh and fish come with the packages; from a build of your own,
`nspawn completions bash > ~/.local/share/bash-completion/completions/nspawn` (or the
equivalent for your shell). Besides commands and flags, TAB completes names, asked
from the service as it goes: the running machines after `stop`, `exec`, `kill`,
`pause`, `top`, `shell` and `stats`, the images that do not run after `start`, every
machine and image after `rm`, `inspect`, `logs`, `restart`, `update` and `images rm`,
the local images after `create` and `push`, their references after `run`, the
networks after `--network`, `network rm` and `network inspect`, and the volumes after
`volume rm`. The names are all the service hands out without asking polkit, so this
works for any user and under `sudo` without a password; secrets are not completed.
Flags come once the word starts with `-`, or where there is no name to offer.

`ps`, `machines ls`, `images ls`, `network ls` and `volume ls` take `--json` and print what the
service answered instead of a table, and `nspawn inspect NAME...` prints the whole
record of machines or images (running or not) as a JSON array, like docker inspect:
what a script or an agent reads. The keys are the ones of the D-Bus interface (see
[DBUS.md](DBUS.md)), so the command line and the bus never disagree.

## Compose

`nspawn compose` runs the files of docker compose with docker compose's commands
and flags, in the same order and with the same output: `nspawn compose up -d` in a
directory with a `compose.yaml` brings its services up, `nspawn compose down` takes
them away. It is a client of the service like every other command, so it takes the
same permissions (`sudo`, or a polkit rule), and what it makes is nspawn's own:
`nspawn ps` lists the machines of a project, `nspawn network ls` its networks.

A service becomes a machine per replica, named as docker names its containers,
`PROJECT-SERVICE-N` with `_` written `-`; its image is pulled from Docker Hub when it
names no registry and kept under its local name; its networks, volumes, secrets and
`depends_on` are nspawn's networks, volumes, secrets and `--depends-on`. The machines
carry docker compose's labels, which is how `ps`, `logs`, `stop`, `down` and the rest
find a project again, from its files or from `-p NAME` alone. A machine is made anew
when its service changed (docker compose's config hash) or its image did.

What nspawn cannot do, a `build` without an image, a fixed `ipv4_address` and the
rest of the table in [COMPOSE.md](COMPOSE.md), `config` shows as warnings, and `up`,
`create`, `run` and `scale` refuse for the services they start. COMPOSE.md has every
attribute and option with what nspawn compose does with it, and where it differs
from docker compose on purpose.

## Registries and credentials

Every command talks to the `org.nspawn` service on the system bus, which decides
through polkit who may do what: `org.nspawn.inspect` for the commands that only look,
`org.nspawn.manage` for the rest, both for administrators by default. `sudo nspawn ...`
therefore works everywhere; to drive nspawn without a password, hand one of those
actions to a group with a polkit rule, as in `nspawn-wheel.rules` in the documentation
directory. Doing that makes the group administrators of the host, since a machine's
commands run as root and any host path can be mounted into one. The service keeps
running the binary it started with: after an upgrade it is the old one until it goes
idle (a minute without work) or is restarted, so a command whose version differs from
the service's warns and says to run `sudo systemctl restart nspawn.service`; the rpm
and the deb restart it on upgrade themselves.

### Signed images

Every image on hub.nspawn.org is signed twice by its build workflow
(nspawn/mkosi-definitions) with cosign: with the project's key, whose public half
(`cosign.pub` there) is built into nspawn, and keyless, with the workflow's own
identity through Sigstore (`https://github.com/nspawn/mkosi-definitions/.github/workflows/mkosi.yml@refs/heads/master`,
issued by GitHub). The signatures are referrers of the image on the registry: Sigstore
bundles that carry the certificate or the key's hint, the transparency log entry and a
timestamp, so `pull` verifies them offline, with nothing but the registry consulted,
against the Sigstore trusted root the binary embeds. One of the two has to verify, and
the check comes before a single layer is downloaded:

```
$ sudo nspawn pull fedora:44
hub.nspawn.org/fedora:44: signature verified (key 6wiWMtJZCUkV, keyless https://github.com/nspawn/mkosi-definitions/.github/workflows/mkosi.yml@refs/heads/master)
hub.nspawn.org/fedora:44: manifest 9240778b2c77 with 1 layer(s), assembling as overlay
...
```

An image without a signature (a tag dated before the signing began, or one pushed by
hand with `nspawn push`) is refused with `carries no signature`, and so is one whose
signatures do not verify, with the reason of each. `--no-verify` on `pull`, `run` and
`create` skips the check for that command, like docker's `--disable-content-trust`;
`create` checks only an image it has to pull, never a local source. `inspect` and
`GetImage` show who signed (`signed_by`: `key <hint>, keyless <identity>`) and when
(`signed_at`), which a machine made with `create` inherits from its source.

Images of other registries are pulled unverified until `/etc/nspawn/nspawn.toml` says
what they must carry, one table per registry (host, or host:port):

```toml
[registries."registry.example.com"]
key = "/etc/nspawn/keys/example.pub"      # a cosign public key (PEM); keys = [...] for several
identity = "https://github.com/org/repo/.github/workflows/build.yml@refs/heads/main"
issuer = "https://token.actions.githubusercontent.com"   # goes with identity
required = true            # default: no verifying signature fails the pull
rekor = true               # default: the signature's transparency log entry must verify
trusted_root = "/etc/nspawn/trusted_root.json"   # another Sigstore deployment; the public one otherwise
```

A signature verifies when it was made with one of the keys, or keyless with the
identity and issuer; a table names at least one of the two. `required = false` turns a
missing signature into a note and lets the pull go on, while a signature that is there
but does not verify still fails it. `rekor = false` trusts the key or certificate alone,
without the log entry (a private deployment that keeps no log); a bundle still has to
carry a log entry or a timestamp, which cosign always adds. A table for
`hub.nspawn.org` replaces the built-in policy, and `verify = false` in a table turns
verification off for that registry, the hub included:

```toml
[registries."hub.nspawn.org"]
verify = false
```

The policies are the service's: what the command line was given with `--registry`
chooses the registry, never the policy. A key or a trusted root the service cannot
read is reported when it starts (`journalctl -u nspawn`) and fails the pulls of that
registry.

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

### User-defined networks

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

`--network container:NAME` puts an app machine in the network namespace of NAME, as
docker's `--network container:` does: the same interfaces and address, NAME's hosts
and resolv.conf files, nothing of its own, and a port its program serves is published
with `-p` on NAME. NAME has to be running on a bridge network when the machine starts
(`--depends-on NAME` has it started first), and cannot be removed while a machine
names it, unless that machine goes in the same `rm`; when NAME stops or restarts, the
machine keeps the namespace it joined and has to be restarted to join the new one, as
with docker. `-p`, `--network-alias`, `--dns`, `--dns-search`, `--add-host` and
`--sysctl` are refused on such a machine, since they shape a network of its own, and
so is a booted image, whose systemd would configure the shared interfaces again. `ps`
and `inspect` show it as `container:NAME`.

### App machines

App images have nothing inside to configure `host0`, so for them nspawn builds the
network namespace before the process starts (`ip netns`, a veth pair on the bridge, the
address and the default route) and hands it to systemd-nspawn with `NamespacePath=`,
together with a generated `/etc/resolv.conf`. The process finds its network ready from
the first instruction, as in docker, and the namespace goes away with `stop`. One
consequence: a user namespace cannot join a network namespace that belongs to the host,
so app machines on the bridge run without one (`PrivateUsers=no`), which is also docker's
default; capabilities, seccomp and the other namespaces still apply. Such an app keeps
docker's default capabilities (CHOWN, DAC_OVERRIDE, FOWNER, FSETID, KILL, MKNOD,
NET_BIND_SERVICE, NET_RAW, SETFCAP, SETGID, SETPCAP, SETUID, SYS_CHROOT, AUDIT_WRITE)
and SYS_BOOT, so that a reboot asked from inside ends the machine as it does in a
booted one (in a pid namespace of its own it can only signal the machine's init, and
the kexec system calls are filtered out), rather than systemd-nspawn's set, whose
CAP_SYS_ADMIN is root on the host outside a user namespace: `/proc/sys` is one remount away from writable, and `kernel.core_pattern`
runs what it names as the host's root. `--cap-add` puts one back (`NET_ADMIN` for a VPN
client, and it is kept on its own with `--interface`), `--privileged` all of them. For
the same reason app images are assembled with the overlay backend even where mstack is
available.

### Published ports

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

### Firewalls

Hosts running docker (in its default iptables mode) or ufw have a FORWARD policy of
DROP; `start` then adds two rules to the DOCKER-USER chain, which docker reserves for
that, or to FORWARD itself: anything out of the bridge, and into the bridge only what was
published or belongs to a connection a machine opened. That needs the `iptables` command,
which those tools bring with them; where the rules are there and the command is not, or
where a hand-written nftables firewall drops forwarded traffic, `start` says so and the
exception has to be made by hand. Without it the machines reach nothing beyond the bridge
and published ports answer on the host alone.

### host and veth

`--network host` shares the host's network instead, and `--network veth` keeps the classic
systemd-nspawn setup: a virtual ethernet pair configured by systemd-networkd on the host
through `80-container-ve.network`. In that mode `start` activates systemd-networkd when
the host has no `.network` files of its own and refuses with an explanation otherwise,
and binds `ve-<name>` to firewalld's trusted zone while the machine runs.

### Physical interfaces

`--interface IFACE` on `run`, `create` and `start` gives a machine a network interface
of the host, whole: a second ethernet port, or a wifi adapter for a machine doing
wireless work, which `--device` cannot do (a network interface is not a node under
`/dev`) and `--network host` cannot either (the machines run in a user namespace, which
may not configure the host's interfaces). The interface is moved into the machine's
network namespace before its program or init runs and is back on the host when the
machine stops or is removed, with its name kept on both sides, as systemd-nspawn's
`Interface=` and LXC's `phys` type do it. It arrives down and unconfigured, as the
kernel moves it: the machine brings it up and configures it, with `ip` in an app or its
systemd-networkd in a booted machine. `--interface` is repeatable and remembered like
the other flags; `--interface none` forgets them; `inspect` lists them as
`interfaces`.

Where systemd-nspawn makes the machine's network namespace (a booted machine, on the
bridge or with veth, and any machine with `--network none`) the interface goes into the
machine's settings file as `Interface=` and systemd-nspawn moves it at start and back
at exit. An app machine on a bridge network gets its namespace from nspawn itself
(`NamespacePath=`, which allows no `Interface=` beside it), so the unit's hooks move the
interface in with `ip` before systemd-nspawn runs and give it back after; such an app
runs without a user namespace, so `iw reg set` and monitor mode work inside as on the
host. Either way the unit wants and waits for the interface's device unit, so that a
machine started at boot waits for a USB adapter udev has not seen yet.

A wifi adapter cannot leave its phy, so the phy moves whole, every interface it carries
with it (nspawn says which), and a driver without namespace support (`ath6kl`,
`wilc1000`) refuses the move naming the phy. An app on the bridge takes a wireless
interface on any systemd, through `iw`, which has to be installed; `Interface=` moves a
phy since systemd 256, so on an older host a wireless interface is refused for a
booted machine or one with `--network none`, with a message that says so. Regulatory
domain (`iw reg set`) and rfkill are host-wide: they are set on the host, or from an
app on the bridge; `--device /dev/rfkill` hands the switch to such an app.

One machine at a time takes an interface: a second one naming it is refused while the
first runs (`stop` it, or start it with `--interface none`). Refused as well: `lo`, the
bridges and veth ends of nspawn's own, a port of a bridge or bond (take it off with
`ip link set IFACE nomaster` first), a name that is not on the host (a machine that
ended a moment ago may still hold it: the kernel gives interfaces back a little after
their namespace dies), a machine with `--network host` (it is on the host's interfaces
already), one with `--network container:NAME` (give the interface to NAME) and the
mstack backend. The interface of the host's default route is allowed with a warning,
since the host loses its route while the machine runs. When the interface comes back,
NetworkManager or systemd-networkd manage it again as before, and a name taken on the
host in the meantime makes the kernel rename it (`dev0`, `wlan0`). `stop --force` kills
the machine: an app on the bridge still gets every interface back from its release
hook, while a machine that took them through `Interface=` may not get to give them
back, and the kernel then returns a physical interface or a wifi phy a moment later
and destroys a virtual one (a VLAN, a macvlan, a dummy) with the namespace.
