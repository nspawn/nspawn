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
`ListNetwork`, `ListVolumes`) ask for
`org.nspawn.inspect`, the rest for `org.nspawn.manage`, and both are for
administrators by default, so `sudo nspawn ...` works as before and a
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
| `GetImage(s name) -> a{sv}` | | everything recorded: name, reference, digest, backend, origin, mode, created, network, address, ports, volumes, env, entrypoint, cmd, command, image_env, working_dir, user, stop_signal, labels (a{ss}: the image's with the machine's on top), image_labels, restart, memory (bytes), cpus (d), pids_limit |
| `PullImage(s reference, a{sv} options) -> o` | `pull` | options name, backend, mode, force, registry, ca_cert; a job |
| `CreateMachine(s source, s name, a{sv} options) -> o` | `create` | options backend, network, publish, force, entrypoint, env, volume, label, restart, memory (t, bytes), cpus (d), pids_limit (t), command, registry, ca_cert; a job |
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
| `ListMachines(b all) -> aa{sv}` | `ps`, `ps -a` | name, state, started (unix seconds), leader, os, machine_path, plus the image's record; state is machined's (opening, running, closing), or for a machine machined does not list restarting (listed without `all` too), starting, closing or stopped; containers only: the virtual machines machined also registers are left out, and naming one to `GetMachine`, `StopMachine`, `Exec`, `Shell`, `Logs`, `CopyFrom` or `CopyTo` fails with an error that says so |
| `GetMachine(s name) -> a{sv}` | `inspect` | one machine as `ListMachines` has it, whether it runs or not (its state is then restarting, starting, closing or stopped, as there); fails for a name that is neither running nor an image of nspawn |
| `StartMachine(s name, a{sv} options) -> (s, as)` | `start` | options wait (default true), network, publish, entrypoint, env, volume, label, restart, memory (t, bytes, 0 removes the limit), cpus (d), pids_limit (t), image_command, command; "started", "ended" (the program returned before the machine registered) or "restarting" (it ended and its restart policy brings it back), and the notes made on the way |
| `StopMachine(s name, a{sv} options) -> (s, as)` | `stop` | options force, wait (default true), timeout (seconds, default 10, a day at most); "stopped" or "was-not-running", and the notes (a program that had to be killed) |
| `KillMachine(s name, a{sv} options) -> as` | `kill` | option signal (a name such as KILL, SIGHUP or RTMIN+3, or a number; SIGKILL by default): SIGKILL is StopMachine with force, other signals go to an app's program or a booted machine's init; the machine's own stop signal keeps a restart policy from bringing it back, other signals leave that to the policy; fails for a machine that is not running; the notes |
| `Exec(s machine, as argv, s user, a{sv} options) -> (a{sh}, o)` | `exec` | user "" for root; options tty (default true), rows, cols, env (the caller's `TERM=` among them; xterm otherwise on a terminal); returns the streams ("tty", or "stdin", "stdout", "stderr") and a process object |
| `Shell(s machine, s user, a{sv} options) -> (h, s)` | `shell` | the login session machined offers for a booted machine: its pseudo terminal and the terminal's path; options env, as for `Exec`; apps get `Exec` of a shell with a tty instead |
| `CopyFrom(s machine, s path, a{sv} options) -> (h, o)` | `cp NAME:PATH ...` | a tar stream of `path` on the pipe, owners as the machine sees them, and a job with the outcome (its result: entries, bytes); a running machine, or a stopped overlay or flat one |
| `CopyTo(s machine, s path, h stream, a{sv} options) -> o` | `cp ... NAME:PATH` | the tar stream read from `stream` unpacked at `path` by docker cp's rules, everything root's inside; option contents (the source was `DIR/.`); a job |
| `Logs(s machine, a{sv} options) -> (a{sh}, o)` | `logs` | options follow, lines, since, timestamps, all, inside; journalctl's "stdout" and "stderr" and a process object for its exit status; journalctl is stopped once nobody reads its output |

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

### Network and credentials

| Method | Like |
|---|---|
| `ListNetwork() -> (a{sv}, aa{sv})` | `network ls`: the bridge (bridge, subnet, gateway, host_name) and the machines on it (name, address, ports, running) |
| `NetworkUp() -> a{sv}` | `network up`: the bridge as above, plus `notes` |
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

`nspawn exec` is a client of `Exec`: it asks for a pseudo terminal when run
from one and for pipes otherwise, pumps them, and takes the exit status from
the process object. The terminal is allocated inside the machine, on its own
devpts, so `tty` and everything that opens its terminal by name work there.

## org.nspawn.Process at /org/nspawn/process/N

What `Exec` or `Logs` started: properties `Machine`, `Argv`, `Pid` (on the
host), `State` (running, exited) and `ExitStatus` (128 plus the signal when
it died of one); the method `Signal(i signal)`, which reaches the process
itself and never a PID handed to someone else since; the signal
`Exited(i status)`, sent after `State` changed. The Manager's `Processes`
property lists them.

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
