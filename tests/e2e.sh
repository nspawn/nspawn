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
  for m in e2e-overlay e2e-flat e2e-mstack e2e-a e2e-b e2e-c e2e-built e2e-roundtrip e2e-busybox e2e-run e2e-dbus e2e-digest e2e-restart e2e-twin-a e2e-twin-b e2e-na-web e2e-na-cli e2e-nb-web e2e-nc-web e2e-nc-pub e2e-def-cli; do
    $NSPAWN stop "$m" --force >/dev/null 2>&1 || true
    $NSPAWN images rm "$m" >/dev/null 2>&1 || true
  done
  $NSPAWN network rm e2e-na e2e-nb e2e-nc e2e-nd e2e-ne >/dev/null 2>&1 || true
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
$NSPAWN start e2e-b -p 18080:80 || fail "start e2e-b with a published port"
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
$NSPAWN exec e2e-a -- /bin/sh -c "getent hosts e2e-b" </dev/null | tr -d '\r' | grep_q "$b_addr" || fail "e2e-a does not resolve e2e-b"
$NSPAWN exec e2e-a -- /bin/sh -c "getent hosts host.nspawn.internal" </dev/null | tr -d '\r' | grep_q "10.99.0.1" || fail "host.nspawn.internal not resolvable"
$NSPAWN exec e2e-a -- /bin/bash -c 'exec 3<>/dev/tcp/e2e-b/80 && echo a-to-b >&3 && read -t 3 l <&3 && echo "reply:$l"' </dev/null | tr -d '\r' | grep_q "reply:a-to-b" || fail "e2e-a cannot reach e2e-b by name"
$NSPAWN stop e2e-b || fail "stop e2e-b"
$NSPAWN stop e2e-a || fail "stop e2e-a"
nft list map ip nspawn ports | grep_q 18080 && fail "published port still mapped after stop"

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
$NSPAWN create $app e2e-nc-pub --network e2e-nc -p 18091:80 -- /bin/sleep 1 >/dev/null || fail "create with a port on an internal network"
out=$($NSPAWN start e2e-nc-pub 2>&1) && fail "a port was published from an internal network"
echo "$out" | grep_q internal || fail "publishing from an internal network was not explained: $out"
$NSPAWN images rm e2e-nc-pub >/dev/null || fail "rm e2e-nc-pub"
out=$($NSPAWN network rm e2e-na 2>&1) && fail "network rm removed a network in use"
echo "$out" | grep_q "in use by e2e-na-cli, e2e-na-web" || fail "network rm of a network in use was not explained: $out"
$NSPAWN network rm bridge 2>/dev/null && fail "network rm removed the default network"
nft list table ip nspawn | grep_q nsbr-e2e-na || fail "no rules for e2e-na in the nspawn table"
# A bridge deleted by hand comes back with the next start of one of its machines.
$NSPAWN stop e2e-na-web >/dev/null && $NSPAWN stop e2e-na-cli >/dev/null || fail "stop the e2e-na machines"
ip link del nsbr-e2e-na || fail "delete the bridge by hand"
$NSPAWN start e2e-na-web >/dev/null && $NSPAWN start e2e-na-cli >/dev/null || fail "start after the bridge went"
retry 5 bash -c "$NSPAWN exec e2e-na-cli -- wget -qO- -T 3 http://e2e-na-web/ </dev/null 2>/dev/null | grep_q na-web" || fail "the network did not come back with its machines"
for m in e2e-na-web e2e-na-cli e2e-nb-web e2e-nc-web e2e-def-cli; do
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

step "run: a machine from an image and started in one step, like docker run -d"
# busybox is here as $app: run makes another machine of it without the registry.
$NSPAWN run docker.io/library/busybox:latest --name e2e-run -p 18082:80 -- /bin/sh -c "mkdir -p /www; echo run-$nonce > /www/index.html; exec /bin/httpd -f -p 80 -h /www" > /tmp/e2e-run.txt 2>&1 || { cat /tmp/e2e-run.txt; fail "run from a local image"; }
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
$NSPAWN rm -f e2e-run >/dev/null || fail "rm -f e2e-run"
systemctl is-failed systemd-nspawn@$app.service >/dev/null 2>&1 && fail "unit left in failed state after stop"

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
$NSPAWN start $app -- /bin/sh -c 'exit 3' >/dev/null 2>&1 || true
retry 10 bash -c "! $NSPAWN ps | grep_q '^ *$app '" || fail "failed app still listed"
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
  for m in ListImages GetImage PullImage CreateMachine PushImage BuildImage RemoveImages SearchImages ListRepositories ListTags ListMachines GetMachine MachineStats StartMachine StopMachine KillMachine UpdateMachine Exec Events Shell Logs ListNetwork ListNetworks GetNetwork CreateNetwork RemoveNetworks PruneNetworks NetworkUp Login Logout RemoveMachines CopyFrom CopyTo ListVolumes CreateVolume RemoveVolumes PruneVolumes; do
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
  $B call $M ListNetwork | grep_q '"name" s "e2e-dbus"' || fail "ListNetwork misses the machine"
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
