# Hacking

## Building

```
cargo build --release
```

The binary is `target/release/nspawn`. Dependencies are pinned in Cargo.lock;
`cargo update` is run deliberately, not as a side effect.

## Checks

```
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

Unit tests need no root and touch nothing outside temporary directories. The
`[lints.clippy]` table of Cargo.toml turns on a few lints about clones and
allocations beyond clippy's default set; they are part of the same check.

`src/getent.rs` carries the getent nspawn hands an image without a shell as
machine code, x86_64 and aarch64, each instruction beside its bytes. A change to
it is assembled with GNU as (`as --64` with `.intel_syntax noprefix`, and
`aarch64-linux-gnu-as`) from those instructions, the bytes of `objcopy -O binary
-j .text` copied back; the code reads the table of addresses that follows it at
the offset its length gives, which a unit test checks. The e2e suite runs it in a
machine; an aarch64 build can be tried without such a host under `qemu-aarch64`.

## nspawn compose against docker compose

`nspawn compose` is checked against the docker compose release that
`src/compose/support` names, run for real. Docker is all it needs on the host:
Go runs in a container.

- `tests/compose/cases/*` hold compose files, a `runs` file with the arguments
  of each run (an `env` file with the environment and a `stdin` file with the
  standard input, where they need one), and what docker compose printed for
  every run under `expected/`.
  `cargo test` makes the runs in process and compares output, errors and
  warnings. `tests/compose/oracle.sh build` builds that docker compose from its
  source, and `tests/compose/oracle.sh record` records the cases again; a case
  added needs its record. `nspawn-N.error` next to `expected/` holds the error
  nspawn gives on purpose for a run docker compose takes. The end-to-end suite
  makes the same runs with the binary, through `tests/compose/binary.py`, which
  covers what the test in process does not: the environment, directory and stdin
  it starts with, how warnings and errors reach stderr, the exit status.
- `NSPAWN_COMPOSE_CORPUS=DIR cargo test corpus -- --ignored` does the same with
  cases kept outside the repository, recorded with `oracle.sh record DIR`.
- The Docker API (`src/dockerapi`) is checked against what docker answers:
  `tests/dockerapi/oracle.sh record DIR` runs `scenario.sh` in a docker-in-docker
  container of the release it follows (2 GiB of memory at most), keeps docker's
  answers under `tests/dockerapi/golden/` (the shapes `cargo test` compares,
  `sanitize.py` replacing what tells of the host) and, for caddy-docker-proxy,
  Traefik, nginx-proxy, acme-companion, homepage and Dozzle, every request each made
  of the socket under `tests/dockerapi/requests/` (`logproxy.py` sits between them),
  which `cargo test` routes to see that each is served.
- The YAML parser and emitter, `src/compose/libyaml.rs` and
  `src/compose/emitter.rs`, are ports of those of go-yaml, the release docker
  compose builds with. `tests/compose/go-yaml.sh events DIR` writes what
  go-yaml's parser makes of every file under DIR (events, marks, errors) and
  `NSPAWN_YAML_EVENTS=DIR cargo test parser_matches -- --ignored` compares;
  `go-yaml.sh emit DIR` and `NSPAWN_YAML_EMIT=DIR cargo test emitter_matches --
  --ignored` do the same with the YAML written of the JSON files under DIR. Any
  inputs serve: compose files, the YAML test suite, files cut or changed at
  random.
- `src/compose/unicode.rs` holds what Go's packages decide character by
  character (which characters `%q` escapes, which are letters, digits and spaces,
  lower case) and the texts of system errors, as the Go release docker compose
  builds with has them; `tests/compose/go-tables.sh` writes it again.

Their containers get 2 GiB of memory at most: docker compose sets no limit on
what aliases expand to, and a file that expands without end takes what it gets.

## End-to-end suite

`tests/e2e.sh` exercises the whole thing against a real registry and
systemd-machined: pulls with every backend, boots, execs, stops, removes,
builds with mkosi, pushes, creates, publishes ports, mounts volumes, runs apps
from Docker Hub and logs in. It changes host state (bridge, nftables,
firewall zones, units), so it runs as root on a disposable VM only.

Requirements on the VM: systemd-nspawn and machined 255 or newer (Ubuntu 24.04
is the oldest host the suite runs on; Debian 12 with systemd 252 does not
work), overlayfs, `ip`, `nft`, `curl`, `python3`, `openssl` (the suite signs
an image of its own with a key it makes), access to Docker Hub (the images the steps
use, busybox, alpine, memcached, caddy-docker-proxy and curl, are copied once into the
test registry and taken from there, since Docker Hub limits anonymous pulls by address
and the hosts of the suite share one; one step pulls from Docker Hub itself), to
lscr.io (linuxserver.io's s6-overlay bases) and to hub.nspawn.org (a signed image is
pulled from it), a registry with the test image (`fedora:44` by default) that serves
the OCI referrers API and takes OCI media types (zot does; so the copies are of
images published with them),
mkosi for the build step, and for the `--interface` step the `dummy` and
`mac80211_hwsim` kernel modules (Fedora ships the latter in
`kernel-modules-internal`, Ubuntu in `linux-modules-extra`; without it the
wireless part is skipped and says so) and `iw`. The mstack pass runs only on systemd 261 with systemd-nsresourced and
systemd-mountfsd installed; elsewhere it is skipped and says so, and so on
the systemd 262 release, which cannot boot a managed user namespace until
the fix for systemd/systemd#43899 lands in a later version. The
suite installs the bus service first with `nspawn daemon --install` (a
configuration file, /etc/nspawn/e2e.toml, carries the registry and CA), since the command
line is its client; a section drives the service with `busctl` as well, and
the service's files go at the end, another reason the suite belongs on a
disposable VM. On a host with SELinux enforcing the policy module must be
loaded first and the binary labelled, or the bus cuts the service off at the
first descriptor it passes:

```
sudo dnf install selinux-policy-devel
make -f /usr/share/selinux/devel/Makefile -C packaging/selinux nspawn.pp
sudo semodule -i packaging/selinux/nspawn.pp
sudo semanage fcontext -a -t nspawn_exec_t /usr/local/bin/nspawn
sudo restorecon -Rv /usr/local/bin/nspawn /var/lib/nspawn /etc/nspawn
```

Install the binary where a system service may execute it, since the unit
hooks and the service run it:

```
sudo install -m 755 target/release/nspawn /usr/local/bin/nspawn
sudo env NSPAWN=/usr/local/bin/nspawn NSPAWN_REGISTRY=hub.example:8443 \
    NSPAWN_CA_CERT=/etc/zot/ca.crt tests/e2e.sh
```

With `NSPAWN` pointing at a binary a package installed (`/usr/bin/nspawn` from
the rpm, deb or Arch package), the suite leaves the package's service, bus and
polkit files alone and tests them as shipped: a drop-in in
`/etc/systemd/system/nspawn.service.d/` hands the service the configuration,
and goes again at the end. That is the run that matters before a release, on
every distribution of the matrix. With SELinux enforcing, the suite reads
`/var/log/audit/audit.log` at the end and fails on any denial of `nspawn_t`
since it started, grouped by permission, process and target, so a policy that
lags behind the code shows up as a FAIL line.

The suite cleans up before and after itself and ends with `ALL OK` or a
failure count. Every FAIL line names the check. It runs from wherever it sits,
as long as `tests/build-context` and `tests/compose` sit next to it;
`NSPAWN_BUILD_CONTEXT` points it at another mkosi definition.

## Debugging a machine

- `journalctl -u systemd-nspawn@NAME.service` shows systemd-nspawn and the hooks;
  the machine's console is in `journalctl --namespace=nspawn -u
  systemd-nspawn@NAME.service` (`--log-driver local`, the default), and
  `nspawn logs NAME --all` shows the two together.
- `/etc/systemd/nspawn/NAME.nspawn` is what the machine was started with; it is
  regenerated on every start from `/var/lib/nspawn/images/NAME.json`.
- `nft list table ip nspawn` shows the DNAT map and NAT rules;
  `nspawn network ls` the addresses and ports.
- `ip netns list` shows the namespaces prepared for app machines.

## Releasing

See [RELEASING.md](RELEASING.md): the checks and the suite, the documentation
here and on the website, the version in Cargo.toml and in the packaging, the
tag, and the AUR.

## Packages

`packaging/` holds what the packages ship: the unit and the bus files
(`systemd/`, `dbus/`, kept identical to what `nspawn daemon --install`
writes, a unit test checks), the polkit actions (`polkit/`, with an example
rule for a group), the SELinux policy (`selinux/`) and the Fedora spec
(`fedora/nspawn.spec`), which builds `nspawn` and the noarch `nspawn-selinux`
from a source tarball plus a `cargo vendor` tarball.

The shell completions and the manual page are generated from the command line
itself, by `nspawn completions bash|zsh|fish` and `nspawn manpage`; the three
recipes run them at build time, so a new command or flag reaches them without
anyone writing it twice. The completion scripts are only a hook: at each TAB
the shell runs `NSPAWN_COMPLETE=bash nspawn -- WORDS`, and the binary answers
from its own definition and from `org.nspawn.Names` (`src/completion.rs`, whose
tests complete whole command lines against made-up names). clap_complete keeps
that call behind its `unstable-dynamic` feature.

`daemon --install` writes some of the same paths, which belong to the
packages: `/usr/share/dbus-1/system-services/org.nspawn.service` and
`/usr/share/polkit-1/actions/org.nspawn.policy`. Installing a package over
them is refused on Arch (pacman does not take files it does not own) and
silently overwrites them elsewhere, so remove what `--install` listed before
going from a build to a package. The
`packages` workflow builds all three in their own containers on every tag and
on demand, keeps them as artifacts, and on a tag attaches them to the GitHub
release together with the plain binary tarball (built on the oldest supported
distribution, so that it runs on the later ones) and a `SHA256SUMS` over the
lot. The suite on the Fedora VM is where the packages get tested, since a
container has neither SELinux enforcing nor machined.

