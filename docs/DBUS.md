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

writes three files with the path of the binary that ran it and reloads
systemd and the bus:

| File | Role |
|---|---|
| `/etc/dbus-1/system.d/org.nspawn.conf` | bus policy: root may own and call `org.nspawn`; everyone may introspect, read properties and receive its signals |
| `/usr/share/dbus-1/system-services/org.nspawn.service` | bus activation: `SystemdService=nspawn.service` |
| `/etc/systemd/system/nspawn.service` | `Type=dbus` unit running `nspawn daemon` |

Nothing runs until a client calls `org.nspawn`; the bus then starts the unit,
and the service exits again after a minute without a call, a job or a command
running (`nspawn daemon --idle-exit`). The service reads
`/etc/nspawn/nspawn.toml`; `nspawn --config FILE daemon --install` puts
another file on the unit's command line. The methods that reach a registry
take `registry` and `ca_cert` options that override that configuration for
one call; the command line passes its own on every call.

Methods need root for now (the bus policy says so); polkit comes later.

Where SELinux is enforcing (Fedora, RHEL) the service needs a domain of its
own, `nspawn_t`, like machined and the container runtimes have: the base
policy lets no domain touch the pipes of an unconfined service, so the bus
drops the service the moment it hands a descriptor over (`Exec`, `Shell`,
`Logs`). The policy lives in `packaging/selinux` and the `nspawn-selinux`
package loads it; `--install` says so when SELinux is enabled and the module
is missing. Commands run inside a machine take the machine's own context, as
with docker exec.

## org.nspawn.Manager at /org/nspawn

Results are dictionaries (`a{sv}`) whose keys are the command line's
spellings; options are dictionaries too, and a key nothing expects is an
error, so a typo never passes as a default. Errors come back as
`org.nspawn.Error.Failed` with the same message the command line prints.

Properties: `Version`, `Registry` (the hub), `Bridge`, `Subnet`, `Jobs` and
`Processes` (`ao`, every job and every Exec since the service came up).

### Images

| Method | Like | Notes |
|---|---|---|
| `ListImages() -> aa{sv}` | `images ls` | name, kind, backend, origin, reference, size, read_only |
| `GetImage(s name) -> a{sv}` | | everything recorded: reference, digest, backend, origin, mode, created, network, address, ports, volumes, env, entrypoint, cmd, command, image_env, working_dir, user, stop_signal |
| `PullImage(s reference, a{sv} options) -> o` | `pull` | options name, backend, mode, force, registry, ca_cert; a job |
| `CreateMachine(s source, s name, a{sv} options) -> o` | `create` | options backend, network, publish, force, entrypoint, env, volume, command, registry, ca_cert; a job |
| `PushImage(s image, a{sv} options) -> o` | `push` | options to, registry, ca_cert; a job |
| `BuildImage(s directory, s tag, a{sv} options) -> o` | `build` | options name, distribution, release, profile, backend, mode, force, keep_output, mkosi_args, registry, ca_cert; a job whose output includes mkosi's |
| `RemoveImages(as names) -> as` | `images rm` | the lines it prints |
| `SearchImages(s term, s source, u limit, a{sv} options) -> aa{sv}` | `search` | source "", "hub" or "dockerhub"; options registry, ca_cert |
| `ListRepositories(s filter, b with_tags, a{sv} options) -> aa{sv}` | `hub ls` | options registry, ca_cert |
| `ListTags(s repository, a{sv} options) -> as` | `hub tags` | options registry, ca_cert |

### Machines

| Method | Like | Notes |
|---|---|---|
| `ListMachines(b all) -> aa{sv}` | `ps`, `ps -a` | name, state, started (unix seconds), leader, os, machine_path, plus the image's record |
| `StartMachine(s name, a{sv} options) -> s` | `start` | options wait (default true), network, publish, entrypoint, env, volume, image_command, command; "started" or "ended" |
| `StopMachine(s name, a{sv} options) -> s` | `stop` | options force, wait (default true), timeout (seconds, default 10); "stopped" or "was-not-running" |
| `Exec(s machine, as argv, s user, a{sv} options) -> (a{sh}, o)` | `exec` | user "" for root; options tty (default true), rows, cols, env; returns the streams ("tty", or "stdin", "stdout", "stderr") and a process object |
| `Shell(s machine, s user) -> (h, s)` | `shell` | the login session machined offers for a booted machine: its pseudo terminal and the terminal's path; apps get `Exec` of a shell with a tty instead |
| `Logs(s machine, a{sv} options) -> h` | `logs` | options follow, lines, since, timestamps, all, inside; a pipe carrying the lines |

`StartMachine` waits for a booted machine's init and `StopMachine` for the
machine to be gone, which can take longer than a client's default timeout
(`busctl --timeout=120`).

### Network and credentials

| Method | Like |
|---|---|
| `ListNetwork() -> (a{sv}, aa{sv})` | `network ls`: the bridge (bridge, subnet, gateway, host_name) and the machines on it (name, address, ports, running) |
| `NetworkUp() -> a{sv}` | `network up` |
| `Login(s registry, s user, s password, a{sv} options) -> a{sv}` | `login`; "" for the hub; options registry (the hub "" stands for), ca_cert |
| `Logout(s registry) -> b` | `logout` |

### Signals

| Signal | When |
|---|---|
| `JobOutput(o job, s kind, s line)` | a job said a line: kind "line" (progress, a result) or "note" (a remark) |
| `JobRemoved(o job, s result)` | a job ended, "done" or "failed" |
| `ImageAdded(s name)`, `ImageRemoved(s name)` | after a pull, create, build or removal |
| `MachineStarted(s name)`, `MachineStopped(s name)` | machined's own events, for the machines nspawn installed |

`nspawn exec` is a client of `Exec`: it asks for a pseudo terminal when run
from one and for pipes otherwise, pumps them, and takes the exit status from
the process object.

## org.nspawn.Process at /org/nspawn/process/N

What `Exec` started: properties `Machine`, `Argv`, `Pid` (on the host),
`State` (running, exited) and `ExitStatus` (128 plus the signal when it died
of one); the method `Signal(i signal)`; the signal `Exited(i status)`. The
Manager's `Processes` property lists them.

## org.nspawn.Job at /org/nspawn/job/N

A job is returned by the long operations and keeps what happened: properties
`Kind` (pull, create, push, build), `Target`, `State` (running, done,
failed), `Output` (every line so far), `Error` and `Result` (a dictionary,
for a pull its name, reference and mode).

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
