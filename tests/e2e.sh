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
nonce=$$
# The command line is a client of the org.nspawn service: it goes on the bus first,
# with a configuration file that names the registry and its CA for the service's own
# use (the command line passes them on every call anyway).
install_service() {
  printf 'registry = "%s"\nca_cert = "%s"\n' "$NSPAWN_REGISTRY" "$NSPAWN_CA_CERT" > /run/nspawn-e2e.toml
  $NSPAWN --config /run/nspawn-e2e.toml daemon --install > /tmp/e2e-install.txt 2>&1 || { cat /tmp/e2e-install.txt; echo "cannot install the service"; exit 1; }
  cat /tmp/e2e-install.txt
}
# Leftovers of an aborted run would make pulls and creates fail; the same at the end.
cleanup_machines() {
  local m
  for m in e2e-overlay e2e-flat e2e-mstack e2e-a e2e-b e2e-c e2e-built e2e-roundtrip e2e-busybox e2e-dbus; do
    $NSPAWN stop "$m" --force >/dev/null 2>&1 || true
    $NSPAWN images rm "$m" >/dev/null 2>&1 || true
  done
  $NSPAWN logout "$NSPAWN_REGISTRY" >/dev/null 2>&1 || true
  rm -rf /tmp/e2e-bind /tmp/e2e-boot-vol /var/lib/nspawn/volumes/e2evol
  kill "${listener_pid:-}" 2>/dev/null || true
  if [ "$networkd_was" != active ]; then
    systemctl stop systemd-networkd.service systemd-networkd.socket systemd-networkd-varlink.socket systemd-networkd-resolve-hook.socket >/dev/null 2>&1 || true
  fi
}
cleanup_service() {
  systemctl stop nspawn.service >/dev/null 2>&1 || true
  rm -f /etc/dbus-1/system.d/org.nspawn.conf /usr/share/dbus-1/system-services/org.nspawn.service /etc/systemd/system/nspawn.service /run/nspawn-e2e.toml
  semodule -r nspawn >/dev/null 2>&1 || true
  systemctl daemon-reload >/dev/null 2>&1 || true
}
cleanup() {
  cleanup_machines
  cleanup_service
}
install_service
cleanup_machines
trap cleanup EXIT

step "hub ls"
$NSPAWN hub ls | tee /tmp/e2e-hub.txt || fail "hub ls exited non-zero"
grep -q "${IMAGE%%:*}" /tmp/e2e-hub.txt || fail "hub ls does not list ${IMAGE%%:*}"
step "search: the hub and Docker Hub, each hit with its source"
$NSPAWN search "${IMAGE%%:*}" > /tmp/e2e-search.txt || fail "search exited non-zero"
grep "^ *$NSPAWN_REGISTRY " /tmp/e2e-search.txt | grep -q " ${IMAGE%%:*} " || fail "search does not list ${IMAGE%%:*} from the hub"
$NSPAWN search busybox --source dockerhub > /tmp/e2e-search.txt || fail "search on Docker Hub exited non-zero"
grep "^ *Docker Hub " /tmp/e2e-search.txt | grep -q "docker.io/library/busybox" || fail "search does not list busybox from Docker Hub with its source"

step "login and logout"
echo "s3cret" | $NSPAWN login "$NSPAWN_REGISTRY" -u tester --password-stdin || fail "login on the hub"
python3 -c "import json; d = json.load(open('/etc/nspawn/auth.json')); assert '$NSPAWN_REGISTRY' in d['auths']" || fail "credentials not stored"
[ "$(stat -c %a /etc/nspawn/auth.json)" = 600 ] || fail "auth.json is not mode 0600"
$NSPAWN hub ls >/dev/null || fail "hub ls with stored credentials"
out=$(echo "wrong-password" | $NSPAWN login docker.io -u nspawn-e2e-nobody --password-stdin 2>&1) && fail "Docker Hub accepted bogus credentials: $out"
echo "$out" | grep -q "rejected the credentials" || fail "bogus Docker Hub login gave no clear message: $out"
$NSPAWN logout "$NSPAWN_REGISTRY" | grep -q "removed" || fail "logout"
$NSPAWN logout "$NSPAWN_REGISTRY" | grep -q "no credentials" || fail "second logout should find nothing"

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
  grep "^ *$name " /tmp/e2e-img.txt | grep -q "$backend" || fail "$name backend not shown"
  step "start"
  vol_args=""
  if [ "$backend" != flat ]; then
    rm -rf /tmp/e2e-boot-vol; mkdir -p /tmp/e2e-boot-vol
    vol_args="-v /tmp/e2e-boot-vol:/srv/vol"
  fi
  $NSPAWN start "$name" $vol_args || fail "start ($backend)"
  if [ "$backend" = overlay ]; then
    findmnt -n -o FSTYPE "/var/lib/machines/$name" | grep -q overlay || fail "root of $name is not an overlay"
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
  step "exec"
  out=$($NSPAWN exec "$name" -- /usr/bin/systemctl is-system-running --wait </dev/null | tr -d '\r' || true)
  echo "is-system-running: $out"
  echo "$out" | grep -qE "running|degraded|starting" || fail "exec did not reach systemd inside $name"
  $NSPAWN exec "$name" -- /usr/bin/cat /etc/os-release </dev/null | tr -d '\r' | grep -q PRETTY_NAME || fail "exec cat os-release"
  $NSPAWN exec "$name" -- /bin/sh -c 'exit 7' </dev/null; [ $? -eq 7 ] || fail "exec did not propagate the exit code of a booted machine"
  $NSPAWN exec "$name" -- /bin/sh -c 'echo $PATH' </dev/null | tr -d '\r' | grep -q "/usr/bin" || fail "exec has no PATH"
  if [ "$backend" != flat ]; then
    step "volume in a booted machine ($backend: private users, idmapped)"
    $NSPAWN exec "$name" -- /bin/sh -c 'echo booted > /srv/vol/from-machine' </dev/null || fail "cannot write to the volume inside $name"
    [ "$(cat /tmp/e2e-boot-vol/from-machine 2>/dev/null)" = booted ] || fail "volume write not visible on the host"
    [ "$(stat -c %u /tmp/e2e-boot-vol/from-machine)" = 0 ] || fail "root inside did not write as root on the host (idmap)"
    $NSPAWN exec "$name" -- /usr/bin/systemctl is-active nspawn-volumes.service </dev/null | tr -d '\r' | grep -qx active || fail "nspawn-volumes.service not active inside $name"
  fi
  step "network through the nspawn bridge"
  if [ "$networkd_was" != active ]; then
    systemctl is-active systemd-networkd >/dev/null && fail "systemd-networkd got started on the host; the bridge must not need it"
  fi
  ip -br addr show nspawn0 | grep -q "10.99.0.1/24" || fail "bridge nspawn0 missing or without its address"
  nft list map ip nspawn ports >/dev/null 2>&1 || fail "nftables table of the bridge missing"
  if firewall-cmd --state >/dev/null 2>&1; then
    # NetworkManager takes a new bridge over for a moment and firewalld follows it before
    # settling on the binding nspawn made; the binding itself is not in question.
    retry 5 bash -c "[ \"\$(firewall-cmd --get-zone-of-interface=nspawn0)\" = trusted ]" || fail "nspawn0 is not in the trusted zone of firewalld"
  fi
  addr=$($NSPAWN network ls | awk -v n="$name" '$1 == n {print $2}')
  echo "$name has address $addr"
  echo "$addr" | grep -q "^10\.99\.0\." || fail "no bridge address recorded for $name"
  retry 15 bash -c "$NSPAWN exec $name -- /bin/sh -c 'ip -4 -o addr show host0 | grep -q $addr/24 && curl -sf -m 5 -o /dev/null https://download.opensuse.org/ && echo NET-OK' </dev/null | tr -d '\r' | grep -q NET-OK" \
    || fail "no network inside $name through the bridge"
  step "stop"
  $NSPAWN stop "$name" || fail "stop ($backend)"
  retry 15 bash -c "! $NSPAWN machines ls | grep -q '^ *$name '" || fail "$name still running after stop"
  if [ "$backend" = flat ]; then
    step "legacy veth network (systemd-networkd on the host)"
    $NSPAWN start "$name" --network veth || fail "start --network veth"
    systemctl is-active systemd-networkd >/dev/null || fail "start --network veth did not activate systemd-networkd"
    if firewall-cmd --state >/dev/null 2>&1; then
      [ "$(firewall-cmd --get-zone-of-interface="ve-$name")" = trusted ] || fail "ve-$name is not in the trusted zone of firewalld"
    fi
    retry 15 bash -c "$NSPAWN exec $name -- /bin/sh -c 'curl -sf -m 5 -o /dev/null https://download.opensuse.org/ && echo NET-OK' </dev/null | tr -d '\r' | grep -q NET-OK" \
      || fail "no network inside $name over the veth"
    $NSPAWN stop "$name" || fail "stop veth machine"
    if firewall-cmd --state >/dev/null 2>&1; then
      firewall-cmd --zone=trusted --list-interfaces | grep -qw "ve-$name" && fail "ve-$name still bound in firewalld after stop"
    fi
    $NSPAWN start "$name" --network bridge >/dev/null && $NSPAWN stop "$name" >/dev/null || fail "back to the bridge network"
    if [ "$networkd_was" != active ]; then
      systemctl stop systemd-networkd.service systemd-networkd.socket systemd-networkd-varlink.socket systemd-networkd-resolve-hook.socket >/dev/null 2>&1 || true
    fi
  fi
  step "stop right after start"
  $NSPAWN start "$name" && $NSPAWN stop "$name" || fail "stop right after start ($backend)"
  step "images rm"
  $NSPAWN images rm "$name" || fail "images rm ($backend)"
  $NSPAWN images ls > /tmp/e2e-img.txt; grep -q "^ *$name " /tmp/e2e-img.txt && fail "$name still listed after rm"
  [ -e "/var/lib/machines/$name" ] && fail "/var/lib/machines/$name still exists"
  ls /etc/systemd/system/ | grep -q "e2e" && fail "unit files left behind for $name"
done

step "layer sharing between two images"
$NSPAWN pull "$IMAGE" --name e2e-a --backend overlay --force >/dev/null || fail "pull e2e-a"
$NSPAWN pull "$IMAGE" --name e2e-b --backend overlay --force | tee /tmp/e2e-p2.txt || fail "pull e2e-b"
grep -q "already present" /tmp/e2e-p2.txt || fail "second pull downloaded the layer again"

step "create: arguments are checked before anything is made"
$NSPAWN create e2e-a e2e-bad -e X=1 >/dev/null 2>&1 && fail "create accepted -e for a booted image"
$NSPAWN images ls | grep -q "^ *e2e-bad " && fail "a refused create left a machine behind"
$NSPAWN create e2e-a e2e-bad -v "bad volume" >/dev/null 2>&1 && fail "create accepted a bad volume"
[ -e /etc/systemd/nspawn/e2e-bad.nspawn ] && fail "a refused create left settings behind"

step "create: another machine from a local image, without the registry"
env NSPAWN_REGISTRY=127.0.0.1:9 $NSPAWN create e2e-a e2e-c || fail "create from a local image"
$NSPAWN images ls | grep "^ *e2e-c " | grep -q "create" || fail "created machine not listed with origin create"
$NSPAWN start e2e-c || fail "start created machine"
retry 10 $NSPAWN exec e2e-c -- /usr/bin/test -f /etc/os-release </dev/null || fail "exec in created machine"
$NSPAWN network ls | grep -q "^ *e2e-c " || fail "created machine not on the bridge"
$NSPAWN stop e2e-c || fail "stop created machine"
$NSPAWN images rm e2e-c | tee /tmp/e2e-rmc.txt || fail "rm created machine"
grep -q "freed" /tmp/e2e-rmc.txt && fail "removing the created machine freed a layer still used by e2e-a and e2e-b"

step "two machines on the bridge: names and published ports"
$NSPAWN start e2e-a || fail "start e2e-a"
$NSPAWN start e2e-b -p 18080:80 || fail "start e2e-b with a published port"
$NSPAWN exec e2e-b -- /usr/bin/systemctl is-system-running --wait </dev/null >/dev/null 2>&1 || true
# An echo service on port 80 inside e2e-b, from socket activation: no extra packages needed.
$NSPAWN exec e2e-b -- /bin/sh -c 'printf "[Socket]\nListenStream=80\nAccept=yes\n" > /etc/systemd/system/echo.socket; printf "[Service]\nExecStart=/usr/bin/cat\nStandardInput=socket\n" > /etc/systemd/system/echo@.service; systemctl daemon-reload; systemctl start echo.socket && echo ECHO-UP' </dev/null | tr -d '\r' | grep -q ECHO-UP || fail "echo service inside e2e-b"
b_addr=$($NSPAWN network ls | awk '$1 == "e2e-b" {print $2}')
echo "e2e-b has address $b_addr"
$NSPAWN network ls | grep "e2e-b" | grep -q "18080->80/tcp" || fail "published port not listed by network ls"
$NSPAWN ps | grep "^ *e2e-b " | grep -q "18080->80/tcp" || fail "published port not shown by ps"
echo_test() { timeout 5 bash -c "exec 3<>/dev/tcp/$1/$2 || exit 1; echo $3 >&3; read -t 3 l <&3; [ \"\$l\" = $3 ]" 2>/dev/null; }
retry 5 echo_test "$b_addr" 80 direct || fail "e2e-b not reachable on its bridge address $b_addr"
echo_test 127.0.0.1 18080 loopback || fail "published port not reachable on 127.0.0.1"
host_ip=$(ip -4 route get 1.1.1.1 | awk '{for (i = 1; i <= NF; i++) if ($i == "src") print $(i + 1); exit}')
echo_test "$host_ip" 18080 hostaddr || fail "published port not reachable on the host address $host_ip"
$NSPAWN exec e2e-a -- /bin/sh -c "getent hosts e2e-b" </dev/null | tr -d '\r' | grep -q "$b_addr" || fail "e2e-a does not resolve e2e-b"
$NSPAWN exec e2e-a -- /bin/sh -c "getent hosts host.nspawn.internal" </dev/null | tr -d '\r' | grep -q "10.99.0.1" || fail "host.nspawn.internal not resolvable"
$NSPAWN exec e2e-a -- /bin/bash -c 'exec 3<>/dev/tcp/e2e-b/80 && echo a-to-b >&3 && read -t 3 l <&3 && echo "reply:$l"' </dev/null | tr -d '\r' | grep -q "reply:a-to-b" || fail "e2e-a cannot reach e2e-b by name"
$NSPAWN stop e2e-b || fail "stop e2e-b"
$NSPAWN stop e2e-a || fail "stop e2e-a"
nft list map ip nspawn ports | grep -q 18080 && fail "published port still mapped after stop"

layers_before=$(ls /var/lib/nspawn/layers | wc -l)
$NSPAWN images rm e2e-a >/dev/null || fail "rm e2e-a"
[ "$(ls /var/lib/nspawn/layers | wc -l)" = "$layers_before" ] || fail "layer removed while still referenced"
$NSPAWN images rm e2e-b | tee /tmp/e2e-rm.txt || fail "rm e2e-b"
grep -q "freed 1 unused layer" /tmp/e2e-rm.txt || fail "unused layer not garbage collected"

step "build, push and pull round trip (docker-like flow)"
if command -v mkosi >/dev/null 2>&1; then
  ctx=$(dirname "$0")/build-context
  [ -f "$ctx/mkosi.conf" ] || ctx=/home/edu4rdshl/nspawn-build/build-context
  built=e2e-built
  $NSPAWN build -t e2e/built:1 --name $built --force "$ctx" || fail "build"
  $NSPAWN images ls | tee /tmp/e2e-img.txt
  grep "^ *$built " /tmp/e2e-img.txt | grep -qw "build" || fail "built image not listed with origin build"
  $NSPAWN start $built || fail "start built image"
  out=$($NSPAWN exec $built -- /usr/bin/systemctl is-system-running --wait </dev/null | tr -d '\r' || true)
  echo "built image is-system-running: $out"
  echo "$out" | grep -qE "running|degraded|starting" || fail "built image did not boot"
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
retry 10 bash -c "$NSPAWN exec $app -- ip -4 -o addr show host0 </dev/null | tr -d '\r' | grep -q 10.99.0" || fail "no bridge address after machinectl start of a fresh app"
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
echo "$out" | grep -q "inside:$app" || fail "exec via namespaces did not run inside the machine"
[ "$($NSPAWN exec $app -- id -u </dev/null | tr -d '\r')" = "0" ] || fail "exec does not run as the machine's root"
[ "$($NSPAWN exec $app --user 65534 -- id -u </dev/null | tr -d '\r')" = "65534" ] || fail "exec --user ignored"
$NSPAWN exec $app -- /bin/sh -c 'exit 7' </dev/null; [ $? -eq 7 ] || fail "exec did not propagate the exit code"
[ "$(printf 'a\nb' | $NSPAWN exec $app -- cat)" = "$(printf 'a\nb')" ] || fail "piped stdin/stdout through exec is not byte exact"
step "app on the bridge: address, DNS, internet and published port (no networkd anywhere)"
if [ "$networkd_was" != active ]; then
  systemctl is-active systemd-networkd >/dev/null && fail "systemd-networkd is running during the app section"
fi
app_addr=$($NSPAWN network ls | awk -v n="$app" '$1 == n {print $2}')
echo "$app has address $app_addr"
echo "$app_addr" | grep -q "^10\.99\.0\." || fail "no bridge address for the app"
$NSPAWN exec $app -- ip -4 -o addr show host0 </dev/null | tr -d '\r' | grep -q "$app_addr/24" || fail "host0 not configured inside the app"
$NSPAWN exec $app -- cat /etc/hosts </dev/null | tr -d '\r' | grep -q "host.nspawn.internal" || fail "generated /etc/hosts missing in the app"
$NSPAWN exec $app -- nslookup download.opensuse.org </dev/null >/dev/null 2>&1 || fail "DNS does not work inside the app"
$NSPAWN exec $app -- wget -qO- -T 5 http://detectportal.firefox.com/success.txt </dev/null | tr -d '\r' | grep -q success || fail "no internet from the app"
curl -sf -m 5 http://127.0.0.1:18081/ | grep -q app-web || fail "published app port not reachable on 127.0.0.1"
$NSPAWN ps | grep "^ *$app " | grep -q "18081->80/tcp" || fail "app port not shown by ps"
$NSPAWN stop $app || fail "stop busybox"
retry 15 bash -c "! $NSPAWN machines ls | grep -q '^ *$app '" || fail "busybox still running after stop"
[ -e /run/netns/nspawn-$app ] && fail "network namespace left behind for $app"
systemctl is-failed systemd-nspawn@$app.service >/dev/null 2>&1 && fail "unit left in failed state after stop"

step "stop: a program that ignores its stop signal is killed after --timeout"
$NSPAWN start $app -- /bin/sh -c 'trap "" TERM; exec /bin/sleep 300' || fail "start stubborn app"
t0=$(date +%s)
$NSPAWN stop $app -t 2 || fail "stop of a stubborn app"
[ $(( $(date +%s) - t0 )) -lt 20 ] || fail "stop of a stubborn app took too long"
retry 5 bash -c "! $NSPAWN ps | grep -q '^ *$app '" || fail "stubborn app still running"

step "the remembered command, the unit hooks and an app that exits on its own"
$NSPAWN start $app -p 18081:80 -- /bin/sh -c 'mkdir -p /www; echo app-web > /www/index.html; exec /bin/httpd -f -p 80 -h /www' || fail "start httpd app"
$NSPAWN stop $app >/dev/null || fail "stop httpd app"
$NSPAWN start $app || fail "start without a command"
retry 5 bash -c "curl -sf -m 2 http://127.0.0.1:18081/ | grep -q app-web" || fail "the remembered command did not run"
$NSPAWN stop $app >/dev/null || fail "stop remembered app"
machinectl start $app || fail "machinectl start of an app (the hooks must prepare its network)"
retry 10 bash -c "curl -sf -m 2 http://127.0.0.1:18081/ | grep -q app-web" || fail "no network or ports after machinectl start"
$NSPAWN stop $app || fail "stop after machinectl start"
t0=$(date +%s)
$NSPAWN start $app -- /bin/true || fail "start of a program that returns at once"
[ $(( $(date +%s) - t0 )) -lt 15 ] || fail "start waited for a program that had already returned"
$NSPAWN start $app -- /bin/sh -c 'exit 3' >/dev/null 2>&1 || true
retry 10 bash -c "! $NSPAWN ps | grep -q '^ *$app '" || fail "failed app still listed"
out=$($NSPAWN stop $app 2>&1); echo "$out" | grep -q "was not running" || fail "stop after a failed program: $out"
systemctl is-failed systemd-nspawn@$app.service >/dev/null 2>&1 && fail "unit left failed after stop of a program that exited 3"
# A program that fails at once leaves the unit on its way down with the release hook
# running; a start issued right then must not have its namespace pulled away.
$NSPAWN start $app -- /bin/sh -c 'exit 3' >/dev/null 2>&1 || true
# "already running" is the right answer while the failed program is still alive (the
# service answers within milliseconds); anything else at that moment is a bug.
for i in 1 2 3 4 5 6 7 8 9 10; do
  out=$($NSPAWN start $app -- /bin/sleep 300 2>&1) && break
  echo "$out" | grep -q "already running" || { echo "$out"; fail "start right after a program that failed at once"; break; }
  sleep 0.2
done
retry 10 bash -c "$NSPAWN exec $app -- ip -4 -o addr show host0 </dev/null | tr -d '\r' | grep -q 10.99.0" || fail "no bridge address after a start that followed a failed program"
$NSPAWN stop $app >/dev/null || fail "stop after the quick restart"
$NSPAWN start $app -- /bin/sh -c 'sleep 1' || fail "start short-lived app"
retry 10 bash -c "! $NSPAWN ps | grep -q '^ *$app '" || fail "short-lived app still listed"
sleep 1
[ -e /run/netns/nspawn-$app ] && fail "namespace left behind by an app that exited on its own"
nft list map ip nspawn ports | grep -q 18081 && fail "ports of an exited app still mapped"
out=$($NSPAWN stop $app 2>&1); echo "$out" | grep -q "was not running" || fail "stop of a stopped machine is not a no-op: $out"
python3 -c 'import socket,time; s=socket.socket(); s.bind(("0.0.0.0",18099)); s.listen(); time.sleep(120)' &
listener_pid=$!
sleep 1
out=$($NSPAWN start $app -p 18099:80 2>&1); echo "$out" | grep -q "in use by a service on the host" || fail "publishing a port a host service listens on was not refused: $out"
$NSPAWN ps -a | grep "^ *$app " | grep -q "18099->80" && fail "a refused port was remembered"
kill "$listener_pid" 2>/dev/null; listener_pid=

step "entrypoint, environment and volumes, docker style"
rm -rf /tmp/e2e-bind /var/lib/nspawn/volumes/e2evol; mkdir -p /tmp/e2e-bind; echo from-host > /tmp/e2e-bind/hello
export E2E_HOST_VAR=fromhost
# The unit's journal keeps the lines of earlier runs, so every line carries the nonce.
$NSPAWN start $app --entrypoint /bin/sh -e GREETING=hola -e E2E_HOST_VAR -v /tmp/e2e-bind:/bind -v e2evol:/vol -v /etc/os-release:/host-os-release:ro -p none -- -c "echo \"greeting=\$GREETING hostvar=\$E2E_HOST_VAR nonce=$nonce\"; cat /bind/hello; echo from-app > /vol/written; { echo blocked > /host-os-release; } 2>/dev/null && echo RO-FAIL-$nonce || echo RO-OK-$nonce; exec /bin/sleep 300" || fail "start with entrypoint, env and volumes"
retry 10 bash -c "$NSPAWN logs $app | grep -q RO-[A-Z]*-$nonce" || fail "app did not run"
$NSPAWN logs $app > /tmp/e2e-logs.txt
grep -q "greeting=hola hostvar=fromhost nonce=$nonce" /tmp/e2e-logs.txt || fail "-e variables not seen by the program"
grep -q "^from-host" /tmp/e2e-logs.txt || fail "bind mount not visible inside"
grep -q "RO-OK-$nonce" /tmp/e2e-logs.txt || fail "read-only volume was writable"
[ "$(cat /var/lib/nspawn/volumes/e2evol/written 2>/dev/null)" = from-app ] || fail "named volume not written on the host"
$NSPAWN ps | grep "^ *$app " | grep -q "/bin/sh -c" || fail "ps does not show the entrypoint plus arguments"
[ "$($NSPAWN exec $app -- /bin/sh -c 'echo $GREETING' </dev/null | tr -d '\r')" = hola ] || fail "exec does not see -e variables"
# The entrypoint (/bin/sh) is remembered, so the arguments are its.
$NSPAWN stop $app >/dev/null; $NSPAWN start $app -e PATH=/opt/none:/usr/bin:/bin -- -c 'exec /bin/sleep 300' >/dev/null || fail "start with a PATH override"
retry 5 bash -c "$NSPAWN ps | grep -q '^ *$app '" || fail "app with a PATH override is not running"
[ "$($NSPAWN exec $app -- /bin/sh -c 'echo $PATH' </dev/null | tr -d '\r')" = "/opt/none:/usr/bin:/bin" ] || fail "exec does not apply a -e override of an image variable"
$NSPAWN stop $app || fail "stop app with volumes"
$NSPAWN start $app --image-command -e none -v none || fail "start with the image's own command"
$NSPAWN ps | grep "^ *$app " | grep -q " sh " || fail "--image-command did not restore the image's cmd"
grep -q "Bind=" /etc/systemd/nspawn/$app.nspawn && fail "-v none left volumes in the settings"
$NSPAWN stop $app -t 2 || fail "stop app running its own cmd"
$NSPAWN images rm $app || fail "rm busybox"
[ -e /etc/systemd/nspawn/$app.nspawn ] && fail "settings file left behind for $app"
ls /etc/systemd/system/ | grep -q "$app" && fail "unit files left behind for $app"

step "pipelines: a reader that closes early must not make nspawn fail"
$NSPAWN hub ls | head -c 1 >/dev/null; rc=${PIPESTATUS[0]}
[ "$rc" = 0 ] || [ "$rc" = 141 ] || fail "nspawn exited with $rc when the pipe closed"

step "D-Bus: org.nspawn as other clients see it"
if command -v busctl >/dev/null 2>&1; then
  grep -q "wrote /etc/systemd/system/nspawn.service" /tmp/e2e-install.txt || fail "install did not write the unit"
  B="busctl --system --timeout=120"
  M="org.nspawn /org/nspawn org.nspawn.Manager"
  $B introspect $M > /tmp/e2e-introspect.txt || fail "org.nspawn not reachable; the bus should have started it"
  for m in ListImages GetImage PullImage CreateMachine PushImage BuildImage RemoveImages SearchImages ListRepositories ListTags ListMachines StartMachine StopMachine Exec Logs ListNetwork NetworkUp Login Logout; do
    grep -q "^\.$m  *method" /tmp/e2e-introspect.txt || fail "method $m missing from org.nspawn.Manager"
  done
  for sig in JobOutput JobRemoved ImageAdded ImageRemoved MachineStarted MachineStopped; do
    grep -q "^\.$sig  *signal" /tmp/e2e-introspect.txt || fail "signal $sig missing from org.nspawn.Manager"
  done
  systemctl is-active nspawn.service >/dev/null || fail "the bus did not start nspawn.service"
  [ "$($B get-property $M Version)" = "s \"$($NSPAWN --version | awk '{print $2}')\"" ] || fail "Version property"
  job=$($B call $M PullImage 'sa{sv}' "$IMAGE" 3 name s e2e-dbus backend s overlay force b true | awk '{print $2}' | tr -d '"')
  echo "pull job: $job"
  echo "$job" | grep -q "^/org/nspawn/job/" || fail "PullImage did not return a job path"
  retry 90 bash -c "[ \"\$($B get-property org.nspawn $job org.nspawn.Job State)\" != 's \"running\"' ]" || fail "the pull job did not end"
  [ "$($B get-property org.nspawn $job org.nspawn.Job State)" = 's "done"' ] || { $B get-property org.nspawn $job org.nspawn.Job Error; fail "the pull job failed"; }
  $B get-property org.nspawn $job org.nspawn.Job Output | grep -q "assembling as overlay" || fail "the job kept no output"
  $B get-property org.nspawn $job org.nspawn.Job Result | grep -q '"name" s "e2e-dbus"' || fail "the job kept no result"
  $B get-property $M Jobs | grep -q "$job" || fail "Jobs property misses the job"
  $B call $M ListImages | grep -q '"name" s "e2e-dbus"' || fail "ListImages misses the pulled image"
  $B call $M GetImage s e2e-dbus | grep -q '"mode" s "boot"' || fail "GetImage"
  [ "$($B call $M StartMachine 'sa{sv}' e2e-dbus 0)" = 's "started"' ] || fail "StartMachine"
  $B call $M ListMachines b false > /tmp/e2e-lm.txt
  grep -q '"name" s "e2e-dbus"' /tmp/e2e-lm.txt || fail "ListMachines misses the machine"
  grep -q '"machine_path" s "/org/freedesktop/machine1/machine/e2e_2ddbus"' /tmp/e2e-lm.txt || fail "ListMachines has no machined path"
  grep -q '"state" s "running"' /tmp/e2e-lm.txt || fail "ListMachines: not running"
  $B call $M ListNetwork | grep -q '"name" s "e2e-dbus"' || fail "ListNetwork misses the machine"
  # Every exec of this run went through Exec; each left a process object behind.
  out=$($NSPAWN exec e2e-dbus -- /bin/sh -c "echo via-bus-$nonce; exit 7" </dev/null); code=$?
  [ "$code" = 7 ] || fail "exec did not propagate the exit code (got $code)"
  echo "$out" | grep -q "via-bus-$nonce" || fail "exec lost the output: $out"
  proc=$($B get-property $M Processes | awk '{print $NF}' | tr -d '"')
  echo "$proc" | grep -q "^/org/nspawn/process/" || fail "no process object after Exec"
  [ "$($B get-property org.nspawn $proc org.nspawn.Process State)" = 's "exited"' ] || fail "the process object did not see the exit"
  [ "$($B get-property org.nspawn $proc org.nspawn.Process ExitStatus)" = "i 7" ] || fail "the process object kept the wrong exit status"
  $B get-property org.nspawn $proc org.nspawn.Process Argv | grep -q "via-bus-$nonce" || fail "the process object has the wrong argv"
  [ "$($B call $M StopMachine 'sa{sv}' e2e-dbus 0)" = 's "stopped"' ] || fail "StopMachine"
  $B call $M ListMachines b true | grep -q '"state" s "stopped"' || fail "ListMachines with all misses the stopped machine"
  $B call $M RemoveImages as 1 e2e-dbus | grep -q "removed e2e-dbus" || fail "RemoveImages"
  $B call $M Login 'sssa{sv}' "$NSPAWN_REGISTRY" tester s3cret 0 > /dev/null || fail "Login over the bus"
  python3 -c "import json; d = json.load(open('/etc/nspawn/auth.json')); assert '$NSPAWN_REGISTRY' in d['auths']" || fail "credentials from the bus not stored"
  [ "$($B call $M Logout s "$NSPAWN_REGISTRY")" = "b true" ] || fail "Logout over the bus"
  out=$($B call $M StartMachine 'sa{sv}' e2e-dbus 1 bogus s x 2>&1) && fail "an unknown option was accepted"
  echo "$out" | grep -q "unknown option" || fail "unknown option not named: $out"
  out=$($B call $M GetImage s e2e-nonexistent 2>&1) && fail "GetImage of a missing image succeeded"
  echo "$out" | grep -q "no image named" || fail "missing image not explained: $out"
else
  echo "busctl not installed: skipping the D-Bus section"
fi

step "error handling (these commands must fail with a useful message)"
out=$($NSPAWN pull "$NSPAWN_REGISTRY/does-not-exist:1" --name e2e-x 2>&1); rc=$?
echo "$out"
[ $rc -ne 0 ] || fail "pull of a missing image succeeded"
echo "$out" | grep -qi "manifest" || fail "missing image error does not mention the manifest"
out=$($NSPAWN start e2e-nonexistent 2>&1); rc=$?
echo "$out"
[ $rc -ne 0 ] || fail "start of an unknown image succeeded"
echo "$out" | grep -q "no image named" || fail "start of an unknown image gave no hint"
out=$($NSPAWN stop e2e-nonexistent 2>&1); [ $? -ne 0 ] && echo "$out" | grep -q "not running" || fail "stop of unknown machine"

echo
if [ "$failures" = 0 ]; then echo "ALL OK"; else echo "$failures FAILURE(S)"; exit 1; fi
