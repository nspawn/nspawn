# The D-Bus interface

nspawn serves `org.nspawn` on the system bus, and the command line is a
client of it: everything `nspawn` does is a method here, so another client
sees exactly what the command line sees. machined stays an implementation
detail: a client never has to talk to it, though every machine carries its
machined object path for whoever wants that view.

## Getting it on the bus

```
sudo nspawn daemon --install
```

writes four files, the unit among them carrying the path of the binary that
ran the command, and reloads systemd and the bus:

| File | Role |
|---|---|
| `/etc/dbus-1/system.d/org.nspawn.conf` | bus policy: root may own `org.nspawn`, everyone may call it and receive its signals (those of a job or a command are sent to the client that started it) |
| `/usr/share/polkit-1/actions/org.nspawn.policy` | the two actions polkit authorizes callers for |
| `/usr/share/dbus-1/system-services/org.nspawn.service` | bus activation: `SystemdService=nspawn.service` |
| `/etc/systemd/system/nspawn.service` | `Type=dbus` unit running `nspawn daemon` |

Nothing runs until a client calls `org.nspawn`; the bus then starts the unit,
and the service exits again after a minute without a call, a job or a command
running (`nspawn daemon --idle-exit`). The service reads
`/etc/nspawn/nspawn.toml`; `nspawn --config FILE daemon --install` puts
another file on the unit's command line. The methods that reach a registry
take `registry` and `ca_cert` options that override that configuration for
one call; the command line passes them when it was given a registry (flag,
`NSPAWN_REGISTRY` or its own configuration file) and leaves the service's
alone otherwise.

Every user may call; who may do what is polkit's answer. The methods that
only read (`ListImages`, `GetImage`, `ListMachines`, `GetMachine`,
`MachineStats`, `MachineProcesses`, `Events`, `ListNetworks`, `GetNetwork`,
`ListVolumes`, `ListSecrets`, `GetSecret`) ask for
`org.nspawn.inspect`, the rest for `org.nspawn.manage`, and both are for
administrators by default, so `sudo nspawn ...` works and a
desktop or `pkttyagent` session is asked for a password. Root is allowed
without asking, which is also how the service keeps working where polkit is
not installed: there, nobody else can call it.

An administrator hands either action to a group with a rule of their own,
which is how to drive nspawn without a password. The packages ship one as an
example in their documentation directory
(`packaging/polkit/nspawn-wheel.rules`):

```
polkit.addRule(function (action, subject) {
    if (action.id.startsWith("org.nspawn.") && subject.isInGroup("wheel")) {
        return polkit.Result.YES;
    }
});
```

Copy it to `/etc/polkit-1/rules.d/50-nspawn.rules`. Everyone in that group can
run commands as root inside a machine and mount any host path into one, so it
makes them administrators of the host, as the docker group does.

Errors from a refusal come back as `org.nspawn.Error.NotAuthorized`.

A job and a command belong to the user who started them: their objects
answer that user and root, and anyone else gets `AccessDenied`. Their
signals (`JobOutput`, `JobProgress`, `JobRemoved`, `Exited` and the
`PropertiesChanged` of their objects) go to the client that started them and
to nobody else; `ImageAdded`, `ImageRemoved`, `MachineStarted` and
`MachineStopped` reach everyone listening.

Where SELinux is enforcing (Fedora, RHEL) the service needs a domain of its
own, `nspawn_t`, like machined and the container runtimes have: the base
policy lets no domain touch the pipes of an unconfined service, so the bus
drops the service the moment it hands a descriptor over (`Exec`, `Shell`,
`Logs`, `CopyFrom`). The policy lives in `packaging/selinux` and the `nspawn-selinux`
package loads it; `--install` says so when SELinux is enabled and the module
is missing, and also when the binary it just wired up is not labelled
`nspawn_exec_t`, which a build installed by hand is not: the service would
then run unconfined and the bus would drop it at the first descriptor, so
`Exec`, `Shell`, `Logs`, `CopyFrom` and `CopyTo` fail with the client
disconnected. Commands run
inside a machine take the machine's own context, as with docker exec.

## org.nspawn.Manager at /org/nspawn

Results are dictionaries (`a{sv}`) whose keys are the command line's
spellings; options are dictionaries too, and a key nothing expects is an
error, so a typo never passes as a default. An integer option takes any
unsigned type (`t`, `u`, `q` or `y`). Errors come back as
`org.nspawn.Error.Failed` with the same message the command line prints.

Properties: `Version`, `Registry` (the hub), `Bridge`, `Subnet`, `Jobs` and
`Processes` (`ao`, every job and every Exec since the service came up).

### Images

| Method | Like | Notes |
|---|---|---|
| `ListImages() -> aa{sv}` | `images ls` | name, kind, backend, origin, reference, size, read_only |
| `GetImage(s name) -> a{sv}` | | everything recorded: name, reference, digest, backend, origin, mode, created, network (bridge, veth, host, none or the primary network's name), networks (as, every bridge network joined, the primary one first), address (on the primary network), addresses (a{ss}, by network), aliases (as, NETWORK=NAME), ports, volumes, env, entrypoint, cmd, command, image_env, working_dir, user, stop_signal, labels (a{ss}: the image's with the machine's on top), image_labels, restart, memory (bytes), cpus (d), pids_limit, and docker's other flags as the machine has them: hostname, cap_add, cap_drop, tmpfs, dns, dns_search, extra_hosts (HOST:IP), devices (HOST:CONTAINER:PERMISSIONS) (as), privileged, read_only, init (b), shm_size, stop_timeout (t), oom_score_adj (x), ulimits, sysctls (a{ss}), secrets (as, NAME:TARGET:MODE:UID:GID), image_volumes (as, what the image declares as volumes); working_dir, user and stop_signal are the flags' when given, the image's otherwise; healthcheck (a{sv}: test (as), interval, timeout, start_period, start_interval (t, microseconds, 0 for docker's default), retries (u); empty without one) |
| `PullImage(s reference, a{sv} options) -> o` | `pull` | options name, backend, mode, force, registry, ca_cert; a job |
| `CreateMachine(s source, s name, a{sv} options) -> o` | `create` | options backend, network (s) or networks (as, several bridge networks, the first one primary), aliases (as, NAME or NETWORK=NAME), publish (as, [IP:]HOST:CONTAINER[/udp], or ranges of equal length on both sides), force, entrypoint, env, volume, label, restart, memory (t, bytes), cpus (d), pids_limit (t), command, registry, ca_cert; a job |
| `PushImage(s image, a{sv} options) -> o` | `push` | options to, registry, ca_cert; a job |
| `BuildImage(s directory, s tag, a{sv} options) -> o` | `build` | options name, distribution, release, profile, backend, mode, force, keep_output, mkosi_args, registry, ca_cert; a job whose output includes mkosi's |
| `RemoveMachines(as names, a{sv} options) -> o` | `rm` | `RemoveImages` with the option force, which stops a running machine first (SIGKILL) instead of refusing it |
| `RemoveImages(as names) -> o` | `images rm` | a job: every name is tried, its result lists `removed`, and it fails at the end when one could not be removed |
| `SearchImages(s term, s source, u limit, a{sv} options) -> (aa{sv}, as)` | `search` | source "", "hub" or "dockerhub"; the hits and the notes (a source that could not be reached); options registry, ca_cert |
| `ListRepositories(s filter, b with_tags, a{sv} options) -> aa{sv}` | `hub ls` | options registry, ca_cert |
| `ListTags(s repository, a{sv} options) -> as` | `hub tags` | options registry, ca_cert |

### Machines

| Method | Like | Notes |
|---|---|---|
| `ListMachines(b all) -> aa{sv}` | `ps`, `ps -a` | name, state, started (unix seconds), leader, os, machine_path, plus the image's record, and for a running machine with a healthcheck health (s: starting, healthy or unhealthy), health_failing_streak (u) and health_log (as, the last probes as "END EXIT_CODE OUTPUT"); state is machined's (opening, running, closing), or for a machine machined does not list restarting (listed without `all` too), starting, closing or stopped; containers only: the virtual machines machined also registers are left out, and naming one to `GetMachine`, `StopMachine`, `Exec`, `Shell`, `Logs`, `CopyFrom` or `CopyTo` fails with an error that says so |
| `GetMachine(s name) -> a{sv}` | `inspect` | one machine as `ListMachines` has it, whether it runs or not (its state is then restarting, starting, closing or stopped, as there, and a stopped one carries exit_code (i), docker's code for its last run); a running machine whose cgroup is frozen has state paused; fails for a name that is neither running nor an image of nspawn |
| `StartMachine(s name, a{sv} options) -> (s, as)` | `start` | options wait (default true), network (s: bridge, veth, host, none or a network made with CreateNetwork) or networks (as, several bridge networks, the first one primary), aliases (as, NAME or NETWORK=NAME; "none" forgets them), publish (as, [IP:]HOST:CONTAINER[/udp], or ranges of equal length on both sides; "none" forgets them), entrypoint, env, volume, label, restart, memory (t, bytes, 0 removes the limit), cpus (d), pids_limit (t), image_command, command, remove (b, `run -d --rm`: removed once it ends), health_cmd (s), health_interval, health_timeout, health_start_period, health_start_interval (t, microseconds), health_retries (u), no_healthcheck (b), and docker's other flags: hostname, user (USER[:GROUP]), working_dir, stop_signal (s; "" forgets), cap_add, cap_drop, tmpfs, devices, dns, dns_search, extra_hosts, ulimits (NAME=SOFT:HARD), sysctls (KEY=VALUE), secrets (NAME[:TARGET[:MODE[:UID:GID]]]) (as; "none" forgets), privileged, read_only, init (b), shm_size, stop_timeout (t), oom_score_adj (i); "started", "ended" (the program returned before the machine registered) or "restarting" (it ended and its restart policy brings it back), and the notes made on the way |
| `RunMachine(s name, a{sv} options, a{sh} fds) -> (a{sh}, o, as)` | `run` without `-d` | starts a machine PullImage or CreateMachine made, attached; options those of StartMachine (wait defaults to false, remove (b) is `--rm`), tty (b), rows and cols (t), term (s); descriptor stdin for the program's input without tty. An app gets a pseudo terminal with tty, its master under "tty"; otherwise the machine's output, a line at a time, under "stdout". The process object's exit status is docker run's: the program's, 128 plus the signal it died of, 137 after SIGKILL; `--rm` machines are removed by a unit of their own once theirs is down |
| `StopMachine(s name, a{sv} options) -> (s, as)` | `stop` | options force, wait (default true), timeout (seconds; the machine's stop_timeout by default, else 10; a day at most); "stopped" or "was-not-running", and the notes (a program that had to be killed) |
| `MachineStats(as names) -> aa{sv}` | `stats` | one sample of each named machine that runs (every running container when none is named; names that do not run are left out): name, time_usec (CLOCK_MONOTONIC), cpu_usec, memory (memory.current without inactive_file, as docker), memory_limit (the host's memory without a limit), pids, io_read, io_write, net_rx, net_tx; a counter that cannot be read is left out, the network ones on the host's network; rates come from two samples |
| `UpdateMachine(s name, a{sv} options) -> b` | `update` | options restart, memory, cpus, pids_limit and the health_* ones as for StartMachine (0 removes a limit, absent keeps it); a running machine gets the limits at once and its probes start over; true when it was running |
| `PauseMachine(s name)`, `UnpauseMachine(s name)` | `pause`, `unpause` | the machine's cgroup frozen and thawed (docker pause); fail for a machine that is not running |
| `MachineProcesses(s name) -> aa{sv}` | `top` | the processes in the machine's PID namespace, from its cgroup: pid (u), user (s, the uid as the machine sees it, root for 0), time (s, CPU time HH:MM:SS), command (s) |
| `KillMachine(s name, a{sv} options) -> as` | `kill` | option signal (a name such as KILL, SIGHUP or RTMIN+3, or a number; SIGKILL by default): SIGKILL is StopMachine with force, other signals go to an app's program or a booted machine's init; the machine's own stop signal keeps a restart policy from bringing it back, other signals leave that to the policy; fails for a machine that is not running; the notes |
| `Exec(s machine, as argv, s user, a{sv} options) -> (a{sh}, o)` | `exec` | user "" for root; options tty (default true), rows, cols, env (the caller's `TERM=` among them; xterm otherwise on a terminal), workdir (s, instead of the machine's), detach (b: no streams come back, the output is discarded and the command runs on); returns the streams ("tty", or "stdin", "stdout", "stderr") and a process object |
| `Shell(s machine, s user, a{sv} options) -> (h, s)` | `shell` | the login session machined offers for a booted machine: its pseudo terminal and the terminal's path; options env, as for `Exec`; apps get `Exec` of a shell with a tty instead |
| `CopyFrom(s machine, s path, a{sv} options) -> (h, o)` | `cp NAME:PATH ...` | a tar stream of `path` on the pipe, owners as the machine sees them, and a job with the outcome (its result: entries, bytes); a running machine, or a stopped overlay or flat one |
| `CopyTo(s machine, s path, h stream, a{sv} options) -> o` | `cp ... NAME:PATH` | the tar stream read from `stream` unpacked at `path` by docker cp's rules, everything root's inside; option contents (the source was `DIR/.`); a job |
| `Events(a{sv} options) -> (a{sh}, o)` | `events` | options since, until (journalctl's time syntax; without until it follows, and until needs since), filters (as: name=, type=, event=, label=KEY or KEY=VALUE); under "stdout" one JSON object per line: time (RFC 3339, UTC), time_usec, type (machine, network, volume, secret), action (start, die, stop, restart, oom, fail, health_status with attribute status, and nspawn's own), name, attributes (the same for every action of a type: image on a machine's, subnet, interface and internal on a network's, path on a volume's, size on a secret's, plus the action's own), labels; journalctl's complaints under "stderr" and a process object for its end; entries are PID 1's about systemd-nspawn@ units and nspawn's own (message ID b0b60147942247cab22cc49510006a0b), both only from root |
| `Logs(s machine, a{sv} options) -> (a{sh}, o)` | `logs` | options follow, lines, since, until (journalctl's time syntax; with until nothing is followed), timestamps, all, inside; journalctl's "stdout" and "stderr" and a process object for its exit status; journalctl is stopped once nobody reads its output |

`StartMachine` waits for a booted machine's init and `StopMachine` for the
machine to be gone, which can take longer than a client's default timeout
(`busctl --timeout=120`).

### Volumes

| Method | Like | Notes |
|---|---|---|
| `ListVolumes() -> aa{sv}` | `volume ls` | name, path, used_by (the machines whose records mount it), created (unix seconds) |
| `CreateVolume(s name) -> s` | `volume create` | the volume's path; one that exists already is not an error |
| `RemoveVolumes(as names) -> o` | `volume rm` | a job: every name is tried, its result lists `removed`, and it fails at the end when one was in use, unknown or not a volume |
| `PruneVolumes() -> o` | `volume prune` | a job removing every volume no machine uses; its result lists `removed` |

### Secrets

| Method | Like | Notes |
|---|---|---|
| `ListSecrets() -> aa{sv}` | `secret ls` | name, created (unix seconds), size (t, bytes of the plaintext), labels (a{ss}), used_by (as, the machines whose records take it); never the content |
| `GetSecret(s name) -> a{sv}` | `secret inspect` | one secret as `ListSecrets` has it |
| `CreateSecret(s name, ay content, a{sv} options)` | `secret create` | the content, encrypted for this host with systemd-creds; options labels (as, KEY=VALUE); a name in use is refused |
| `RemoveSecrets(as names) -> o` | `secret rm` | a job (kind secret-rm): every name is tried, its result lists `removed`, and it fails at the end when one was in use or unknown |

### Network and credentials

| Method | Like |
|---|---|
| `ListNetworks() -> aa{sv}` | `network ls`: every network, the default one ("bridge") first: name, interface, subnet, gateway, internal, created, labels (a{ss}), machines (as, every machine joining it) |
| `GetNetwork(s name) -> (a{sv}, aa{sv})` | `network inspect`: the network as above without machines, and its machines (name, address on this network, aliases (as) on it, ports, running) |
| `CreateNetwork(s name, a{sv} options) -> a{sv}` | `network create`: options subnet (CIDR; the next free /24 of network_pool otherwise), internal (b), labels (as, KEY=VALUE); the network as ListNetworks has it, plus `notes` |
| `RemoveNetworks(as names) -> o` | `network rm`: a job (kind network-rm); a network in use, unknown or the default one is refused without stopping the others; result `removed` (as) |
| `PruneNetworks() -> o` | `network prune`: a job (kind network-prune) removing every user-defined network no machine names; result `removed` (as) |
| `NetworkUp() -> a{sv}` | `network up`: every network's bridge comes up; the default network (bridge, subnet, gateway, host_name), `networks` (as, every network) and `notes` |
| `Login(s registry, s user, s password, a{sv} options) -> a{sv}` | `login`; "" for the hub; options registry (the hub "" stands for), ca_cert |
| `Logout(s registry) -> b` | `logout` |

### Signals

| Signal | When |
|---|---|
| `JobOutput(o job, s kind, s line)` | a job said a line: kind "line" (progress, a result) or "note" (a remark) |
| `JobProgress(o job, s item, t done, t total)` | how far a download of `pull` or an upload of `push` got: `done` bytes of `total` (0 when unknown) of blob `item` (its short digest); when the transfer starts, a few times a second, and when it ends. Not kept in `Output` |
| `JobRemoved(o job, s result)` | a job ended, "done" or "failed" |
| `ImageAdded(s name)`, `ImageRemoved(s name)` | after a pull, create, build or removal |
| `MachineStarted(s name)`, `MachineStopped(s name)` | machined's own events, for the machines nspawn installed |

### Events

`Events` hands out one JSON object per line, the shape `nspawn events --json`
prints (`Event` in `src/api/events.rs`):

```json
{"time": "2026-09-25T10:20:16.502186Z", "time_usec": 1790331616502186,
 "type": "machine", "action": "health_status", "name": "web",
 "attributes": {"image": "docker.io/library/busybox:latest", "status": "healthy"},
 "labels": {"role": "web"}}
```

`attributes` are the same for every action of a type, plus what the action
adds; they travel with the journal entry, so a `remove` says as much as a
`create` once the record is gone. `labels` are the machine's while it has a
record, and empty otherwise.

| Type | Actions | Attributes of every action | The action's own |
|---|---|---|---|
| `machine` | `start`, `die`, `stop`, `restart`, `oom`, `fail` (systemd's, for every `systemd-nspawn@` unit), `pull`, `build`, `create`, `push`, `kill`, `update`, `pause`, `unpause`, `health_status`, `remove` (nspawn's) | `image` (the record's reference; on systemd's actions only while the record exists) | `die`: `code` (`exited`, `killed`, `dumped`), `exit_code`, `signal`; `restart`: `restarts`; `fail`: `result`; `pull`, `build`: `reference`; `create`: `from`, `reference`; `push`: `reference` (the destination); `kill`: `signal`; `health_status`: `status` |
| `network` | `create`, `remove` | `subnet`, `interface`, `internal` | |
| `volume` | `create`, `remove` | `path` | |
| `secret` | `create`, `remove` | `size` (bytes of the plaintext) | |

`nspawn exec` is a client of `Exec`: it asks for a pseudo terminal when run
from one and for pipes otherwise, pumps them, and takes the exit status from
the process object. The terminal is allocated inside the machine, on its own
devpts, so `tty` and everything that opens its terminal by name work there.

## org.nspawn.Process at /org/nspawn/process/N

What `Exec`, `Logs`, `Events` or `RunMachine` started: properties `Machine`,
`Argv`, `Pid` (on the host), `State` (running, exited) and `ExitStatus` (128
plus the signal when it died of one); the method `Signal(i signal)`, which
reaches the process itself and never a PID handed to someone else since (for a
run, the program of an app machine, whichever process it is by then, or a
poweroff request for a booted one); the signal `Exited(i status)`, sent after
`State` changed. The Manager's `Processes` property lists them.

## org.nspawn.Job at /org/nspawn/job/N

A job is returned by the long operations and keeps what happened: properties
`Kind` (pull, create, push, build, rm, volume-rm, volume-prune, cp), `Target`, `State` (running, done,
failed), `Output` (every line so far), `Error` and `Result` (a dictionary:
for a pull its name, reference and mode; for a build its name, reference,
mode and output; for a push its name, destination and url; for a create its
name and mode; for an rm, volume-rm or volume-prune the names removed; for a cp
the entries and bytes copied). The
service does not go idle before a job or a process has announced its end.

## Example

```
job=$(busctl --system call org.nspawn /org/nspawn org.nspawn.Manager \
    PullImage 'sa{sv}' fedora:44 2 name s web force b true | awk '{print $2}' | tr -d '"')
busctl --system get-property org.nspawn "$job" org.nspawn.Job State Output
busctl --system --timeout=120 call org.nspawn /org/nspawn org.nspawn.Manager \
    StartMachine 'sa{sv}' web 1 publish as 1 8080:80
busctl --system call org.nspawn /org/nspawn org.nspawn.Manager ListMachines b false
busctl --system monitor org.nspawn
```
