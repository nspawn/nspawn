# Using nspawn

This page has the commands in the detail the README leaves out: what `run`, `start`
and the rest do, which docker flags they take and where they differ, how machines get
their network, and where credentials live. `nspawn COMMAND --help` and `man nspawn`
list every flag but the hidden ones kept for docker's command lines;
[DBUS.md](DBUS.md) has the bus interface the commands call, and
[ARCHITECTURE.md](ARCHITECTURE.md) how it is all built.

The documentation on [nspawn.org](https://nspawn.org/docs/) covers the same ground as
guides: [getting started](https://nspawn.org/docs/getting-started/),
[images](https://nspawn.org/docs/images/), [machines](https://nspawn.org/docs/machines/),
[networking](https://nspawn.org/docs/networking/),
[compose](https://nspawn.org/docs/compose/),
[building images](https://nspawn.org/docs/building/),
[the Docker API](https://nspawn.org/docs/docker-api/),
[configuration](https://nspawn.org/docs/configuration/), the
[command reference](https://nspawn.org/docs/reference/) and a
[FAQ](https://nspawn.org/docs/faq/).

## run and build

`run` is `pull` followed by `start`, with the flags of both; when an image of the same
reference is here, it is `create` from that image instead of the pull, as docker's
`--pull missing` has it. `--pull always` asks the registry every time and `--pull never`
never does. A name without a registry is looked for here under the configured registry
first, then as docker reads it (`app` is `docker.io/library/app`, the name of an image
compose pulled from Docker Hub), and only then pulled from the configured registry;
`create` looks for its source the same way.

A machine made from a local image is checked against the start's flags as it is made,
so what docker refuses before it makes a container (a mount point given twice, an app
with nothing to run) leaves no machine behind. A name that is taken is refused unless
`--force`, which makes the machine anew. What follows the image replaces an app's
command, as with docker (`nspawn run -it alpine sh`; after `--` as well).

Like `docker run`, `run` stays attached: the machine's output follows until it ends,
stdout and stderr together, a line at a time. It is read from the journal (nspawn's
own with the default `--log-driver local`, the system's with `journal`), so `logs`
shows it later too; with `--log-driver none` it comes straight from the program and
nothing is kept. `run` exits with the program's exit code, or 128 plus the signal it
died of. Ctrl-C, SIGTERM, SIGHUP and SIGQUIT go to the program; a third Ctrl-C within a
second leaves it running and returns.

Only a signal nspawn sent itself is known to be one: Ctrl-C without `-t`, `kill`,
`stop`. systemd-nspawn ends with 255 whatever signal killed a program under its stub
init, and with 1 for a program that is PID 1, so a crash, a kill from inside the
machine or Ctrl-C with `-t` (the machine's terminal raises that one) gives 255 or 1
where docker gives 128 plus the signal.

- `-i` gives the program this standard input, `-t` a terminal (`-it` for a shell).
- `--rm` removes the machine once it ends. Named volumes stay; an image it had to pull
  is kept under its own name when that name is free, as docker keeps images, and the
  machine gets a name of its own unless `--name` says one.
- `-d` starts the machine in the background and returns.

A booted image shows its console until it powers off (Ctrl-C powers it off); `run -it`
on one waits for its boot, opens a root shell and powers the machine off when the shell
ends, with the shell's exit code. Closing the terminal of `run -t` stops the machine,
since nothing would read its terminal any more; `-d` and `exec` are for machines that
stay.

`pull` fetches the layers the store lacks three at a time, as docker does, and says of
each blob whether it was already present, is downloading or has been downloaded; on a
terminal a bar follows every transfer under way. An image of hub.nspawn.org is verified
before any layer comes down (see [Signed images](#signed-images)): `pull` says who
signed it, refuses it without a valid signature, and `--no-verify` skips the check for
that command, on `run` and `create` too. Docker Hub's names read as docker reads them:
`docker.io/alpine` is `docker.io/library/alpine`, and `index.docker.io` is `docker.io`.
A reference by digest takes a sha256 digest; sha384 and sha512 ones are refused, in a
Dockerfile's `FROM` before anything is built.

`build` builds a Dockerfile the way `docker build` does, or runs mkosi on an mkosi
configuration. A directory with a `Dockerfile` (or `dockerfile`, `Containerfile`), `-f`
naming one, `-` on standard input or any of the Dockerfile flags below make it a
Dockerfile build; a directory with `mkosi.conf` or `mkosi.conf.d` and no Dockerfile, or
the mkosi flags, an mkosi build. Either result is stored like a pulled image (blobs,
manifest, assembled machine) and can be started right away or pushed. A Dockerfile
build of a tag replaces the image it built or pulled under that reference before, as
long as no machine ran from it; `--force` replaces any other image of the name, an
mkosi build's included. `-f` is the Dockerfile, as with docker, so `--force` has no
short form.

### Dockerfiles

`nspawn build -t app:1 .` reads the Dockerfile as BuildKit's frontend does (a
`# syntax=` of docker/dockerfile:1, 1.N, 1.N.M or labs is taken, another frontend image
is refused) and runs it with a builder of nspawn's own. It takes:

- every stage the target needs (`--target`, the last one otherwise), multi-stage builds,
  `COPY --from`, `ARG` and `--build-arg`, `ONBUILD` and heredocs;
- `COPY --chown --chmod --parents --exclude --link`, and `ADD` of local archives (gzip,
  bzip2, xz, zstd or plain tar, unpacked) and of http(s) URLs (`--checksum`);
- `RUN --mount` of the cache, bind, tmpfs and secret kinds. `--secret id=ID,src=PATH`
  reads a file, `env=VAR` a variable, which wins over a file; with neither, the
  variable named ID when it is set, else the file. The command line reads them as the
  user;
- `RUN --network=none|host`, host only with `--allow network.host`;
- `--network`, `--add-host`, `--label`, `--shm-size`, `--ulimit`, `--pull` (the registry
  is asked for base images even when one with the reference is here), `--no-cache`,
  `--no-cache-filter`, `-q`, `--iidfile` and `--push` (the tags, once built).

The context is the directory given, packed by the command line as the user, everything
owned by root, as docker sends it, with what its ignore file excludes left out:
`NAME.dockerignore` next to the Dockerfile `NAME`, which wins, else the context's
`.dockerignore`. `-` takes a context tar, or a Dockerfile with no context, from standard
input; `-f` then names the Dockerfile in the tar, and is refused with a Dockerfile on
standard input, as buildx refuses it. An ignore file that cannot be read fails the
build, and so does a file of the context that fails partway through its reading; a file
the user cannot open, or a socket, goes in as an empty file, as BuildKit sends it. The
flags buildx keeps for compatibility and ignores (`--rm`, `--force-rm`, `-m`,
`--memory-swap`, `-c`, `--cpu-period`, `--cpu-quota`, `--cpuset-cpus`, `--cpuset-mems`,
`--compress`, `--squash`, `--isolation`, `--security-opt`) are taken and ignored here
too, with buildx's warnings; `--cache-from` is taken and ignored with a note, the build
cache being this host's.

Every step runs in a root of its own, an overlay of the layers so far, and what it
changes becomes a layer. A `RUN` runs under systemd-nspawn as PID 1, with the
confinement of an app machine (the default capabilities, the system call filter, no
user namespace, so that files keep their owners), on a network namespace of the build's
on the default bridge. Steps that ran before with the same inputs (the instruction, the
build arguments, the files of the context it copies) come from the build cache, so a
build taken whole from it gives the same image ID.

The output follows BuildKit's plain progress, a numbered line per step. Each output
stream of a step is clipped as BuildKit clips it: `[output clipped, log limit 2MiB
reached]` past 2 MiB, `[output clipped, log limit 200KiB/s reached]` past 200 KiB a
second since the step began, and the last 256 KiB shown if the step fails. A failing
step ends the build with BuildKit's text, `process "/bin/sh -c false" did not complete
successfully: exit code: 1`. A build whose command line goes away (Ctrl-C) stops, its
step killed and nothing of it left behind; two builds at once each run on their own.

`builder du` lists the build cache and `builder prune` removes the part no image uses
(`-a` all of it, `--filter until=`, `--keep-storage SIZE`); `system df` counts it and
`system prune` removes it too.

Differences from docker build:

- No SSH forwarding (`--ssh`), no output other than a local image (`--output`,
  `--cache-to`), no named contexts, no remote contexts or git sources, no attestations,
  only this host's platform, no `RUN --security=insecure` or `RUN --device`, no
  `--check` or `--call`, and no linter.
- Stages run one after another, and `COPY --link` copies as a plain `COPY`.
- `/run` is a tmpfs during a `RUN`, so what a step writes there is not kept, and
  systemd-nspawn adds `container=systemd-nspawn` to a step's environment.
- The mount points systemd-nspawn makes for a step (an empty `/etc/machine-id`, `/bin`
  and the like as links into `/usr` where the base had none) are taken out of its layer,
  even when the step made them itself.
- A layer's gzip blob differs from BuildKit's; the tar inside is the one BuildKit
  writes of the same tree.
- Each step starts from the layers as the image keeps them, so what a layer does not
  carry (ACLs and extended attributes other than file capabilities) is gone for the
  next step, where BuildKit keeps it until the image is written.
- FIFOs and device nodes of the context are left out of it.
- A Dockerfile is read as UTF-8: bytes that are not (Latin-1 text in a heredoc) become
  U+FFFD. A `.dockerignore` pattern with a Unicode class of Go's regular expressions
  (`\p`, `\P`) is refused.
- `ONBUILD` triggers of a base image that copy from a stage of this Dockerfile need that
  stage to be one the target needs already.

### mkosi

`build` runs `mkosi` in the given directory with `--format=oci`, so the same
`mkosi.conf` tree that works on its own works here; `--distribution`, `--release`,
`--profile` and anything after `--` are passed through, and `-t` names the result.

## Machines and apps

An image that ships an init system (systemd) is booted with `--boot`, like `machinectl
start` does, when its command (the entrypoint, else the cmd) is that init or when it has
no command at all; `shell` opens machined's login session there. Any other image, for
example nearly anything from Docker Hub, is an "app": its program runs under
systemd-nspawn's stub init, with the environment, working directory, user and stop
signal from the OCI config, and joins the bridge network like any other machine (see
Networking). The `NOTIFY_SOCKET` systemd-nspawn hands its payload is set empty for an
app: a program that notifies systemd takes an empty one for none, instead of failing on
a socket only root may write to.

An image with s6-overlay (found by its files: linuxserver.io's images, Home Assistant's
and others) runs as an app with its `/init` as PID 1 of the machine, which s6 insists on
and which is how docker runs every program. `--init=false` asks the same of any app,
and `--init` puts the program back under the stub init. A program that is PID 1 ignores
the signals it has no handler for, as under docker, so `stop` kills it after its timeout
when it does not handle its stop signal.

The arguments given to `create` or `start` after `--` replace the image's cmd and follow
its entrypoint, exactly as with docker: `nspawn start web -- nginx -T` still runs
`/docker-entrypoint.sh` first. `--entrypoint PROGRAM` replaces the entrypoint and drops
the image's cmd, so only the arguments given after it follow it (`--entrypoint /bin/sh`
alone is a shell), and `--entrypoint ""` drops it, so that the arguments given run
alone. An app left with nothing to run (an empty entrypoint without arguments, or an
image with neither an entrypoint nor a cmd) is refused with docker's `no command
specified`. Both are remembered, like the command of a docker container; `start
--image-command` goes back to the image's own.

`-e VAR=value` adds environment on top of the image's (`-e VAR` copies it from your
shell). `-v SOURCE:TARGET[:ro]` mounts a host directory, which has to exist, or a named
volume, into any kind of machine. nspawn keeps a named volume under
`/var/lib/nspawn/volumes/NAME` and creates it on first use with what the image has at
that path, owner and files included, as docker seeds one. In machines that run with
private users the mount is idmapped, so a file root writes inside belongs to root on
the host, and the other way round. `-e none` and `-v none` forget them.

Every booted machine with volumes gets a small unit mounted into it,
`nspawn-volumes.service`, that holds `local-fs.target` until all of them are mounted and
fails visibly otherwise, so services find their configuration and data in place
whatever the backend. On overlay and flat the volumes are mounted before the machine's
init starts (they come from the settings file); mstack machines get them attached from
the host right after their init starts, since systemd-nspawn cannot idmap binds under
managed user namespaces, which is what the unit waits for.

Named volumes outlive the machines that use them, as in docker: removing a machine
keeps them and says so. `nspawn volume ls` lists them with the machines whose records
mount them, `volume create NAME` makes one ahead of its first use, and `volume rm` and
`volume prune` remove the ones no machine uses; a volume still named by a machine is
refused until that machine is started with other volumes (or `-v none`) or removed. A
volume made ahead is filled from the image of the first machine that takes it while it
is empty, as docker fills it, but not while another machine that mounts it runs, where
docker fills it anyway: the service would be writing where that machine can change
things under it.

### Restart policies, limits and stats

`--restart` takes docker's policies. `on-failure` restarts the machine when its program
or its init dies with an error (a count, `on-failure:N`, is taken and not kept, since
systemd restarts on every failure), `always` whenever it ends and also at boot, and
`unless-stopped` like `always` until `nspawn stop`, which takes the boot start back
until the next `start`. Restarts come after a second, then later and later, up to half
a minute, for as long as they keep failing, and `ps` shows such a machine as
`restarting`. A `poweroff` inside the machine is an ending like any other, which
`always` and `unless-stopped` restart.

`nspawn stop` always stops the machine, one waiting to be restarted included (an
`always` machine still starts at the next boot). With a policy, `stop --no-wait` of an
app queues the unit's stop right after the stop signal, so the stub init also sends the
program SIGTERM and SIGHUP: nothing stays to stop the unit later. `machinectl stop`
stops it too but does not take an `unless-stopped` machine off the boot list.

`kill` works as docker's. SIGKILL, its default, stops the machine for good. Any other
signal goes to the program (or the init of a booted machine) and, as with docker, counts
as a stop that keeps the policy from bringing the machine back, unless the machine has
a stop signal of its own (its image's `STOPSIGNAL` or `--stop-signal`) and this is
another one: then the policy decides, should the signal end it. A booted machine's halt
and poweroff requests count as a stop; other signals leave it to the policy. Right after
a start, `kill` waits a few seconds at most for the program to be running, so that the
signal is not taken by systemd-nspawn's child, which would end the machine before the
program ran.

`-m/--memory`, `--cpus` and `--pids-limit` bound the whole machine (its unit's
MemoryMax=, CPUQuota= and TasksMax=), which is why they cannot be seen from inside. As
with docker, `--memory` also lets the machine use as much swap again (MemorySwapMax=),
and no more, unless `--memory-swap` gives memory and swap together: equal to `--memory`
for no swap, larger for more, `-1` for swap without a bound (a size needs `--memory`,
and `0` goes back to the default). They are remembered like the ports and apply at the
next start; `--restart no` and a limit of 0 remove them. `update` changes them without a
start, like docker update: a running machine gets the new limits in its cgroup at once,
and an unless-stopped machine stopped by hand stays off the boot list until its next
start, its policy given again or not. `stats` shows what each running machine uses
against them, its unit's cgroup read every second (`--no-stream` for one reading,
`--json` for scripts).

### Healthchecks

Healthchecks are docker's: an image's `HEALTHCHECK`, or `--health-cmd` with
`--health-interval`, `--health-timeout`, `--health-retries`, `--health-start-period` and
`--health-start-interval` on `run`, `start`, `create` and `update` (`--no-healthcheck`
turns the image's off), run a probe inside the machine, as the machine's user, at the
interval. `ps` shows `(healthy)`, `(unhealthy)` or `(health: starting)` next to the
state, `inspect` the last probes with their output, and `events` a `health_status` on
every change. The probes run from a unit of their own, `nspawn-health-NAME.service`,
which requires the machine's to be running and leaves once it is gone. Timings without
a command on an image that has no healthcheck are kept and probe nothing, as with
docker; an image's test that no probe can run (`["CMD"]` alone) counts as none, so a
`:healthy` dependency on it fails at once instead of waiting.

### Dependencies

`--depends-on NAME[:CONDITION][:optional]` on `run`, `create` and `start` is compose's
`depends_on` for one machine: the other machine is started first, and this one waits for
what the condition asks of it.

- `started`, the default: its start went through.
- `healthy`: its healthcheck says healthy. It needs one, and an unhealthy verdict fails
  the start.
- `completed`: its program ended with 0, for a machine that prepares something and
  exits, such as a migration. It runs until one run of it succeeds: it runs first only
  when no run of it ended with 0 yet (a run of an earlier boot counts by the exit code
  the journal kept), not at every start or restart of the machines that depend on it.
  `nspawn start` of it runs it again.

With `:optional` a failure of the other machine is a warning and this one starts anyway.
The flag is repeatable and remembered, and `--depends-on none` forgets them; the
machines named must be nspawn's, and a cycle is refused.

systemd does the work: the machine's unit wants the other one's and is ordered after it
(after a `completed` one it is only ordered, and the hook starts it when it has to run),
and its first hook waits for the condition. So `machinectl start`, a start at boot and a
restart policy follow the same order as `nspawn start`, which says which machines it
started or ran first. A start that waits for `healthy` or `completed`, directly or
through the machines its dependencies depend on, has no time limit, since it lasts as
long as the other machine takes; `nspawn stop` ends it.

A machine on another one's network (`--network container:NAME`) that also depends on it
has that machine started first, at boot too; without `--depends-on` its start needs the
other one running already. Stopping a machine leaves the ones that depend on it running,
as compose does, and `rm` refuses a machine others depend on unless they are removed in
the same command, and then removes it after them.

### The other flags of docker run

`--env-file FILE` and `--label-file FILE` read variables and labels from files of
KEY=VALUE lines as docker reads them (no quoting, no expansion, `#` comments, a KEY alone
copied from here for a variable and left out for a label), before the `-e` and `--label`
that follow and win. A variable's value may hold a tab but no newline or other control
character, which docker passes on: the settings file that carries the machine's
variables is read a line at a time.

`--mount type=bind|volume|tmpfs,source=...,target=...` is the long form of `-v` and
`--tmpfs` (`readonly`, `tmpfs-size`, `tmpfs-mode`). A volume needs its name, since nspawn
has no anonymous volumes; image, cluster and npipe mounts, `volume-subpath` and driver
options are refused.

`--ip ADDRESS` fixes the address on the primary network, `--ip NETWORK=ADDRESS` on
another one. The network must be one made with `network create` (docker refuses it on
its default network too), the address inside its subnet and nobody else's there, or the
start or create is refused as docker words it.

The other flags of `docker run` are there too, remembered like the rest:

- `--hostname`, which reaches a booted machine as its `/etc/hostname`.
- `-u/--user USER[:GROUP]` and `-w/--workdir`, instead of the image's. A group, a name
  of the image's group file or a number, becomes the primary and only group, as with
  docker; a name the image lacks is refused.
- `--cap-add` and `--cap-drop`, on top of the default set: docker's and SYS_BOOT for an
  app on a bridge network, which runs without a user namespace, systemd-nspawn's for a
  machine in one. `--cap-drop ALL --cap-add NET_BIND_SERVICE` keeps NET_BIND_SERVICE
  alone, as with docker.
- `--privileged` and `--read-only`.
- `--tmpfs PATH[:OPTIONS]`. Given without options it is mode 1777, as docker's, so a
  program of any user writes to it; unlike docker's it is not mounted `noexec,nosuid`,
  which systemd-nspawn cannot ask for. A `--tmpfs` on or below `/run` (`/var/run` in
  most images) is left out with a note: `/run` is a tmpfs of every machine already.
- `--shm-size`.
- `--device HOST[:CONTAINER[:rwm]]`: a node, or a directory such as `/dev/dri`, whose
  nodes are allowed one by one, or a CDI name such as `nvidia.com/gpu=all` (see
  [GPUs and CDI devices](#gpus-and-cdi-devices)), and `--gpus`.
- `--dns` and `--dns-search`.
- `--add-host HOST:IP` or `HOST=IP`, with `host-gateway`; the second form for IPv6
  addresses.
- `--ulimit NAME=SOFT[:HARD]`, numbers or `unlimited`.
- `--oom-score-adj`.
- `--stop-signal` and `--stop-timeout`, what `stop` uses unless `-t` says otherwise; -1,
  as with docker, for no SIGKILL at all.
- `--timezone MODE`, systemd-nspawn's `Timezone=`: its default, `auto`, points
  `/etc/localtime` at the host's zone at every start, and `off` leaves the machine's
  own, so a zone set inside with `timedatectl` survives a restart. An app also takes
  `-e TZ=`.
- `--log-driver DRIVER`, see [Where the output goes](#where-the-output-goes).
- `--init` and `--init=false`: the program under the stub init, which reaps orphans and
  turns a poweroff into SIGTERM, or as PID 1 (see
  [Machines and apps](#machines-and-apps)).
- `--sysctl KEY=VALUE`, `net.*` keys, set in an app machine's network namespace. docker's
  defaults come first there, `net.ipv4.ping_group_range=0 2147483647` and
  `net.ipv4.ip_unprivileged_port_start=0`, so that a user other than root has the ICMP
  echo sockets a ping without raw sockets uses and binds a port below 1024, as in a
  docker container; a `--sysctl` of either key wins.
- `--interface IFACE`, which has no docker counterpart: a network interface of the host
  moved into the machine while it runs (see [Physical interfaces](#physical-interfaces)).

systemd-nspawn resolves the user with getent inside the machine, which busybox lacks and
musl's cannot answer for initgroups or for a uid the image's passwd does not list, as
docker takes one. nspawn binds a stand-in that answers from the image's passwd and group
files, a small program in an image without a shell (distroless, scratch), so `-u
1000:1000` runs there too. An image without `/usr`, which systemd-nspawn wants, gets an
empty one.

Each flag becomes a line of the machine's settings file or of its unit; `inspect` shows
them all. A path inside the machine, the target of a volume or a secret, a `--tmpfs` or
a `--device` path, must be plain: a `.` or `..` component is refused, since it would
land elsewhere once mounted. A path the image declares as a volume and nothing is
mounted over gets a note at start: nspawn has no anonymous volumes, so what is written
there goes with the machine.

### GPUs and CDI devices

GPUs reach a machine through the Container Device Interface, as with docker and podman.
The vendor's tool writes a spec under `/etc/cdi` or `/var/run/cdi` that lists the device
nodes, libraries, variables and hooks each device needs (`nvidia-ctk cdi generate
--output=/etc/cdi/nvidia.yaml` for NVIDIA, `amd-ctk cdi generate` for AMD), and the
machine names the devices: `--device nvidia.com/gpu=all` (or `=0`, a UUID, whatever the
spec calls them), or docker's `--gpus all`, `--gpus 2`, `--gpus '"device=0,1"'`, which
stand for the devices of the NVIDIA spec (the AMD one when only that is there;
`driver=amd` or `driver=cdi,"device=VENDOR/CLASS=NAME"` say which). Compose's `gpus:`,
the devices of `deploy.resources.reservations` and CDI names in `devices:` do the same.

The specs are read again at every start, so a driver update that rewrites them takes
effect at the next start, one at boot or by a restart policy included:

- their device nodes are bound into the machine and allowed in its cgroup (an
  unbuffered one, type `u`, is bound and not allowed, as the CDI reference has it);
- their libraries are bound where they say, and their variables set after the
  machine's own;
- their hooks (`nvidia-cdi-hook update-ldcache`, `create-symlinks`) run just before the
  machine starts, with the OCI state of the machine on their standard input and its root
  mounted in a private mount namespace as a runtime's hooks find it (the CDI mounts in
  place, a `/dev` of the CDI nodes, a `/proc`): what they write to the root stays in the
  machine's writable layer.

A device the specs do not have, or a spec with a field the CDI reference does not know,
fails the start with the reference's message; `--gpus` without a spec of the vendor
fails with docker's `could not select device driver "" with capabilities: [[gpu]]`.
nspawn leaves out what systemd-nspawn has no setting for, with a note: extra groups for
the program, Intel RDT classes, and the poststart and poststop hooks. A spec that moves
network interfaces is refused (`--interface` does that). The nodes are the host's, with
the host's owner and mode. mstack machines take no CDI devices. It has been tested with
specs written for the test suite only, not with real GPUs.

### Secrets

Secrets are docker's too, without a swarm. `nspawn secret create db-password` reads the
content from standard input (or `-f/--file`) and keeps it encrypted with
`systemd-creds`, bound to the host's TPM2 where there is one and to its credential key
otherwise. `--secret db-password` on `run`, `start` or `create` gives the machine the
plaintext at `/run/secrets/db-password`, a read-only file of root's with mode 0444
(`NAME:TARGET:MODE:UID:GID` says otherwise), decrypted into a tmpfs of root's alone for
as long as the machine runs. `secret ls` shows who takes each one, `secret inspect` what
is known about it (never the content), and `secret rm` refuses one a machine still
takes. mstack machines take no secrets.

### Events

`events` is docker events: what happens to machines, networks, volumes and secrets, as
it happens or between `--since` and `--until` (a duration ago such as `42m`, an RFC
3339 time, or journalctl's `-1h` or `today`), filtered by name, type, event or label,
as text or `--json`. The actions of a machine are systemd's `start`, `die` (with its
exit code), `stop`, `restart`, `oom` and `fail`, and nspawn's own `pull`, `build`,
`create`, `push`, `kill`, `update`, `pause`, `unpause`, `health_status`, `auto-update`
(with `result=`, `from=` and `to=`) and `remove`; those of a network `create`,
`remove`, `connect` and `disconnect`; those of a volume or a secret `create` and
`remove`.

nspawn's own events of a type carry the same attributes whatever the action, so a
`remove` says as much as a `create`: `image=` and `created=` (which machine of the name
it was) on a machine's, with its labels while it has a record; `subnet=`,
`interface=`, `internal=` and `created=` on a network's; `path=` on a volume's; `size=`
on a secret's. systemd's take `image=`, `created=` and the labels from the machine's
record, and lack them once it is removed. `events` reads the journal, so machines
started by `machinectl`, at boot or by a restart policy are there too, and so is what
happened while nobody watched.

### The Docker API for proxies and dashboards

Tools that follow containers through docker's socket, reverse proxies routing by
labels (caddy-docker-proxy, Traefik, nginx-proxy with docker-gen and acme-companion),
dashboards (homepage) and log viewers (Dozzle), find nspawn's machines the same way: a
machine that mounts `/var/run/docker.sock` or `/run/docker.sock` from the host (`-v
/var/run/docker.sock:/var/run/docker.sock[:ro]`, or compose's `use_api_socket`) gets
nspawn's Docker API there instead of the host's docker.

It answers in the shapes of docker's API 1.51, and of the versions down to 1.24 that
clients ask for: `_ping`, `version` and `info`, the machines as containers with their
labels, networks and addresses, inspect with the environment, events as they happen,
logs, stats and top, and the networks. Reads of images, volumes and swarm objects get
501. It changes nothing, with one exception: `POST /containers/ID/kill` sends a signal
as `nspawn kill` does, so that docker-gen and acme-companion can have nginx read new
configuration and certificates. Any other write (create, exec, pull...) is refused with
403. As with docker's socket, a machine that has it sees every machine and their `-e`
variables (`--secret` values never show), and only root inside can connect.

Each such machine has its own socket, `/run/nspawn/api/NAME.sock`, opened by systemd
(`nspawn-api@NAME.socket`) before the machine starts and closed after it stops; the
server behind it starts when something connects and leaves after a minute without a
connection. A container ID is derived from the machine's name and creation time, so it
stays across restarts and updates. The tools that look for their own container in
`/proc/self/mountinfo` find it, as nspawn binds the machine's `/etc/hostname` from a
directory named after its container ID, as docker does. mstack machines cannot take the
socket.

What one machine's clients hold at once is bounded: 64 connections (one more is closed
at once), 16 of them logs or events streams, each running a `journalctl` (one more gets
503). Stats streams count as connections only.

### Updates of the image a tag names

`nspawn auto-update` gives the machines labelled `org.nspawn.auto-update=registry` the
image their reference names now at its registry, as podman auto-update does and
watchtower did. Podman's `io.containers.autoupdate=registry` is taken too, on the
machine, never from its image.

For each machine the registry is asked for the tag's digest, with a HEAD, which
registries do not count against their limits. A newer image is pulled, signature policy
included, and the machine is made anew from it with every option it had: its networks
and address, ports, volumes, variables, labels, limits, policy and the rest (`generate`
prints the same command before and after), and its creation time, so its Docker API ID
stays. A running machine is stopped and started again, and has to come up: healthy when
it has a healthcheck (within its start period and as many probes as would make it
unhealthy, ten minutes at most), still running ten seconds after its start otherwise.
When it does not, the old image and what the machine had written are put back and it
starts again, which the table reports as `rolled back`.

A machine on the new image starts with a fresh writable layer, as docker's recreate
does: what it keeps belongs in volumes. Machines that share its network (`--network
container:`) start again with it, and so do compose services whose `depends_on` asks for
`restart: true`; a compose machine's image label follows, so that `compose up` does not
take it back.

These are left alone as `skipped`: built images, references pinned by digest, `run
--rm` machines, booted machines (their state is their writable layer), flat and mstack
ones, a policy other than `registry`, and machines whose image declares a volume no
`-v` covers. The reason goes to standard error for the machines named on the command
line, and is in `--json` for all.

An update cut short (a crash, a stop of the service) is put back at the next run (not
`--dry-run`) or start of the service, unless its new image was in place and the machine
runs on it, which it keeps. Cut short before that, the machine does not start until it
is put back, and `pull`, `create`, `run` and `build` refuse its name without `--force`,
which lets the old machine go.

`--dry-run` only says which machines have a newer image waiting (`pending`). The output
is podman's table, `UNIT CONTAINER IMAGE POLICY UPDATED`, with `false`, `pending`,
`true`, `failed`, `rolled back` or `skipped`; the reasons of failures and rollbacks go
to standard error, `--json` prints an object per machine for scripts, and the command
fails when a machine failed or went back. The packages ship
`nspawn-auto-update.timer`, daily with up to fifteen minutes of random delay, disabled:
`systemctl enable --now nspawn-auto-update.timer` turns the updates on.

### Where the output goes

What a machine prints goes, by default (`--log-driver local`), to a journal of
nspawn's own: journald's `nspawn` namespace, kept apart from the system's journal, so a
program that writes a lot never floods the host's logs. `logs` and an attached `run`
read it, with `-f`, `-n`, `--since`, `--until` and timestamps, and
`journalctl --namespace=nspawn` shows it all. `--since` and `--until`
take a duration ago (`42m`, `1h30m`), Unix seconds, an RFC 3339 time or date, or a time
as journalctl reads it (`-1h`, `"10 min ago"`, `"2026-09-24 10:00"`), as `events`
does. `logs` reads the system's journal beside it, which holds systemd's lines
about the unit and what the machine wrote there under another driver. As with docker,
`logs` shows what this machine wrote, from its creation on: a removed machine of the
same name, whose lines are still in the journal, does not show, even with an earlier
`--since`.

The namespace holds 1 GiB at most, the oldest lines going first
(`/usr/lib/systemd/journald@nspawn.conf`, which `/etc/systemd/journald@nspawn.conf` or
a drop-in overrides). The limit is the namespace's, shared by every machine, and a
removed machine's lines stay until they age out.

`--log-driver journal` keeps a machine's output in the system's journal, and
`--log-driver none` drops it: `logs` refuses such a machine (`--inside` still reads a
booted one's own journal), an attached `run` of an app still shows the output, straight
from the program, and a booted machine's console goes nowhere, so `run` takes such a
machine only with `-d`. Like docker's, the driver is a per-machine setting, remembered.

### exec, shell, stop and the rest

`exec` enters the machine's namespaces for both kinds of machine, like docker exec: the
exit code comes back, the image's environment applies and nothing is needed inside (no
D-Bus, no PAM). The command runs with the machine's capabilities and resource limits,
like its own processes, with one exception: in an app that shares the host's user
namespace and is not `--privileged`, it goes without CAP_SYS_BOOT, which the machine's
processes keep only beside a seccomp filter against kexec that the command does not
carry. Right after a start, `exec` waits up to five seconds until systemd-nspawn has
finished confining the machine.

`exec` attaches a terminal when standard input and output are one (`-t` and `-T`
decide otherwise), so redirected output is byte-exact. It runs as the machine's user,
its `--user` or else its image's `USER`, as docker exec does (root for a booted
machine). `-u USER[:GROUP]` takes names or numbers of the image's passwd and group
files, with the home of the passwd entry and the user's supplementary groups unless a
group is given, which is then the only one; a uid the passwd lacks runs with group 0 and
`/` as home, as with docker and as the machine's own program does.

- `shell` opens machined's login session on booted machines and a plain shell on apps.
- `stop` sends the image's stop signal to the program of an app machine and SIGKILLs it
  after `--timeout` seconds (its `--stop-timeout` by default, else 10; a negative
  number waits for as long as it takes, as docker's `-t -1`), or asks a booted machine
  to power off; `--force` kills at once. Stopping a machine that already ended is not
  an error: it only drops what the machine left behind.
- `restart` is a stop and a start with the remembered options.
- `pause` and `unpause` freeze and thaw a machine's cgroup like docker pause (`ps` shows
  it paused).
- `top` lists a machine's processes, and `inspect` of a stopped machine shows the exit
  code of its last run.
- `wait NAME...` waits for machines to stop and prints each one's exit code, and `port
  NAME [PORT[/PROTO]]` the ports a running machine publishes (`80/tcp ->
  0.0.0.0:8080`), as docker's do.

### The unit hooks

Every machine's unit gets a drop-in that calls nspawn around its life (ExecStartPre,
ExecStartPost, ExecStopPost), so `machinectl start`, an enabled unit at boot, a program
that exits on its own or a crash all prepare and release the network the same way as
`nspawn start` and `nspawn stop`. The drop-in names the nspawn binary that wrote it, so
install nspawn where a system service may run it (`/usr/bin` or `/usr/local/bin`; on
SELinux hosts a binary below a home directory is refused with "Permission denied").

### Labels

Labels work as in docker. An image's own labels (`LABEL` in a Dockerfile, `OciLabels=`
in a mkosi.conf) are read from its OCI config when it is pulled or built, and `--label
KEY=VALUE` on `run`, `start` or `create` adds labels of the machine's own on top,
remembered like the ports (`--label none` forgets them). nspawn itself does nothing with
them; `inspect` and `ps --json` show both the merged `labels` and the `image_labels`,
for tools such as a reverse proxy that configures itself from them.

### cp

`cp` copies files and directories between the host and a machine, running or not (a
stopped mstack machine excepted: its tree only exists while it runs), with docker cp's
rules: an existing directory receives the source under its own name, anything else is
the name of the copy, and `DIR/.` copies the contents of DIR. What goes in belongs to
root inside the machine, whatever user namespace it runs in; what comes out belongs to
whoever ran `cp`, because the command line writes it.

Paths inside the machine are resolved inside it, so a link there, absolute or not, never
leads to the host. Links are copied as links (a source that ends in `/` or `/.` names
the directory a link leads to, on either side); devices, sockets and fifos are left out,
and so are the kernel's file systems mounted inside a running machine (`/proc`, `/sys`
and the like). A tree more than 2048 directories deep is refused with an error, where
docker goes on: a machine can make its tree as deep as it likes. A file that fails to
read while it is copied fails the copy: the file of that name on the other side stays as
it was, and what was copied before it stays copied.

The service and the command line exchange a tar stream, as docker's API does. Where
SELinux enforces, a host directory mounted with `-v` keeps its own label, which the
service may not be allowed to write (docker needs `:z` for the same); named volumes are
nspawn's and always work.

### One image, many machines

One image, as many machines as you like: `nspawn create SOURCE NAME` makes another
machine from an image that is already local, without touching the registry. A reference
that is not local is pulled first, under its own name and with the same signature check,
as `run` does. `pull --name NAME` also makes another machine of an image, but resolves
the manifest through the registry first.

A SOURCE given by name is taken as it is, even a machine that ran. A SOURCE given by
reference is, as for `run`, the last pulled image of it that never ran; when every local
copy has run, the image is pulled again beside them, since a machine that ran has a
backend, a mode and a network of its own. The new machine shares the source's layers
and gets a writable layer, an address, settings and ports of its own (`-p`,
`--network`); removing one never affects the others.

A pulled image is a machine too, so `nspawn rm NAME` and `nspawn images rm NAME` remove
the same thing: the record, the tree, the unit files and whatever layers nobody else
uses. `rm --force` stops a running machine first (SIGKILL, like `docker rm -f`) where
both refuse otherwise. A name with nothing behind it (no record, no image machined
knows, no leftovers of a failed install) is refused with `no machine or image named
NAME`; the other names of the same command are still tried.

### The same machines on another host

`nspawn generate [NAME...]` prints the commands that make machines again, on another
host or on this one after a reinstall: a shell script with `network create` for the
networks they join, then `run -d` for each machine with every flag it keeps (networks,
aliases and fixed addresses, ports, entrypoint and command, environment, volumes,
labels, restart policy, limits, healthcheck, docker's other flags and `--depends-on`).
Each comes after the machines it depends on or takes its network from, which come along
when they are not named; an image without flags of its own is pulled again instead,
unless another machine takes its network, which needs it running. Each `run -d` pulls
its machine on its own (`--pull always`; blobs already there are not downloaded again),
so that a machine never takes the network kind, mode or backend of another one made from
the same image. Without names it covers every machine nspawn has. Run on the other host,
the script starts the machines as they were brought up here.

What a flag cannot carry is said in comments at the top: a secret is encrypted for this
host, so `nspawn secret create` it there first; a host directory a machine mounts has to
exist there; an image built here has to be pushed first; a healthcheck in exec form
comes back as `--health-cmd`, which runs it through `/bin/sh`, so the image needs one.
The contents of named volumes and of a machine's writable layer stay behind, and so do
the addresses nspawn handed out: volumes are seeded again from the image, and only
addresses fixed with `--ip` come along.

### Completions, the man page and JSON

Completions for bash, zsh and fish come with the packages, and so does the manual page,
`man nspawn`; from a build of your own, `nspawn completions bash >
~/.local/share/bash-completion/completions/nspawn` (or the equivalent for your shell).
Besides commands and flags, TAB completes names, asked from the service as it goes, for
example:

- the running machines after `stop`, `exec`, `kill`, `pause`, `unpause`, `top`,
  `shell`, `stats` and `port`;
- the images that do not run after `start`;
- every machine and image after `rm`, `inspect`, `logs`, `restart`, `update` and
  `images rm`, and every machine after `wait`, `generate`, `auto-update`,
  `--depends-on`, `network connect` and `network disconnect`;
- the local images after `create` and `push`, and their references after `run`;
- the networks after `--network`, `network rm`, `network inspect`, `network connect`
  and `network disconnect`, and the volumes after `volume rm`.

The names are all the service hands out without asking polkit, so this works for any
user and under `sudo` without a password; secrets are not completed. Flags come once
the word starts with `-`, or where there is no name to offer.

`ps`, `machines ls`, `images ls`, `network ls`, `volume ls` and `secret ls` take
`--json` and print what the service answered instead of a table (so do `system df`,
`stats`, `events`, `auto-update` and `builder du`), and `nspawn inspect NAME...` prints
the whole record of machines or images (running or not) as a JSON array, like docker
inspect: what a script reads. The keys are the ones of the D-Bus interface (see
[DBUS.md](DBUS.md)), so the command line and the bus never disagree.

`ps` also takes docker's scripting flags:

- `-q` prints the names alone, where docker prints IDs.
- `--filter KEY=VALUE` keeps the machines that match: `name` (a regular expression),
  `label`, `status`, `network`, `ancestor`, `health`, `exited`, `volume`, `publish`,
  `expose`, `id`, `before`, `since`. The same key twice matches either value, different
  keys all.
- `--format` takes a Go template over docker's fields (`.Names`, `.Image`, `.Command`,
  `.CreatedAt`, `.RunningFor`, `.Ports`, `.State`, `.Status`, `.Labels`, `.Label
  "KEY"`, `.Mounts`, `.Networks`) and nspawn's (`.Mode`, `.Address`, `.OS`, `.Pid`),
  `table TEMPLATE` for aligned columns with headers, or `json` for a JSON object per
  line. As in docker, `.Command` is cut to 20 columns and each of `.Mounts` to 15, and
  `.Image` leaves out a digest the reference is pinned to, unless `--no-trunc` is given.

`inspect -f TEMPLATE` prints a template over each object, a line each, as docker
inspect -f does, over the keys inspect prints (`{{.state}}`, `{{json .labels}}`,
`{{index .addresses "front"}}`). The templates are Go's text/template, with docker's
functions (`json`, `join`, `split`, `lower`, `upper`, `title`, `pad`, `truncate`).

`system df` is docker's: images, machines (containers), volumes and build cache, with
what each takes and what a prune would give back (`-v` for each of them, `--json`).
Images and machines are one kind of record here: an image is a record pulled or built
and never started, any other record a machine. `images prune -a` removes the images no
machine was made from; without `-a` it removes the dangling ones, which nspawn's store
never has (a tag pulled or built again replaces its record). `system prune` removes
stopped machines made from an image (by `create` or `run`), networks no machine uses and
the build cache no image uses; with `-a` also the images no machine was made from and
the whole build cache. A pulled or built image that was started is the image as well and
stays. `--filter label=...` and `until=...` narrow what goes, networks included (a
label filter leaves the build cache alone), and `-f` skips the question.

## Compose

`nspawn compose` runs the files of docker compose with docker compose's commands and
flags, doing their steps in the same order and printing the same output: `nspawn
compose up -d` in a directory with a `compose.yaml` brings its services up, `nspawn
compose down` takes them away. It is a client of the service like every other command,
so it takes the same permissions (`sudo`, or a polkit rule), and what it makes is
nspawn's own: `nspawn ps` lists the machines of a project, `nspawn network ls` its
networks.

A service becomes a machine per replica, named as docker names its containers,
`PROJECT-SERVICE-N` with `_` written `-`. Its image is pulled from Docker Hub when it
names no registry and kept under its local name; its networks, volumes and secrets are
nspawn's networks, volumes and secrets. `depends_on` stays compose's own, as with
docker: the machines carry no `--depends-on`. The machines carry docker compose's
labels, which is how `ps`, `logs`, `stop`, `down` and the rest find a project again,
from its files or from `-p NAME` alone. A machine is made anew when its service changed
(docker compose's config hash) or its image did.

A service with a `build` section is built by nspawn's builder (see
[Dockerfiles](#dockerfiles)) as docker compose decides it: when its image is not here
(an image it names is pulled first, and built when the pull fails), every time with
`pull_policy: build`, and always with `up --build` or `compose build`; `--no-build`
builds nothing. Its image is the one it names or `PROJECT-SERVICE`, with the section's
`tags`, `labels` and compose's project labels. A name without a registry or a namespace
is nspawn's own, under the configured registry as `nspawn build -t` puts it, so `nspawn
run NAME` takes it, while one like `user/app` stays Docker Hub's. `build --push` sends
the named ones, and `down --rmi local` removes the images the project built.

`config` shows as warnings what nspawn cannot do (a remote build context, SSH forwarding
into a build and the rest of what [COMPOSE.md](COMPOSE.md) refuses), and `up`, `create`,
`run` and `scale` refuse it for the services they start. COMPOSE.md has every attribute
and option with what nspawn compose does with it, and where it differs from docker
compose on purpose.

## Permissions and the service

Every command talks to the `org.nspawn` service on the system bus, which decides
through polkit who may do what: `org.nspawn.inspect` for the commands that only look,
`org.nspawn.manage` for the rest, both for administrators by default. `sudo nspawn ...`
therefore works everywhere. To drive nspawn without a password, hand one of those
actions to a group with a polkit rule, as in `nspawn-wheel.rules` in the documentation
directory; doing that makes the group administrators of the host, since a machine's
commands run as root and any host path can be mounted into one.

The service keeps running the binary it started with: after an upgrade it is the old
one until it goes idle (a minute without work) or is restarted, so a command whose
version differs from the service's warns and says to run `sudo systemctl restart
nspawn.service`. The rpm and the deb restart it on upgrade themselves.

## Registries and credentials

### Signed images

Every image on hub.nspawn.org is signed twice by its build workflow
(nspawn/mkosi-definitions) with cosign: with the project's key, whose public half
(`cosign.pub` there) is built into nspawn, and keyless, with the workflow's own identity
through Sigstore
(`https://github.com/nspawn/mkosi-definitions/.github/workflows/mkosi.yml@refs/heads/master`,
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
`create` skips the check for that command, like docker's `--disable-content-trust`, and
on `build` for the base images it pulls; `create` checks only an image it has to pull,
never a local source. `inspect` and `GetImage` show who signed (`signed_by`: `key
<hint>, keyless <identity>`) and when (`signed_at`), which a machine made with `create`
inherits from its source.

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

A signature verifies when it was made with one of the keys, or keyless with the identity
and issuer; a table names at least one of the two. `required = false` turns a missing
signature into a note and lets the pull go on. A signature that is there but does not
verify still fails the pull, and so does one listed whose manifest or bundle cannot be
fetched, unless a valid signature sits beside it. An attestation (a bundle whose
predicate type is not cosign's signature) is no signature. `rekor = false` trusts the
key or certificate alone, without the log entry (a private deployment that keeps no
log); a bundle still has to carry a log entry or a timestamp, which cosign always adds.
A table for `hub.nspawn.org` replaces the built-in policy, and `verify = false` in a
table turns verification off for that registry, the hub included:

```toml
[registries."hub.nspawn.org"]
verify = false
```

The policies are the service's: what the command line was given with `--registry`
chooses the registry, never the policy. A key or a trusted root the service cannot read
is reported when it starts (`journalctl -u nspawn`) and fails the pulls of that
registry.

### Credentials

Registries are used anonymously until `nspawn login [REGISTRY] -u USER` (password asked
on the terminal, or `--password-stdin`) checks the credentials the way docker login does
and keeps them in `/etc/nspawn/auth.json`, mode 0600, in the auth.json format podman and
skopeo use. That file is the only one consulted: the service has no home of the caller's
to look into, so `docker login` or `podman login` do not carry over. Every operation
chooses the credentials of the registry it talks to, so the hub's never travel to Docker
Hub; `nspawn logout [REGISTRY]` forgets them. Anonymous pulls from Docker Hub are rate
limited per address; logging in raises the limit.

## Networking

Booted machines join the `nspawn0` bridge, a docker0 style network that nspawn manages
itself, so it behaves the same whether the host runs systemd-networkd, NetworkManager or
nothing at all. On the first `start` nspawn creates the bridge with the first address of
the subnet (`10.99.0.0/24` by default; `bridge`, `subnet` and `dns` can be set in
`nspawn.toml`), enables forwarding and installs the nftables table `ip nspawn` with
masquerading. An existing interface with that name is only taken over when it is a
bridge nspawn made (or an empty one); `bridge = "docker0"` is refused rather than acted
on.

Each machine gets a fixed address from the subnet, remembered with the image and handed
to the systemd-networkd inside it through a `.network` file mounted at
`/run/systemd/network/10-host0.network`, with the DNS servers (`--dns`, else
`nspawn.toml`'s, else the host's upstream ones) and the `--dns-search` domains. A
generated `/etc/hosts` gives every machine the names of the other machines on the bridge
and `host.nspawn.internal` for the host, and hosts with systemd 258 or newer resolve
machine names themselves through machined. The bridge carries IPv4 only, so neither the
machines' `host0` nor the bridge get an IPv6 link-local address: a machine's name leads
to its bridge address, not to a `fe80::` one. `nspawn network inspect bridge` shows the
addresses and ports; `nspawn network up` creates the bridges without starting anything.

### User-defined networks

More networks are made like docker's user-defined ones: `nspawn network create backend`
gives a bridge of its own (`nsbr-backend`) with the next free /24 of `network_pool`
(`10.99.0.0/16` unless set in `nspawn.toml`, never one the host already routes) or the
one given with `--subnet`, and `--network backend` on `start`, `run` or `create` puts a
machine on it. Machines of one network reach each other by name, and nothing of another
network, the default one included, reaches them; a port they publish is reachable from
every network through the host, as from the LAN. `--internal` makes a network with no
way out: its machines reach each other and the host, publish nothing and are reached by
nothing else.

`--network` given several times joins several networks, the first one primary: its
gateway is the default route, and when it is internal the first network that leads out
carries it. A proxy on `front` and `back` reaches both, while `front` and `back` still
do not reach each other. `--network-alias NAME` (or `NETWORK=NAME`) gives a machine
another name on a network, as docker does; `--network none` gives it no interface but
`lo`. `network ls` lists the networks, `network rm` and `network prune` remove the ones
no machine joins, bridge and rules included; `network create --label KEY=VALUE` tags
one, and `network prune --filter label=KEY[=VALUE]` (or `until=`) removes only the
unused ones it selects.

`--network container:NAME` puts an app machine in the network namespace of NAME, as
docker's `--network container:` does: the same interfaces and address, NAME's hosts and
resolv.conf files, nothing of its own, and a port its program serves is published with
`-p` on NAME. NAME has to be running on a bridge network when the machine starts
(`--depends-on NAME` has it started first), and cannot be removed while a machine names
it, unless that machine goes in the same `rm`. When NAME stops or restarts, the machine
keeps the namespace it joined and has to be restarted to join the new one, as with
docker. `-p`, `--ip`, `--network-alias`, `--dns`, `--dns-search`, `--add-host` and
`--sysctl` are refused on such a machine, since they shape a network of its own, and so
is a booted image, whose systemd would configure the shared interfaces again. `ps` and
`inspect` show it as `container:NAME`.

`network connect [--alias NAME] [--ip ADDRESS] NETWORK MACHINE` and `network disconnect
NETWORK MACHINE` change the networks a machine joins, like docker's. A running app or
booted machine gets an interface on the network at once (`hostN`, with its address; its
default route stays where it was, unless it had none and the network leads out) and
loses it at once, and from its next start the network is one of those it joins as
`--network` would have made it. A machine on host networking, on veth, sharing another's
or without a network, and an mstack machine, cannot; nor can a machine leave the only
network it has (connect another one first, or start it with `--network none`). Leaving
the primary network makes the next one primary, where its published ports go at once
while it runs, from its next start otherwise.

### App machines

App images have nothing inside to configure `host0`, so for them nspawn builds the
network namespace before the process starts (`ip netns`, a veth pair on the bridge, the
address and the default route) and hands it to systemd-nspawn (`NamespacePath=` on
systemd 259 or newer, `--network-namespace-path=` before), together with a generated
`/etc/resolv.conf`. The process finds its network ready when it starts, as in docker,
and the namespace goes away when the machine stops.

One consequence: a user namespace cannot join a network namespace that belongs to the
host, so app machines on the bridge run without one (`PrivateUsers=no`), which is also
docker's default; capabilities, seccomp and the other namespaces still apply. Such an
app keeps docker's default capabilities (CHOWN, DAC_OVERRIDE, FOWNER, FSETID, KILL,
MKNOD, NET_BIND_SERVICE, NET_RAW, SETFCAP, SETGID, SETPCAP, SETUID, SYS_CHROOT,
AUDIT_WRITE) plus SYS_BOOT, not systemd-nspawn's set: without a user namespace that
set's CAP_SYS_ADMIN is root on the host, one remount away from a writable `/proc/sys`
and a `kernel.core_pattern` that runs what it names as the host's root. SYS_BOOT lets a
reboot asked from inside end the machine as it does a booted one: in a pid namespace of
its own reboot(2) only signals the machine's init, and the kexec system calls are
filtered out. `--cap-add` puts one back (`NET_ADMIN` for a VPN client, and it is kept on
its own with `--interface`), `--privileged` all of them. For the same reason app images
are assembled with the overlay backend even when `--backend mstack` asks for mstack;
nspawn says so.

### Published ports

Ports are published like docker: `nspawn start web -p 8080:80 -p 5353:53/udp`. On IPv4
each one is a DNAT entry in the same nftables table, reachable from other hosts, from
the host's own addresses and from 127.0.0.1, and it goes away when the machine stops.
`-p 8000-8010:8000-8010` publishes a range, one mapping per port. A port another
running machine publishes on an address they share, or one a service of the host
listens on, is refused. The list is remembered for the machine; `-p none` forgets it.
With firewalld running, the bridge is bound to the trusted zone at runtime, which also
lets published ports through on IPv4; the binding does not survive `firewall-cmd
--reload`, and the next `start` or `nspawn network up` puts it back.

The machines have IPv4 addresses alone, and nftables cannot translate IPv6 into IPv4,
so a port answers on the host's IPv6 addresses through a proxy, as docker's userland
proxy serves it: `nspawn-proxy-NAME.service`, which the unit's hooks start once the
machine runs and stop with it, listens there and relays TCP connections and UDP
datagrams to the machine's address. The IP of `-p` says where a port is published, as
with docker:

| `-p` | Where |
| --- | --- |
| `8080:80` | every IPv4 address and, on a host with IPv6, every IPv6 address |
| `0.0.0.0:8080:80` | every IPv4 address |
| `127.0.0.1:8080:80` | that IPv4 address, for a reverse proxy in front |
| `[::]:8080:80` | every IPv6 address |
| `[2001:db8::1]:8080:80`, `[::1]:8080:80` | that IPv6 address |

A link-local address (`fe80::`) cannot be named, as it needs an interface; `[::]`
takes it with the others. An IPv6 client reaches the machine from the bridge's gateway
address, not its own, as with docker's proxy. It comes in through the host's input
path, not its forwarding, so a host firewall (firewalld's zone of the interface, ufw,
an nftables input policy) has to let the port in, as for any service of the host; nspawn
changes no rule for it. `nspawn port`, `ps --format '{{.Ports}}'`, compose and the
Docker API show each side, `0.0.0.0:8080->80/tcp` and `[::]:8080->80/tcp`, as docker
does.

### Firewalls

Hosts running docker (in its default iptables mode) or ufw have a FORWARD policy of
DROP; `start` then adds two rules to the DOCKER-USER chain, which docker reserves for
that, or to FORWARD itself: anything out of the bridge, and into the bridge only what
was published or belongs to a connection a machine opened. Where docker or ufw is
installed and has not set the policy yet, the rules go to FORWARD all the same, so that
they are there when it changes: ufw sets it when it is enabled, machines running or
not, and docker when it is the one that turns IP forwarding on. Neither empties
FORWARD. That needs the `iptables` command, which those tools bring with them. Where
the rules are there and the command is not, or where a hand-written nftables firewall
drops forwarded traffic, `start` says so and the exception has to be made by hand;
without it the machines reach nothing beyond the bridge and published ports answer on
the host alone.

### host and veth

`--network host` shares the host's network instead, and `--network veth` keeps the
classic systemd-nspawn setup: a virtual ethernet pair configured by systemd-networkd on
the host through `80-container-ve.network`. In that mode `start` activates
systemd-networkd when the host has no `.network` files of its own and refuses with an
explanation otherwise, and binds `ve-<name>` to firewalld's trusted zone while the
machine runs.

### Physical interfaces

`--interface IFACE` on `run`, `create` and `start` gives a machine a network interface
of the host, whole: a second ethernet port, or a wifi adapter for a machine doing
wireless work. `--device` cannot do that (a network interface is not a node under
`/dev`), and `--network host` cannot either (the machines run in a user namespace, which
may not configure the host's interfaces). The interface is moved into the machine's
network namespace before its program or init runs and is back on the host when the
machine stops or is removed, with its name kept on both sides, as systemd-nspawn's
`Interface=` and LXC's `phys` type do it. It arrives down and unconfigured, as the
kernel moves it: the machine brings it up and configures it, with `ip` in an app or its
systemd-networkd in a booted machine. `--interface` is repeatable and remembered like
the other flags; `--interface none` forgets them; `inspect` lists them as `interfaces`.

Where systemd-nspawn makes the machine's network namespace (a booted machine, on the
bridge or with veth, and any machine with `--network none`) the interface goes into the
machine's settings file as `Interface=`, and systemd-nspawn moves it at start and back
at exit. An app machine on a bridge network gets its namespace from nspawn itself, by
its path, which takes no `Interface=` beside it, so the unit's hooks move the interface
in with `ip` before systemd-nspawn runs and give it back after; such an app runs without
a user namespace, so `iw reg set` and monitor mode work inside as on the host. Either
way the unit wants and waits for the interface's device unit, so that a machine started
at boot waits for a USB adapter udev has not seen yet.

A wifi adapter cannot leave its phy, so the phy moves whole, every interface it carries
with it (nspawn says which), and a driver without namespace support (`ath6kl`,
`wilc1000`) refuses the move naming the phy. An app on the bridge takes a wireless
interface on any systemd, through `iw`, which the host needs; `Interface=` moves a phy
since systemd 256, so on an older host a wireless interface is refused for a booted
machine or one with `--network none`, with a message that says so. Regulatory domain
(`iw reg set`) and rfkill are host-wide: they are set on the host, or from an app on the
bridge; `--device /dev/rfkill` hands the switch to such an app.

One machine at a time takes an interface: a second one naming it is refused while the
first runs (`stop` it, or start it with `--interface none`). Refused as well:

- `lo`, and the bridges and veth ends of nspawn's own;
- a port of a bridge or bond (take it off with `ip link set IFACE nomaster` first);
- a name that is not on the host (a machine that ended a moment ago may still hold it:
  the kernel gives interfaces back a little after their namespace dies);
- a machine with `--network host` (it is on the host's interfaces already), one with
  `--network container:NAME` (give the interface to NAME), and the mstack backend.

The interface of the host's default route is allowed with a warning, since the host
loses its route while the machine runs. When the interface comes back, NetworkManager or
systemd-networkd manage it again as before, and a name taken on the host in the meantime
makes the kernel rename it (`dev0`, `wlan0`). `stop --force` kills the machine: an app
on the bridge still gets every interface back from its release hook, while a machine
that took them through `Interface=` may not get to give them back, and the kernel then
returns a physical interface or a wifi phy a moment later and destroys a virtual one (a
VLAN, a macvlan, a dummy) with the namespace.
