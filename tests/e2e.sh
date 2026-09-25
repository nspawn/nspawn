#!/bin/bash
# End-to-end test of the nspawn binary against a real registry and systemd-machined.
# Run as root on a host with systemd-nspawn: NSPAWN=./nspawn NSPAWN_REGISTRY=hub:8443 ./e2e.sh
set -uo pipefail
NSPAWN=${NSPAWN:-./nspawn}
export NSPAWN_REGISTRY=${NSPAWN_REGISTRY:-hub.nspawn.test:8443}
export NSPAWN_CA_CERT=${NSPAWN_CA_CERT:-/etc/zot/ca.crt}
IMAGE=${IMAGE:-fedora:44}
failures=0
networkd_was=$(systemctl is-active systemd-networkd)
fail() { echo "FAIL: $*"; failures=$((failures + 1)); }
step() { echo; echo "### $*"; }
retry() { local n=$1; shift; local i; for i in $(seq 1 "$n"); do "$@" && return 0; sleep 2; done; return 1; }
# The bridge is IPv4 only: a machine has no IPv6 link-local address for machined to hand
# out with its name, and where the host resolves machine names (nss-mymachines or
# resolved) the name leads to the bridge address, which answers a ping.
ipv4_only() {
  local name=$1 addr=$2 first
  $NSPAWN exec "$name" -- cat /proc/net/if_inet6 </dev/null 2>/dev/null | tr -d '\r' | grep_q " host0\$" && fail "host0 of $name has an IPv6 address"
  if command -v busctl >/dev/null 2>&1; then
    busctl --system --json=short call org.freedesktop.machine1 /org/freedesktop/machine1 org.freedesktop.machine1.Manager GetMachineAddresses s "$name" \
      | python3 -c "import json,sys; d = json.load(sys.stdin)['data'][0]; assert d and all(f == 2 for f, _ in d), d" || fail "machined hands out addresses of $name other than IPv4"
  fi
  if getent hosts "$name" >/dev/null 2>&1; then
    first=$(getent ahosts "$name" | awk 'NR == 1 {print $1}')
    [ "$first" = "$addr" ] || fail "$name resolves to $first first, not to its bridge address $addr"
    getent ahosts "$name" | grep_q "^fe80" && fail "$name resolves to a link-local IPv6 address"
    ping -c 1 -W 3 "$name" >/dev/null || fail "ping $name"
  else
    echo "this host does not resolve machine names: name and ping checks skipped"
  fi
}
# grep -q in a pipeline stops reading at the first match, and the writer then dies of
# SIGPIPE, which pipefail counts as a failure: read everything instead.
grep_q() { grep "$@" >/dev/null; }
export -f grep_q
# Runs a command on a pseudo terminal of its own (script(1) is not on every host).
pyrun() { python3 -c 'import os, pty, sys; sys.exit(os.waitstatus_to_exitcode(pty.spawn(sys.argv[1:])))' "$@"; }
# A machine's address on its network, as inspect has it.
addr_of() { $NSPAWN inspect "$1" | python3 -c "import json,sys; print(json.load(sys.stdin)[0].get('address') or '')"; }
nonce=$$
# The command line is a client of the org.nspawn service: it goes on the bus first,
# with a configuration file that names the registry and its CA for the service's own
# use (the command line passes them on every call anyway). A binary a package installed
# keeps the package's own service, bus and polkit files, which is what is under test
# then; only a drop-in hands it the configuration. Anything else is installed with
# daemon --install and removed at the end.
packaged=no
if rpm -qf "$NSPAWN" >/dev/null 2>&1 || dpkg -S "$(readlink -f "$NSPAWN")" >/dev/null 2>&1 || pacman -Qo "$NSPAWN" >/dev/null 2>&1; then
  packaged=yes
fi
install_service() {
  # With SELinux enforcing the service needs its domain (packaging/selinux) loaded and
  # the binary labelled nspawn_exec_t, or the bus drops it at the first descriptor.
  if [ "$(getenforce 2>/dev/null)" = Enforcing ]; then
    semodule -l 2>/dev/null | grep_q -x nspawn || { echo "SELinux is enforcing and the nspawn policy module is not loaded; see docs/HACKING.md"; exit 1; }
    [ "$(stat -c %C "$NSPAWN" | cut -d: -f3)" = nspawn_exec_t ] || { echo "$NSPAWN is not labelled nspawn_exec_t; see docs/HACKING.md"; exit 1; }
  fi
  mkdir -p /etc/nspawn
  printf 'registry = "%s"\nca_cert = "%s"\n' "$NSPAWN_REGISTRY" "$NSPAWN_CA_CERT" > /etc/nspawn/e2e.toml
  # A service left running from before would serve this run with its own configuration.
  systemctl stop nspawn.service >/dev/null 2>&1 || true
  if [ "$packaged" = yes ]; then
    mkdir -p /etc/systemd/system/nspawn.service.d
    printf '[Service]\nExecStart=\nExecStart=%s --config /etc/nspawn/e2e.toml daemon\n' "$NSPAWN" > /etc/systemd/system/nspawn.service.d/50-e2e.conf
    systemctl daemon-reload
    echo "$NSPAWN belongs to a package: testing the packaged service with a configuration drop-in" | tee /tmp/e2e-install.txt
    return
  fi
  $NSPAWN --config /etc/nspawn/e2e.toml daemon --install > /tmp/e2e-install.txt 2>&1 || { cat /tmp/e2e-install.txt; echo "cannot install the service"; exit 1; }
  cat /tmp/e2e-install.txt
}
# Leftovers of an aborted run would make pulls and creates fail; the same at the end.
cleanup_machines() {
  local m
  for m in e2e-overlay e2e-flat e2e-mstack e2e-a e2e-b e2e-c e2e-built e2e-roundtrip e2e-busybox e2e-run e2e-dbus e2e-digest e2e-restart e2e-twin-a e2e-twin-b e2e-na-web e2e-na-cli e2e-nb-web e2e-nc-web e2e-nc-pub e2e-def-cli e2e-nab e2e-none e2e-boot2 e2e-pclash e2e-run-boot busybox-1.37; do
    $NSPAWN stop "$m" --force >/dev/null 2>&1 || true
    $NSPAWN images rm "$m" >/dev/null 2>&1 || true
  done
  $NSPAWN network rm e2e-na e2e-nb e2e-nc e2e-nd e2e-ne >/dev/null 2>&1 || true
  $NSPAWN secret rm e2e-pw e2e-pw2 >/dev/null 2>&1 || true
  rm -f /var/lib/nspawn/secrets/e2e-pw.cred /var/lib/nspawn/secrets/e2e-pw.json /var/lib/nspawn/secrets/e2e-pw2.cred /var/lib/nspawn/secrets/e2e-pw2.json
  # Machines of run --rm have names of their own.
  for m in $($NSPAWN images ls 2>/dev/null | awk '$1 ~ /^busybox-1\.37-/ {print $1}'); do
    $NSPAWN rm -f "$m" >/dev/null 2>&1 || true
  done
  $NSPAWN images rm busybox-1.37 >/dev/null 2>&1 || true
  $NSPAWN logout "$NSPAWN_REGISTRY" >/dev/null 2>&1 || true
  rm -rf /tmp/e2e-cp /tmp/e2e-cp-* /tmp/e2e-bind /tmp/e2e-boot-vol /var/lib/nspawn/volumes/e2evol /var/lib/nspawn/volumes/e2evol2 /var/lib/nspawn/volumes/e2evol-free /var/lib/nspawn/volumes/e2evol-events /var/lib/nspawn/volumes/e2e-bootvol /var/lib/nspawn/volumes/.e2e-hidden
  kill "${listener_pid:-}" 2>/dev/null || true
  if [ "$networkd_was" != active ]; then
    systemctl stop systemd-networkd.service systemd-networkd.socket systemd-networkd-varlink.socket systemd-networkd-resolve-hook.socket >/dev/null 2>&1 || true
  fi
}
cleanup_service() {
  systemctl stop nspawn.service >/dev/null 2>&1 || true
  if [ "$packaged" = yes ]; then
    rm -f /etc/systemd/system/nspawn.service.d/50-e2e.conf /etc/nspawn/e2e.toml
    rmdir /etc/systemd/system/nspawn.service.d 2>/dev/null || true
  else
    rm -f /etc/dbus-1/system.d/org.nspawn.conf /usr/share/dbus-1/system-services/org.nspawn.service /etc/systemd/system/nspawn.service /etc/nspawn/e2e.toml
  fi
  systemctl daemon-reload >/dev/null 2>&1 || true
}
cleanup() {
  cleanup_machines
  cleanup_service
  # A step that hides iptables must not leave it hidden, however it ended.
  if [ -n "${hidden_iptables:-}" ] && [ -e "${hidden_iptables}.e2e-hidden" ]; then
    mv "${hidden_iptables}.e2e-hidden" "$hidden_iptables"
  fi
}
install_service
cleanup_machines
trap cleanup EXIT

step "hub ls"
$NSPAWN hub ls | tee /tmp/e2e-hub.txt || fail "hub ls exited non-zero"
grep -q "${IMAGE%%:*}" /tmp/e2e-hub.txt || fail "hub ls does not list ${IMAGE%%:*}"
step "search: the hub and Docker Hub, each hit with its source"
$NSPAWN search "${IMAGE%%:*}" > /tmp/e2e-search.txt || fail "search exited non-zero"
grep "^ *$NSPAWN_REGISTRY " /tmp/e2e-search.txt | grep_q " ${IMAGE%%:*} " || fail "search does not list ${IMAGE%%:*} from the hub"
$NSPAWN search busybox --source dockerhub > /tmp/e2e-search.txt || fail "search on Docker Hub exited non-zero"
grep "^ *Docker Hub " /tmp/e2e-search.txt | grep_q "docker.io/library/busybox" || fail "search does not list busybox from Docker Hub with its source"

step "login and logout"
echo "s3cret" | $NSPAWN login "$NSPAWN_REGISTRY" -u tester --password-stdin || fail "login on the hub"
python3 -c "import json; d = json.load(open('/etc/nspawn/auth.json')); assert '$NSPAWN_REGISTRY' in d['auths']" || fail "credentials not stored"
[ "$(stat -c %a /etc/nspawn/auth.json)" = 600 ] || fail "auth.json is not mode 0600"
$NSPAWN hub ls >/dev/null || fail "hub ls with stored credentials"
out=$(echo "wrong-password" | $NSPAWN login docker.io -u nspawn-e2e-nobody --password-stdin 2>&1) && fail "Docker Hub accepted bogus credentials: $out"
echo "$out" | grep_q "rejected the credentials" || fail "bogus Docker Hub login gave no clear message: $out"
$NSPAWN logout "$NSPAWN_REGISTRY" | grep_q "removed" || fail "logout"
$NSPAWN logout "$NSPAWN_REGISTRY" | grep_q "no credentials" || fail "second logout should find nothing"

step "hub tags"
$NSPAWN hub tags "${IMAGE%%:*}" > /tmp/e2e-tags.txt || fail "hub tags"
grep -qx "${IMAGE##*:}" /tmp/e2e-tags.txt || fail "tag ${IMAGE##*:} missing"

# mstack images need systemd 261 with managed user namespaces (nsresourced, mountfsd).
mstack_supported=no
if [ "$(systemctl --version | awk 'NR==1{print $2}' | tr -dc 0-9)" -ge 261 ] 2>/dev/null \
  && [ -e /usr/lib/systemd/system/systemd-nsresourced.socket ] \
  && [ -e /usr/lib/systemd/system/systemd-mountfsd.socket ]; then
  mstack_supported=yes
fi
for backend in overlay flat mstack; do
  if [ "$backend" = mstack ] && [ "$mstack_supported" != yes ]; then
    echo "mstack: needs systemd 261 with nsresourced and mountfsd; skipped on this host"
    continue
  fi
  name=e2e-$backend
  step "pull $IMAGE --backend $backend"
  $NSPAWN pull "$IMAGE" --name "$name" --backend "$backend" --force > /tmp/e2e-pull.txt 2>&1 || { cat /tmp/e2e-pull.txt; fail "pull ($backend)"; continue; }
  cat /tmp/e2e-pull.txt
  grep -q "(boot image)" /tmp/e2e-pull.txt || fail "$IMAGE not detected as a boot image"
  step "images ls"
  $NSPAWN images ls | tee /tmp/e2e-img.txt
  grep -q "^ *$name " /tmp/e2e-img.txt || fail "$name not listed"
  grep "^ *$name " /tmp/e2e-img.txt | grep_q "$backend" || fail "$name backend not shown"
  $NSPAWN images ls --json | python3 -c "import json,sys; d = [i for i in json.load(sys.stdin) if i['name'] == '$name']; assert d and d[0]['backend'] == '$backend', d" || fail "images ls --json misses $name or its backend"
  step "start"
  vol_args=""
  if [ "$backend" != flat ]; then
    rm -rf /tmp/e2e-boot-vol; mkdir -p /tmp/e2e-boot-vol
    vol_args="-v /tmp/e2e-boot-vol:/srv/vol -v e2e-bootvol:/srv/named"
  fi
  $NSPAWN start "$name" $vol_args || fail "start ($backend)"
  if [ "$backend" = overlay ]; then
    findmnt -n -o FSTYPE "/var/lib/machines/$name" | grep_q overlay || fail "root of $name is not an overlay"
    # nspawn shifts the tree with a recursive chown (overlayfs cannot be idmapped); with
    # metacopy that copies inodes, without it the whole image.
    upper_mb=$(du -sm "/var/lib/nspawn/machines/$name/upper" | cut -f1)
    echo "upper directory of $name after the first start: ${upper_mb} MB"
    [ "$upper_mb" -lt 128 ] || fail "the first start copied the image into the upper directory of $name (${upper_mb} MB)"
  fi
  if [ "$backend" = mstack ]; then
    [ -d "/var/lib/machines/$name.mstack" ] || fail "$name has no mstack directory"
    systemctl is-active systemd-nsresourced.socket >/dev/null || fail "systemd-nsresourced.socket not started for the mstack machine"
  fi
  step "machines ls"
  $NSPAWN machines ls | tee /tmp/e2e-m.txt
  grep -q "^ *$name " /tmp/e2e-m.txt || fail "$name not running"
  step "machine-readable output: ps --json and inspect"
  $NSPAWN ps --json | python3 -c "import json,sys; d = [m for m in json.load(sys.stdin) if m['name'] == '$name']; assert d and d[0]['state'] == 'running' and d[0]['leader'] > 0, d" || fail "ps --json misses $name running"
  $NSPAWN inspect "$name" | python3 -c "import json,sys; d = json.load(sys.stdin); assert len(d) == 1 and d[0]['name'] == '$name' and d[0]['mode'] == 'boot' and d[0]['backend'] == '$backend', d" || fail "inspect $name"
  $NSPAWN inspect "$name" e2e-nonexistent >/dev/null 2>&1 && fail "inspect of a missing machine succeeded"
  step "exec"
  out=$($NSPAWN exec "$name" -- /usr/bin/systemctl is-system-running --wait </dev/null | tr -d '\r' || true)
  echo "is-system-running: $out"
  echo "$out" | grep_q -E "running|degraded|starting" || fail "exec did not reach systemd inside $name"
  $NSPAWN exec "$name" -- /usr/bin/cat /etc/os-release </dev/null | tr -d '\r' | grep_q PRETTY_NAME || fail "exec cat os-release"
  $NSPAWN exec "$name" -- /bin/sh -c 'exit 7' </dev/null; [ $? -eq 7 ] || fail "exec did not propagate the exit code of a booted machine"
  $NSPAWN exec "$name" -- /bin/sh -c 'echo $PATH' </dev/null | tr -d '\r' | grep_q "/usr/bin" || fail "exec has no PATH"
  step "terminal: exec with a pty, shell through machined, the machine's own journal"
  out=$(python3 "$(dirname "$0")/terminal.py" $NSPAWN exec "$name" -- /bin/sh -c 'tty; echo term=$TERM; exit 3' </dev/null 2>&1 | tr -d '\r'); rc=${PIPESTATUS[0]}
  [ "$rc" = 3 ] || fail "exec on a pty did not propagate the exit code (got $rc)"
  echo "$out" | grep_q "^/dev/pts/" || fail "exec from a terminal got no pty of the machine's: $out"
  echo "$out" | grep_q "term=${TERM:-xterm}" || fail "exec on a pty has no TERM: $out"
  out=$(printf 'echo shell-%s term=$TERM; exit\n' "$nonce" | python3 "$(dirname "$0")/terminal.py" $NSPAWN shell "$name" 2>&1 | tr -d '\r')
  echo "$out" | grep_q "shell-$nonce term=${TERM:-xterm}" || fail "shell on $name did not run a command with a TERM: $(echo "$out" | tail -2)"
  $NSPAWN logs "$name" --inside -n 3 </dev/null | grep_q . || fail "logs --inside of $name is empty"
  $NSPAWN logs "$name" --all -n 3 </dev/null >/dev/null || fail "logs --all of $name"
  $NSPAWN logs "$name" --since bogus </dev/null >/tmp/e2e-logs-out.txt 2>/tmp/e2e-logs-err.txt; rc=$?
  [ "$rc" != 0 ] || fail "logs --since bogus exited 0"
  [ ! -s /tmp/e2e-logs-out.txt ] || fail "journalctl's complaint went to stdout: $(cat /tmp/e2e-logs-out.txt)"
  grep -qi "bogus\|timestamp" /tmp/e2e-logs-err.txt || fail "logs --since bogus said nothing on stderr: $(cat /tmp/e2e-logs-err.txt)"
  timeout 5 $NSPAWN logs "$name" -f -n 2 </dev/null >/dev/null 2>&1; [ $? = 124 ] || fail "logs --follow did not keep following"
  retry 5 bash -c "! pgrep -f '[j]ournalctl.*$name' >/dev/null" || fail "journalctl kept following after its client left"
  step "cp: files in and out of a running machine ($backend)"
  cpd=/tmp/e2e-cp; rm -rf $cpd; mkdir -p $cpd/src/sub
  echo "hello-$nonce" > $cpd/src/a.txt; chmod 640 $cpd/src/a.txt; touch -d @1000000 $cpd/src/a.txt
  head -c 70000 /dev/urandom > $cpd/src/sub/b.bin; ln -s a.txt $cpd/src/rel
  $NSPAWN cp $cpd/src/a.txt "$name:/root/" || fail "cp of a file into $name"
  $NSPAWN cp $cpd/src/a.txt "$name:/tmp/tmpfs.txt" || fail "cp into the /tmp (tmpfs) of $name"
  [ "$($NSPAWN exec "$name" -- stat -c %u /tmp/tmpfs.txt </dev/null | tr -d '\r')" = 0 ] || fail "a file copied into the /tmp of $name is not root's"
  $NSPAWN cp "$name:/tmp/tmpfs.txt" $cpd/tmpfs-back.txt && cmp -s $cpd/src/a.txt $cpd/tmpfs-back.txt || fail "cp out of the /tmp (tmpfs) of $name"
  [ "$($NSPAWN exec "$name" -- stat -c '%u:%g %a %Y' /root/a.txt </dev/null | tr -d '\r')" = "0:0 640 1000000" ] || fail "a file copied into $name is not root's with its mode and time: $($NSPAWN exec "$name" -- stat -c '%u:%g %a %Y' /root/a.txt </dev/null)"
  $NSPAWN exec "$name" -- cat /root/a.txt </dev/null | tr -d '\r' | grep_q "hello-$nonce" || fail "the copied file has the wrong content"
  $NSPAWN cp $cpd/src "$name:/opt/copied" || fail "cp of a directory to a new name in $name"
  [ "$($NSPAWN exec "$name" -- readlink /opt/copied/rel </dev/null | tr -d '\r')" = a.txt ] || fail "a link inside a copied directory did not stay a link"
  $NSPAWN cp $cpd/src "$name:/opt/" && $NSPAWN exec "$name" -- test -f /opt/src/sub/b.bin </dev/null || fail "cp of a directory into an existing one"
  $NSPAWN cp $cpd/src/a.txt "$name:/no-such-dir-$nonce/" >/dev/null 2>&1 && fail "cp to a missing directory with a trailing slash succeeded"
  $NSPAWN cp "$name:/opt/copied" $cpd/out || fail "cp of a directory out of $name"
  diff -r $cpd/src $cpd/out >/dev/null || fail "the directory copied out differs from what went in"
  # An absolute link in the middle of the path is followed inside the machine.
  $NSPAWN exec "$name" -- ln -sfn /root /tmp/rootlink </dev/null
  $NSPAWN cp "$name:/tmp/rootlink/a.txt" $cpd/through-link || fail "cp through an absolute link inside $name"
  grep -q "hello-$nonce" $cpd/through-link 2>/dev/null || fail "an absolute link inside $name was not followed inside it"
  $NSPAWN exec "$name" -- ln -sfn /proc/1/root/etc /root/magic </dev/null
  $NSPAWN cp "$name:/root/magic/hostname" $cpd/magic >/dev/null 2>&1 && fail "cp went through a magic link of /proc"
  $NSPAWN cp "$name:/usr/bin/bash" $cpd/bash || fail "cp of a binary out of $name"
  [ "$(sha256sum < $cpd/bash | cut -d' ' -f1)" = "$($NSPAWN exec "$name" -- sha256sum /usr/bin/bash </dev/null | tr -d '\r' | cut -d' ' -f1)" ] || fail "a binary copied out is not byte exact"
  timeout 1 $NSPAWN cp "$name:/usr" $cpd/usr-partial >/dev/null 2>&1
  sleep 1
  systemctl is-active nspawn.service >/dev/null || fail "a cp client that went away took the service down"
  if [ "$backend" != flat ]; then
    step "volume in a booted machine ($backend: private users, idmapped)"
    $NSPAWN exec "$name" -- /bin/sh -c 'echo booted > /srv/vol/from-machine' </dev/null || fail "cannot write to the volume inside $name"
    [ "$(cat /tmp/e2e-boot-vol/from-machine 2>/dev/null)" = booted ] || fail "volume write not visible on the host"
    [ "$(stat -c %u /tmp/e2e-boot-vol/from-machine)" = 0 ] || fail "root inside did not write as root on the host (idmap)"
    $NSPAWN exec "$name" -- /usr/bin/systemctl is-active nspawn-volumes.service </dev/null | tr -d '\r' | grep_q -x active || fail "nspawn-volumes.service not active inside $name"
    # Volumes are idmapped binds: a copy into one has to be made as the machine's root,
    # and lands as root on the host. A named volume is nspawn's own directory; a host
    # directory keeps its SELinux label, which the service may not write to (as with
    # docker without :z), so that one is only tried where SELinux does not enforce.
    $NSPAWN cp /tmp/e2e-cp/src/a.txt "$name:/srv/named/" || fail "cp into the named volume of $name"
    [ "$(stat -c %u /var/lib/nspawn/volumes/e2e-bootvol/a.txt 2>/dev/null)" = 0 ] || fail "a file copied into the named volume of $name is not root's on the host"
    [ "$($NSPAWN exec "$name" -- stat -c %u /srv/named/a.txt </dev/null | tr -d '\r')" = 0 ] || fail "a file copied into the named volume of $name is not root's inside"
    if [ "$(getenforce 2>/dev/null)" != Enforcing ]; then
      $NSPAWN cp /tmp/e2e-cp/src/a.txt "$name:/srv/vol/" || fail "cp into the host-directory volume of $name"
      [ "$(stat -c %u /tmp/e2e-boot-vol/a.txt 2>/dev/null)" = 0 ] || fail "a file copied into the volume of $name is not root's on the host"
    fi
  fi
  step "network through the nspawn bridge"
  if [ "$networkd_was" != active ]; then
    systemctl is-active systemd-networkd >/dev/null && fail "systemd-networkd got started on the host; the bridge must not need it"
  fi
  ip -br addr show nspawn0 | grep_q "10.99.0.1/24" || fail "bridge nspawn0 missing or without its address"
  nft list map ip nspawn ports >/dev/null 2>&1 || fail "nftables table of the bridge missing"
  if firewall-cmd --state >/dev/null 2>&1; then
    # NetworkManager takes a new bridge over for a moment and firewalld follows it before
    # settling on the binding nspawn made; the binding itself is not in question.
    retry 5 bash -c "[ \"\$(firewall-cmd --get-zone-of-interface=nspawn0)\" = trusted ]" || fail "nspawn0 is not in the trusted zone of firewalld"
  fi
  addr=$(addr_of "$name")
  echo "$name has address $addr"
  echo "$addr" | grep_q "^10\.99\.0\." || fail "no bridge address recorded for $name"
  $NSPAWN network inspect bridge | python3 -c "import json,sys; d = json.load(sys.stdin)[0]; assert d['interface'] == 'nspawn0' and any(m['name'] == '$name' and m['address'] == '$addr' and m['running'] for m in d['machines']), d" || fail "network inspect bridge"
  $NSPAWN network ls --json | python3 -c "import json,sys; d = json.load(sys.stdin); assert d[0]['name'] == 'bridge' and '$name' in d[0]['machines'], d" || fail "network ls --json"
  ip -6 addr show dev nspawn0 scope link 2>/dev/null | grep_q inet6 && fail "the bridge has an IPv6 link-local address"
  # ps lists containers only: machined also registers virtual machines (libvirt).
  for m in $($NSPAWN ps --json | python3 -c "import json,sys; print(' '.join(m['name'] for m in json.load(sys.stdin)))"); do
    [ "$(machinectl show "$m" -p Class --value 2>/dev/null)" = container ] || fail "ps lists $m, which is not a container"
  done
  retry 15 bash -c "$NSPAWN exec $name -- /bin/sh -c 'ip -4 -o addr show host0 | grep -q $addr/24 && curl -sf -m 5 -o /dev/null https://download.opensuse.org/ && echo NET-OK' </dev/null | tr -d '\r' | grep_q NET-OK" \
    || { echo "-- inside $name:"; $NSPAWN exec "$name" -- /bin/sh -c 'ip -4 -o addr; ip route; ping -c 1 -W 2 10.99.0.1; curl -sS -m 5 -o /dev/null https://download.opensuse.org/' </dev/null 2>&1 | tr -d '\r'; bridge link show; fail "no network inside $name through the bridge"; }
  # Once host0 is configured (above), what the host resolves for the machine's name.
  ipv4_only "$name" "$addr"
  step "stop"
  $NSPAWN stop "$name" || fail "stop ($backend)"
  retry 15 bash -c "! $NSPAWN machines ls | grep_q '^ *$name '" || fail "$name still running after stop"
  if [ "$backend" = flat ]; then
    step "legacy veth network (systemd-networkd on the host)"
    $NSPAWN start "$name" --network veth || fail "start --network veth"
    systemctl is-active systemd-networkd >/dev/null || fail "start --network veth did not activate systemd-networkd"
    if firewall-cmd --state >/dev/null 2>&1; then
      [ "$(firewall-cmd --get-zone-of-interface="ve-$name")" = trusted ] || fail "ve-$name is not in the trusted zone of firewalld"
    fi
    retry 15 bash -c "$NSPAWN exec $name -- /bin/sh -c 'curl -sf -m 5 -o /dev/null https://download.opensuse.org/ && echo NET-OK' </dev/null | tr -d '\r' | grep_q NET-OK" \
      || fail "no network inside $name over the veth"
    $NSPAWN stop "$name" || fail "stop veth machine"
    if firewall-cmd --state >/dev/null 2>&1; then
      firewall-cmd --zone=trusted --list-interfaces | grep_q -w "ve-$name" && fail "ve-$name still bound in firewalld after stop"
    fi
    $NSPAWN start "$name" --network bridge >/dev/null && $NSPAWN stop "$name" >/dev/null || fail "back to the bridge network"
    if [ "$networkd_was" != active ]; then
      systemctl stop systemd-networkd.service systemd-networkd.socket systemd-networkd-varlink.socket systemd-networkd-resolve-hook.socket >/dev/null 2>&1 || true
    fi
  fi
  $NSPAWN inspect "$name" | python3 -c "import json,sys; d = json.load(sys.stdin); assert d[0]['state'] == 'stopped', d" || fail "inspect of a stopped $name"
  step "cp into a stopped machine ($backend)"
  if [ "$backend" = mstack ]; then
    out=$($NSPAWN cp /tmp/e2e-cp/src/a.txt "$name:/root/stopped.txt" 2>&1) && fail "cp into a stopped mstack machine succeeded"
    echo "$out" | grep_q "start it first" || fail "cp into a stopped mstack machine was not explained: $out"
  else
    if [ "$backend" = overlay ]; then
      systemctl stop "$(systemd-escape -p --suffix=mount "/var/lib/machines/$name")" || fail "cannot unmount the overlay of $name"
    fi
    $NSPAWN cp /tmp/e2e-cp/src/a.txt "$name:/root/stopped.txt" || fail "cp into a stopped $backend machine"
  fi
  step "stop right after start"
  $NSPAWN start "$name" || fail "start after cp ($backend)"
  if [ "$backend" != mstack ]; then
    [ "$($NSPAWN exec "$name" -- stat -c %u /root/stopped.txt </dev/null | tr -d '\r')" = 0 ] || fail "a file copied into the stopped $backend machine is not root's inside"
  fi
  $NSPAWN stop "$name" || fail "stop right after start ($backend)"
  step "images rm"
  $NSPAWN images rm "$name" || fail "images rm ($backend)"
  $NSPAWN images ls > /tmp/e2e-img.txt; grep -q "^ *$name " /tmp/e2e-img.txt && fail "$name still listed after rm"
  [ -e "/var/lib/machines/$name" ] && fail "/var/lib/machines/$name still exists"
  ls /etc/systemd/system/ | grep_q "e2e" && fail "unit files left behind for $name"
done

step "layer sharing between two images"
$NSPAWN pull "$IMAGE" --name e2e-a --backend overlay --force >/dev/null || fail "pull e2e-a"
$NSPAWN pull "$IMAGE" --name e2e-b --backend overlay --force | tee /tmp/e2e-p2.txt || fail "pull e2e-b"
grep -q "already present" /tmp/e2e-p2.txt || fail "second pull downloaded the layer again"

step "a machine an administrator enabled at boot stays enabled when its image is replaced"
systemctl enable systemd-nspawn@e2e-b.service >/dev/null 2>&1 || fail "systemctl enable e2e-b"
$NSPAWN pull "$IMAGE" --name e2e-b --backend overlay --force >/dev/null || fail "pull --force over an enabled machine"
[ "$(systemctl is-enabled systemd-nspawn@e2e-b.service 2>/dev/null)" = enabled ] || fail "pull --force took e2e-b off the boot list an administrator put it on"

step "create: arguments are checked before anything is made"
$NSPAWN create e2e-a e2e-bad -e X=1 >/dev/null 2>&1 && fail "create accepted -e for a booted image"
$NSPAWN images ls | grep_q "^ *e2e-bad " && fail "a refused create left a machine behind"
$NSPAWN create e2e-a e2e-bad -v "bad volume" >/dev/null 2>&1 && fail "create accepted a bad volume"
[ -e /etc/systemd/nspawn/e2e-bad.nspawn ] && fail "a refused create left settings behind"

step "create: another machine from a local image, without the registry"
env NSPAWN_REGISTRY=127.0.0.1:9 $NSPAWN create e2e-a e2e-c || fail "create from a local image"
$NSPAWN images ls | grep "^ *e2e-c " | grep_q "create" || fail "created machine not listed with origin create"
$NSPAWN start e2e-c || fail "start created machine"
retry 10 $NSPAWN exec e2e-c -- /usr/bin/test -f /etc/os-release </dev/null || fail "exec in created machine"
$NSPAWN network inspect bridge | grep_q '"name": "e2e-c"' || fail "created machine not on the bridge"
$NSPAWN stop e2e-c || fail "stop created machine"
$NSPAWN images rm e2e-c | tee /tmp/e2e-rmc.txt || fail "rm created machine"
grep -q "freed" /tmp/e2e-rmc.txt && fail "removing the created machine freed a layer still used by e2e-a and e2e-b"

step "two machines on the bridge: names and published ports"
$NSPAWN start e2e-a || fail "start e2e-a"
$NSPAWN start e2e-b -p 18080:80 -p 127.0.0.1:18082:80 -p 18100-18101:80-81 || fail "start e2e-b with published ports"
$NSPAWN exec e2e-b -- /usr/bin/systemctl is-system-running --wait </dev/null >/dev/null 2>&1 || true
# An echo service on port 80 inside e2e-b, from socket activation: no extra packages needed.
$NSPAWN exec e2e-b -- /bin/sh -c 'printf "[Socket]\nListenStream=80\nAccept=yes\n" > /etc/systemd/system/echo.socket; printf "[Service]\nExecStart=/usr/bin/cat\nStandardInput=socket\n" > /etc/systemd/system/echo@.service; systemctl daemon-reload; systemctl start echo.socket && echo ECHO-UP' </dev/null | tr -d '\r' | grep_q ECHO-UP || fail "echo service inside e2e-b"
b_addr=$(addr_of e2e-b)
echo "e2e-b has address $b_addr"
$NSPAWN network inspect bridge | grep_q "18080->80/tcp" || fail "published port not listed by network inspect"
$NSPAWN ps | grep "^ *e2e-b " | grep_q "18080->80/tcp" || fail "published port not shown by ps"
echo_test() { timeout 5 bash -c "exec 3<>/dev/tcp/$1/$2 || exit 1; echo $3 >&3; read -t 3 l <&3; [ \"\$l\" = $3 ]" 2>/dev/null; }
retry 5 echo_test "$b_addr" 80 direct || fail "e2e-b not reachable on its bridge address $b_addr"
echo_test 127.0.0.1 18080 loopback || fail "published port not reachable on 127.0.0.1"
host_ip=$(ip -4 route get 1.1.1.1 | awk '{for (i = 1; i <= NF; i++) if ($i == "src") print $(i + 1); exit}')
echo_test "$host_ip" 18080 hostaddr || fail "published port not reachable on the host address $host_ip"
# One address of the host alone, and a range.
echo_test 127.0.0.1 18082 onlyloop || fail "a port published on 127.0.0.1 does not answer there"
echo_test "$host_ip" 18082 leaked && fail "a port published on 127.0.0.1 answered on $host_ip"
nft list map ip nspawn addr_ports | grep_q "127.0.0.1 . tcp . 18082" || fail "the port on one address is not in addr_ports"
echo_test 127.0.0.1 18100 range || fail "the first port of a range does not answer"
$NSPAWN inspect e2e-b | python3 -c "import json,sys; d = json.load(sys.stdin)[0]; assert d['ports'] == ['18080->80/tcp', '127.0.0.1:18082->80/tcp', '18100->80/tcp', '18101->81/tcp'], d['ports']" || fail "inspect does not list the published ports as expected"
$NSPAWN create e2e-a e2e-pclash -p 127.0.0.1:18080:80 >/dev/null || fail "create e2e-pclash"
out=$($NSPAWN start e2e-pclash 2>&1) && fail "a port published on every address was published again on one"
echo "$out" | grep_q "already published by e2e-b" || fail "the clash was not explained: $out"
$NSPAWN images rm e2e-pclash >/dev/null || fail "rm e2e-pclash"
$NSPAWN exec e2e-a -- /bin/sh -c "getent hosts e2e-b" </dev/null | tr -d '\r' | grep_q "$b_addr" || fail "e2e-a does not resolve e2e-b"
$NSPAWN exec e2e-a -- /bin/sh -c "getent hosts host.nspawn.internal" </dev/null | tr -d '\r' | grep_q "10.99.0.1" || fail "host.nspawn.internal not resolvable"
$NSPAWN exec e2e-a -- /bin/bash -c 'exec 3<>/dev/tcp/e2e-b/80 && echo a-to-b >&3 && read -t 3 l <&3 && echo "reply:$l"' </dev/null | tr -d '\r' | grep_q "reply:a-to-b" || fail "e2e-a cannot reach e2e-b by name"
$NSPAWN stop e2e-b || fail "stop e2e-b"
$NSPAWN stop e2e-a || fail "stop e2e-a"
nft list map ip nspawn ports | grep_q 18080 && fail "published port still mapped after stop"
nft list map ip nspawn addr_ports | grep_q 18082 && fail "the port on one address is still mapped after stop"

step "run of a booted image: its console until it powers off, a shell with -it"
$NSPAWN run --rm "$IMAGE" --name e2e-run-boot > /tmp/e2e-run-boot.txt 2>/tmp/e2e-run-boot.err &
run_pid=$!
retry 30 $NSPAWN exec e2e-run-boot -- /usr/bin/systemctl is-system-running --wait </dev/null >/dev/null 2>&1 || true
$NSPAWN exec e2e-run-boot -- /usr/bin/systemctl poweroff </dev/null >/dev/null 2>&1
wait $run_pid; rc=$?
[ "$rc" = 0 ] || fail "run of a booted image did not end with 0 after a poweroff: $rc $(tail -3 /tmp/e2e-run-boot.err)"
grep_q -i "reached target" /tmp/e2e-run-boot.txt || fail "run of a booted image did not show its console"
$NSPAWN images ls | grep_q "^ *e2e-run-boot " && fail "run --rm left the booted machine behind"
# The line waits in the terminal until the shell reads it, once the machine is up.
echo 'exit 7' | pyrun $NSPAWN run -it --rm "$IMAGE" --name e2e-run-boot >/dev/null 2>&1; rc=$?
[ "$rc" = 7 ] || fail "run -it of a booted image did not give the shell's exit code: $rc"
$NSPAWN images ls | grep_q "^ *e2e-run-boot " && fail "run -it --rm left the booted machine behind"
out=$($NSPAWN run -t --rm "$IMAGE" --name e2e-run-boot 2>&1) && fail "run -t alone of a booted image succeeded"
echo "$out" | grep_q "shell" || fail "run -t alone of a booted image was not explained: $out"
$NSPAWN images ls | grep_q "^ *e2e-run-boot " && fail "a refused run --rm left its machine behind"

step "restart policy on a booted machine: its init killed, it boots again"
$NSPAWN start e2e-a --restart on-failure --memory 256m || fail "start e2e-a with a restart policy"
[ "$(systemctl show -p MemoryMax --value systemd-nspawn@e2e-a.service)" = 268435456 ] || fail "--memory not applied to the unit of a booted machine"
kill -KILL "$(machinectl show e2e-a -p Leader --value)" || fail "cannot kill the init of e2e-a"
retry 20 bash -c "[ \"\$(systemctl show -p NRestarts --value systemd-nspawn@e2e-a.service)\" -ge 1 ] && $NSPAWN exec e2e-a -- /usr/bin/systemctl is-system-running --wait </dev/null | tr -d '\r' | grep_q -E 'running|degraded'" || fail "e2e-a did not boot again after its init was killed"
$NSPAWN stop e2e-a || fail "stop e2e-a with a restart policy"
sleep 3
$NSPAWN ps | grep_q "^ *e2e-a " && fail "e2e-a came back after stop"
systemctl is-failed systemd-nspawn@e2e-a.service >/dev/null 2>&1 && fail "the unit of e2e-a was left failed"
# The hammer on a booted machine that would come back: killed, and it stays down.
$NSPAWN start e2e-a --restart always >/dev/null || fail "start e2e-a with --restart always"
$NSPAWN stop e2e-a --force || fail "stop --force of a booted machine with a restart policy"
sleep 5
$NSPAWN ps | grep_q "^ *e2e-a " && fail "e2e-a came back after stop --force"
systemctl is-failed systemd-nspawn@e2e-a.service >/dev/null 2>&1 && fail "stop --force left the unit of e2e-a failed"
$NSPAWN start e2e-a --restart no >/dev/null && $NSPAWN stop e2e-a >/dev/null || fail "back to no restart policy for e2e-a"

layers_before=$(ls /var/lib/nspawn/layers | wc -l)
$NSPAWN images rm e2e-a >/dev/null || fail "rm e2e-a"
[ "$(ls /var/lib/nspawn/layers | wc -l)" = "$layers_before" ] || fail "layer removed while still referenced"
# An image of the host's own may share the layer, which then rightly stays.
shared=$(python3 -c "
import glob, json
layers = set(json.load(open('/var/lib/nspawn/images/e2e-b.json'))['layers'])
print(' '.join(sorted(p for p in glob.glob('/var/lib/nspawn/images/*.json')
    if not p.endswith('/e2e-b.json') and layers & set(json.load(open(p)).get('layers', [])))))")
$NSPAWN images rm e2e-b | tee /tmp/e2e-rm.txt || fail "rm e2e-b"
if [ -z "$shared" ]; then
  grep -q "freed 1 unused layer" /tmp/e2e-rm.txt || fail "unused layer not garbage collected"
else
  echo "the layer of e2e-b is also used by $shared: its collection is not checked"
  grep_q "unused layer" /tmp/e2e-rm.txt && fail "a layer another image uses was collected"
fi
[ -e /etc/systemd/system/machines.target.wants/systemd-nspawn@e2e-b.service ] && fail "images rm left e2e-b enabled at boot"

step "build, push and pull round trip (docker-like flow)"
if command -v mkosi >/dev/null 2>&1; then
  # The fixture next to this script, or one the caller names with a definition of
  # its own (NSPAWN_BUILD_CONTEXT).
  ctx=${NSPAWN_BUILD_CONTEXT:-$(dirname "$0")/build-context}
  [ -f "$ctx/mkosi.conf" ] || fail "no mkosi.conf under $ctx; copy tests/build-context along with this script"
  built=e2e-built
  $NSPAWN build -t e2e/built:1 --name $built --force "$ctx" || fail "build"
  $NSPAWN images ls | tee /tmp/e2e-img.txt
  grep "^ *$built " /tmp/e2e-img.txt | grep_q -w "build" || fail "built image not listed with origin build"
  $NSPAWN inspect $built | python3 -c "import json,sys; d = json.load(sys.stdin)[0]; assert d['image_labels'].get('org.nspawn.e2e') == 'built' and d['labels'].get('org.nspawn.e2e') == 'built', d" || fail "the image's own labels (OciLabels=) are not read"
  $NSPAWN start $built || fail "start built image"
  out=$($NSPAWN exec $built -- /usr/bin/systemctl is-system-running --wait </dev/null | tr -d '\r' || true)
  echo "built image is-system-running: $out"
  echo "$out" | grep_q -E "running|degraded|starting" || fail "built image did not boot"
  $NSPAWN stop $built || fail "stop built image"
  $NSPAWN push $built | tee /tmp/e2e-push.txt || fail "push"
  grep -q "^pushed " /tmp/e2e-push.txt || fail "push did not report success"
  $NSPAWN hub tags e2e/built > /tmp/e2e-tags.txt; grep -qx 1 /tmp/e2e-tags.txt || fail "pushed tag not on the hub"
  $NSPAWN push $built > /tmp/e2e-push2.txt || fail "second push"
  cat /tmp/e2e-push2.txt
  grep -q "already on the registry" /tmp/e2e-push2.txt || fail "second push re-uploaded blobs"
  grep -q "uploading" /tmp/e2e-push2.txt && fail "second push uploaded a blob again"
  $NSPAWN images rm $built || fail "rm built image"
  $NSPAWN pull e2e/built:1 --name e2e-roundtrip --backend overlay --force || fail "pull of the pushed image"
  $NSPAWN start e2e-roundtrip || fail "start round-trip image"
  retry 10 $NSPAWN exec e2e-roundtrip -- /usr/bin/test -f /etc/os-release </dev/null || fail "exec in round-trip image"
  $NSPAWN stop e2e-roundtrip || fail "stop round-trip image"
  $NSPAWN push e2e-roundtrip --to e2e/built:copy > /tmp/e2e-push3.txt || fail "push --to"
  grep -q "^pushed " /tmp/e2e-push3.txt || fail "push of a pulled image under another tag"
  $NSPAWN hub tags e2e/built > /tmp/e2e-tags.txt; grep -qx copy /tmp/e2e-tags.txt || fail "retagged push missing on the hub"
  $NSPAWN images rm e2e-roundtrip || fail "rm round-trip image"
else
  echo "mkosi not installed: skipping the build round trip"
fi

step "app images without an init system (docker-style)"
app=e2e-busybox
$NSPAWN pull docker.io/library/busybox:latest --name $app --backend overlay --force > /tmp/e2e-app.txt 2>&1 || fail "pull busybox from Docker Hub"
cat /tmp/e2e-app.txt
grep -q "(app image)" /tmp/e2e-app.txt || fail "busybox not detected as an app image"
grep -q "Boot=no" /etc/systemd/nspawn/$app.nspawn || fail "settings file does not disable --boot"
step "machinectl start right after pull: the unit hooks prepare everything"
[ -e /etc/systemd/system/systemd-nspawn@$app.service.d/nspawn-hooks.conf ] || fail "no hooks drop-in after pull"
machinectl start $app || fail "machinectl start of a freshly pulled app"
retry 10 bash -c "$NSPAWN exec $app -- ip -4 -o addr show host0 </dev/null | tr -d '\r' | grep_q 10.99.0" || fail "no bridge address after machinectl start of a fresh app"
grep -q "PrivateUsers=no" /etc/systemd/nspawn/$app.nspawn || fail "the prepare hook did not regenerate the settings"
$NSPAWN stop $app || fail "stop after machinectl start of a fresh app"
grep -q "ProcessTwo=yes" /etc/systemd/nspawn/$app.nspawn || fail "settings file does not use a stub init"
$NSPAWN start $app -p 18081:80 -- /bin/sh -c "echo hello-from-app-$nonce; echo to-stderr-$nonce >&2; mkdir -p /www; echo app-web > /www/index.html; exec /bin/httpd -f -p 80 -h /www" || fail "start busybox with a command override and a published port"
grep -q "Parameters=/bin/sh -c" /etc/systemd/nspawn/$app.nspawn || fail "command override not written"
retry 10 bash -c "$NSPAWN logs $app > /tmp/e2e-logs.txt; grep -q hello-from-app-$nonce /tmp/e2e-logs.txt" || fail "logs do not show the app's stdout"
grep -q to-stderr-$nonce /tmp/e2e-logs.txt || fail "logs do not show the app's stderr"
# systemd 261 words it "Started Container NAME", older ones "Started systemd-nspawn@NAME.service".
unit_started="Started (systemd-nspawn@$app.service|Container $app)"
grep -qE "$unit_started" /tmp/e2e-logs.txt && fail "logs include systemd's unit messages without --all"
$NSPAWN logs $app --all > /tmp/e2e-logs.txt; grep -qE "$unit_started" /tmp/e2e-logs.txt || fail "logs --all misses the unit messages"
$NSPAWN machines ls | tee /tmp/e2e-m.txt
grep -q "^ *$app " /tmp/e2e-m.txt || fail "busybox machine not running"
out=$($NSPAWN exec $app -- /bin/sh -c 'echo inside:$(uname -n); cat /etc/os-release | head -1' </dev/null | tr -d '\r')
echo "$out"
echo "$out" | grep_q "inside:$app" || fail "exec via namespaces did not run inside the machine"
[ "$($NSPAWN exec $app -- id -u </dev/null | tr -d '\r')" = "0" ] || fail "exec does not run as the machine's root"
[ "$($NSPAWN exec $app --user 65534 -- id -u </dev/null | tr -d '\r')" = "65534" ] || fail "exec --user ignored"
$NSPAWN exec $app -- /bin/sh -c 'exit 7' </dev/null; [ $? -eq 7 ] || fail "exec did not propagate the exit code"
[ "$(printf 'a\nb' | $NSPAWN exec $app -- cat)" = "$(printf 'a\nb')" ] || fail "piped stdin/stdout through exec is not byte exact"
echo "app-$nonce" > /tmp/e2e-cp-app
$NSPAWN cp /tmp/e2e-cp-app "$app:/tmp/" || fail "cp into an app"
[ "$($NSPAWN exec $app -- stat -c %u /tmp/e2e-cp-app </dev/null | tr -d '\r')" = 0 ] || fail "a file copied into an app is not root's"
$NSPAWN cp "$app:/etc/passwd" /tmp/e2e-cp-app-passwd && grep -q "^root:" /tmp/e2e-cp-app-passwd || fail "cp out of an app"
step "app on the bridge: address, DNS, internet and published port (no networkd anywhere)"
if [ "$networkd_was" != active ]; then
  systemctl is-active systemd-networkd >/dev/null && fail "systemd-networkd is running during the app section"
fi
app_addr=$(addr_of $app)
echo "$app has address $app_addr"
echo "$app_addr" | grep_q "^10\.99\.0\." || fail "no bridge address for the app"
ipv4_only "$app" "$app_addr"
$NSPAWN exec $app -- ip -4 -o addr show host0 </dev/null | tr -d '\r' | grep_q "$app_addr/24" || fail "host0 not configured inside the app"
$NSPAWN exec $app -- cat /etc/hosts </dev/null | tr -d '\r' | grep_q "host.nspawn.internal" || fail "generated /etc/hosts missing in the app"
# The service ignores SIGPIPE; what exec runs must not inherit that.
ignored=$($NSPAWN exec $app -- cat /proc/self/status </dev/null | tr -d '\r' | awk '/^SigIgn:/ {print $2}')
[ $(( 16#${ignored:-0} & 0x1000 )) = 0 ] || fail "a command run by exec ignores SIGPIPE (SigIgn $ignored)"
$NSPAWN exec $app -- nslookup download.opensuse.org </dev/null >/dev/null 2>&1 || fail "DNS does not work inside the app"
$NSPAWN exec $app -- wget -qO- -T 5 http://detectportal.firefox.com/success.txt </dev/null | tr -d '\r' | grep_q success || fail "no internet from the app"
curl -sf -m 5 http://127.0.0.1:18081/ | grep_q app-web || fail "published app port not reachable on 127.0.0.1"
$NSPAWN ps | grep "^ *$app " | grep_q "18081->80/tcp" || fail "app port not shown by ps"
$NSPAWN stop $app || fail "stop busybox"
retry 15 bash -c "! $NSPAWN machines ls | grep_q '^ *$app '" || fail "busybox still running after stop"
[ -e /run/netns/nspawn-$app ] && fail "network namespace left behind for $app"

step "user-defined networks: machines of one network reach each other, the rest does not reach them"
$NSPAWN network create e2e-na >/dev/null || fail "network create"
$NSPAWN network create e2e-nb --subnet 10.98.7.0/24 >/dev/null || fail "network create --subnet"
$NSPAWN network create e2e-nc --internal >/dev/null || fail "network create --internal"
ip link show nsbr-e2e-na >/dev/null 2>&1 || fail "network create did not bring the bridge up"
na_subnet=$($NSPAWN network inspect e2e-na | python3 -c "import json,sys; print(json.load(sys.stdin)[0]['subnet'])")
case "$na_subnet" in 10.99.0.0/24|"") fail "e2e-na got no subnet of its own: $na_subnet" ;; esac
$NSPAWN network ls | grep "^ *e2e-nb " | grep_q "10.98.7.0/24" || fail "network ls does not list e2e-nb with its subnet"
$NSPAWN network ls | grep "^ *e2e-nc " | grep_q " yes " || fail "network ls does not show e2e-nc internal"
$NSPAWN network create e2e-nd --subnet 10.98.7.128/25 2>/dev/null && fail "a subnet overlapping another network was accepted"
$NSPAWN network create host 2>/dev/null && fail "a network took a reserved name"
$NSPAWN network create e2e-na 2>/dev/null && fail "a network was made twice"
web='mkdir -p /www; echo "$0" > /www/index.html; exec /bin/httpd -f -p 80 -h /www'
$NSPAWN create $app e2e-na-web --network e2e-na -- /bin/sh -c "$web" na-web >/dev/null || fail "create on e2e-na"
$NSPAWN create $app e2e-na-cli --network e2e-na -- /bin/sleep 600 >/dev/null || fail "create a client on e2e-na"
$NSPAWN create $app e2e-nb-web --network e2e-nb -p 18090:80 -- /bin/sh -c "$web" nb-web >/dev/null || fail "create on e2e-nb"
$NSPAWN create $app e2e-nc-web --network e2e-nc -- /bin/sh -c "$web" nc-web >/dev/null || fail "create on e2e-nc"
$NSPAWN create $app e2e-def-cli --network bridge -p none -- /bin/sleep 600 >/dev/null || fail "create on the default network"
for m in e2e-na-web e2e-na-cli e2e-nb-web e2e-nc-web e2e-def-cli; do
  $NSPAWN start $m >/dev/null || fail "start $m"
done
$NSPAWN inspect e2e-na-web | python3 -c "import json,sys; d = json.load(sys.stdin)[0]; assert d['network'] == 'e2e-na', d" || fail "inspect does not name the network"
# A machine made from one on a network of its own is not put there unasked.
$NSPAWN create e2e-na-web e2e-nc-pub >/dev/null || fail "create from a machine on e2e-na"
$NSPAWN inspect e2e-nc-pub | python3 -c "import json,sys; d = json.load(sys.stdin)[0]; assert d['network'] == 'bridge', d" || fail "create put the new machine on its source's network"
$NSPAWN images rm e2e-nc-pub >/dev/null || fail "rm the machine made from e2e-na-web"
na_web=$(addr_of e2e-na-web); nb_web=$(addr_of e2e-nb-web); nc_web=$(addr_of e2e-nc-web)
echo "e2e-na-web $na_web, e2e-nb-web $nb_web, e2e-nc-web $nc_web"
[ "$nb_web" != "${nb_web#10.98.7.}" ] || fail "e2e-nb-web is not on 10.98.7.0/24: $nb_web"
get() { $NSPAWN exec "$1" -- wget -qO- -T 3 "$2" </dev/null 2>/dev/null | tr -d '\r'; }
retry 5 bash -c "$NSPAWN exec e2e-na-cli -- wget -qO- -T 3 http://e2e-na-web/ </dev/null 2>/dev/null | grep_q na-web" || fail "a machine does not reach another of its network by name"
$NSPAWN exec e2e-na-cli -- cat /etc/hosts </dev/null | grep_q e2e-nb-web && fail "the hosts file lists a machine of another network"
get e2e-na-cli "http://$nb_web/" | grep_q nb-web && fail "a machine reached one of another network"
get e2e-def-cli "http://$na_web/" | grep_q na-web && fail "a machine of the default network reached one of e2e-na"
get e2e-na-cli "http://host.nspawn.internal:18090/" | grep_q nb-web || fail "a port published on e2e-nb is not reachable from e2e-na through the host"
curl -sf -m 5 http://127.0.0.1:18090/ | grep_q nb-web || fail "the published port of e2e-nb is not reachable from the host"
get e2e-na-cli "http://detectportal.firefox.com/success.txt" | grep_q success || fail "no internet from e2e-na"
get e2e-nc-web "http://detectportal.firefox.com/success.txt" | grep_q success && fail "a machine of an internal network got out"
get e2e-na-cli "http://$nc_web/" | grep_q nc-web && fail "a machine reached one of an internal network"
# One machine on two networks, with aliases: reached from both, and listed on both.
$NSPAWN create $app e2e-nab --network e2e-na --network e2e-nb --network-alias both --network-alias e2e-nb=web-b -- /bin/sh -c "$web" nab >/dev/null || fail "create on two networks"
$NSPAWN start e2e-nab >/dev/null || fail "start e2e-nab"
$NSPAWN inspect e2e-nab | python3 -c "
import json, sys
d = json.load(sys.stdin)[0]
assert d['network'] == 'e2e-na' and d['networks'] == ['e2e-na', 'e2e-nb'], d
assert d['addresses']['e2e-na'].startswith('${na_subnet%.*}.') and d['addresses']['e2e-nb'].startswith('10.98.7.'), d
assert sorted(d['aliases']) == ['e2e-na=both', 'e2e-nb=web-b'], d" || fail "inspect does not show both networks of e2e-nab"
[ "$($NSPAWN exec e2e-nab -- ip -4 -o addr show </dev/null | tr -d '\r' | grep -c 'host[01] ')" = 2 ] || fail "e2e-nab does not have host0 and host1"
retry 5 bash -c "$NSPAWN exec e2e-na-cli -- wget -qO- -T 3 http://both/ </dev/null 2>/dev/null | grep_q nab" || fail "an alias on e2e-na does not resolve"
get e2e-nab "http://e2e-nb-web/" | grep_q nb-web || fail "e2e-nab does not reach e2e-nb by name"
get e2e-nab "http://e2e-na-web/" | grep_q na-web || fail "e2e-nab does not reach e2e-na by name"
get e2e-nab "http://detectportal.firefox.com/success.txt" | grep_q success || fail "no internet from e2e-nab"
$NSPAWN exec e2e-nb-web -- cat /etc/hosts </dev/null | tr -d '\r' | grep_q " e2e-nab web-b" || fail "the alias of e2e-nab on e2e-nb is not in the hosts file of e2e-nb-web"
$NSPAWN exec e2e-nb-web -- cat /etc/hosts </dev/null | tr -d '\r' | grep_q "both" && fail "an alias of another network leaked into the hosts file of e2e-nb-web"
$NSPAWN network inspect e2e-nb | python3 -c "
import json, sys
m = [m for m in json.load(sys.stdin)[0]['machines'] if m['name'] == 'e2e-nab'][0]
assert m['aliases'] == ['web-b'] and m['address'].startswith('10.98.7.'), m" || fail "network inspect does not list e2e-nab on e2e-nb"
$NSPAWN ps | grep "^ *e2e-nab " | grep_q "e2e-nb:10.98.7." || fail "ps does not show the second network of e2e-nab"
out=$($NSPAWN network rm e2e-nb 2>&1) && fail "network rm removed a network a machine joins besides its primary one"
echo "$out" | grep_q "in use by e2e-nab, e2e-nb-web" || fail "network rm of e2e-nb was not explained: $out"
# A booted machine on two networks: both interfaces up, both networks in its hosts.
$NSPAWN pull "$IMAGE" --name e2e-boot2 --backend overlay --force >/dev/null || fail "pull e2e-boot2"
$NSPAWN start e2e-boot2 --network e2e-nb --network e2e-na >/dev/null || fail "start a booted machine on two networks"
retry 20 bash -c "$NSPAWN exec e2e-boot2 -- ip -4 -o addr show </dev/null | tr -d '\r' | grep -q 'host1 .*10\.' " || fail "the booted machine has no address on host1"
$NSPAWN exec e2e-boot2 -- ip -4 -o addr show host0 </dev/null | tr -d '\r' | grep_q "10.98.7." || fail "host0 of the booted machine is not on e2e-nb"
$NSPAWN exec e2e-boot2 -- ip -4 route show default </dev/null | tr -d '\r' | grep_q "via 10.98.7.1" || fail "the default route of the booted machine does not go through its primary network"
$NSPAWN exec e2e-boot2 -- cat /etc/hosts </dev/null | tr -d '\r' | grep_q " e2e-na-web" || fail "the booted machine's hosts file misses e2e-na-web"
$NSPAWN exec e2e-boot2 -- bash -c 'exec 3<>/dev/tcp/e2e-na-web/80; printf "GET / HTTP/1.0\r\n\r\n" >&3; cat <&3' </dev/null | tr -d '\r' | grep_q na-web || fail "the booted machine does not reach e2e-na-web through host1"
$NSPAWN stop e2e-boot2 >/dev/null || fail "stop e2e-boot2"
ip -o link show | grep_q "vb1-e2e-boot2" && fail "the extra veth of the booted machine was left behind"
# No network at all.
$NSPAWN create $app e2e-none --network none -- /bin/sleep 600 >/dev/null || fail "create with --network none"
$NSPAWN start e2e-none >/dev/null || fail "start e2e-none"
[ "$($NSPAWN exec e2e-none -- ip -o link show </dev/null | tr -d '\r' | grep -vc ' lo:')" = 0 ] || fail "a machine with --network none has an interface besides lo"
$NSPAWN inspect e2e-none | python3 -c "import json,sys; d = json.load(sys.stdin)[0]; assert d['network'] == 'none' and d['networks'] == [], d" || fail "inspect of a machine without network"
$NSPAWN ps | grep "^ *e2e-none " | grep_q " none " || fail "ps does not show none"
$NSPAWN create $app e2e-nc-pub --network e2e-nc -p 18091:80 -- /bin/sleep 1 >/dev/null || fail "create with a port on an internal network"
out=$($NSPAWN start e2e-nc-pub 2>&1) && fail "a port was published from an internal network"
echo "$out" | grep_q internal || fail "publishing from an internal network was not explained: $out"
$NSPAWN images rm e2e-nc-pub >/dev/null || fail "rm e2e-nc-pub"
out=$($NSPAWN network rm e2e-na 2>&1) && fail "network rm removed a network in use"
echo "$out" | grep_q "in use by e2e-boot2, e2e-na-cli, e2e-na-web, e2e-nab" || fail "network rm of a network in use was not explained: $out"
$NSPAWN network rm bridge 2>/dev/null && fail "network rm removed the default network"
nft list table ip nspawn | grep_q nsbr-e2e-na || fail "no rules for e2e-na in the nspawn table"
# A bridge deleted by hand comes back with the next start of one of its machines.
$NSPAWN stop e2e-na-web >/dev/null && $NSPAWN stop e2e-na-cli >/dev/null || fail "stop the e2e-na machines"
ip link del nsbr-e2e-na || fail "delete the bridge by hand"
$NSPAWN start e2e-na-web >/dev/null && $NSPAWN start e2e-na-cli >/dev/null || fail "start after the bridge went"
retry 5 bash -c "$NSPAWN exec e2e-na-cli -- wget -qO- -T 3 http://e2e-na-web/ </dev/null 2>/dev/null | grep_q na-web" || fail "the network did not come back with its machines"
for m in e2e-na-web e2e-na-cli e2e-nb-web e2e-nc-web e2e-def-cli e2e-nab e2e-none e2e-boot2; do
  $NSPAWN rm -f $m >/dev/null || fail "rm -f $m"
done
$NSPAWN network create e2e-ne >/dev/null || fail "network create e2e-ne"
$NSPAWN network prune -f >/dev/null || fail "network prune"
$NSPAWN network ls | grep_q "^ *e2e-n[a-e] " && fail "network prune left an unused network: $($NSPAWN network ls)"
ip -o link show | grep_q nsbr-e2e && fail "a removed network left its bridge"
nft list table ip nspawn | grep_q nsbr-e2e && fail "a removed network left rules in the nspawn table"
if command -v iptables >/dev/null 2>&1; then
  iptables -w -S 2>/dev/null | grep_q nsbr-e2e && fail "a removed network left iptables rules"
fi
if firewall-cmd --state >/dev/null 2>&1; then
  firewall-cmd --zone=trusted --list-interfaces | grep_q nsbr-e2e && fail "a removed network is still in the trusted zone"
fi
$NSPAWN network ls | grep_q "^ *bridge " || fail "the default network is not listed"

step "run -d: a machine from an image and started in one step, like docker run -d"
# busybox is here as $app: run makes another machine of it without the registry.
$NSPAWN run -d docker.io/library/busybox:latest --name e2e-run -p 18082:80 -- /bin/sh -c "mkdir -p /www; echo run-$nonce > /www/index.html; exec /bin/httpd -f -p 80 -h /www" > /tmp/e2e-run.txt 2>&1 || { cat /tmp/e2e-run.txt; fail "run from a local image"; }
cat /tmp/e2e-run.txt
grep_q "started e2e-run" /tmp/e2e-run.txt || fail "run did not say it started e2e-run"
grep_q "downloading" /tmp/e2e-run.txt && fail "run downloaded an image that is here already"
retry 10 bash -c "curl -sf -m 5 http://127.0.0.1:18082/ | grep_q run-$nonce" || fail "the port run published does not answer"
$NSPAWN inspect e2e-run | python3 -c "import json,sys; d = json.load(sys.stdin)[0]; assert d['state'] == 'running' and d['origin'] == 'create' and d['ports'] == ['18082->80/tcp'], d" || fail "inspect of the machine run made"
out=$($NSPAWN run docker.io/library/busybox:latest --name e2e-run 2>&1) && fail "run made a machine over a running one"
echo "$out" | grep_q "nspawn start e2e-run" || fail "run over an existing name did not point to start: $out"
out=$($NSPAWN run e2e/nothing-$nonce:1 --pull never 2>&1) && fail "run --pull never pulled"
echo "$out" | grep_q "no local image" || fail "run --pull never without a local image was not explained: $out"
$NSPAWN images ls | grep_q "nothing-$nonce" && fail "run --pull never left an image behind"
# Several machines at once: every line carries its machine's name; --until ends a follow.
$NSPAWN logs $app e2e-run --until now > /tmp/e2e-logs2.txt 2>&1 || fail "logs of two machines"
grep -q "^e2e-run  *| " /tmp/e2e-logs2.txt || fail "logs of two machines carry no names: $(head -3 /tmp/e2e-logs2.txt)"
grep -q "^$app | " /tmp/e2e-logs2.txt || fail "logs of two machines miss $app"
timeout 20 $NSPAWN logs e2e-run -f --until now >/dev/null 2>&1 || fail "logs -f --until did not end by itself"
$NSPAWN rm -f e2e-run >/dev/null || fail "rm -f e2e-run"
systemctl is-failed systemd-nspawn@$app.service >/dev/null 2>&1 && fail "unit left in failed state after stop"

step "run attached, like docker run: output, input, terminal, signals, exit code, --rm"
bb=docker.io/library/busybox:latest
out=$($NSPAWN run --rm $bb --name e2e-run -- /bin/sh -c 'echo out-$0; echo err-$0 >&2; exit 3' $nonce 2>/tmp/e2e-run.err); rc=$?
[ "$rc" = 3 ] || fail "run did not exit with the program's code: $rc"
# stdout and stderr come merged, from the journal; nothing of nspawn's own is among them.
[ "$out" = "$(printf 'out-%s\nerr-%s' $nonce $nonce)" ] || fail "run did not show the program's output, and that alone, on stdout: $out"
$NSPAWN images ls | grep_q "^ *e2e-run " && fail "run --rm left the machine behind"
[ "$(echo abc | $NSPAWN run -i --rm $bb --name e2e-run -- wc -c 2>/dev/null)" = 4 ] || fail "run -i did not give the program its input"
out=$(pyrun $NSPAWN run -it --rm $bb --name e2e-run -- /bin/sh -c 'stty size; tty; exit 5' </dev/null 2>/dev/null | tr -d '\0\r'); rc=${PIPESTATUS[0]}
[ "$rc" = 5 ] || fail "run -it did not exit with the program's code: $rc"
echo "$out" | grep_q "^[0-9]* [0-9]*$" || fail "run -it gave the program no terminal size: $out"
echo "$out" | grep_q "not a tty" && fail "run -it gave the program no terminal: $out"
$NSPAWN run --rm $bb --name e2e-run -- /bin/sleep 300 >/dev/null 2>&1 &
run_pid=$!
retry 10 bash -c "$NSPAWN ps | grep_q '^ *e2e-run '" || fail "the attached run did not start"
sleep 1; kill -INT $run_pid; wait $run_pid; rc=$?
[ "$rc" = 130 ] || fail "Ctrl-C did not reach the program as SIGINT: $rc"
$NSPAWN run --rm $bb --name e2e-run -- /bin/sleep 300 >/dev/null 2>&1 &
run_pid=$!
retry 10 bash -c "$NSPAWN ps | grep_q '^ *e2e-run '" || fail "the attached run did not start"
$NSPAWN kill e2e-run >/dev/null || fail "kill of an attached run"
wait $run_pid; rc=$?
[ "$rc" = 137 ] || fail "run killed with SIGKILL did not exit with 137: $rc"
retry 15 bash -c "! $NSPAWN images ls | grep_q '^ *e2e-run '" || fail "run --rm left a killed machine behind"
# docker keeps the image of a --rm run: what run pulled stays, the machine goes.
# (A tag of its own, so that no image of the host is replaced.)
out=$($NSPAWN run --rm docker.io/library/busybox:1.37 /bin/echo kept-$nonce 2>/dev/null) || fail "run --rm of an image not here yet"
[ "$out" = "kept-$nonce" ] || fail "run --rm of an image not here yet did not show the output: $out"
$NSPAWN images ls | grep_q "^ *busybox-1.37 " || fail "run --rm did not keep the image it pulled"
$NSPAWN images ls | grep_q "^ *busybox-1.37-" && fail "run --rm left its machine behind"
$NSPAWN images rm busybox-1.37 >/dev/null || fail "images rm of the image run kept"
# A reboot asked from inside ends a --rm run (the machine is not restarted then).
timeout 60 $NSPAWN run --rm $bb --name e2e-run -- /bin/reboot -f >/dev/null 2>&1; rc=$?
[ "$rc" = 133 ] || fail "run --rm of a program that reboots did not end with 133: $rc"
retry 15 bash -c "! $NSPAWN images ls | grep_q '^ *e2e-run '" || fail "run --rm left a machine that rebooted behind"
$NSPAWN run --name e2e-run $bb -- /bin/sh -c 'echo logged-$0' $nonce >/dev/null 2>&1 || fail "run without --rm"
$NSPAWN logs e2e-run | grep_q "logged-$nonce" || fail "logs does not show what an attached run showed"
$NSPAWN rm e2e-run >/dev/null || fail "rm the machine of an attached run"
$NSPAWN run -d --rm $bb --name e2e-run -- /bin/sh -c 'sleep 2' >/dev/null || fail "run -d --rm"
retry 20 bash -c "! $NSPAWN images ls | grep_q '^ *e2e-run '" || fail "run -d --rm left the machine behind once it ended"
$NSPAWN run -d --rm $bb --name e2e-run -- /bin/sleep 300 >/dev/null || fail "run -d --rm of a long program"
$NSPAWN stop e2e-run >/dev/null || fail "stop a machine of run -d --rm"
retry 20 bash -c "! $NSPAWN images ls | grep_q '^ *e2e-run '" || fail "run -d --rm left the machine behind after stop"
systemctl list-units --all 'nspawn-rm-*' --no-legend | grep_q . && fail "a removal unit was left behind"
ls /run/nspawn/attach/ 2>/dev/null | grep_q . && fail "a run left its socket behind"
out=$($NSPAWN run --rm --restart always $bb --name e2e-run -- /bin/true 2>&1) && fail "run --rm --restart always succeeded"
echo "$out" | grep_q "restart" || fail "run --rm with a restart policy was not explained: $out"
$NSPAWN run -d -t $bb --name e2e-run -- /bin/true 2>/dev/null && fail "run -d -t succeeded"
$NSPAWN run --no-wait $bb --name e2e-run -- /bin/true 2>/dev/null && fail "run --no-wait without -d succeeded"
$NSPAWN images ls | grep_q "^ *e2e-run " && fail "a refused run --rm left its machine behind"
$NSPAWN rm -f e2e-run >/dev/null 2>&1 || true

step "host network, --no-wait, an app's shell and a missing program"
$NSPAWN stop $app >/dev/null || fail "stop before the host-network start"
$NSPAWN start $app --network host -p none -- /bin/sleep 300 || fail "start with --network host"
$NSPAWN exec $app -- ip -o addr </dev/null | tr -d '\r' | grep_q -v "host0" || fail "host network shows host0"
$NSPAWN exec $app -- ip -o link </dev/null | tr -d '\r' | grep_q "$(ip -o link | awk -F': ' 'NR==2{print $2}' | cut -d@ -f1)" || fail "host network does not see the host's interfaces"
out=$(printf 'echo appshell-%s; exit\n' "$nonce" | python3 "$(dirname "$0")/terminal.py" $NSPAWN shell $app 2>&1 | tr -d '\r')
echo "$out" | grep_q "appshell-$nonce" || fail "shell on an app: $(echo "$out" | tail -2)"
$NSPAWN exec $app -- /bin/sh -c 'exec >/dev/null 2>&1; sleep 12; exit 3' </dev/null; [ $? = 3 ] || fail "exec lost the exit code of a command that closed its streams early"
$NSPAWN exec $app -- /no/such/program </dev/null >/dev/null 2>&1; [ $? = 127 ] || fail "exec of a missing program is not 127"
$NSPAWN exec $app -- no-such-command-$nonce </dev/null >/dev/null 2>&1; [ $? = 127 ] || fail "exec of a command missing on PATH is not 127"
$NSPAWN stop $app --no-wait || fail "stop --no-wait"
retry 10 bash -c "! $NSPAWN ps | grep_q '^ *$app '" || fail "app still running after stop --no-wait"
$NSPAWN start $app --network bridge --no-wait -- /bin/sleep 300 || fail "start --no-wait"
retry 10 bash -c "$NSPAWN ps | grep_q '^ *$app '" || fail "app not running after start --no-wait"
$NSPAWN stop $app >/dev/null || fail "stop after --no-wait start"

step "stop: a program that ignores its stop signal is killed after --timeout"
$NSPAWN start $app -- /bin/sh -c 'trap "" TERM; exec /bin/sleep 300' || fail "start stubborn app"
t0=$(date +%s)
out=$($NSPAWN stop $app -t 2 2>&1 >/dev/null) || fail "stop of a stubborn app"
[ $(( $(date +%s) - t0 )) -lt 20 ] || fail "stop of a stubborn app took too long"
echo "$out" | grep_q "ignored SIGTERM for 2 seconds; killing it" || fail "stop did not say that it had to kill the program: $out"
retry 5 bash -c "! $NSPAWN ps | grep_q '^ *$app '" || fail "stubborn app still running"

step "the remembered command, the unit hooks and an app that exits on its own"
$NSPAWN start $app -p 18081:80 -- /bin/sh -c 'mkdir -p /www; echo app-web > /www/index.html; exec /bin/httpd -f -p 80 -h /www' || fail "start httpd app"
$NSPAWN stop $app >/dev/null || fail "stop httpd app"
$NSPAWN start $app || fail "start without a command"
retry 5 bash -c "curl -sf -m 2 http://127.0.0.1:18081/ | grep_q app-web" || fail "the remembered command did not run"
$NSPAWN stop $app >/dev/null || fail "stop remembered app"
machinectl start $app || fail "machinectl start of an app (the hooks must prepare its network)"
retry 10 bash -c "curl -sf -m 2 http://127.0.0.1:18081/ | grep_q app-web" || fail "no network or ports after machinectl start"
$NSPAWN stop $app || fail "stop after machinectl start"
t0=$(date +%s)
$NSPAWN start $app -- /bin/true || fail "start of a program that returns at once"
[ $(( $(date +%s) - t0 )) -lt 15 ] || fail "start waited for a program that had already returned"
# The unit of the program that returned at once may still be on its way down.
retry 10 bash -c "systemctl show -p ActiveState --value systemd-nspawn@$app.service | grep -qE '^(failed|inactive)$'" || fail "the unit of the program that returned at once is still up"
$NSPAWN start $app -- /bin/sh -c 'exit 3' >/dev/null 2>&1 || true
retry 10 bash -c "! $NSPAWN ps | grep_q '^ *$app '" || fail "failed app still listed"
# machined drops the machine a few milliseconds before its unit is down.
retry 10 bash -c "systemctl show -p ActiveState --value systemd-nspawn@$app.service | grep -qE '^(failed|inactive)$'" || fail "the unit of the failed program is still up"
$NSPAWN inspect $app | python3 -c "import json,sys; d = json.load(sys.stdin)[0]; assert d['state'] == 'stopped' and d['exit_code'] == 3, d" || fail "inspect does not show the exit code of the last run"
out=$($NSPAWN stop $app 2>&1); echo "$out" | grep_q "was not running" || fail "stop after a failed program: $out"
systemctl is-failed systemd-nspawn@$app.service >/dev/null 2>&1 && fail "unit left failed after stop of a program that exited 3"
# A program that fails at once leaves the unit on its way down with the release hook
# running; a start issued right then must not have its namespace pulled away.
$NSPAWN start $app -- /bin/sh -c 'exit 3' >/dev/null 2>&1 || true
# "already running" is the right answer while the failed program is still alive (the
# service answers within milliseconds); anything else at that moment is a bug.
for i in 1 2 3 4 5 6 7 8 9 10; do
  out=$($NSPAWN start $app -- /bin/sleep 300 2>&1) && break
  echo "$out" | grep_q "already running" || { echo "$out"; fail "start right after a program that failed at once"; break; }
  sleep 0.2
done
retry 10 bash -c "$NSPAWN exec $app -- ip -4 -o addr show host0 </dev/null | tr -d '\r' | grep_q 10.99.0" || fail "no bridge address after a start that followed a failed program"
$NSPAWN stop $app >/dev/null || fail "stop after the quick restart"
$NSPAWN start $app -- /bin/sh -c 'sleep 1' || fail "start short-lived app"
retry 10 bash -c "! $NSPAWN ps | grep_q '^ *$app '" || fail "short-lived app still listed"
sleep 1
[ -e /run/netns/nspawn-$app ] && fail "namespace left behind by an app that exited on its own"
nft list map ip nspawn ports | grep_q 18081 && fail "ports of an exited app still mapped"
out=$($NSPAWN stop $app 2>&1); echo "$out" | grep_q "was not running" || fail "stop of a stopped machine is not a no-op: $out"
python3 -c 'import socket,time; s=socket.socket(); s.bind(("0.0.0.0",18099)); s.listen(); time.sleep(120)' &
listener_pid=$!
sleep 1
out=$($NSPAWN start $app -p 18099:80 2>&1); echo "$out" | grep_q "in use by a service on the host" || fail "publishing a port a host service listens on was not refused: $out"
$NSPAWN ps -a | grep "^ *$app " | grep_q "18099->80" && fail "a refused port was remembered"
kill "$listener_pid" 2>/dev/null; listener_pid=

step "entrypoint, environment and volumes, docker style"
rm -rf /tmp/e2e-bind /var/lib/nspawn/volumes/e2evol; mkdir -p /tmp/e2e-bind; echo from-host > /tmp/e2e-bind/hello
export E2E_HOST_VAR=fromhost
# The unit's journal keeps the lines of earlier runs, so every line carries the nonce.
$NSPAWN start $app --label caddy=app.example --label tier=web --entrypoint /bin/sh -e GREETING=hola -e E2E_HOST_VAR -v /tmp/e2e-bind:/bind -v e2evol:/vol -v /etc/os-release:/host-os-release:ro -p none -- -c "echo \"greeting=\$GREETING hostvar=\$E2E_HOST_VAR nonce=$nonce\"; cat /bind/hello; echo from-app > /vol/written; { echo blocked > /host-os-release; } 2>/dev/null && echo RO-FAIL-$nonce || echo RO-OK-$nonce; exec /bin/sleep 300" || fail "start with entrypoint, env and volumes"
retry 10 bash -c "$NSPAWN logs $app | grep_q RO-[A-Z]*-$nonce" || fail "app did not run"
$NSPAWN logs $app > /tmp/e2e-logs.txt
grep -q "greeting=hola hostvar=fromhost nonce=$nonce" /tmp/e2e-logs.txt || fail "-e variables not seen by the program"
# What -e carries is often a secret, and the state holds the images' setuid programs.
[ "$(stat -c %a /etc/systemd/nspawn/$app.nspawn)" = 600 ] || fail "the settings file with the -e variables is readable by others"
[ "$(stat -c %a /var/lib/nspawn)" = 711 ] || fail "the state directory is open to others"
for dir in layers blobs images volumes machines/$app; do
  [ "$(stat -c %a /var/lib/nspawn/$dir)" = 700 ] || fail "/var/lib/nspawn/$dir is open to others"
done
grep -q "^from-host" /tmp/e2e-logs.txt || fail "bind mount not visible inside"
grep -q "RO-OK-$nonce" /tmp/e2e-logs.txt || fail "read-only volume was writable"
[ "$(cat /var/lib/nspawn/volumes/e2evol/written 2>/dev/null)" = from-app ] || fail "named volume not written on the host"
$NSPAWN ps | grep "^ *$app " | grep_q "/bin/sh -c" || fail "ps does not show the entrypoint plus arguments"
[ "$($NSPAWN exec $app -- /bin/sh -c 'echo $GREETING' </dev/null | tr -d '\r')" = hola ] || fail "exec does not see -e variables"
[ "$($NSPAWN exec -e X=1 -e E2E_HOST_VAR -w /tmp $app -- /bin/sh -c 'echo $X $E2E_HOST_VAR $(pwd)' </dev/null | tr -d '\r')" = "1 fromhost /tmp" ] || fail "exec -e or -w not applied"
$NSPAWN exec -d $app -- /bin/sleep 7 </dev/null || fail "exec -d"
$NSPAWN exec $app -- /bin/sh -c 'ps -o args | grep -q "^/bin/sleep 7$"' </dev/null || fail "exec -d did not leave the command running"
# exec's command gets the machine's capabilities and nothing of the service's descriptors.
leader_bnd=$(grep CapBnd "/proc/$(machinectl show $app -p Leader --value)/status")
[ "$($NSPAWN exec $app -- /bin/grep CapBnd /proc/self/status </dev/null | tr -d '\r')" = "$leader_bnd" ] || fail "exec's command has other capabilities than the machine ($leader_bnd)"
[ "$($NSPAWN exec $app -- /bin/sh -c 'ls /proc/$$/fd; true' </dev/null | tr -d '\r' | tr '\n' ' ')" = "0 1 2 " ] || fail "exec's command has descriptors beyond its streams"
$NSPAWN inspect $app | python3 -c "import json,sys; d = json.load(sys.stdin)[0]; assert d['labels'].get('caddy') == 'app.example' and d['labels'].get('tier') == 'web', d" || fail "--label not recorded"
$NSPAWN ps --json | python3 -c "import json,sys; d = [m for m in json.load(sys.stdin) if m['name'] == '$app']; assert d and d[0]['labels'].get('caddy') == 'app.example', d" || fail "labels not listed by ps --json"
# The entrypoint (/bin/sh) is remembered, so the arguments are its.
$NSPAWN stop $app >/dev/null; $NSPAWN start $app -e PATH=/opt/none:/usr/bin:/bin -- -c 'exec /bin/sleep 300' >/dev/null || fail "start with a PATH override"
retry 5 bash -c "$NSPAWN ps | grep_q '^ *$app '" || fail "app with a PATH override is not running"
[ "$($NSPAWN exec $app -- /bin/sh -c 'echo $PATH' </dev/null | tr -d '\r')" = "/opt/none:/usr/bin:/bin" ] || fail "exec does not apply a -e override of an image variable"
$NSPAWN stop $app || fail "stop app with volumes"
out=$($NSPAWN start $app --label novalue 2>&1) && fail "a label without a value was accepted"
echo "$out" | grep_q "KEY=VALUE" || fail "a bad label was not explained: $out"
$NSPAWN start $app --image-command -e none -v none --label none || fail "start with the image's own command"
$NSPAWN inspect $app | python3 -c "import json,sys; d = json.load(sys.stdin)[0]; assert 'caddy' not in d['labels'], d" || fail "--label none did not forget the labels"
$NSPAWN ps | grep "^ *$app " | grep_q " sh " || fail "--image-command did not restore the image's cmd"
grep -q "Bind=" /etc/systemd/nspawn/$app.nspawn && fail "-v none left volumes in the settings"
$NSPAWN stop $app -t 2 || fail "stop app running its own cmd"

step "docker's other flags: hostname, user, workdir, capabilities, read-only, tmpfs, devices, dns, hosts, ulimits, signals"
$NSPAWN start $app --hostname h-$nonce -u nobody -w /tmp --cap-drop ALL --cap-add NET_BIND_SERVICE --read-only --tmpfs /scratch:size=16m --device /dev/null:/dev/nullo:r --dns 10.99.0.1 --dns-search example.test --add-host peer:10.1.1.1 --add-host gw:host-gateway --ulimit nofile=64:128 --stop-signal SIGINT --stop-timeout 2 --oom-score-adj 100 --sysctl net.ipv4.icmp_echo_ignore_all=1 -p none -- /bin/sh -c 'trap "" INT; pwd; exec /bin/sleep 300' || fail "start with docker's other flags"
x() { $NSPAWN exec $app -- /bin/sh -c "$1" </dev/null 2>/dev/null | tr -d '\r'; }
[ "$(x hostname)" = "h-$nonce" ] || fail "--hostname not applied: $(x hostname)"
# The program is the stub init's child: systemd-nspawn runs getent in the machine
# ahead of it to resolve the user, which takes PIDs 2 and 3 (busybox has no getent;
# nspawn stands one in).
pid=$(x 'cat /proc/1/task/1/children' | tr -d ' ')
[ -n "$pid" ] || fail "no program under the stub init: $(x 'ls /proc')"
[ "$(x "awk '/^Uid:/ {print \$2}' /proc/$pid/status")" = 65534 ] || fail "--user not applied: $(x "cat /proc/$pid/status")"
# The program prints its directory: exec runs with the machine's capabilities, and
# without CAP_SYS_PTRACE root cannot read the cwd link of nobody's process.
retry 5 bash -c "$NSPAWN logs $app -n 5 | tr -d '\r' | grep -qx /tmp" || fail "--workdir not applied: $($NSPAWN logs $app -n 5)"
# --cap-drop ALL --cap-add X keeps X, as with docker: bit 10 is CAP_NET_BIND_SERVICE.
[ "$(x 'awk "/^CapBnd:/ {print \$2}" /proc/1/status')" = 0000000000000400 ] || fail "--cap-drop ALL --cap-add NET_BIND_SERVICE left other capabilities: $(x 'grep CapBnd /proc/1/status')"
[ "$(x 'touch /x 2>/dev/null && echo RW || echo RO')" = RO ] || fail "--read-only root is writable"
[ "$(x 'touch /scratch/a && echo OK')" = OK ] || fail "--tmpfs is not writable"
x 'cat /proc/mounts' | grep_q " /scratch tmpfs" || fail "--tmpfs is not a tmpfs: $(x 'cat /proc/mounts')"
[ "$(x 'test -c /dev/nullo && echo DEV')" = DEV ] || fail "--device node missing inside"
x 'cat /etc/resolv.conf' | grep_q "^nameserver 10.99.0.1$" || fail "--dns not applied: $(x 'cat /etc/resolv.conf')"
x 'cat /etc/resolv.conf' | grep_q "^search example.test$" || fail "--dns-search not applied"
x 'cat /etc/hosts' | grep_q "^10.1.1.1 peer$" || fail "--add-host not applied: $(x 'cat /etc/hosts')"
x 'cat /etc/hosts' | grep_q "^10.99.0.1 gw$" || fail "--add-host host-gateway not applied: $(x 'cat /etc/hosts')"
x "grep 'open files' /proc/$pid/limits" | grep_q "64 *128" || fail "--ulimit not applied: $(x "grep 'open files' /proc/$pid/limits")"
[ "$(x "cat /proc/$pid/oom_score_adj")" = 100 ] || fail "--oom-score-adj not applied: $(x "cat /proc/$pid/oom_score_adj")"
[ "$(x 'cat /proc/sys/net/ipv4/icmp_echo_ignore_all')" = 1 ] || fail "--sysctl not applied in the namespace"
[ "$(cat /proc/sys/net/ipv4/icmp_echo_ignore_all)" = 0 ] || fail "--sysctl leaked into the host"
$NSPAWN inspect $app | python3 -c "
import json, sys
d = json.load(sys.stdin)[0]
assert d['hostname'] == 'h-$nonce' and d['user'] == 'nobody' and d['working_dir'] == '/tmp', d
assert d['cap_drop'] == ['ALL'] and d['cap_add'] == ['NET_BIND_SERVICE'] and d['read_only'] and d['tmpfs'] == ['/scratch:size=16m'], d
assert d['devices'] == ['/dev/null:/dev/nullo:r'] and d['ulimits'] == {'nofile': '64:128'}, d
assert d['stop_signal'] == 'SIGINT' and d['stop_timeout'] == 2 and d['oom_score_adj'] == 100, d
assert d['extra_hosts'] == ['peer:10.1.1.1', 'gw:host-gateway'] and d['sysctls'] == {'net.ipv4.icmp_echo_ignore_all': '1'}, d" || fail "inspect does not show the flags"
stop_start=$(date +%s)
out=$($NSPAWN stop $app 2>&1) || fail "stop with --stop-signal and --stop-timeout: $out"
echo "$out" | grep_q "ignored SIGINT for 2 seconds" || fail "stop did not use --stop-signal and --stop-timeout: $out"
[ $(( $(date +%s) - stop_start )) -le 8 ] || fail "stop took longer than --stop-timeout allows"
out=$($NSPAWN start $app -u 1000:1000 2>&1) && fail "a user with a group was accepted"
echo "$out" | grep_q "without a group" || fail "the refused user was not explained: $out"
out=$($NSPAWN start $app --sysctl kernel.shmmax=1 2>&1) && fail "a sysctl beyond net.* was accepted"
# Everything back, and --privileged: the whole bounding set.
$NSPAWN start $app --privileged --cap-drop none --cap-add none --read-only=false -u root -w / --tmpfs none --device none --dns none --dns-search none --add-host none --ulimit none --stop-signal "" --stop-timeout 10 --oom-score-adj 0 --sysctl none --hostname "" -- /bin/sleep 300 || fail "start with the flags taken back"
[ "$(x hostname)" = "$app" ] || fail "--hostname \"\" did not restore the name: $(x hostname)"
[ "$(x 'awk "/^CapBnd:/ {print \$2}" /proc/1/status')" != 0000000000000000 ] || fail "--privileged left no capabilities"
[ "$(x 'touch /x 2>/dev/null && echo RW || echo RO')" = RW ] || fail "--read-only=false left the root read-only"
$NSPAWN inspect $app | python3 -c "import json,sys; d = json.load(sys.stdin)[0]; assert d['privileged'] and not d['read_only'] and d['cap_drop'] == [] and d['cap_add'] == [] and d['hostname'] == '' and d['stop_signal'] == '', d" || fail "inspect after taking the flags back"
$NSPAWN stop $app || fail "stop the privileged machine"
$NSPAWN start $app --privileged=false -- /bin/sleep 300 >/dev/null || fail "start with --privileged=false"
$NSPAWN inspect $app | python3 -c "import json,sys; d = json.load(sys.stdin)[0]; assert not d['privileged'], d" || fail "--privileged=false did not take it back"
$NSPAWN stop $app || fail "stop"

hooks=/etc/systemd/system/systemd-nspawn@$app.service.d/nspawn-hooks.conf
step "restart policy on-failure: a killed program comes back with its network, stop keeps it down"
$NSPAWN start $app --restart on-failure -p 18081:80 -- /bin/sh -c 'mkdir -p /www; echo app-web > /www/index.html; exec /bin/httpd -f -p 80 -h /www' || fail "start with --restart on-failure"
grep -qx "Restart=on-failure" $hooks || fail "no Restart= in the drop-in"
grep -qx "StartLimitIntervalSec=0" $hooks || fail "no StartLimitIntervalSec= in the drop-in"
retry 5 bash -c "curl -sf -m 2 http://127.0.0.1:18081/ | grep_q app-web" || fail "the app does not answer before the kill"
addr_before=$(addr_of $app)
leader=$(machinectl show $app -p Leader --value)
kill -KILL $(pgrep -P "$leader") || fail "cannot kill the app's program"
retry 15 bash -c "[ \"\$(systemctl show -p NRestarts --value systemd-nspawn@$app.service)\" -ge 1 ]" || fail "on-failure did not restart the app"
retry 15 bash -c "curl -sf -m 2 http://127.0.0.1:18081/ | grep_q app-web" || fail "the restarted app does not answer on its published port"
[ "$(addr_of $app)" = "$addr_before" ] || fail "the restarted app changed its address"
[ "$(systemctl is-enabled systemd-nspawn@$app.service 2>/dev/null)" = enabled ] && fail "on-failure enabled the unit at boot"
$NSPAWN inspect $app | python3 -c "import json,sys; d = json.load(sys.stdin)[0]; assert d['restart'] == 'on-failure', d" || fail "inspect does not show the restart policy"
$NSPAWN stop $app || fail "stop an app with a restart policy"
sleep 5
$NSPAWN ps | grep_q "^ *$app " && fail "the app came back after stop"
systemctl is-failed systemd-nspawn@$app.service >/dev/null 2>&1 && fail "stop left the unit failed"
[ -e /run/netns/nspawn-$app ] && fail "stop left the network namespace behind"

step "restart policy: which endings bring a program back"
# on-failure leaves a program that ended well alone; always brings it back anyway.
$NSPAWN start $app --restart on-failure -p none -- /bin/sh -c 'sleep 1; exit 0' >/dev/null || fail "start a program that ends well under on-failure"
sleep 6
[ "$(systemctl show -p NRestarts --value systemd-nspawn@$app.service)" = 0 ] || fail "on-failure restarted a program that exited 0"
$NSPAWN ps | grep_q "^ *$app " && fail "on-failure kept a program that exited 0 running"
$NSPAWN stop $app >/dev/null 2>&1
$NSPAWN start $app --restart always -- /bin/sh -c 'sleep 1; exit 0' >/dev/null || fail "start a program that ends well under always"
retry 10 bash -c "[ \"\$(systemctl show -p NRestarts --value systemd-nspawn@$app.service)\" -ge 1 ]" || fail "always did not restart a program that exited 0"
$NSPAWN stop $app >/dev/null || fail "stop the always program"
[ "$(systemctl is-active systemd-nspawn@$app.service)" = inactive ] || fail "the always program came back after stop"

step "restart policy: a program that keeps failing is restarting, and stop ends it"
out=$($NSPAWN start $app --restart always -p none -- /bin/sh -c 'exit 1' 2>&1) || fail "start of a failing program with --restart always: $out"
retry 10 bash -c "$NSPAWN ps | grep '^ *$app ' | grep_q restarting" || fail "ps does not show the app restarting"
$NSPAWN inspect $app | python3 -c "import json,sys; d = json.load(sys.stdin)[0]; assert d['state'] in ('restarting', 'starting', 'running', 'closing'), d" || fail "inspect of a restarting app"
out=$($NSPAWN images rm $app 2>&1) && fail "images rm removed a machine that was restarting"
echo "$out" | grep_q "stop it first" || fail "images rm of a restarting machine was not explained: $out"
retry 10 bash -c "$NSPAWN ps | grep '^ *$app ' | grep_q restarting" || fail "the app stopped restarting on its own"
out=$($NSPAWN start $app 2>&1) && fail "start of a machine that is restarting succeeded"
echo "$out" | grep_q -E "restarting|already" || fail "start of a restarting machine was not explained: $out"
$NSPAWN stop $app >/dev/null || fail "stop a restarting app"
sleep 3
[ "$(systemctl is-active systemd-nspawn@$app.service)" = inactive ] || fail "the unit kept restarting after stop: $(systemctl is-active systemd-nspawn@$app.service)"
$NSPAWN ps -a | grep "^ *$app " | grep_q stopped || fail "the app is not stopped after stop"

step "restart policy always and unless-stopped: started at boot, stop decides"
$NSPAWN start $app --restart always -- /bin/sleep 300 || fail "start with --restart always"
[ "$(systemctl is-enabled systemd-nspawn@$app.service)" = enabled ] || fail "always did not enable the unit"
[ -L /etc/systemd/system/machines.target.wants/systemd-nspawn@$app.service ] || fail "always did not hook the unit to machines.target"
systemctl is-enabled machines.target >/dev/null || fail "machines.target is not enabled"
# What boot starts: machines.target, which multi-user.target wants, wants the machine.
systemctl show -p Wants --value machines.target | grep_q -w "systemd-nspawn@$app.service" || fail "machines.target does not want the always machine"
systemctl list-dependencies --plain multi-user.target 2>/dev/null | grep_q "machines.target" || fail "machines.target is not reached at boot"
$NSPAWN stop $app || fail "stop an always machine"
[ "$(systemctl is-enabled systemd-nspawn@$app.service)" = enabled ] || fail "stop disabled an always machine"
$NSPAWN start $app --restart unless-stopped || fail "start with --restart unless-stopped"
[ "$(systemctl is-enabled systemd-nspawn@$app.service)" = enabled ] || fail "unless-stopped did not enable the unit"
$NSPAWN stop $app --no-wait || fail "stop --no-wait of an unless-stopped machine"
retry 10 bash -c "! $NSPAWN ps | grep_q '^ *$app '" || fail "unless-stopped machine still running after stop --no-wait"
[ "$(systemctl is-enabled systemd-nspawn@$app.service)" = disabled ] || fail "stop did not disable an unless-stopped machine"
$NSPAWN start $app || fail "start an unless-stopped machine again"
[ "$(systemctl is-enabled systemd-nspawn@$app.service)" = enabled ] || fail "start did not enable an unless-stopped machine again"
$NSPAWN stop $app --force || fail "stop --force of an unless-stopped machine"
[ "$(systemctl is-enabled systemd-nspawn@$app.service)" = disabled ] || fail "stop --force did not disable an unless-stopped machine"
systemctl is-failed systemd-nspawn@$app.service >/dev/null 2>&1 && fail "stop --force left the unit failed"
$NSPAWN start $app --restart always >/dev/null && $NSPAWN stop $app >/dev/null || fail "back to always"
$NSPAWN start $app --restart no >/dev/null || fail "start with --restart no"
[ "$(systemctl is-enabled systemd-nspawn@$app.service)" = enabled ] && fail "--restart no left the unit enabled"
grep -q "^Restart=" $hooks && fail "--restart no left Restart= in the drop-in"
$NSPAWN stop $app >/dev/null || fail "stop after --restart no"
$NSPAWN create $app e2e-restart --restart always -- /bin/sleep 300 || fail "create with --restart always"
$NSPAWN start e2e-restart >/dev/null || fail "start the created always machine"
[ -L /etc/systemd/system/machines.target.wants/systemd-nspawn@e2e-restart.service ] || fail "the created always machine is not enabled"
$NSPAWN stop e2e-restart >/dev/null || fail "stop the created always machine"
# A machine caught in a restart loop is removed with -f: the stop ends the loop first.
$NSPAWN start e2e-restart -- /bin/sh -c 'exit 1' >/dev/null 2>&1
retry 10 bash -c "$NSPAWN ps | grep '^ *e2e-restart ' | grep_q restarting" || fail "the created machine is not restarting"
$NSPAWN rm -f e2e-restart >/dev/null || fail "rm -f of an enabled machine that is restarting"
sleep 3
systemctl is-active systemd-nspawn@e2e-restart.service >/dev/null && fail "the unit of a removed machine is still restarting"
[ -e /etc/systemd/system/machines.target.wants/systemd-nspawn@e2e-restart.service ] && fail "rm left the boot link behind"
[ -e /etc/systemd/system/systemd-nspawn@e2e-restart.service.d ] && fail "rm left the drop-in directory behind"

step "kill: a signal for the program, and what ends a machine for good"
$NSPAWN start $app --restart always -p none -- /bin/sh -c 'trap "echo e2e-got-hup" HUP; while :; do sleep 1; done' >/dev/null || fail "start an app that traps SIGHUP"
[ "$($NSPAWN kill -s HUP $app)" = "$app" ] || fail "kill -s HUP does not print the machine's name"
retry 5 bash -c "$NSPAWN logs $app | grep_q e2e-got-hup" || fail "the program did not get SIGHUP"
$NSPAWN ps | grep_q "^ *$app " || fail "SIGHUP, which the program handles, ended the machine"
restarts=$(systemctl show -p NRestarts --value systemd-nspawn@$app.service)
$NSPAWN kill -s USR1 $app >/dev/null || fail "kill -s USR1"
retry 15 bash -c "[ \"\$(systemctl show -p NRestarts --value systemd-nspawn@$app.service)\" -gt $restarts ]" || fail "a program ended by kill -s USR1 was not restarted by its policy"
retry 15 bash -c "$NSPAWN ps | grep_q '^ *$app '" || fail "the app did not come back after SIGUSR1"
# docker's rule: the stop signal sent by kill ends the machine for good.
$NSPAWN kill -s TERM $app >/dev/null || fail "kill with the stop signal"
sleep 5
# The program died of the signal, so the unit may end failed; it must not come back.
case "$(systemctl is-active systemd-nspawn@$app.service)" in
  active|activating) fail "the stop signal sent by kill did not end the machine for good" ;;
esac
[ -e /var/lib/nspawn/machines/$app/exit-on-next ] && fail "the release hook left the exit-on-next mark behind"
$NSPAWN start $app >/dev/null || fail "start after kill -s TERM"
$NSPAWN kill $app >/dev/null || fail "kill (SIGKILL)"
sleep 3
[ "$(systemctl is-active systemd-nspawn@$app.service)" = inactive ] || fail "kill did not end a machine with --restart always"
systemctl is-failed systemd-nspawn@$app.service >/dev/null 2>&1 && fail "kill left the unit failed"
out=$($NSPAWN kill $app 2>&1) && fail "kill of a stopped machine succeeded"
echo "$out" | grep_q "not running" || fail "kill of a stopped machine was not explained: $out"
$NSPAWN start $app --restart no >/dev/null || fail "start after kill"
$NSPAWN kill -s BOGUS $app 2>/dev/null && fail "kill -s BOGUS succeeded"
$NSPAWN ps | grep_q "^ *$app " || fail "a refused kill ended the machine"
$NSPAWN stop $app >/dev/null || fail "stop after the kill checks"

step "events: what happens to machines, as it happens and afterwards"
since=$(date '+%Y-%m-%d %H:%M:%S')
$NSPAWN events --json > /tmp/e2e-events.json 2>/tmp/e2e-events.err &
events_pid=$!
sleep 2
$NSPAWN start $app --restart no -p none -- /bin/sh -c 'sleep 1; exit 3' >/dev/null 2>&1
retry 10 bash -c "! $NSPAWN ps | grep_q '^ *$app '" || fail "the program that exits 3 did not end"
$NSPAWN start $app -- /bin/sleep 300 >/dev/null || fail "start before kill"
$NSPAWN kill $app >/dev/null || fail "kill for the events"
$NSPAWN volume create e2evol-events >/dev/null || fail "volume create for the events"
$NSPAWN volume rm e2evol-events >/dev/null || fail "volume rm for the events"
# A user cannot pass an entry of their own off as nspawn's.
printf 'MESSAGE=forged\nMESSAGE_ID=b0b60147942247cab22cc49510006a0b\nNSPAWN_TYPE=machine\nNSPAWN_ACTION=start\nNSPAWN_NAME=e2e-forged\n' | runuser -u nobody -- logger --journald 2>/dev/null
sleep 3
kill $events_pid 2>/dev/null; wait $events_pid 2>/dev/null
python3 - "$app" /tmp/e2e-events.json <<'PY' || fail "events did not report what happened: $(cat /tmp/e2e-events.json /tmp/e2e-events.err)"
import json, sys
app, path = sys.argv[1], sys.argv[2]
events = [json.loads(l) for l in open(path) if l.strip()]
seen = {(e["type"], e["action"], e["name"]) for e in events}
for want in [("machine", "start", app), ("machine", "die", app), ("machine", "kill", app),
             ("volume", "create", "e2evol-events"), ("volume", "remove", "e2evol-events")]:
    assert want in seen, (want, sorted(seen))
assert any(e["action"] == "die" and e["name"] == app and e["attributes"].get("exit_code") == "3" for e in events), events
assert all(e["name"] != "e2e-forged" for e in events), "a forged entry was reported"
assert all(e["time"].endswith("Z") for e in events)
PY
out=$(timeout 30 $NSPAWN events --since "$since" --until now --filter name=$app --filter event=die) || fail "events --since --until did not end by itself: $out"
echo "$out" | grep_q "machine die $app (.*exit_code=3" || fail "events --since --until misses the exit: $out"
echo "$out" | grep -v "machine die $app " | grep_q . && fail "events --filter let other events through: $out"
$NSPAWN events --filter colour=red 2>/dev/null && fail "events with an unknown filter succeeded"

step "resource limits: memory, cpus and processes of the whole machine"
$NSPAWN start $app -m 64m --cpus 0.5 --pids-limit 100 -- /bin/sleep 300 || fail "start with limits"
[ "$(systemctl show -p MemoryMax --value systemd-nspawn@$app.service)" = 67108864 ] || fail "MemoryMax not set"
[ "$(systemctl show -p MemorySwapMax --value systemd-nspawn@$app.service)" = 67108864 ] || fail "MemorySwapMax not set"
[ "$(systemctl show -p CPUQuotaPerSecUSec --value systemd-nspawn@$app.service)" = 500ms ] || fail "CPUQuota not set"
[ "$(systemctl show -p TasksMax --value systemd-nspawn@$app.service)" = 100 ] || fail "TasksMax not set"
cg=/sys/fs/cgroup/machine.slice/systemd-nspawn@$app.service
[ "$(cat $cg/memory.max)" = 67108864 ] || fail "memory.max of the machine's cgroup: $(cat $cg/memory.max)"
[ "$(cat $cg/memory.swap.max)" = 67108864 ] || fail "memory.swap.max of the machine's cgroup: $(cat $cg/memory.swap.max)"
[ "$(cat $cg/cpu.max)" = "50000 100000" ] || fail "cpu.max of the machine's cgroup: $(cat $cg/cpu.max)"
[ "$(cat $cg/pids.max)" = 100 ] || fail "pids.max of the machine's cgroup: $(cat $cg/pids.max)"
# The shell's own status is not the point (pipefail would count it): what it said is.
out=$($NSPAWN exec $app -- /bin/sh -c 'i=0; while [ $i -lt 150 ]; do sleep 5 & i=$((i+1)); done; wait' </dev/null 2>&1)
echo "$out" | grep_q -i "fork" || fail "the machine could start more processes than --pids-limit"
$NSPAWN inspect $app | python3 -c "import json,sys; d = json.load(sys.stdin)[0]; assert d['memory'] == 67108864 and d['cpus'] == 0.5 and d['pids_limit'] == 100, d" || fail "inspect does not show the limits"
# The memory limit holds, swap included: a program that wants more than the whole
# machine may have (64m of memory and as much swap) is killed by the kernel (128 +
# SIGKILL).
$NSPAWN exec $app -- /bin/sh -c 'x=$(head -c 268435456 /dev/zero | tr "\\0" a); echo ${#x}' </dev/null >/dev/null 2>&1; rc=$?
[ "$rc" = 137 ] || fail "a program went past --memory without being killed (exit $rc)"
$NSPAWN stop $app --force >/dev/null || fail "stop the limited app"
$NSPAWN start $app -m 0 --cpus 0 --pids-limit 0 -- /bin/sleep 300 >/dev/null || fail "start with the limits removed"
[ "$(systemctl show -p MemoryMax --value systemd-nspawn@$app.service)" = infinity ] || fail "-m 0 did not remove the memory limit"
[ "$(systemctl show -p MemorySwapMax --value systemd-nspawn@$app.service)" = infinity ] || fail "-m 0 did not remove the swap limit"
[ "$(systemctl show -p CPUQuotaPerSecUSec --value systemd-nspawn@$app.service)" = infinity ] || fail "--cpus 0 did not remove the CPU limit"
[ "$(systemctl show -p TasksMax --value systemd-nspawn@$app.service)" = 100 ] && fail "--pids-limit 0 did not remove the process limit"
$NSPAWN stop $app >/dev/null || fail "stop after removing the limits"

step "update: the limits of a running machine change at once, the policy with them"
$NSPAWN start $app -m 64m -- /bin/sleep 300 >/dev/null || fail "start before update"
[ "$($NSPAWN update $app -m 128m --cpus 0.5 --pids-limit 50)" = "$app" ] || fail "update of a running machine"
[ "$(cat $cg/memory.max)" = 134217728 ] || fail "update did not change memory.max at once: $(cat $cg/memory.max)"
[ "$(cat $cg/memory.swap.max)" = 134217728 ] || fail "update did not change memory.swap.max at once: $(cat $cg/memory.swap.max)"
[ "$(cat $cg/cpu.max)" = "50000 100000" ] || fail "update did not change cpu.max at once: $(cat $cg/cpu.max)"
[ "$(cat $cg/pids.max)" = 50 ] || fail "update did not change pids.max at once: $(cat $cg/pids.max)"
[ "$(systemctl show -p MemoryMax --value systemd-nspawn@$app.service)" = 134217728 ] || fail "the unit does not show the updated MemoryMax"
left=$(ls /run/systemd/system.control/systemd-nspawn@$app.service.d/ 2>/dev/null)
[ -z "$left" ] || fail "update left runtime settings behind: $left"
grep -qx "MemoryMax=134217728" $hooks || fail "update did not write the new limit into the drop-in"
$NSPAWN inspect $app | python3 -c "import json,sys; d = json.load(sys.stdin)[0]; assert d['memory'] == 134217728 and d['cpus'] == 0.5 and d['pids_limit'] == 50, d" || fail "inspect does not show the updated limits"
$NSPAWN update $app --restart always >/dev/null || fail "update --restart always"
[ "$(systemctl is-enabled systemd-nspawn@$app.service)" = enabled ] || fail "update --restart always did not enable the unit"
grep -qx "Restart=always" $hooks || fail "update --restart always did not write Restart="
$NSPAWN update $app -m 0 --restart no >/dev/null || fail "update -m 0 --restart no"
[ "$(cat $cg/memory.max)" = max ] || fail "update -m 0 did not remove the memory limit: $(cat $cg/memory.max)"
[ "$(systemctl is-enabled systemd-nspawn@$app.service)" = enabled ] && fail "update --restart no left the unit enabled"
$NSPAWN stop $app >/dev/null || fail "stop after update"
$NSPAWN start $app >/dev/null || fail "start after update"
[ "$(cat $cg/pids.max)" = 50 ] || fail "the updated limits did not outlive a restart: $(cat $cg/pids.max)"
$NSPAWN stop $app >/dev/null || fail "stop the updated machine"
$NSPAWN update $app --pids-limit 0 >/dev/null || fail "update of a stopped machine"
$NSPAWN inspect $app | python3 -c "import json,sys; d = json.load(sys.stdin)[0]; assert d['pids_limit'] == 0, d" || fail "update of a stopped machine was not remembered"
$NSPAWN update $app 2>/dev/null && fail "update with nothing to change succeeded"
$NSPAWN update e2e-nope -m 64m 2>/dev/null && fail "update of an unknown machine succeeded"

step "healthchecks: a probe inside the machine, its verdict in ps, inspect and events"
health_since=$(date +%s)
$NSPAWN start $app --health-cmd "test -f /ok" --health-interval 1s --health-retries 2 --health-timeout 5s -- /bin/sleep 300 || fail "start with a healthcheck"
retry 5 systemctl is-active nspawn-health-$app.service >/dev/null || fail "the health runner unit is not running"
retry 5 bash -c "$NSPAWN ps | grep '^ *$app ' | grep_q 'health: starting'" || fail "ps does not show the health as starting: $($NSPAWN ps | grep $app)"
retry 10 bash -c "$NSPAWN ps | grep '^ *$app ' | grep_q '(unhealthy)'" || fail "two failed probes did not make the machine unhealthy: $($NSPAWN ps | grep $app)"
$NSPAWN exec $app -- touch /ok </dev/null || fail "touch /ok"
retry 10 bash -c "$NSPAWN ps | grep '^ *$app ' | grep_q '(healthy)'" || fail "a successful probe did not make the machine healthy: $($NSPAWN ps | grep $app)"
$NSPAWN inspect $app | python3 -c "
import json, sys
d = json.load(sys.stdin)[0]
assert d['health'] == 'healthy' and d['health_failing_streak'] == 0, d
assert d['healthcheck']['test'] == ['CMD-SHELL', 'test -f /ok'] and d['healthcheck']['retries'] == 2 and d['healthcheck']['interval'] == 1000000, d['healthcheck']
assert len(d['health_log']) >= 1, d" || fail "inspect does not show the health"
out=$($NSPAWN events --since "@$health_since" --until now --filter name=$app --filter event=health_status)
echo "$out" | grep_q "status=unhealthy" || fail "no health_status event for unhealthy: $out"
echo "$out" | grep_q "status=healthy" || fail "no health_status event for healthy: $out"
$NSPAWN update $app --health-retries 5 >/dev/null || fail "update the healthcheck"
retry 5 systemctl is-active nspawn-health-$app.service >/dev/null || fail "the health runner did not come back after update"
$NSPAWN inspect $app | python3 -c "import json,sys; d = json.load(sys.stdin)[0]; assert d['healthcheck']['retries'] == 5, d['healthcheck']" || fail "update did not change the retries"
$NSPAWN stop $app || fail "stop the machine with a healthcheck"
systemctl is-active nspawn-health-$app.service >/dev/null 2>&1 && fail "the health runner outlived the machine"
[ -e /run/nspawn/health/$app.json ] && fail "the health status file was left behind"
$NSPAWN start $app --no-healthcheck -- /bin/sleep 300 || fail "start with --no-healthcheck"
sleep 1
systemctl is-active nspawn-health-$app.service >/dev/null 2>&1 && fail "a runner started for a disabled healthcheck"
$NSPAWN ps | grep "^ *$app " | grep_q "health" && fail "ps shows a health for a machine without healthcheck"
$NSPAWN stop $app || fail "stop"

step "restart, pause, unpause and top, like docker's"
$NSPAWN start $app -- /bin/sleep 300 >/dev/null || fail "start for restart"
pid_before=$(machinectl show $app -p Leader --value)
$NSPAWN restart $app -t 2 | grep_q "restarted $app" || fail "restart"
retry 5 bash -c "$NSPAWN ps | grep '^ *$app ' | grep_q ' running '" || fail "not running after restart"
[ "$(machinectl show $app -p Leader --value)" != "$pid_before" ] || fail "restart did not start the machine anew"
$NSPAWN pause $app || fail "pause"
[ "$(systemctl show -p FreezerState --value systemd-nspawn@$app.service)" = frozen ] || fail "the unit is not frozen after pause"
$NSPAWN ps | grep "^ *$app " | grep_q " paused " || fail "ps does not show the machine paused: $($NSPAWN ps | grep $app)"
$NSPAWN inspect $app | python3 -c "import json,sys; d = json.load(sys.stdin)[0]; assert d['state'] == 'paused', d['state']" || fail "inspect does not show paused"
$NSPAWN unpause $app || fail "unpause"
[ "$(systemctl show -p FreezerState --value systemd-nspawn@$app.service)" = running ] || fail "the unit is still frozen after unpause"
$NSPAWN ps | grep "^ *$app " | grep_q " running " || fail "ps does not show the machine running after unpause"
$NSPAWN top $app | tee /tmp/e2e-top.txt | grep_q "/bin/sleep 300" || fail "top does not list the program: $(cat /tmp/e2e-top.txt)"
grep -q "systemd-nspawn" /tmp/e2e-top.txt && fail "top lists systemd-nspawn itself"
grep -q "^ *PID " /tmp/e2e-top.txt || fail "top has no header"
$NSPAWN pause $app >/dev/null && $NSPAWN stop $app -t 2 | grep_q "stopped $app" || fail "stop of a paused machine"
$NSPAWN pause $app 2>/dev/null && fail "pause of a stopped machine succeeded"
$NSPAWN stop $app e2e-none-such-$nonce >/tmp/e2e-stop2.txt 2>&1 && fail "stop of several with an unknown one succeeded"
grep -q "$app was not running" /tmp/e2e-stop2.txt || fail "stop of several did not go on after the first: $(cat /tmp/e2e-stop2.txt)"

step "stats: what running machines use, rates from two samples"
$NSPAWN start $app -m 64m --pids-limit 0 -- /bin/sh -c 'while :; do :; done' >/dev/null || fail "start a busy app"
out=$($NSPAWN stats --no-stream --json $app) || fail "stats --no-stream --json"
echo "$out" | python3 -c "import json,sys; d = json.loads(sys.stdin.read().strip().splitlines()[0]); assert d['name'] == '$app' and d['cpu_percent'] > 10 and d['memory_limit'] == 67108864 and d['memory'] > 0 and d['pids'] >= 1 and d['net_rx'] is not None and d['io_read'] is not None, d" || fail "stats of a busy app: $out"
$NSPAWN stats --no-stream | grep_q "^ *$app " || fail "stats without names does not list the app"
out=$(timeout 3 $NSPAWN stats $app)
echo "$out" | grep_q "^ *$app " || fail "stats did not draw a table within 3 seconds: $out"
out=$($NSPAWN stats --no-stream e2e-nope 2>&1) && fail "stats of a machine that does not run succeeded"
echo "$out" | grep_q "not running" || fail "stats of a machine that does not run was not explained: $out"
$NSPAWN stop $app --force >/dev/null || fail "stop the busy app"

step "named volumes: listed with their users, made ahead, removed once unused"
$NSPAWN volume create e2evol-free || fail "volume create"
[ "$(stat -c '%u %a' /var/lib/nspawn/volumes/e2evol-free)" = "0 755" ] || fail "volume create did not make a root directory with mode 0755"
$NSPAWN volume create e2evol-free >/dev/null || fail "volume create of an existing volume is not a no-op"
$NSPAWN volume create 'bad name' >/dev/null 2>&1 && fail "volume create accepted a bad name"
mkdir -p /var/lib/nspawn/volumes/.e2e-hidden
$NSPAWN volume ls | tee /tmp/e2e-vol.txt
grep "^ *e2evol-free " /tmp/e2e-vol.txt | grep_q " - " || fail "an unused volume is not shown as unused"
grep -q "e2e-hidden" /tmp/e2e-vol.txt && fail "a dot directory is listed as a volume"
$NSPAWN start $app -v e2evol2:/v -- /bin/sleep 300 >/dev/null || fail "start with a second named volume"
$NSPAWN volume ls | grep "^ *e2evol2 " | grep_q "$app" || fail "volume ls does not show who uses e2evol2"
$NSPAWN volume ls --json | python3 -c "import json,sys; d = {v['name']: v for v in json.load(sys.stdin)}; assert d['e2evol2']['used_by'] == ['$app'] and d['e2evol2']['path'] == '/var/lib/nspawn/volumes/e2evol2', d" || fail "volume ls --json"
out=$($NSPAWN volume rm e2evol2 2>&1) && fail "a volume in use was removed"
echo "$out" | grep_q "in use by $app" || fail "volume rm did not say who uses it: $out"
$NSPAWN volume prune </dev/null > /tmp/e2e-prune.txt 2>&1 || fail "volume prune without an answer failed"
[ -d /var/lib/nspawn/volumes/e2evol-free ] || fail "volume prune without a yes removed an unused volume"
grep_q "nothing removed" /tmp/e2e-prune.txt || fail "volume prune without a yes did not say so: $(cat /tmp/e2e-prune.txt)"
$NSPAWN volume prune -f | tee /tmp/e2e-prune.txt || fail "volume prune"
grep -q "removed e2evol-free" /tmp/e2e-prune.txt || fail "prune left an unused volume"
[ -d /var/lib/nspawn/volumes/e2evol2 ] || fail "prune removed a volume in use"
$NSPAWN stop $app >/dev/null || fail "stop the app with e2evol2"
$NSPAWN start $app -v none -- /bin/sleep 300 >/dev/null && $NSPAWN stop $app >/dev/null || fail "start with -v none"
$NSPAWN volume rm e2evol2 e2e-no-such-volume > /tmp/e2e-volrm.txt 2>&1 && fail "volume rm of a missing volume succeeded"
grep -q "removed e2evol2" /tmp/e2e-volrm.txt || fail "volume rm stopped at the missing volume: $(cat /tmp/e2e-volrm.txt)"
grep -q "no volume named e2e-no-such-volume" /tmp/e2e-volrm.txt || fail "a missing volume was not explained: $(cat /tmp/e2e-volrm.txt)"
$NSPAWN volume rm ../images >/dev/null 2>&1 && fail "volume rm reached outside the volumes directory"
rmdir /var/lib/nspawn/volumes/.e2e-hidden

step "secrets: kept encrypted, handed to a machine as files, removed once unused"
printf 'hunter2' | $NSPAWN secret create e2e-pw --label env=e2e || fail "secret create from stdin"
printf 'other' > /tmp/e2e-secret.txt; $NSPAWN secret create e2e-pw2 --file /tmp/e2e-secret.txt >/dev/null || fail "secret create from a file"; rm -f /tmp/e2e-secret.txt
[ "$(stat -c '%u %a' /var/lib/nspawn/secrets)" = "0 700" ] || fail "the secrets directory is open to others"
grep -q hunter2 /var/lib/nspawn/secrets/e2e-pw.cred && fail "the secret is stored in the clear"
printf 'x' | $NSPAWN secret create e2e-pw >/dev/null 2>&1 && fail "secret create replaced a secret"
printf 'x' | $NSPAWN secret create 'bad name' >/dev/null 2>&1 && fail "secret create accepted a bad name"
$NSPAWN secret ls | grep "^ *e2e-pw " | grep_q " 7 B " || fail "secret ls does not show the size: $($NSPAWN secret ls)"
$NSPAWN secret inspect e2e-pw | python3 -c "import json,sys; d = json.load(sys.stdin)[0]; assert d['labels'] == {'env': 'e2e'} and d['used_by'] == [] and 'hunter2' not in json.dumps(d), d" || fail "secret inspect"
$NSPAWN start $app --secret e2e-pw --secret e2e-pw2:/etc/other/key:0400:65534:65534 -- /bin/sleep 300 || fail "start with secrets"
[ "$($NSPAWN exec $app -- cat /run/secrets/e2e-pw </dev/null | tr -d '\r')" = hunter2 ] || fail "the secret is not readable inside"
[ "$($NSPAWN exec $app -- stat -c '%u %a' /run/secrets/e2e-pw </dev/null | tr -d '\r')" = "0 444" ] || fail "the secret file has the wrong mode or owner: $($NSPAWN exec $app -- stat -c '%u %a' /run/secrets/e2e-pw </dev/null)"
[ "$($NSPAWN exec $app -- stat -c '%u %g %a' /etc/other/key </dev/null | tr -d '\r')" = "65534 65534 400" ] || fail "the second secret has the wrong mode or owner: $($NSPAWN exec $app -- stat -c '%u %g %a' /etc/other/key </dev/null)"
[ "$($NSPAWN exec $app -- cat /etc/other/key </dev/null | tr -d '\r')" = other ] || fail "the second secret is not readable inside"
$NSPAWN exec $app -- /bin/sh -c 'echo x > /run/secrets/e2e-pw' </dev/null 2>/dev/null && fail "a secret was writable inside"
[ "$(stat -c '%u %a' /run/nspawn/secrets/$app)" = "0 700" ] || fail "the decrypted secrets are open to others on the host"
$NSPAWN secret ls | grep "^ *e2e-pw " | grep_q "$app" || fail "secret ls does not show who takes e2e-pw"
out=$($NSPAWN secret rm e2e-pw 2>&1) && fail "a secret in use was removed"
echo "$out" | grep_q "in use by $app" || fail "secret rm did not say who takes it: $out"
$NSPAWN inspect $app | python3 -c "import json,sys; d = json.load(sys.stdin)[0]; assert d['secrets'] == ['e2e-pw:/run/secrets/e2e-pw:0444:0:0', 'e2e-pw2:/etc/other/key:0400:65534:65534'], d['secrets']" || fail "inspect does not list the secrets"
$NSPAWN stop $app >/dev/null || fail "stop the app with secrets"
[ -e /run/nspawn/secrets/$app ] && fail "the decrypted secrets were left behind after stop"
out=$($NSPAWN start $app --secret e2e-missing -- /bin/sleep 1 2>&1) && fail "a missing secret was accepted"
echo "$out" | grep_q "no secret named e2e-missing" || fail "the missing secret was not explained: $out"
$NSPAWN start $app --secret none -- /bin/sleep 300 >/dev/null && $NSPAWN stop $app >/dev/null || fail "start with --secret none"
$NSPAWN secret rm e2e-pw e2e-pw2 e2e-nope > /tmp/e2e-secretrm.txt 2>&1 && fail "secret rm of a missing secret succeeded"
grep -q "removed e2e-pw" /tmp/e2e-secretrm.txt || fail "secret rm stopped at the missing secret: $(cat /tmp/e2e-secretrm.txt)"
grep -q "no secret named e2e-nope" /tmp/e2e-secretrm.txt || fail "a missing secret was not explained: $(cat /tmp/e2e-secretrm.txt)"
ls /var/lib/nspawn/secrets/ | grep_q e2e-pw && fail "secret rm left files behind"
$NSPAWN events --since "-2min" --until now --filter type=secret | grep_q "secret create e2e-pw" || fail "no event for the secret"

step "rm: a running machine is refused, rm -f stops it first, named volumes stay"
$NSPAWN start $app -v e2evol:/vol -- /bin/sleep 300 >/dev/null || fail "start the app with e2evol"
out=$($NSPAWN rm $app 2>&1) && fail "rm removed a running machine"
echo "$out" | grep_q "rm --force" || fail "rm of a running machine does not mention --force: $out"
$NSPAWN rm -f $app > /tmp/e2e-rmf.txt 2>&1 || { cat /tmp/e2e-rmf.txt; fail "rm -f of a running machine"; }
grep -q "removed $app" /tmp/e2e-rmf.txt || fail "rm -f did not say it removed $app: $(cat /tmp/e2e-rmf.txt)"
grep -q "volume e2evol kept" /tmp/e2e-rmf.txt || fail "rm did not say the named volume was kept: $(cat /tmp/e2e-rmf.txt)"
[ -d /var/lib/nspawn/volumes/e2evol ] || fail "rm removed a named volume"
$NSPAWN ps -a | grep_q "^ *$app " && fail "$app still listed after rm -f"
[ -e "/var/lib/machines/$app" ] && fail "/var/lib/machines/$app left after rm -f"
systemctl is-failed systemd-nspawn@$app.service >/dev/null 2>&1 && fail "rm -f left the unit failed"
[ -e /etc/systemd/nspawn/$app.nspawn ] && fail "settings file left behind for $app"
ls /etc/systemd/system/ | grep_q "$app" && fail "unit files left behind for $app"

step "progress: a bar follows each download on a terminal, nothing of it in a pipe"
# rm -f collected the blobs of busybox, so this pull downloads them again.
bar='[0-9] [KMG]?i?B/[0-9.]+ [KMG]?i?B'
if grep_q "blob .*: downloading" /tmp/e2e-app.txt; then
  grep_q -E "$bar" /tmp/e2e-app.txt && fail "progress went into a pipe: $(cat /tmp/e2e-app.txt)"
fi
python3 "$(dirname "$0")/terminal.py" $NSPAWN pull docker.io/library/busybox:latest --name $app --backend overlay --force </dev/null > /tmp/e2e-progress.txt 2>&1 || fail "pull busybox on a terminal"
tr -d '\r' < /tmp/e2e-progress.txt | grep_q "blob .*: downloading" || fail "the pull after rm -f downloaded nothing: $(tr -d '\r' < /tmp/e2e-progress.txt)"
grep_q -aE "$bar" /tmp/e2e-progress.txt || fail "no progress bar on a terminal: $(tr -d '\r' < /tmp/e2e-progress.txt)"
$NSPAWN rm $app >/dev/null || fail "rm $app after the pull on a terminal"

step "pipelines: a reader that closes early must not make nspawn fail"
$NSPAWN hub ls | head -c 1 >/dev/null; rc=${PIPESTATUS[0]}
[ "$rc" = 0 ] || [ "$rc" = 141 ] || fail "nspawn exited with $rc when the pipe closed"

step "D-Bus: org.nspawn as other clients see it"
if command -v busctl >/dev/null 2>&1; then
  if [ "$packaged" != yes ]; then
    grep -q "wrote /etc/systemd/system/nspawn.service" /tmp/e2e-install.txt || fail "install did not write the unit"
  fi
  B="busctl --system --timeout=120"
  M="org.nspawn /org/nspawn org.nspawn.Manager"
  $B introspect $M > /tmp/e2e-introspect.txt || fail "org.nspawn not reachable; the bus should have started it"
  for m in ListImages GetImage PullImage CreateMachine PushImage BuildImage RemoveImages SearchImages ListRepositories ListTags ListMachines GetMachine MachineStats StartMachine RunMachine StopMachine KillMachine PauseMachine UnpauseMachine MachineProcesses UpdateMachine Exec Events Shell Logs ListSecrets GetSecret CreateSecret RemoveSecrets ListNetworks GetNetwork CreateNetwork RemoveNetworks PruneNetworks NetworkUp Login Logout RemoveMachines CopyFrom CopyTo ListVolumes CreateVolume RemoveVolumes PruneVolumes; do
    grep -q "^\.$m  *method" /tmp/e2e-introspect.txt || fail "method $m missing from org.nspawn.Manager"
  done
  for sig in JobOutput JobProgress JobRemoved ImageAdded ImageRemoved MachineStarted MachineStopped; do
    grep -q "^\.$sig  *signal" /tmp/e2e-introspect.txt || fail "signal $sig missing from org.nspawn.Manager"
  done
  systemctl is-active nspawn.service >/dev/null || fail "the bus did not start nspawn.service"
  [ "$($B get-property $M Version)" = "s \"$($NSPAWN --version | awk '{print $2}')\"" ] || fail "Version property"
  job=$($B call $M PullImage 'sa{sv}' "$IMAGE" 3 name s e2e-dbus backend s overlay force b true | awk '{print $2}' | tr -d '"')
  echo "pull job: $job"
  echo "$job" | grep_q "^/org/nspawn/job/" || fail "PullImage did not return a job path"
  retry 90 bash -c "[ \"\$($B get-property org.nspawn $job org.nspawn.Job State)\" != 's \"running\"' ]" || fail "the pull job did not end"
  [ "$($B get-property org.nspawn $job org.nspawn.Job State)" = 's "done"' ] || { $B get-property org.nspawn $job org.nspawn.Job Error; fail "the pull job failed"; }
  $B get-property org.nspawn $job org.nspawn.Job Output | grep_q "assembling as overlay" || fail "the job kept no output"
  $B get-property org.nspawn $job org.nspawn.Job Result | grep_q '"name" s "e2e-dbus"' || fail "the job kept no result"
  $B get-property $M Jobs | grep_q "$job" || fail "Jobs property misses the job"
  $B call $M ListImages | grep_q '"name" s "e2e-dbus"' || fail "ListImages misses the pulled image"
  $B call $M GetImage s e2e-dbus | grep_q '"mode" s "boot"' || fail "GetImage"
  $B call $M GetImage s e2e-dbus | grep_q '"restart" s "no"' || fail "GetImage has no restart policy"
  digest=$($B call $M GetImage s e2e-dbus | grep -oE '"digest" s "sha256:[0-9a-f]+"' | grep -oE 'sha256:[0-9a-f]+')
  [ -n "$digest" ] || fail "GetImage has no digest"
  $NSPAWN pull "${IMAGE%%:*}@$digest" --name e2e-digest --backend flat --force >/dev/null || fail "pull by digest"
  $NSPAWN start e2e-digest >/dev/null && $NSPAWN exec e2e-digest -- /usr/bin/true </dev/null && $NSPAWN stop e2e-digest >/dev/null || fail "flat machine pulled by digest"
  $NSPAWN rm e2e-digest >/dev/null || fail "rm of a stopped machine"
  [ "$($B call $M StartMachine 'sa{sv}' e2e-dbus 0)" = 'sas "started" 0' ] || fail "StartMachine"
  out=$($NSPAWN images rm e2e-nothing-$nonce e2e-dbus 2>&1); rc=$?
  [ "$rc" != 0 ] || fail "images rm of a running machine succeeded"
  echo "$out" | grep_q "removed e2e-nothing-$nonce" || fail "images rm stopped at the first failure: $out"
  echo "$out" | grep_q "machine e2e-dbus is running" || fail "images rm did not explain the failure: $out"
  $B call $M ListMachines b false > /tmp/e2e-lm.txt
  grep -q '"name" s "e2e-dbus"' /tmp/e2e-lm.txt || fail "ListMachines misses the machine"
  grep -q '"machine_path" s "/org/freedesktop/machine1/machine/e2e_2ddbus"' /tmp/e2e-lm.txt || fail "ListMachines has no machined path"
  grep -q '"state" s "running"' /tmp/e2e-lm.txt || fail "ListMachines: not running"
  $B call $M GetMachine s e2e-dbus | grep_q '"state" s "running"' || fail "GetMachine of a running machine"
  $B call $M GetNetwork s bridge | grep_q '"name" s "e2e-dbus"' || fail "GetNetwork misses the machine"
  # Every exec of this run went through Exec; each left a process object behind.
  out=$($NSPAWN exec e2e-dbus -- /bin/sh -c "echo via-bus-$nonce; exit 7" </dev/null); code=$?
  [ "$code" = 7 ] || fail "exec did not propagate the exit code (got $code)"
  echo "$out" | grep_q "via-bus-$nonce" || fail "exec lost the output: $out"
  proc=$($B get-property $M Processes | awk '{print $NF}' | tr -d '"')
  echo "$proc" | grep_q "^/org/nspawn/process/" || fail "no process object after Exec"
  [ "$($B get-property org.nspawn $proc org.nspawn.Process State)" = 's "exited"' ] || fail "the process object did not see the exit"
  [ "$($B get-property org.nspawn $proc org.nspawn.Process ExitStatus)" = "i 7" ] || fail "the process object kept the wrong exit status"
  $B get-property org.nspawn $proc org.nspawn.Process Argv | grep_q "via-bus-$nonce" || fail "the process object has the wrong argv"
  # A command nobody pumps: signalled through its object, it ends with 128 plus the signal.
  proc=$($B call $M Exec 'sassa{sv}' e2e-dbus 2 /bin/sleep 300 "" 1 tty b false | grep -oE '"/org/nspawn/process/[0-9]+"' | tr -d '"')
  [ -n "$proc" ] || fail "Exec over the bus returned no process object"
  [ "$($B get-property org.nspawn $proc org.nspawn.Process State)" = 's "running"' ] || fail "the sleeping command is not running"
  $B call org.nspawn $proc org.nspawn.Process Signal i 15 || fail "Signal over the bus"
  retry 10 bash -c "[ \"\$($B get-property org.nspawn $proc org.nspawn.Process State)\" = 's \"exited\"' ]" || fail "the signalled command did not exit"
  [ "$($B get-property org.nspawn $proc org.nspawn.Process ExitStatus)" = "i 143" ] || fail "the signalled command's status is not 143: $($B get-property org.nspawn $proc org.nspawn.Process ExitStatus)"
  out=$($B call org.nspawn $proc org.nspawn.Process Signal i 15 2>&1) && fail "Signal to an exited process succeeded"
  echo "$out" | grep_q "has exited" || fail "Signal to an exited process not refused with a reason: $out"
  job=$($B call $M RemoveMachines 'asa{sv}' 1 e2e-dbus 1 force b false | awk '{print $2}' | tr -d '"')
  retry 30 bash -c "[ \"\$($B get-property org.nspawn $job org.nspawn.Job State)\" != 's \"running\"' ]" || fail "the refused rm job did not end"
  $B get-property org.nspawn $job org.nspawn.Job Error | grep_q "rm --force" || fail "RemoveMachines without force did not refuse a running machine"
  [ "$($B call $M StopMachine 'sa{sv}' e2e-dbus 0)" = 'sas "stopped" 0' ] || fail "StopMachine"
  $B call $M ListMachines b true | grep_q '"state" s "stopped"' || fail "ListMachines with all misses the stopped machine"
  job=$($B call $M RemoveImages as 1 e2e-dbus | awk '{print $2}' | tr -d '"')
  echo "$job" | grep_q "^/org/nspawn/job/" || fail "RemoveImages did not return a job path"
  retry 30 bash -c "[ \"\$($B get-property org.nspawn $job org.nspawn.Job State)\" != 's \"running\"' ]" || fail "the rm job did not end"
  [ "$($B get-property org.nspawn $job org.nspawn.Job State)" = 's "done"' ] || { $B get-property org.nspawn $job org.nspawn.Job Error; fail "the rm job failed"; }
  $B get-property org.nspawn $job org.nspawn.Job Output | grep_q "removed e2e-dbus" || fail "the rm job said nothing about e2e-dbus"
  $B get-property org.nspawn $job org.nspawn.Job Result | grep_q '"removed" as 1 "e2e-dbus"' || fail "the rm job has no result"
  $B call $M Login 'sssa{sv}' "$NSPAWN_REGISTRY" tester s3cret 0 > /dev/null || fail "Login over the bus"
  python3 -c "import json; d = json.load(open('/etc/nspawn/auth.json')); assert '$NSPAWN_REGISTRY' in d['auths']" || fail "credentials from the bus not stored"
  [ "$($B call $M Logout s "$NSPAWN_REGISTRY")" = "b true" ] || fail "Logout over the bus"
  out=$($B call $M StartMachine 'sa{sv}' e2e-dbus 1 bogus s x 2>&1) && fail "an unknown option was accepted"
  echo "$out" | grep_q "unknown option" || fail "unknown option not named: $out"
  out=$($B call $M GetImage s e2e-nonexistent 2>&1) && fail "GetImage of a missing image succeeded"
  echo "$out" | grep_q "no image named" || fail "missing image not explained: $out"
else
  echo "busctl not installed: skipping the D-Bus section"
fi

step "a host that drops forwarded traffic, with and without iptables"
# Only where nothing else owns that table: deleting it on a host running docker would
# take docker's own rules with it.
if nft list table ip filter >/dev/null 2>&1; then
  echo "an ip filter table is already there (docker? ufw?): skipping"
else
  nft add table ip filter || fail "cannot make the test table"
  nft "add chain ip filter FORWARD { type filter hook forward priority filter; policy drop; }" || fail "cannot make the forward chain"
  nft add chain ip filter DOCKER-USER || fail "cannot make the DOCKER-USER chain"
  hidden_iptables=$(command -v iptables)
  [ -n "$hidden_iptables" ] && mv "$hidden_iptables" "${hidden_iptables}.e2e-hidden"
  out=$($NSPAWN network up 2>&1 >/dev/null)
  [ -n "$hidden_iptables" ] && mv "${hidden_iptables}.e2e-hidden" "$hidden_iptables"
  echo "$out" | grep_q "docker drops forwarded traffic" || fail "no warning when forwarding is dropped and iptables is missing: $out"
  echo "$out" | grep_q "published ports will answer on this host alone" || fail "the warning does not say what breaks: $out"
  hidden_iptables=
  # With iptables there, nspawn adds the exception instead of warning.
  out=$($NSPAWN network up 2>&1 >/dev/null)
  echo "$out" | grep_q "drops forwarded traffic" && fail "still warning although iptables is installed: $out"
  iptables -S DOCKER-USER | grep_q -- "-i nspawn0 -j ACCEPT" || fail "nspawn did not let the bridge through DOCKER-USER: $(iptables -S DOCKER-USER)"
  # iptables prints the conntrack states in its own order, so match the rule, not them.
  iptables -S DOCKER-USER | grep_q -- "-o nspawn0 -m conntrack" || fail "nspawn did not let the answers back in: $(iptables -S DOCKER-USER)"
  nft delete table ip filter || fail "cannot remove the test table"
fi

step "two pulls at once share their blobs and neither corrupts the other"
$NSPAWN pull "$IMAGE" --name e2e-twin-a --backend overlay --force > /tmp/e2e-twin-a.txt 2>&1 &
twin_a=$!
$NSPAWN pull "$IMAGE" --name e2e-twin-b --backend overlay --force > /tmp/e2e-twin-b.txt 2>&1 &
twin_b=$!
wait $twin_a || { cat /tmp/e2e-twin-a.txt; fail "the first of two pulls at once failed"; }
wait $twin_b || { cat /tmp/e2e-twin-b.txt; fail "the second of two pulls at once failed"; }
for blob in /var/lib/nspawn/blobs/sha256-*; do
  [ -e "$blob" ] || continue
  [ "sha256-$(sha256sum "$blob" | awk '{print $1}')" = "$(basename "$blob")" ] || fail "blob $(basename "$blob") does not match its digest after two pulls at once"
done
ls /var/lib/nspawn/blobs/ | grep_q "^\.part-\|^\.hold-" && fail "leftovers in the blob store after two pulls: $(ls -a /var/lib/nspawn/blobs/ | grep '^\.')"
$NSPAWN start e2e-twin-b >/dev/null && $NSPAWN exec e2e-twin-b -- /usr/bin/true </dev/null && $NSPAWN stop e2e-twin-b >/dev/null || fail "a machine pulled alongside another does not run"

step "polkit: a user who is not root, with and without a rule"
who=${SUDO_USER:-}
rules=/etc/polkit-1/rules.d/50-nspawn-e2e.rules
if [ -z "$who" ] || [ "$who" = root ] || ! command -v pkaction >/dev/null 2>&1; then
  echo "no unprivileged user or no polkit here: skipping"
elif ! pkaction --action-id org.nspawn.manage >/dev/null 2>&1; then
  fail "polkit does not know org.nspawn.manage; is the action file installed?"
elif sudo -u "$who" bash -c 'pkcheck --action-id org.nspawn.inspect --process $$' >/dev/null 2>&1; then
  # An administrator who already granted this user leaves no refusal to see.
  echo "this host already lets $who through: skipping"
else
  group=$(id -gn "$who")
  rm -f "$rules"
  out=$(sudo -u "$who" $NSPAWN ps 2>&1); rc=$?
  [ $rc -ne 0 ] || fail "$who could list machines with no rule in place"
  echo "$out" | grep_q "org.nspawn.inspect" || fail "the refusal does not name the action: $out"
  echo "$out" | grep_q "polkit rule" || fail "the refusal does not say how to allow it: $out"
  cat > "$rules" <<RULE
polkit.addRule(function (action, subject) {
    if (action.id.startsWith("org.nspawn.") && subject.isInGroup("$group")) {
        return polkit.Result.YES;
    }
});
RULE
  retry 5 bash -c "sudo -u $who $NSPAWN ps >/dev/null 2>&1" || fail "$who still cannot list machines with the rule in place: $(sudo -u "$who" $NSPAWN ps 2>&1 | tail -2)"
  sudo -u "$who" $NSPAWN logout "$NSPAWN_REGISTRY" >/dev/null 2>&1 || fail "$who cannot run a command that changes things with the rule in place"
  cpd=$(mktemp -d /tmp/e2e-cp-user.XXXXXX); chown "$who" "$cpd"
  sudo -u "$who" $NSPAWN cp e2e-twin-b:/etc/passwd "$cpd/" || fail "$who cannot copy out of a machine"
  [ "$(stat -c %U "$cpd/passwd" 2>/dev/null)" = "$who" ] || fail "a file copied out does not belong to $who"
  cmp -s "$cpd/passwd" /var/lib/machines/e2e-twin-b/etc/passwd || fail "the file $who copied out differs"
  # The rule lets them call; another user's command is still not theirs to read.
  if [ -n "${proc:-}" ] && command -v busctl >/dev/null 2>&1; then
    out=$(sudo -u "$who" busctl --system get-property org.nspawn "$proc" org.nspawn.Process Argv 2>&1) && fail "$who could read a command root ran: $out"
    echo "$out" | grep_q -i "denied" || fail "reading another user's command failed for the wrong reason: $out"
  fi
  rm -f "$rules"
  retry 5 bash -c "! sudo -u $who $NSPAWN ps >/dev/null 2>&1" || fail "$who can still list machines after the rule went"
  # Root never needs any of this.
  $NSPAWN ps >/dev/null || fail "root cannot list machines"
fi

$NSPAWN images rm e2e-twin-a e2e-twin-b >/dev/null || fail "rm the twin images"

step "error handling (these commands must fail with a useful message)"
out=$($NSPAWN cp /etc/hosts /tmp/e2e-cp-x 2>&1) && fail "cp between two local paths succeeded"
echo "$out" | grep_q "MACHINE:PATH" || fail "cp of two local paths not explained: $out"
out=$($NSPAWN cp a:/x b:/y 2>&1) && fail "cp between two machines succeeded"
echo "$out" | grep_q "between machines" || fail "cp between machines not explained: $out"
out=$($NSPAWN cp e2e-nonexistent:/etc/hosts /tmp/ 2>&1) && fail "cp out of a missing machine succeeded"
echo "$out" | grep_q "no machine or image named" || fail "cp out of a missing machine not explained: $out"
out=$($NSPAWN cp e2e-nonexistent: /tmp/ 2>&1) && fail "cp with no path after the colon succeeded"
out=$($NSPAWN pull "$NSPAWN_REGISTRY/does-not-exist:1" --name e2e-x 2>&1); rc=$?
echo "$out"
[ $rc -ne 0 ] || fail "pull of a missing image succeeded"
echo "$out" | grep_q -i "manifest" || fail "missing image error does not mention the manifest"
out=$($NSPAWN start e2e-nonexistent 2>&1); rc=$?
echo "$out"
[ $rc -ne 0 ] || fail "start of an unknown image succeeded"
echo "$out" | grep_q "no image named" || fail "start of an unknown image gave no hint"
out=$($NSPAWN stop e2e-nonexistent 2>&1); [ $? -ne 0 ] && echo "$out" | grep_q "not running" || fail "stop of unknown machine"
# A name is joined to paths: one that climbs out of its directory reaches none of them.
mkdir -p /tmp/e2e-sentinel/keep
for cmd in "images rm" "rm" "rm -f" "stop" "inspect"; do
  out=$($NSPAWN $cmd ../../../tmp/e2e-sentinel 2>&1) && fail "$cmd accepted a name that leaves its directory"
  echo "$out" | grep_q "invalid machine name" || fail "$cmd of ../../../tmp/e2e-sentinel was not refused by name: $out"
done
[ -d /tmp/e2e-sentinel/keep ] || fail "a removal reached a directory outside the store"
rm -rf /tmp/e2e-sentinel
out=$($NSPAWN --registry 127.0.0.1:9 search --source hub e2e-$nonce 2>&1 >/dev/null); rc=$?
[ $rc -eq 0 ] || fail "search with an unreachable hub failed instead of warning: $out"
echo "$out" | grep_q "warning: 127.0.0.1:9" || fail "search did not warn about the unreachable hub: $out"

echo
if [ "$failures" = 0 ]; then echo "ALL OK"; else echo "$failures FAILURE(S)"; exit 1; fi
