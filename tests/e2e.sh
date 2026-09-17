#!/bin/bash
# End-to-end test of the nspawn binary against a real registry and systemd-machined.
# Run as root on a host with systemd-nspawn: NSPAWN=./nspawn NSPAWN_REGISTRY=hub:8443 ./e2e.sh
set -uo pipefail
NSPAWN=${NSPAWN:-./nspawn}
export NSPAWN_REGISTRY=${NSPAWN_REGISTRY:-hub.nspawn.test:8443}
export NSPAWN_CA_CERT=${NSPAWN_CA_CERT:-/etc/zot/ca.crt}
IMAGE=${IMAGE:-fedora:44}
failures=0
fail() { echo "FAIL: $*"; failures=$((failures + 1)); }
step() { echo; echo "### $*"; }
retry() { local n=$1; shift; local i; for i in $(seq 1 "$n"); do "$@" && return 0; sleep 2; done; return 1; }

step "hub ls"
$NSPAWN hub ls | tee /tmp/e2e-hub.txt || fail "hub ls exited non-zero"
grep -q "${IMAGE%%:*}" /tmp/e2e-hub.txt || fail "hub ls does not list ${IMAGE%%:*}"
step "hub tags"
$NSPAWN hub tags "${IMAGE%%:*}" > /tmp/e2e-tags.txt || fail "hub tags"
grep -qx "${IMAGE##*:}" /tmp/e2e-tags.txt || fail "tag ${IMAGE##*:} missing"

for backend in overlay flat; do
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
  $NSPAWN start "$name" || fail "start ($backend)"
  if [ "$backend" = overlay ]; then
    findmnt -n -o FSTYPE "/var/lib/machines/$name" | grep -q overlay || fail "root of $name is not an overlay"
  fi
  step "machines ls"
  $NSPAWN machines ls | tee /tmp/e2e-m.txt
  grep -q "^ *$name " /tmp/e2e-m.txt || fail "$name not running"
  step "exec"
  out=$(retry 10 $NSPAWN exec "$name" -- /usr/bin/systemctl is-system-running --wait </dev/null | tr -d '\r')
  echo "is-system-running: $out"
  echo "$out" | grep -qE "running|degraded|starting" || fail "exec did not reach systemd inside $name"
  $NSPAWN exec "$name" -- /usr/bin/cat /etc/os-release </dev/null | tr -d '\r' | grep -q PRETTY_NAME || fail "exec cat os-release"
  step "network through the veth"
  systemctl is-active systemd-networkd >/dev/null || fail "start did not activate systemd-networkd on the host"
  if firewall-cmd --state >/dev/null 2>&1; then
    [ "$(firewall-cmd --get-zone-of-interface="ve-$name")" = trusted ] || fail "ve-$name is not in the trusted zone of firewalld"
  fi
  retry 15 bash -c "$NSPAWN exec $name -- /bin/sh -c 'curl -sf -m 5 -o /dev/null https://download.opensuse.org/ && echo NET-OK' </dev/null | tr -d '\r' | grep -q NET-OK" \
    || fail "no network inside $name"
  step "stop"
  $NSPAWN stop "$name" || fail "stop ($backend)"
  retry 15 bash -c "! $NSPAWN machines ls | grep -q '^ *$name '" || fail "$name still running after stop"
  if firewall-cmd --state >/dev/null 2>&1; then
    firewall-cmd --zone=trusted --list-interfaces | grep -qw "ve-$name" && fail "ve-$name still bound in firewalld after stop"
  fi
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
  out=$(retry 10 $NSPAWN exec $built -- /usr/bin/systemctl is-system-running --wait </dev/null | tr -d '\r')
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
grep -q "ProcessTwo=yes" /etc/systemd/nspawn/$app.nspawn || fail "settings file does not use a stub init"
$NSPAWN start $app -- /bin/sh -c 'echo hello-from-app; echo to-stderr >&2; exec /bin/sleep 300' || fail "start busybox with a command override"
grep -q "Parameters=/bin/sh -c" /etc/systemd/nspawn/$app.nspawn || fail "command override not written"
retry 10 bash -c "$NSPAWN logs $app > /tmp/e2e-logs.txt; grep -q hello-from-app /tmp/e2e-logs.txt" || fail "logs do not show the app's stdout"
grep -q to-stderr /tmp/e2e-logs.txt || fail "logs do not show the app's stderr"
grep -q "Started systemd-nspawn" /tmp/e2e-logs.txt && fail "logs include systemd's unit messages without --all"
$NSPAWN logs $app --all > /tmp/e2e-logs.txt; grep -q "Started systemd-nspawn" /tmp/e2e-logs.txt || fail "logs --all misses the unit messages"
$NSPAWN machines ls | tee /tmp/e2e-m.txt
grep -q "^ *$app " /tmp/e2e-m.txt || fail "busybox machine not running"
out=$($NSPAWN exec $app -- /bin/sh -c 'echo inside:$(uname -n); cat /etc/os-release | head -1' </dev/null | tr -d '\r')
echo "$out"
echo "$out" | grep -q "inside:$app" || fail "exec via namespaces did not run inside the machine"
[ "$($NSPAWN exec $app -- id -u </dev/null | tr -d '\r')" = "0" ] || fail "exec does not run as the machine's root"
[ "$($NSPAWN exec $app --user 65534 -- id -u </dev/null | tr -d '\r')" = "65534" ] || fail "exec --user ignored"
$NSPAWN exec $app -- /bin/false </dev/null; [ $? -eq 1 ] || fail "exec did not propagate the exit code"
$NSPAWN stop $app || fail "stop busybox"
retry 15 bash -c "! $NSPAWN machines ls | grep -q '^ *$app '" || fail "busybox still running after stop"
$NSPAWN images rm $app || fail "rm busybox"
[ -e /etc/systemd/nspawn/$app.nspawn ] && fail "settings file left behind for $app"
grep -q "(boot image)" /tmp/e2e-hub.txt 2>/dev/null || true

step "pipelines: a reader that closes early must not make nspawn fail"
$NSPAWN hub ls | head -c 1 >/dev/null; rc=${PIPESTATUS[0]}
[ "$rc" = 0 ] || [ "$rc" = 141 ] || fail "nspawn exited with $rc when the pipe closed"

step "error handling (these commands must fail with a useful message)"
out=$($NSPAWN pull "$NSPAWN_REGISTRY/does-not-exist:1" --name e2e-x 2>&1); rc=$?
echo "$out"
[ $rc -ne 0 ] || fail "pull of a missing image succeeded"
echo "$out" | grep -qi "manifest" || fail "missing image error does not mention the manifest"
out=$($NSPAWN start e2e-nonexistent 2>&1); rc=$?
echo "$out"
[ $rc -ne 0 ] || fail "start of an unknown image succeeded"
echo "$out" | grep -q "journalctl" || fail "start of unknown image gave no hint"
out=$($NSPAWN stop e2e-nonexistent 2>&1); [ $? -ne 0 ] && echo "$out" | grep -q "not running" || fail "stop of unknown machine"

echo
if [ "$failures" = 0 ]; then echo "ALL OK"; else echo "$failures FAILURE(S)"; exit 1; fi
