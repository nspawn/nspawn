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
# Whether the host makes IPv6 sockets: a port given no IP is published there too.
host_ipv6=$(python3 -c 'import socket; socket.socket(socket.AF_INET6, socket.SOCK_DGRAM)' 2>/dev/null && echo yes || true)
# Runs a command on a pseudo terminal of its own (script(1) is not on every host).
pyrun() { python3 -c 'import os, pty, sys; sys.exit(os.waitstatus_to_exitcode(pty.spawn(sys.argv[1:])))' "$@"; }
# A machine's address on its network, as inspect has it.
addr_of() { $NSPAWN inspect "$1" | python3 -c "import json,sys; print(json.load(sys.stdin)[0].get('address') or '')"; }
# What inspect says of a machine that generate's script must give again: not what a new
# machine of the same flags gets anew, or what only runs have; the script pulls the tag
# again, which may have moved meanwhile.
gen_keys() {
  $NSPAWN inspect "$1" | python3 -c "
import json, sys
d = json.load(sys.stdin)[0]
for k in ['created', 'address', 'addresses', 'state', 'started', 'leader', 'os', 'health', 'health_failing_streak', 'health_log', 'exit_code', 'machine_path', 'origin', 'digest']:
    d.pop(k, None)
print(json.dumps(d, sort_keys=True, indent=1))"
}
# Unique across runs, since the journals keep what earlier runs wrote: the PID is not,
# as a VM that has just booted gives the script the same one every time.
nonce=$(date +%s)
# With SELinux enforcing, the service's domain must cover everything the suite makes it
# do: its denials from here on fail the run (checked at the end, by the time of each
# record, since the log may rotate meanwhile).
audit_dir=/var/log/audit
audit_start=$(date +%s)
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
# A binary of its own where a package is installed would overwrite the package's bus,
# polkit and unit files, and remove them at the end.
if [ "$packaged" = no ] && { rpm -q nspawn >/dev/null 2>&1 || dpkg-query -W -f='${Status}' nspawn 2>/dev/null | grep_q "install ok installed" || pacman -Q nspawn >/dev/null 2>&1; }; then
  echo "an nspawn package is installed here: run the suite with its binary (NSPAWN=/usr/bin/nspawn), or remove the package first"
  exit 1
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
  for m in e2e-overlay e2e-flat e2e-mstack e2e-a e2e-b e2e-c e2e-built e2e-roundtrip e2e-busybox e2e-relogs e2e-run e2e-dbus e2e-digest e2e-restart e2e-caps e2e-tz e2e-noshell e2e-quiet e2e-drain e2e-twin-a e2e-twin-b e2e-na-web e2e-na-cli e2e-nb-web e2e-nc-web e2e-nc-pub e2e-def-cli e2e-nab e2e-none e2e-boot2 e2e-pclash e2e-cpull e2e-mix-app e2e-mix-boot e2e-auto e2e-run-boot e2e-multi e2e-side e2e-side-net e2e-side-p e2e-side-boot e2e-mix-side e2e-cshort e2e-signed e2e-unsigned e2e-nv e2e-nvc e2e-phys-b e2e-phys-boot e2e-dep-web e2e-dep-db e2e-dep-job e2e-gen-web e2e-gen-db e2e-gen-side e2e-gen-own e2e-s6 e2e-s6u e2e-pid1 e2e-api-caddy e2e-api-web e2e-api-web2 e2e-api-boot e2e-capi-a-1 e2e-capi-b-1 e2e-au e2e-au-side e2e-au-137 e2e-cli e2e-cli-peer e2e-cli-boot e2e-cli-wait e2e-cli-wrm e2e-cli-wrs e2e-cli-img e2e-cli-keep e2e-ip e2e-ip2 e2e-cip-app-1 e2e-cdi-bad e2e-cdi-boot e2e-cdi-ml-1 e2e-cdi-perm e2e-bb e2e-case-settings e2e-case-layers e2e-case-multistage e2e-case-copy-rules e2e-case-mounts e2e-case-add e2e-case-expansion e2e-case-user-workdir e2e-case-run-fails e2e-build-sandbox e2e-build-two-a e2e-build-two-b e2e-build-cancel e2e-build-url e2e-cb-pulled e2e-share-a-1 e2e-share-b-1 e2e-names-m e2e-sc e2e-v6 e2e-v6b e2e-v6c e2e-v6d e2e-v6u e2e-v6low e2e-notify e2e-fwd e2e-cbind-w-1; do
    $NSPAWN stop "$m" --force >/dev/null 2>&1 || true
    $NSPAWN images rm "$m" >/dev/null 2>&1 || true
  done
  # The names pulling by reference gives, only once the run has claimed them.
  if [ -f "$claim" ]; then
    for m in "${fixed_names[@]}"; do
      $NSPAWN stop "$m" --force >/dev/null 2>&1 || true
      $NSPAWN images rm "$m" >/dev/null 2>&1 || true
    done
    # Machines of run --rm have names of their own.
    for m in $($NSPAWN images ls 2>/dev/null | awk '$1 ~ /^busybox-1\.37-/ {print $1}'); do
      $NSPAWN rm -f "$m" >/dev/null 2>&1 || true
    done
    $NSPAWN images rm busybox-1.37 >/dev/null 2>&1 || true
  fi
  # The interfaces the --interface step makes, back on the host by now.
  ip link del e2e-dummy0 >/dev/null 2>&1 || true
  rmmod mac80211_hwsim >/dev/null 2>&1 || true
  $NSPAWN network rm e2e-na e2e-nb e2e-nc e2e-nd e2e-ne e2e-nf e2e-gn e2e-api e2e-capi_default e2e-au-net >/dev/null 2>&1 || true
  $NSPAWN volume rm e2e-au-vol e2e-cli-vol >/dev/null 2>&1 || true
  $NSPAWN network rm e2e-cli-net e2e-cli-pnet e2e-ip-net e2e-ip-net2 e2e-cip_fixed e2e-v6net >/dev/null 2>&1 || true
  rm -rf /tmp/e2e-cli /tmp/e2e-cdi
  rm -f /etc/cdi/e2e-nspawn.yaml /etc/cdi/e2e-nspawn-bad.yaml /var/run/cdi/e2e-nspawn-gpu.json /usr/local/bin/e2e-cdi-hook
  $NSPAWN network rm e2e-cdi_default e2e-cbind_default >/dev/null 2>&1 || true
  $NSPAWN images rm e2e-mirror e2e-api-curl >/dev/null 2>&1 || true
  rm -rf /tmp/e2e-compose-api
  $NSPAWN secret rm e2e-pw e2e-pw2 e2e-gen-pw >/dev/null 2>&1 || true
  rm -f /var/lib/nspawn/secrets/e2e-pw.cred /var/lib/nspawn/secrets/e2e-pw.json /var/lib/nspawn/secrets/e2e-pw2.cred /var/lib/nspawn/secrets/e2e-pw2.json
  # The compose step's project, whatever it left.
  for m in $($NSPAWN ps -a 2>/dev/null | awk '$1 ~ /^e2e-compose-/ {print $1}'); do
    $NSPAWN rm -f "$m" >/dev/null 2>&1 || true
  done
  $NSPAWN network rm e2e-compose_default >/dev/null 2>&1 || true
  $NSPAWN volume rm e2e-compose_data >/dev/null 2>&1 || true
  rm -rf /tmp/e2e-compose
  # The project of the compose build check: its machines, the images it built, its
  # network.
  for m in $($NSPAWN ps -a 2>/dev/null | awk '$1 ~ /^e2e-cb-/ {print $1}'); do
    $NSPAWN rm -f "$m" >/dev/null 2>&1 || true
  done
  for m in $($NSPAWN images ls 2>/dev/null | awk '$1 ~ /^e2e-cb-/ {print $1}'); do
    $NSPAWN images rm "$m" >/dev/null 2>&1 || true
  done
  for n in $($NSPAWN network ls 2>/dev/null | awk '$1 ~ /^e2e-cb-.*_default$/ {print $1}'); do
    $NSPAWN network rm "$n" >/dev/null 2>&1 || true
  done
  rm -rf /tmp/e2e-compose-build /tmp/e2e-build-two
  # The signature step keeps a dated hub image under the name pulling it gives; one cut
  # short leaves it behind. Only one this run pulled: the host may keep others.
  if [ -s /tmp/e2e-dated-image ]; then
    m=$(cat /tmp/e2e-dated-image)
    $NSPAWN stop "$m" --force >/dev/null 2>&1 || true
    $NSPAWN images rm "$m" >/dev/null 2>&1 || true
    rm -f /tmp/e2e-dated-image
  fi
  $NSPAWN logout "$NSPAWN_REGISTRY" >/dev/null 2>&1 || true
  rm -rf /tmp/e2e-cp /tmp/e2e-cp-* /tmp/e2e-bind /tmp/e2e-boot-vol /var/lib/nspawn/volumes/e2evol /var/lib/nspawn/volumes/e2evol2 /var/lib/nspawn/volumes/e2evol-free /var/lib/nspawn/volumes/e2evol-home /var/lib/nspawn/volumes/e2evol-etc /var/lib/nspawn/volumes/e2evol-events /var/lib/nspawn/volumes/e2evol-gen /var/lib/nspawn/volumes/e2e-bootvol /var/lib/nspawn/volumes/.e2e-hidden
  # The review steps' machines, images, projects and files; the configuration and the
  # empty record they break things with for a moment go first.
  [ -f /tmp/e2e-rv/e2e.toml ] && cp /tmp/e2e-rv/e2e.toml /etc/nspawn/e2e.toml
  rm -f /var/lib/nspawn/images/e2e-rv3-broken.json
  for m in $($NSPAWN ps -a 2>/dev/null | awk '$1 ~ /^e2e-rv/ {print $1}'); do
    $NSPAWN rm -f "$m" >/dev/null 2>&1 || true
  done
  for m in $($NSPAWN images ls 2>/dev/null | awk '$1 ~ /^e2e-rv/ {print $1}'); do
    $NSPAWN images rm "$m" >/dev/null 2>&1 || true
  done
  for n in $($NSPAWN network ls 2>/dev/null | awk '$1 ~ /^e2e-rv/ {print $1}'); do
    $NSPAWN network rm "$n" >/dev/null 2>&1 || true
  done
  $NSPAWN volume rm e2e-rvr_data >/dev/null 2>&1 || true
  for v in e2e-rv3-deep e2e-rv3-vol e2e-rv3-dupvol e2e-rv3-sv e2e-rv3j_data; do
    $NSPAWN volume rm "$v" >/dev/null 2>&1 || true
  done
  $NSPAWN secret rm e2e-rv-pw e2e-rv3s_secret_pw >/dev/null 2>&1 || true
  rm -f /etc/cdi/e2e-rv3.yaml
  rm -rf /var/lib/nspawn/rollback/e2e-rv3-rb
  rm -rf /tmp/e2e-rv
  # The build cache the steps made, when the host had none before them.
  [ "${cache_before:-}" = 0 ] && { $NSPAWN builder prune -a -f >/dev/null 2>&1 || true; }
  kill "${listener_pid:-}" "${http_pid:-}" "${http3_pid:-}" 2>/dev/null || true
  if [ "$networkd_was" != active ]; then
    systemctl stop systemd-networkd.service systemd-networkd.socket systemd-networkd-varlink.socket systemd-networkd-resolve-hook.socket >/dev/null 2>&1 || true
  fi
}
cleanup_service() {
  systemctl stop nspawn.service >/dev/null 2>&1 || true
  # A machine the service wrote meanwhile names the run's configuration in its hooks,
  # which is gone after this: the service's own command line is given back.
  local f
  for f in $(grep -l -- "--config /etc/nspawn/e2e.toml" /etc/systemd/system/systemd-nspawn@*.service.d/*.conf 2>/dev/null); do
    sed -i 's# --config /etc/nspawn/e2e\.toml##g' "$f"
    echo "note: $f named the suite's configuration; taken out"
  done
  if [ "$packaged" = yes ]; then
    rm -f /etc/systemd/system/nspawn.service.d/50-e2e.conf /etc/nspawn/e2e.toml /etc/nspawn/e2e-cosign.pub
    rmdir /etc/systemd/system/nspawn.service.d 2>/dev/null || true
  else
    rm -f /etc/dbus-1/system.d/org.nspawn.conf /usr/share/dbus-1/system-services/org.nspawn.service /usr/share/polkit-1/actions/org.nspawn.policy /usr/lib/systemd/journald@nspawn.conf /etc/systemd/system/nspawn.service /etc/systemd/system/nspawn-api@.socket /etc/systemd/system/nspawn-api@.service /etc/systemd/system/nspawn-auto-update.service /etc/systemd/system/nspawn-auto-update.timer /etc/nspawn/e2e.toml /etc/nspawn/e2e-cosign.pub
  fi
  systemctl daemon-reload >/dev/null 2>&1 || true
}
cleanup() {
  cleanup_machines
  # The claim goes once none of the names it claims is left, or the next run would take
  # an image left here for the host's own.
  local left
  if left=$($NSPAWN images ls 2>/dev/null); then
    left=$(echo "$left" | awk 'NR > 1 {print $1}' | while read -r m; do
      if [[ " ${fixed_names[*]} " == *" $m "* || $m == busybox-1.37-* ]]; then echo "$m"; fi
    done)
    if [ -z "$left" ]; then
      rm -f "$claim"
    else
      echo "note: $(echo $left) could not be removed; $claim stays"
    fi
  fi
  cleanup_service
  # A step that hides iptables must not leave it hidden, however it ended; the polkit
  # rule and the forward table of other steps go the same way.
  if [ -n "${hidden_iptables:-}" ] && [ -e "${hidden_iptables}.e2e-hidden" ]; then
    mv "${hidden_iptables}.e2e-hidden" "$hidden_iptables"
  fi
  rm -f /etc/polkit-1/rules.d/50-nspawn-e2e.rules
  if [ "${forward_table_made:-}" = yes ]; then
    nft delete table ip filter >/dev/null 2>&1 || true
  fi
}
# The names an image pulled by its reference gets (busybox:1.36 is busybox-1.36) are
# names a host may give images of its own: a run claims them, and refuses to start when
# one is there that no run of the suite made. The claim goes once a run has cleaned up.
claim=/var/tmp/nspawn-e2e-claim
fixed_names=(busybox-1.37 busybox-1.36 "$(basename "$IMAGE" | tr : -)")
claim_names() {
  local listed m
  listed=$($NSPAWN images ls 2>/dev/null) || { echo "cannot list the images, to check the names the suite takes"; exit 1; }
  [ -f "$claim" ] && return
  for m in $(echo "$listed" | awk 'NR > 1 {print $1}'); do
    if [[ " ${fixed_names[*]} " == *" $m "* || $m == busybox-1.37-* ]]; then
      echo "this host has $m of its own, which the suite would replace: rename or remove it, or run the suite on another host"
      exit 1
    fi
  done
  : > "$claim"
}
# Set first: whatever the start leaves (the service's drop-in above all) goes on any exit.
trap cleanup EXIT
install_service
claim_names
cleanup_machines

# Docker Hub limits anonymous pulls by address, which the hosts that run the suite
# share: the images of Docker Hub the steps use are copied once into the test registry,
# at their Docker Hub path without library/, so that their local names stay the same
# (busybox:1.36 is busybox-1.36 either way), and taken from there. The step on app
# images pulls from Docker Hub itself.
mirror() {
  local src=$1 path=${1#docker.io/}
  path=${path#library/}
  local repo=${path%:*} tag=${path##*:}
  local here="curl -sf --cacert $NSPAWN_CA_CERT -u tester:s3cret -I -H Accept:application/vnd.oci.image.manifest.v1+json,application/vnd.docker.distribution.manifest.v2+json https://$NSPAWN_REGISTRY/v2/$repo/manifests/$tag"
  # Another host of the suite may be copying it at the same time: tried again once.
  for attempt in 1 2; do
    $here >/dev/null 2>&1 && break
    $NSPAWN pull "$src" --name e2e-mirror --force >/dev/null 2>&1 \
      && $NSPAWN push e2e-mirror --to "$NSPAWN_REGISTRY/$repo:$tag" >/dev/null 2>&1
    $NSPAWN images rm e2e-mirror >/dev/null 2>&1
    sleep 5
  done
  echo "$NSPAWN_REGISTRY/$repo:$tag"
}
echo s3cret | $NSPAWN login "$NSPAWN_REGISTRY" -u tester --password-stdin >/dev/null || { echo "cannot log in to the test registry"; exit 1; }
busybox_ref=$(mirror docker.io/library/busybox:latest)
busybox136_ref=$(mirror docker.io/library/busybox:1.36)
busybox137_ref=$(mirror docker.io/library/busybox:1.37)
memcached_ref=$(mirror docker.io/library/memcached:alpine)
caddy_image=$(mirror docker.io/lucaslorentz/caddy-docker-proxy:2.13-alpine)
# A curl of OCI media types: the test registry takes no others.
curl_image=$(mirror docker.io/alpine/curl:8.22.0)
alpine322_ref=$(mirror docker.io/library/alpine:3.22)
$NSPAWN logout "$NSPAWN_REGISTRY" >/dev/null 2>&1

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
systemd_major=$(systemctl --version | awk 'NR==1{print $2}' | tr -dc 0-9)
mstack_supported=no
if [ "$systemd_major" -ge 261 ] 2>/dev/null \
  && [ -e /usr/lib/systemd/system/systemd-nsresourced.socket ] \
  && [ -e /usr/lib/systemd/system/systemd-mountfsd.socket ]; then
  mstack_supported=yes
fi
step "the default backend is overlay, whatever the host; mstack only by name"
# As on a host that booted a moment ago: the module is not loaded (where it is a
# module at all), and the service has to load it itself.
rmmod overlay 2>/dev/null || true
$NSPAWN pull "$IMAGE" --name e2e-auto --force > /tmp/e2e-auto.txt 2>&1 || { cat /tmp/e2e-auto.txt; fail "pull without --backend"; }
$NSPAWN images ls | grep "^ *e2e-auto " | grep_q " overlay " || fail "auto did not choose overlay: $($NSPAWN images ls | grep e2e-auto)"
grep -q "experimental" /tmp/e2e-auto.txt && fail "a pull without mstack was told mstack is experimental"
$NSPAWN images rm e2e-auto >/dev/null || fail "rm e2e-auto"
# The 262 release itself (not its later point releases, once the fix lands) cannot boot
# a managed user namespace: systemd/systemd#43899. mstack is experimental, so that is
# the host's business, not nspawn's to check; the suite only skips what cannot work.
if [ "$mstack_supported" = yes ] && [ "$(systemctl --version | awk 'NR==1{print $2}')" = 262 ]; then
  echo "mstack: the systemd 262 release cannot boot managed user namespaces (systemd/systemd#43899); skipped on this host"
  mstack_supported=no
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
  if [ "$backend" = mstack ]; then
    grep -q "mstack backend is experimental" /tmp/e2e-pull.txt || fail "pull --backend mstack did not say it is experimental"
  fi
  # A booted machine's systemd tells systemd-nspawn through the socket that it is up.
  grep -q "NOTIFY_SOCKET" /etc/systemd/nspawn/$name.nspawn && fail "a booted machine lost systemd-nspawn's notify socket: $(grep NOTIFY_SOCKET /etc/systemd/nspawn/$name.nspawn)"
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
    # directory keeps its SELinux label, which the service may not write to, so
    # that one is only tried where SELinux does not enforce.
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
      # A machine that ends on its own is released by its hook, not by stop.
      $NSPAWN start "$name" --network veth >/dev/null || fail "start --network veth again"
      [ "$(firewall-cmd --get-zone-of-interface="ve-$name")" = trusted ] || fail "ve-$name is not in the trusted zone on the second start"
      machinectl poweroff "$name" >/dev/null 2>&1 || fail "machinectl poweroff of the veth machine"
      retry 20 bash -c "! $NSPAWN machines ls | grep_q '^ *$name '" || fail "$name still running after poweroff"
      retry 10 bash -c "! firewall-cmd --zone=trusted --list-interfaces | grep -qw ve-$name" || fail "ve-$name still bound in firewalld after the machine powered off on its own"
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
# A flag refused by create --force is refused before the old machine goes.
$NSPAWN start e2e-c -l keep=me >/dev/null && $NSPAWN stop e2e-c >/dev/null || fail "label e2e-c"
out=$($NSPAWN create e2e-a e2e-c --force --hostname 'bad host' 2>&1) && fail "create --force accepted a bad hostname"
$NSPAWN inspect e2e-c | python3 -c "import json,sys; d = json.load(sys.stdin)[0]; assert d['labels'].get('keep') == 'me', d" || fail "a refused create --force replaced the machine: $out"
# --network none is a kind of network, inherited like the bridge.
$NSPAWN start e2e-c --network none >/dev/null && $NSPAWN stop e2e-c >/dev/null || fail "e2e-c on no network"
$NSPAWN create e2e-c e2e-cshort >/dev/null || fail "create from a machine on no network"
$NSPAWN inspect e2e-cshort | python3 -c "import json,sys; d = json.load(sys.stdin)[0]; assert d['network'] == 'none', d" || fail "create did not inherit --network none"
$NSPAWN rm e2e-cshort >/dev/null || fail "rm e2e-cshort"
$NSPAWN start e2e-c --network bridge >/dev/null && $NSPAWN stop e2e-c >/dev/null || fail "e2e-c back on the bridge"
$NSPAWN images ls | grep "^ *e2e-c " | grep_q "create" || fail "created machine not listed with origin create"
# An image pulled under its own name is found by its short reference too, and not
# pulled again under that name.
own=$(basename "$IMAGE" | tr : -)
$NSPAWN pull "$IMAGE" --backend overlay --force >/dev/null || fail "pull $IMAGE under its own name"
out=$($NSPAWN create "$IMAGE" e2e-cshort 2>&1) || fail "create by the short reference of a local image: $out"
echo "$out" | grep_q "manifest" && fail "create by a short reference pulled the image again: $out"
$NSPAWN images ls | grep "^ *e2e-cshort " | grep_q "create" || fail "e2e-cshort not listed with origin create"
$NSPAWN rm e2e-cshort >/dev/null && $NSPAWN images rm "$own" >/dev/null || fail "rm e2e-cshort and $own"
$NSPAWN start e2e-c || fail "start created machine"
retry 10 $NSPAWN exec e2e-c -- /usr/bin/test -f /etc/os-release </dev/null || fail "exec in created machine"
$NSPAWN network inspect bridge | grep_q '"name": "e2e-c"' || fail "created machine not on the bridge"
$NSPAWN stop e2e-c || fail "stop created machine"
$NSPAWN images rm e2e-c | tee /tmp/e2e-rmc.txt || fail "rm created machine"
grep -q "freed" /tmp/e2e-rmc.txt && fail "removing the created machine freed a layer still used by e2e-a and e2e-b"

step "a port another machine took over is left alone when the first one is stopped again"
# A stopped machine remembers its ports; a second one may publish the same port. Stopping
# the first one again (or its unit failing to start) must not withdraw the second one's.
$NSPAWN start e2e-a -p 18085:80 >/dev/null || fail "start e2e-a with a port"
$NSPAWN stop e2e-a >/dev/null || fail "stop e2e-a"
$NSPAWN start e2e-b -p 18085:80 >/dev/null || fail "start e2e-b on the port e2e-a remembers"
nft list map ip nspawn ports | grep_q "tcp . 18085 : " || fail "the port of e2e-b is not mapped"
$NSPAWN stop e2e-a >/dev/null || fail "stop e2e-a again while stopped"
nft list map ip nspawn ports | grep_q "tcp . 18085 : " || fail "stopping e2e-a again withdrew the port of e2e-b"
out=$($NSPAWN start e2e-a 2>&1) && fail "e2e-a started on a port e2e-b holds"
echo "$out" | grep_q "already published by e2e-b" || fail "the conflict was not explained: $out"
nft list map ip nspawn ports | grep_q "tcp . 18085 : " || fail "the refused start of e2e-a withdrew the port of e2e-b"
$NSPAWN stop e2e-b >/dev/null || fail "stop e2e-b"
nft list map ip nspawn ports | grep_q "tcp . 18085 : " && fail "the port of e2e-b stayed mapped after its stop"
$NSPAWN start e2e-a -p none >/dev/null && $NSPAWN stop e2e-a >/dev/null || fail "forget the port of e2e-a"
$NSPAWN start e2e-b -p none >/dev/null && $NSPAWN stop e2e-b >/dev/null || fail "forget the port of e2e-b"

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

step "build, push and pull round trip"
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

step "app images without an init system"
app=e2e-busybox
$NSPAWN pull docker.io/library/busybox:latest --name $app --backend overlay --force > /tmp/e2e-app.txt 2>&1 || fail "pull busybox from Docker Hub"
cat /tmp/e2e-app.txt
grep -q "(app image)" /tmp/e2e-app.txt || fail "busybox not detected as an app image"
grep -q "Boot=no" /etc/systemd/nspawn/$app.nspawn || fail "settings file does not disable --boot"
# systemd-nspawn's notify socket is root's alone and of no use to an app: its program
# finds it empty, which sd_notify's clients take for none. By the reference $app was
# pulled from, which run takes from it: no other image keeps the layers the progress
# step downloads again.
out=$($NSPAWN run --rm docker.io/library/busybox:latest --name e2e-notify -- /bin/sh -c 'echo "notify=[${NOTIFY_SOCKET-unset}]"' 2>/dev/null | tr -d '\r')
[ "$out" = "notify=[]" ] || fail "an app's program found systemd-nspawn's notify socket: $out"
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
# What a removed machine of the same name wrote is not this one's, whatever --since says.
$NSPAWN create $app e2e-relogs -- /bin/sh -c "echo e2e-old-$nonce; sleep 1" >/dev/null || fail "create e2e-relogs"
$NSPAWN start e2e-relogs >/dev/null || fail "start e2e-relogs"
retry 10 bash -c "$NSPAWN logs e2e-relogs | grep_q e2e-old-$nonce" || fail "logs of the first e2e-relogs"
$NSPAWN rm -f e2e-relogs >/dev/null || fail "rm the first e2e-relogs"
sleep 1
$NSPAWN create $app e2e-relogs -- /bin/sh -c "echo e2e-new-$nonce; sleep 1" >/dev/null || fail "create e2e-relogs again"
$NSPAWN start e2e-relogs >/dev/null || fail "start e2e-relogs again"
retry 10 bash -c "$NSPAWN logs e2e-relogs | grep_q e2e-new-$nonce" || fail "logs of the second e2e-relogs"
$NSPAWN logs e2e-relogs | grep_q e2e-old-$nonce && fail "logs shows what a removed machine of the same name wrote"
$NSPAWN logs e2e-relogs --since -1h | grep_q e2e-old-$nonce && fail "logs --since reaches back past the machine's creation"
out=$($NSPAWN logs e2e-relogs --until -1h 2>&1) || fail "logs --until before the machine was made: $out"
[ -z "$out" ] || fail "logs --until before the machine was made showed: $out"
$NSPAWN rm -f e2e-relogs >/dev/null || fail "rm e2e-relogs"
$NSPAWN machines ls | tee /tmp/e2e-m.txt
grep -q "^ *$app " /tmp/e2e-m.txt || fail "busybox machine not running"
out=$($NSPAWN exec $app -- /bin/sh -c 'echo inside:$(uname -n); cat /etc/os-release | head -1' </dev/null | tr -d '\r')
echo "$out"
echo "$out" | grep_q "inside:$app" || fail "exec via namespaces did not run inside the machine"
[ "$($NSPAWN exec $app -- id -u </dev/null | tr -d '\r')" = "0" ] || fail "exec does not run as the machine's root"
[ "$($NSPAWN exec $app --user 65534 -- id -u </dev/null | tr -d '\r')" = "65534" ] || fail "exec --user ignored"
# A uid the passwd lacks runs with group 0 and / as home.
[ "$($NSPAWN exec $app --user 12345 -- /bin/sh -c 'id -u; id -g; echo $HOME' </dev/null | tr -d '\r' | tr '\n' ' ')" = "12345 0 / " ] || fail "exec -u with a uid the passwd lacks: $($NSPAWN exec $app --user 12345 -- /bin/sh -c 'id; echo $HOME' </dev/null 2>&1)"
# -u by names: the group named in /etc/group, the home the passwd entry has.
grp=$($NSPAWN exec $app -- /bin/sh -c 'sed -n 1p /etc/group | cut -d: -f1' </dev/null | tr -d '\r')
gid=$($NSPAWN exec $app -- /bin/sh -c 'sed -n 1p /etc/group | cut -d: -f3' </dev/null | tr -d '\r')
home=$($NSPAWN exec $app -- /bin/sh -c 'grep ^nobody: /etc/passwd | cut -d: -f6' </dev/null | tr -d '\r')
[ -n "$grp" ] && [ -n "$home" ] || fail "no group or nobody in the image: $grp $home"
[ "$($NSPAWN exec $app --user nobody:$grp -- /bin/sh -c 'id -u; id -g; echo $HOME' </dev/null | tr -d '\r' | tr '\n' ' ')" = "65534 $gid $home " ] || fail "exec -u NAME:GROUP by names, with the passwd home: $($NSPAWN exec $app --user nobody:$grp -- /bin/sh -c 'id -u; id -g; echo $HOME' </dev/null 2>&1)"
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
# sysfs is mounted inside the namespace: the app's interfaces, not the host's.
sys_net=$($NSPAWN exec $app -- ls /sys/class/net </dev/null | tr -d '\r' | tr '\n' ' ')
echo "$sys_net" | grep_q "host0" || fail "/sys/class/net in the app lacks host0: $sys_net"
echo "$sys_net" | grep_q "nspawn0" && fail "/sys/class/net in the app shows the host's interfaces: $sys_net"
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
# A prune here selects what this run made: the host may have unused networks of its own.
$NSPAWN network create e2e-ne --label e2e.prune=$nonce >/dev/null || fail "network create e2e-ne"
$NSPAWN network create e2e-nf >/dev/null || fail "network create e2e-nf"
out=$($NSPAWN network prune -f --filter label=e2e.prune=$nonce) || fail "network prune: $out"
[ "$out" = e2e-ne ] || fail "network prune with a filter removed otherwise: $out"
$NSPAWN network ls | grep_q "^ *e2e-nf " || fail "network prune removed a network its filter does not select"
# The event reaches the journal a moment after prune returns.
retry 5 bash -c "$NSPAWN events --since -10min --until now --filter type=network --filter event=remove | grep -qE 'network remove e2e-ne \(created=[0-9]+, interface=nsbr-e2e-ne, internal=false, subnet='" || fail "network remove carries no metadata: $($NSPAWN events --since -10min --until now --filter type=network | tail -3)"
for n in $($NSPAWN network ls | awk '$1 ~ /^e2e-n[a-f]$/ {print $1}'); do $NSPAWN network rm "$n" >/dev/null || fail "network rm $n"; done
$NSPAWN network ls | grep_q "^ *e2e-n[a-f] " && fail "an e2e network is left: $($NSPAWN network ls)"
ip -o link show | grep_q nsbr-e2e && fail "a removed network left its bridge"
nft list table ip nspawn | grep_q nsbr-e2e && fail "a removed network left rules in the nspawn table"
if command -v iptables >/dev/null 2>&1; then
  iptables -w -S 2>/dev/null | grep_q nsbr-e2e && fail "a removed network left iptables rules"
fi
if firewall-cmd --state >/dev/null 2>&1; then
  firewall-cmd --zone=trusted --list-interfaces | grep_q nsbr-e2e && fail "a removed network is still in the trusted zone"
fi
$NSPAWN network ls | grep_q "^ *bridge " || fail "the default network is not listed"

step "--network container: a machine in another one's network namespace"
$NSPAWN create $app e2e-side-net -p 18095:8080 -- /bin/sleep 300 >/dev/null || fail "create the owner of the shared network"
$NSPAWN start e2e-side-net >/dev/null || fail "start the owner of the shared network"
$NSPAWN create $app e2e-side --network container:e2e-side-net -- /bin/sh -c "mkdir -p /www && echo side-$nonce > /www/index.html && exec /bin/httpd -f -p 8080 -h /www" >/dev/null || fail "create a machine in another one's network"
$NSPAWN start e2e-side >/dev/null || fail "start a machine in another one's network"
owner_addr=$($NSPAWN inspect e2e-side-net | python3 -c "import json,sys; print(json.load(sys.stdin)[0]['address'])")
$NSPAWN exec e2e-side -- ip -4 -o addr show host0 </dev/null | tr -d '\r' | grep_q " $owner_addr/" || fail "the sharing machine does not see the owner's address: $($NSPAWN exec e2e-side -- ip -4 -o addr show </dev/null)"
retry 10 bash -c "curl -s -m 3 http://127.0.0.1:18095/ | grep -q side-$nonce" || fail "the port the owner publishes does not reach the sharing machine's program"
$NSPAWN exec e2e-side -- cat /etc/hosts </dev/null | tr -d '\r' | grep_q " host.nspawn.internal" || fail "the sharing machine lacks the hosts file of the network: $($NSPAWN exec e2e-side -- cat /etc/hosts </dev/null)"
$NSPAWN ps | grep "^ *e2e-side " | grep_q " container:e2e-side-net " || fail "ps does not show the shared network: $($NSPAWN ps)"
$NSPAWN inspect e2e-side | python3 -c "import json,sys; d = json.load(sys.stdin)[0]; assert d['network'] == 'container:e2e-side-net' and d['networks'] == [] and d['address'] == '', d" || fail "inspect of a machine in another one's network"
$NSPAWN network inspect bridge | python3 -c "import json,sys; ms = [m['name'] for m in json.load(sys.stdin)[0]['machines']]; assert 'e2e-side-net' in ms and 'e2e-side' not in ms, ms" || fail "network inspect lists the sharing machine as a member"
# update rewrites the unit's ExecStart: the shared namespace must stay on it.
$NSPAWN update e2e-side -m 64m >/dev/null || fail "update of a machine in another one's network"
if [ "$systemd_major" -lt 259 ]; then
  grep -q -- "--network-namespace-path=/run/netns/nspawn-e2e-side" /etc/systemd/system/systemd-nspawn@e2e-side.service.d/nspawn-hooks.conf || fail "update dropped the shared namespace from ExecStart: $(cat /etc/systemd/system/systemd-nspawn@e2e-side.service.d/nspawn-hooks.conf)"
fi
$NSPAWN restart e2e-side >/dev/null || fail "restart the sharing machine after update"
$NSPAWN exec e2e-side -- ip -4 -o addr show host0 </dev/null | tr -d '\r' | grep_q " $owner_addr/" || fail "after update, the sharing machine lost the owner's address: $($NSPAWN exec e2e-side -- ip -4 -o addr show 2>&1 </dev/null)"
out=$($NSPAWN start e2e-side --network-alias side 2>&1) && fail "an alias was accepted on a machine without a network of its own"
out=$($NSPAWN create $app e2e-side-p --network container:e2e-side-net --network e2e-x -- /bin/sleep 1 2>&1) && fail "container: was combined with another network"
echo "$out" | grep_q "stands alone" || fail "the combination was not explained: $out"
$NSPAWN create $app e2e-side-p --network container:e2e-side-net -p 18096:8080 -- /bin/sleep 1 >/dev/null || fail "create with container: and a port"
out=$($NSPAWN start e2e-side-p 2>&1) && fail "a port was published from a machine in another one's network"
echo "$out" | grep_q "with -p on e2e-side-net" || fail "the refused port was not explained: $out"
$NSPAWN rm e2e-side-p >/dev/null || fail "rm e2e-side-p"
out=$($NSPAWN rm -f e2e-side-net 2>&1) && fail "the owner of a shared network was removed"
echo "$out" | grep_q "network of e2e-side" || fail "the refused removal was not explained: $out"
$NSPAWN ps | grep_q "^ *e2e-side-net .* running" || fail "the refused rm -f stopped the owner: $($NSPAWN ps)"
$NSPAWN stop e2e-side >/dev/null || fail "stop the sharing machine"
ip netns list | grep_q -E "^nspawn-e2e-side( |$)" && fail "the namespace name of the sharing machine was left behind"
ip netns list | grep_q -E "^nspawn-e2e-side-net( |$)" || fail "stopping the sharing machine took the owner's namespace"
$NSPAWN stop e2e-side-net >/dev/null || fail "stop the owner"
out=$($NSPAWN start e2e-side 2>&1) && fail "a machine started in the network of a stopped one"
echo "$out" | grep_q "not running" || fail "the stopped owner was not explained: $out"
# A booted machine's network is shared the same way.
$NSPAWN pull "$IMAGE" --name e2e-side-boot --backend overlay --force >/dev/null || fail "pull e2e-side-boot"
$NSPAWN start e2e-side-boot >/dev/null || fail "start a booted owner"
boot_addr=$($NSPAWN inspect e2e-side-boot | python3 -c "import json,sys; print(json.load(sys.stdin)[0]['address'])")
$NSPAWN start e2e-side --network container:e2e-side-boot -- /bin/sh -c 'ip -4 -o addr show host0' >/dev/null 2>&1
retry 10 bash -c "$NSPAWN logs e2e-side | tr -d '\r' | grep -q ' $boot_addr/'" || fail "the sharing machine does not see the booted owner's address: $($NSPAWN logs e2e-side)"
$NSPAWN rm -f e2e-side >/dev/null || fail "rm e2e-side"
$NSPAWN rm -f e2e-side-boot e2e-side-net >/dev/null || fail "rm the owners"

step "run -d: a machine from an image and started in one step"
# An image pulled under a name of its own and never run: run makes the machine of it
# without the registry. ($app ran: a machine is no image to make another one of.)
$NSPAWN pull "$busybox_ref" --name e2e-bb --force >/dev/null || fail "pull e2e-bb"
$NSPAWN run -d "$busybox_ref" --name e2e-run -p 18082:80 -- /bin/sh -c "mkdir -p /www; echo run-$nonce > /www/index.html; echo serving-$nonce; exec /bin/httpd -f -p 80 -h /www" > /tmp/e2e-run.txt 2>&1 || { cat /tmp/e2e-run.txt; fail "run from a local image"; }
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
# create takes a reference too: what is not local is pulled under its own name first.
$NSPAWN create "$busybox136_ref" e2e-cpull -- /bin/sleep 300 > /tmp/e2e-cpull.txt 2>&1 || { cat /tmp/e2e-cpull.txt; fail "create from a reference"; }
$NSPAWN images ls | grep "^ *busybox-1.36 " | grep_q pull || fail "create did not keep the pulled image under its name"
$NSPAWN images ls | grep "^ *e2e-cpull " | grep_q create || fail "create from a reference did not make the machine"
$NSPAWN create "$busybox136_ref" e2e-cpull --force > /tmp/e2e-cpull.txt 2>&1 || fail "create again from the reference"
grep -q "downloading" /tmp/e2e-cpull.txt && fail "create pulled an image that is here already"
$NSPAWN rm e2e-cpull >/dev/null && $NSPAWN images rm busybox-1.36 >/dev/null || fail "rm the created machine and its image"
# Several machines at once: every line carries its machine's name; --until ends a follow.
# The line looked for is this run's: the journal keeps what earlier machines of the name
# printed, and a fresh host has none of it.
$NSPAWN logs $app e2e-run --until now > /tmp/e2e-logs2.txt 2>&1 || fail "logs of two machines"
retry 5 bash -c "$NSPAWN logs $app e2e-run --until now > /tmp/e2e-logs2.txt 2>&1; grep -q '^e2e-run  *| serving-$nonce' /tmp/e2e-logs2.txt" || fail "logs of two machines carry no names: $(head -3 /tmp/e2e-logs2.txt)"
grep -q "^$app | " /tmp/e2e-logs2.txt || fail "logs of two machines miss $app"
timeout 20 $NSPAWN logs e2e-run -f --until now >/dev/null 2>&1 || fail "logs -f --until did not end by itself"
$NSPAWN rm -f e2e-run >/dev/null || fail "rm -f e2e-run"
systemctl is-failed systemd-nspawn@$app.service >/dev/null 2>&1 && fail "unit left in failed state after stop"

step "run attached: output, input, terminal, signals, exit code, --rm"
bb=$busybox_ref
# Here under a name of the run's own, which run then finds by its reference.
$NSPAWN pull "$bb" --name e2e-bb --force >/dev/null || fail "pull e2e-bb"
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
# run --rm keeps the image: what run pulled stays, the machine goes.
# (A tag of its own, so that no image of the host is replaced.)
out=$($NSPAWN run --rm "$busybox137_ref" /bin/echo kept-$nonce 2>/dev/null) || fail "run --rm of an image not here yet"
[ "$out" = "kept-$nonce" ] || fail "run --rm of an image not here yet did not show the output: $out"
$NSPAWN images ls | grep_q "^ *busybox-1.37 " || fail "run --rm did not keep the image it pulled"
$NSPAWN images ls | grep_q "^ *busybox-1.37-" && fail "run --rm left its machine behind"
$NSPAWN images rm busybox-1.37 >/dev/null || fail "images rm of the image run kept"
# A reboot asked from inside ends a --rm run (the machine is not restarted then): an app
# keeps CAP_SYS_BOOT for it, which in its pid namespace can only signal its init.
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
# The shell has to have set its trap and become sleep before the signal comes.
retry 5 bash -c "$NSPAWN top $app | grep -q 'sleep 300'" || fail "the stubborn program is not up: $($NSPAWN top $app)"
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

step "entrypoint, environment and volumes"
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
# exec's command gets the machine's capabilities and nothing of the service's descriptors;
# an app on the bridge shares the host's user namespace, so CAP_SYS_BOOT (bit 22) stays
# with the machine's own processes, which carry the kexec filter.
leader_bnd=$(awk '/^CapBnd:/ {print $2}' "/proc/$(machinectl show $app -p Leader --value)/status")
exec_bnd=$($NSPAWN exec $app -- /bin/sh -c 'awk "/^CapBnd:/ {print \$2}" /proc/self/status' </dev/null | tr -d '\r')
[ "$(( 0x$exec_bnd ))" = "$(( 0x$leader_bnd & ~0x400000 ))" ] || fail "exec's command has other capabilities than the machine less CAP_SYS_BOOT: $exec_bnd, the machine $leader_bnd"
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

# A path inside the machine with a dot component would mount elsewhere, the root for one.
out=$($NSPAWN start $app -v /tmp:/. -- /bin/sleep 1 2>&1) && fail "a volume target of /. was accepted"
echo "$out" | grep_q "without . or .." || fail "the dot target was not explained: $out"
out=$($NSPAWN start $app --tmpfs /x/.. -- /bin/sleep 1 2>&1) && fail "a tmpfs of /x/.. was accepted"

step "per-machine settings: hostname, user, workdir, capabilities, read-only, tmpfs, devices, dns, hosts, ulimits, signals"
$NSPAWN start $app --hostname h-$nonce -u nobody -w /tmp --cap-drop ALL --cap-add NET_BIND_SERVICE --read-only --tmpfs /scratch:size=16m --device /dev/null:/dev/nullo:r --dns 10.99.0.1 --dns-search example.test --add-host peer:10.1.1.1 --add-host gw:host-gateway --add-host v6=fd00::1 --ulimit nofile=64:128 --ulimit core=unlimited --stop-signal SIGINT --stop-timeout 2 --oom-score-adj 100 --sysctl net.ipv4.icmp_echo_ignore_all=1 -p none -- /bin/sh -c 'trap "" INT; pwd; exec /bin/sleep 300' || fail "start with the per-machine settings"
# Root's view of the machine, which runs as nobody.
x() { $NSPAWN exec -u root $app -- /bin/sh -c "$1" </dev/null 2>/dev/null | tr -d '\r'; }
[ "$(x hostname)" = "h-$nonce" ] || fail "--hostname not applied: $(x hostname)"
# The program is the stub init's child: systemd-nspawn runs getent in the machine
# ahead of it to resolve the user, which takes PIDs 2 and 3 (busybox has no getent;
# nspawn stands one in).
pid=$(x 'cat /proc/1/task/1/children' | tr -d ' ')
[ -n "$pid" ] || fail "no program under the stub init: $(x 'ls /proc')"
[ "$(x "awk '/^Uid:/ {print \$2}' /proc/$pid/status")" = 65534 ] || fail "--user not applied: $(x "cat /proc/$pid/status")"
# exec without -u runs as the machine's user, as docker exec does; -u root as root.
[ "$($NSPAWN exec $app -- id -u </dev/null | tr -d '\r')" = 65534 ] || fail "exec without -u does not run as the machine's user: $($NSPAWN exec $app -- id </dev/null 2>&1)"
[ "$($NSPAWN exec -u root $app -- id -u </dev/null | tr -d '\r')" = 0 ] || fail "exec -u root does not run as root"
# The program prints its directory: exec runs with the machine's capabilities, and
# without CAP_SYS_PTRACE root cannot read the cwd link of nobody's process.
retry 5 bash -c "$NSPAWN logs $app -n 5 | tr -d '\r' | grep -qx /tmp" || fail "--workdir not applied: $($NSPAWN logs $app -n 5)"
# --cap-drop ALL --cap-add X keeps X: bit 10 is CAP_NET_BIND_SERVICE.
[ "$(x 'awk "/^CapBnd:/ {print \$2}" /proc/1/status')" = 0000000000000400 ] || fail "--cap-drop ALL --cap-add NET_BIND_SERVICE left other capabilities: $(x 'grep CapBnd /proc/1/status')"
# An app without a user namespace keeps the default set and CAP_SYS_BOOT, no more
# (CapBnd 00000000a84425fb), and /proc/sys cannot be remounted writable; --cap-add
# SYS_ADMIN gives that back, a booted machine keeps systemd-nspawn's set in its own
# user namespace.
$NSPAWN create $app e2e-caps -- /bin/sleep 300 >/dev/null || fail "create e2e-caps"
$NSPAWN start e2e-caps >/dev/null || fail "start e2e-caps"
c() { $NSPAWN exec e2e-caps -- /bin/sh -c "$1" </dev/null 2>&1 | tr -d '\r'; }
out=$(c 'awk "/^CapBnd:/ {print \$2}" /proc/1/status')
[ "$out" = 00000000a84425fb ] || fail "an app on the bridge does not have the default set and CAP_SYS_BOOT: $out"
# systemd-nspawn reports a machine started before its child confines itself; exec waits
# for that, so a command right after start has the machine's set every time, less
# CAP_SYS_BOOT: the command does not carry the seccomp filter that keeps kexec out.
for i in 1 2 3 4 5; do
  $NSPAWN stop e2e-caps >/dev/null || fail "stop e2e-caps ($i)"
  $NSPAWN start e2e-caps >/dev/null || fail "start e2e-caps ($i)"
  out=$(c 'awk "/^CapBnd:/ {print \$2}" /proc/self/status')
  [ "$out" = 00000000a80425fb ] || fail "exec right after start ran with another bounding set ($i): $out"
done
grep -q "^SystemCallFilter=~kexec_load kexec_file_load" /etc/systemd/nspawn/e2e-caps.nspawn || fail "the kexec system calls are not filtered for an app: $(cat /etc/systemd/nspawn/e2e-caps.nspawn)"
[ "$(c 'cat /proc/self/uid_map')" = "$(printf '         0          0 4294967295')" ] || fail "an app on the bridge is not in the initial user namespace: $(c 'cat /proc/self/uid_map')"
c 'mount -o remount,rw /proc/sys' >/dev/null 2>&1 && fail "an app on the bridge remounted /proc/sys writable"
c 'v=$(cat /proc/sys/kernel/core_pattern); printf %s "$v" > /proc/sys/kernel/core_pattern' >/dev/null 2>&1 && fail "an app on the bridge wrote a kernel sysctl"
$NSPAWN stop e2e-caps >/dev/null || fail "stop e2e-caps"
$NSPAWN start e2e-caps --cap-add SYS_ADMIN >/dev/null || fail "start e2e-caps with --cap-add SYS_ADMIN"
[ $(( 0x$(c 'awk "/^CapBnd:/ {print \$2}" /proc/1/status') & 0x200000 )) != 0 ] || fail "--cap-add SYS_ADMIN did not give CAP_SYS_ADMIN back: $(c 'grep CapBnd /proc/1/status')"
$NSPAWN stop e2e-caps >/dev/null || fail "stop e2e-caps"
$NSPAWN start e2e-caps --cap-add none --network none >/dev/null || fail "start e2e-caps with --network none"
[ "$(c 'cat /proc/self/uid_map')" != "$(printf '         0          0 4294967295')" ] || fail "an app with --network none is not in a user namespace of its own"
[ $(( 0x$(c 'awk "/^CapBnd:/ {print \$2}" /proc/1/status') & 0x200000 )) != 0 ] || fail "an app in a user namespace lost systemd-nspawn's CAP_SYS_ADMIN: $(c 'grep CapBnd /proc/1/status')"
[ $(( 0x$(c 'awk "/^CapBnd:/ {print \$2}" /proc/self/status') & 0x400000 )) != 0 ] || fail "a command in a machine with a user namespace of its own lost CAP_SYS_BOOT: $(c 'grep CapBnd /proc/self/status')"
$NSPAWN rm -f e2e-caps >/dev/null || fail "rm e2e-caps"
[ "$(x 'touch /x 2>/dev/null && echo RW || echo RO')" = RO ] || fail "--read-only root is writable"
[ "$(x 'touch /scratch/a && echo OK')" = OK ] || fail "--tmpfs is not writable"
x 'cat /proc/mounts' | grep_q " /scratch tmpfs" || fail "--tmpfs is not a tmpfs: $(x 'cat /proc/mounts')"
[ "$(x 'test -c /dev/nullo && echo DEV')" = DEV ] || fail "--device node missing inside"
x 'cat /etc/resolv.conf' | grep_q "^nameserver 10.99.0.1$" || fail "--dns not applied: $(x 'cat /etc/resolv.conf')"
x 'cat /etc/resolv.conf' | grep_q "^search example.test$" || fail "--dns-search not applied"
x 'cat /etc/hosts' | grep_q "^10.1.1.1 peer$" || fail "--add-host not applied: $(x 'cat /etc/hosts')"
x 'cat /etc/hosts' | grep_q "^fd00::1 v6$" || fail "--add-host with an IPv6 address (HOST=IP) not applied: $(x 'cat /etc/hosts')"
# The program's own limits (exec's command has the machine's too, set the same way).
x "grep 'Max core' /proc/$pid/limits" | grep_q "unlimited *unlimited" || fail "--ulimit core=unlimited not applied to the program: $(x "grep 'Max core' /proc/$pid/limits")"
x "grep 'Max open files' /proc/$pid/limits" | grep_q " 64 *128 " || fail "--ulimit nofile=64:128 not applied to the program: $(x "grep 'Max open files' /proc/$pid/limits")"
x 'grep "Max open files" /proc/self/limits' | grep_q " 64 *128 " || fail "exec's command does not get the machine's limits: $(x 'grep "Max open files" /proc/self/limits')"
# The command's limits come last: with the service holding many descriptors (log
# followers here), a low open-files limit must not refuse what exec opens on the way,
# the passwd file or SELinux's exec attribute.
followers=()
for _ in $(seq 1 40); do $NSPAWN logs -f $app >/dev/null 2>&1 & followers+=($!); done
service_pid=$(systemctl show nspawn.service -p MainPID --value)
for _ in $(seq 1 20); do [ "$(ls /proc/$service_pid/fd 2>/dev/null | wc -l)" -ge 80 ] && break; sleep 0.5; done
out=$($NSPAWN exec $app -- /bin/sh -c 'echo ok' </dev/null 2>&1 | tr -d '\r')
[ "$out" = ok ] || fail "exec failed while the service held many descriptors: $out"
out=$($NSPAWN exec -u nobody $app -- /bin/sh -c 'echo ok' </dev/null 2>&1 | tr -d '\r')
[ "$out" = ok ] || fail "exec -u failed while the service held many descriptors: $out"
kill "${followers[@]}" 2>/dev/null
wait "${followers[@]}" 2>/dev/null
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
assert d['devices'] == ['/dev/null:/dev/nullo:r'] and d['ulimits'] == {'core': '18446744073709551615:18446744073709551615', 'nofile': '64:128'}, d
assert d['stop_signal'] == 'SIGINT' and d['stop_timeout'] == 2 and d['oom_score_adj'] == 100, d
assert d['extra_hosts'] == ['peer:10.1.1.1', 'gw:host-gateway', 'v6:fd00::1'] and d['sysctls'] == {'net.ipv4.icmp_echo_ignore_all': '1'}, d" || fail "inspect does not show the flags"
stop_start=$(date +%s)
out=$($NSPAWN stop $app 2>&1) || fail "stop with --stop-signal and --stop-timeout: $out"
echo "$out" | grep_q "ignored SIGINT for 2 seconds" || fail "stop did not use --stop-signal and --stop-timeout: $out"
[ $(( $(date +%s) - stop_start )) -le 8 ] || fail "stop took longer than --stop-timeout allows"
# USER:GROUP: the group is the primary one and the only one, a
# number the image's group file does not list included; a name it lacks is refused.
# The program comes after systemd-nspawn's getent runs, so its pid is waited for.
program_pid() {
  local p
  for _ in $(seq 1 20); do
    p=$(x 'for c in $(cat /proc/1/task/1/children); do [ "$(cat /proc/$c/comm 2>/dev/null)" = sleep ] && echo $c; done' | head -1)
    [ -n "$p" ] && { echo "$p"; return 0; }
    sleep 0.5
  done
  return 1
}
$NSPAWN start $app -u 1000:1000 -- /bin/sleep 300 >/dev/null || fail "start with -u UID:GID"
pid=$(program_pid) || fail "no program under the stub init after -u UID:GID: $(x 'ls /proc')"
[ "$(x "awk '/^Uid:/ {print \$2} /^Gid:/ {print \$2}' /proc/$pid/status" | tr '\n' ' ')" = "1000 1000 " ] || fail "-u 1000:1000 not applied: $(x "grep -E '^(Uid|Gid|Groups):' /proc/$pid/status")"
[ "$(x "awk '/^Groups:/ {print \$2}' /proc/$pid/status")" = 1000 ] || fail "-u UID:GID left other groups: $(x "grep ^Groups: /proc/$pid/status")"
$NSPAWN stop $app >/dev/null || fail "stop after -u UID:GID"
$NSPAWN start $app -u nobody:root -- /bin/sleep 300 >/dev/null || fail "start with -u NAME:GROUP"
pid=$(program_pid) || fail "no program under the stub init after -u NAME:GROUP: $(x 'ls /proc')"
[ "$(x "awk '/^Uid:/ {print \$2} /^Gid:/ {print \$2}' /proc/$pid/status" | tr '\n' ' ')" = "65534 0 " ] || fail "-u nobody:root not applied: $(x "grep -E '^(Uid|Gid):' /proc/$pid/status")"
$NSPAWN inspect $app | python3 -c "import json,sys; d = json.load(sys.stdin)[0]; assert d['user'] == 'nobody:root', d" || fail "inspect does not show the user with its group"
$NSPAWN stop $app >/dev/null || fail "stop after -u NAME:GROUP"
out=$($NSPAWN start $app -u nobody:nosuch 2>&1) && fail "an unknown group was accepted"
echo "$out" | grep_q "unable to find group nosuch" || fail "the unknown group was not explained: $out"
out=$($NSPAWN start $app -u nobody: 2>&1) && fail "an empty group was accepted"
# An image without /bin/sh, getent or /usr (distroless, scratch) runs, as a user too:
# the getent systemd-nspawn runs inside is a program nspawn writes, which needs
# neither, and an empty /usr is made for it.
$NSPAWN create $app e2e-noshell -- /bin/sleep 300 >/dev/null || fail "create e2e-noshell"
$NSPAWN start e2e-noshell >/dev/null || fail "start e2e-noshell"
$NSPAWN exec e2e-noshell -- /bin/rm -r /bin/sh /usr </dev/null || fail "take /bin/sh and /usr out of e2e-noshell"
$NSPAWN stop e2e-noshell >/dev/null || fail "stop e2e-noshell"
ns() { $NSPAWN exec -u root e2e-noshell -- /bin/busybox sh -c "$1" </dev/null 2>/dev/null | tr -d '\r'; }
noshell_pid() {
  local p
  for _ in $(seq 1 20); do
    p=$(ns 'for c in $(cat /proc/1/task/1/children); do [ "$(cat /proc/$c/comm 2>/dev/null)" = sleep ] && echo $c; done' | head -1)
    [ -n "$p" ] && { echo "$p"; return 0; }
    sleep 0.5
  done
  return 1
}
out=$($NSPAWN start e2e-noshell -u 1000:1000 -- /bin/sleep 300 2>&1) || fail "start an image without a shell as 1000:1000: $out"
pid=$(noshell_pid) || fail "no program in the image without a shell: $(ns 'ls /proc')"
[ "$(ns "awk '/^Uid:/ {print \$2} /^Gid:/ {print \$2} /^Groups:/ {print \$2}' /proc/$pid/status" | tr '\n' ' ')" = "1000 1000 1000 " ] || fail "-u 1000:1000 not applied without a shell: $(ns "grep -E '^(Uid|Gid|Groups):' /proc/$pid/status")"
[ "$(ns 'getent passwd 1000')" = "1000:x:1000:1000::/:/bin/sh" ] || fail "the program does not answer as getent: $(ns 'getent passwd 1000; echo status $?')"
[ -z "$(ns 'getent hosts localhost')" ] || fail "the program answered what it was not asked"
# systemd-nspawn runs a program only in a root with /usr, which a scratch image lacks;
# what is in it now is the mount point of getent.
[ "$(ns 'ls -A /usr')" = bin ] || fail "e2e-noshell did not get a /usr: $(ns 'ls -la /usr')"
$NSPAWN stop e2e-noshell >/dev/null || fail "stop e2e-noshell as 1000:1000"
out=$($NSPAWN start e2e-noshell -u nobody -- /bin/sleep 300 2>&1) || fail "start an image without a shell as nobody: $out"
pid=$(noshell_pid) || fail "no program in the image without a shell as nobody"
[ "$(ns "awk '/^Uid:/ {print \$2}' /proc/$pid/status")" = 65534 ] || fail "-u nobody not applied without a shell: $(ns "grep ^Uid: /proc/$pid/status")"
$NSPAWN stop e2e-noshell >/dev/null || fail "stop e2e-noshell as nobody"
# A uid the image does not list: group 0 for the program and for exec alike.
out=$($NSPAWN start e2e-noshell -u 4242 -- /bin/sleep 300 2>&1) || fail "start an image without a shell as 4242: $out"
pid=$(noshell_pid) || fail "no program in the image without a shell as 4242"
[ "$(ns "awk '/^Uid:/ {print \$2} /^Gid:/ {print \$2}' /proc/$pid/status" | tr '\n' ' ')" = "4242 0 " ] || fail "-u 4242 without a shell: $(ns "grep -E '^(Uid|Gid):' /proc/$pid/status")"
[ "$($NSPAWN exec e2e-noshell -u 4242 -- /bin/busybox sh -c 'id -u; id -g' </dev/null | tr -d '\r' | tr '\n' ' ')" = "4242 0 " ] || fail "exec -u 4242 differs from the start: $($NSPAWN exec e2e-noshell -u 4242 -- /bin/busybox id </dev/null 2>&1)"
$NSPAWN stop e2e-noshell >/dev/null || fail "stop e2e-noshell as 4242"
out=$($NSPAWN start e2e-noshell -u ghost -- /bin/sleep 300 2>&1) && fail "a user the image lacks was started"
echo "$out" | grep_q "unable to find user ghost" || fail "the unknown user was not explained: $out"
$NSPAWN rm -f e2e-noshell >/dev/null || fail "rm e2e-noshell"
out=$($NSPAWN start $app --sysctl kernel.shmmax=1 2>&1) && fail "a sysctl beyond net.* was accepted"
# A refused combination is not remembered: the next start without flags must work.
out=$($NSPAWN start $app --network none --sysctl net.ipv4.ip_forward=1 -- /bin/sleep 300 2>&1) && fail "--sysctl was accepted with --network none"
echo "$out" | grep_q "has none" || fail "the refused sysctl was not explained: $out"
$NSPAWN inspect $app | python3 -c "import json,sys; d = json.load(sys.stdin)[0]; assert d['network'] != 'none' and 'net.ipv4.ip_forward' not in d['sysctls'], d" || fail "a refused start changed the record"
# Everything back, and --privileged: the whole bounding set.
$NSPAWN start $app --privileged --cap-drop none --cap-add none --read-only=false -u root -w / --tmpfs none --device none --dns none --dns-search none --add-host none --ulimit none --stop-signal "" --stop-timeout 10 --oom-score-adj 0 --sysctl none --hostname "" -- /bin/sleep 300 || fail "start with the flags taken back"
[ "$(x hostname)" = "$app" ] || fail "--hostname \"\" did not restore the name: $(x hostname)"
[ "$(x 'awk "/^CapBnd:/ {print \$2}" /proc/1/status')" != 0000000000000000 ] || fail "--privileged left no capabilities"
[ $(( 0x$(x 'awk "/^CapBnd:/ {print \$2}" /proc/self/status') & 0x400000 )) != 0 ] || fail "a command in a privileged machine lost CAP_SYS_BOOT: $(x 'grep CapBnd /proc/self/status')"
[ "$(x 'touch /x 2>/dev/null && echo RW || echo RO')" = RW ] || fail "--read-only=false left the root read-only"
$NSPAWN inspect $app | python3 -c "import json,sys; d = json.load(sys.stdin)[0]; assert d['privileged'] and not d['read_only'] and d['cap_drop'] == [] and d['cap_add'] == [] and d['hostname'] == '' and d['stop_signal'] == '', d" || fail "inspect after taking the flags back"
$NSPAWN stop $app || fail "stop the privileged machine"
$NSPAWN start $app --privileged=false -- /bin/sleep 300 >/dev/null || fail "start with --privileged=false"
$NSPAWN inspect $app | python3 -c "import json,sys; d = json.load(sys.stdin)[0]; assert not d['privileged'], d" || fail "--privileged=false did not take it back"
$NSPAWN stop $app || fail "stop"

step "--timezone: how systemd-nspawn sets the machine's /etc/localtime"
$NSPAWN create $app e2e-tz -- /bin/sleep 300 >/dev/null || fail "create e2e-tz"
$NSPAWN start e2e-tz --timezone off >/dev/null || fail "start e2e-tz with --timezone off"
grep -qx "Timezone=off" /etc/systemd/nspawn/e2e-tz.nspawn || fail "--timezone off is not in the settings: $(cat /etc/systemd/nspawn/e2e-tz.nspawn)"
tz() { $NSPAWN exec e2e-tz -- /bin/sh -c "$1" </dev/null 2>/dev/null | tr -d '\r'; }
# off: a zone set inside survives a restart, and nothing is mounted over it.
tz 'ln -sf /usr/share/zoneinfo/Asia/Tokyo /etc/localtime'
$NSPAWN stop e2e-tz >/dev/null || fail "stop e2e-tz"
$NSPAWN start e2e-tz >/dev/null || fail "start e2e-tz again"
[ "$(tz 'readlink /etc/localtime')" = /usr/share/zoneinfo/Asia/Tokyo ] || fail "--timezone off did not keep the zone set inside: $(tz 'ls -l /etc/localtime')"
tz 'cat /proc/self/mountinfo' | grep_q " /etc/localtime " && fail "--timezone off still mounted /etc/localtime"
$NSPAWN inspect e2e-tz | python3 -c "import json,sys; d = json.load(sys.stdin)[0]; assert d['timezone'] == 'off', d" || fail "inspect does not show --timezone"
# copy: the host's zone as a file of the machine's own, which a package may replace.
if [ -e /etc/localtime ]; then
  $NSPAWN stop e2e-tz >/dev/null || fail "stop e2e-tz before copy"
  $NSPAWN start e2e-tz --timezone copy >/dev/null || fail "start e2e-tz with --timezone copy"
  [ "$(tz 'test -f /etc/localtime && ! test -L /etc/localtime && md5sum < /etc/localtime')" = "$(md5sum < /etc/localtime)" ] || fail "--timezone copy did not copy the host's zone: $(tz 'ls -l /etc/localtime')"
  tz 'cat /proc/self/mountinfo' | grep_q " /etc/localtime " && fail "--timezone copy mounted /etc/localtime"
else
  echo "this host has no /etc/localtime: --timezone copy not checked"
fi
$NSPAWN stop e2e-tz >/dev/null || fail "stop e2e-tz before auto"
$NSPAWN start e2e-tz --timezone auto >/dev/null || fail "start e2e-tz with --timezone auto"
grep -q "^Timezone=" /etc/systemd/nspawn/e2e-tz.nspawn && fail "--timezone auto left a Timezone= line: $(cat /etc/systemd/nspawn/e2e-tz.nspawn)"
$NSPAWN start e2e-tz --timezone utc 2>/dev/null && fail "an unknown --timezone mode was accepted"
$NSPAWN rm -f e2e-tz >/dev/null || fail "rm e2e-tz"

step "--log-driver: nspawn's own journal by default, the system's on request, or nothing"
bb=$busybox_ref
hooks_of() { echo /etc/systemd/system/systemd-nspawn@$1.service.d/nspawn-hooks.conf; }
in_system_journal() { journalctl -u systemd-nspawn@$1.service --output=cat --no-pager 2>/dev/null | grep_q "$2"; }
in_nspawn_journal() { journalctl --namespace=nspawn -u systemd-nspawn@$1.service --output=cat --no-pager 2>/dev/null | grep_q "$2"; }
# local, the default: the output goes to journald's nspawn namespace, which logs and an
# attached run read, and never to the system's journal.
out=$($NSPAWN run --rm $bb --name e2e-quiet -- /bin/sh -c 'echo local-$0' attached$nonce 2>/dev/null) || fail "run with the default log driver"
[ "$out" = "local-attached$nonce" ] || fail "an attached run did not show the output of a local machine: $out"
in_system_journal e2e-quiet "local-attached$nonce" && fail "the default log driver wrote to the system's journal"
retry 5 in_nspawn_journal e2e-quiet "local-attached$nonce" || fail "the default log driver did not write to nspawn's journal namespace"
$NSPAWN create $app e2e-quiet -- /bin/sh -c "echo detached-$nonce; exec sleep 300" >/dev/null || fail "create e2e-quiet"
$NSPAWN start e2e-quiet >/dev/null || fail "start e2e-quiet"
grep -qx "Wants=systemd-journald@nspawn.socket" "$(hooks_of e2e-quiet)" || fail "a local machine does not want journald's namespace: $(cat "$(hooks_of e2e-quiet)")"
grep -q "attach-exec e2e-quiet --journal-namespace nspawn -- " "$(hooks_of e2e-quiet)" || fail "a local machine's output is not sent to the namespace: $(cat "$(hooks_of e2e-quiet)")"
retry 10 bash -c "$NSPAWN logs e2e-quiet 2>/dev/null | tr -d '\r' | grep_q '^detached-$nonce\$'" || fail "logs of a local machine: $($NSPAWN logs e2e-quiet 2>&1)"
in_system_journal e2e-quiet "detached-$nonce" && fail "a detached local machine wrote to the system's journal"
# A duration ago and journalctl's times.
for since in 5m -5min "10 min ago"; do
  $NSPAWN logs --since "$since" e2e-quiet 2>&1 | tr -d '\r' | grep_q "^detached-$nonce\$" || fail "logs --since $since: $($NSPAWN logs --since "$since" e2e-quiet 2>&1 | tail -2)"
done
out=$($NSPAWN logs --until 1h e2e-quiet 2>&1) || fail "logs --until 1h: $out"
echo "$out" | grep_q "detached-$nonce" && fail "logs --until 1h shows what came after: $out"
$NSPAWN inspect e2e-quiet | python3 -c "import json,sys; d = json.load(sys.stdin)[0]; assert d['log_driver'] == 'local', d" || fail "inspect does not show the default log driver"
# journal: the system's journal, as before local existed.
$NSPAWN stop e2e-quiet >/dev/null || fail "stop e2e-quiet"
$NSPAWN start e2e-quiet --log-driver journal >/dev/null || fail "start e2e-quiet with --log-driver journal"
grep -q "journald@nspawn\|--journal-namespace" "$(hooks_of e2e-quiet)" && fail "--log-driver journal still sends the output to the namespace"
retry 10 in_system_journal e2e-quiet "detached-$nonce" || fail "--log-driver journal did not write to the system's journal"
retry 10 bash -c "$NSPAWN logs e2e-quiet 2>/dev/null | tr -d '\r' | grep_q '^detached-$nonce\$'" || fail "logs of a machine in the system's journal: $($NSPAWN logs e2e-quiet 2>&1)"
# none: the unit drops the output, logs says so, and an attached run still shows it.
$NSPAWN rm -f e2e-quiet >/dev/null || fail "rm e2e-quiet"
out=$($NSPAWN run --rm --log-driver none $bb --name e2e-quiet -- /bin/sh -c 'echo out-$0; echo err-$0 >&2; exit 3' quiet$nonce 2>/dev/null); rc=$?
[ "$rc" = 3 ] || fail "run --log-driver none did not exit with the program's code: $rc"
[ "$out" = "$(printf 'out-quiet%s\nerr-quiet%s' $nonce $nonce)" ] || fail "run --log-driver none did not show the program's output: $out"
in_system_journal e2e-quiet "quiet$nonce" && fail "--log-driver none wrote to the system's journal"
in_nspawn_journal e2e-quiet "quiet$nonce" && fail "--log-driver none wrote to nspawn's journal"
[ "$(echo abc | $NSPAWN run -i --rm --log-driver none $bb --name e2e-quiet -- wc -c 2>/dev/null)" = 4 ] || fail "run -i --log-driver none did not give the program its input"
$NSPAWN create $app e2e-quiet -- /bin/sh -c "echo dropped-$nonce; exec sleep 300" >/dev/null || fail "create e2e-quiet again"
$NSPAWN start e2e-quiet --log-driver none >/dev/null || fail "start e2e-quiet with --log-driver none"
grep -qx "StandardOutput=null" "$(hooks_of e2e-quiet)" || fail "--log-driver none is not in the unit: $(cat "$(hooks_of e2e-quiet)")"
grep -qx "StandardError=journal" "$(hooks_of e2e-quiet)" || fail "--log-driver none took systemd-nspawn's messages too: $(cat "$(hooks_of e2e-quiet)")"
sleep 2
in_system_journal e2e-quiet "dropped-$nonce" && fail "a detached machine with --log-driver none wrote to the system's journal"
in_nspawn_journal e2e-quiet "dropped-$nonce" && fail "a detached machine with --log-driver none wrote to nspawn's journal"
out=$($NSPAWN logs e2e-quiet 2>&1) && fail "logs of a machine that drops its output succeeded: $out"
echo "$out" | grep_q "keeps no output" || fail "logs of a machine that drops its output did not say so: $out"
$NSPAWN inspect e2e-quiet | python3 -c "import json,sys; d = json.load(sys.stdin)[0]; assert d['log_driver'] == 'none', d" || fail "inspect does not show the log driver none"
$NSPAWN rm -f e2e-quiet >/dev/null || fail "rm e2e-quiet"
# The caller of an attached run goes: the machine goes on, what it writes read and
# dropped by the service, which stays up for it past its idle time.
$NSPAWN run --log-driver none --name e2e-drain $bb -- /bin/sh -c 'i=0; while true; do i=$((i+1)); echo line-$i-0123456789012345678901234567890123456789; echo $i > /tmp/c.new; mv /tmp/c.new /tmp/count; done' >/dev/null 2>&1 &
run_pid=$!
retry 10 bash -c "$NSPAWN ps | grep_q '^ *e2e-drain '" || fail "the attached run of e2e-drain did not start"
sleep 2
kill -9 $run_pid
wait $run_pid 2>/dev/null
sleep 70
count() { $NSPAWN exec e2e-drain -- cat /tmp/count </dev/null 2>/dev/null | tr -d '\r'; }
c1=$(count); sleep 2; c2=$(count)
[ -n "$c1" ] && [ -n "$c2" ] && [ "$c2" -gt "$c1" ] || fail "the program stalled once the caller of its attached run went: $c1 then $c2"
$NSPAWN rm -f e2e-drain >/dev/null || fail "rm e2e-drain"

step "--interface: a host interface inside the machine while it runs, back on the host when it stops"
ip link add e2e-dummy0 type dummy || fail "make a dummy interface"
ip link set e2e-dummy0 up && ip route add default dev e2e-dummy0 metric 9999 || fail "route through the dummy interface"
# An app on the bridge: the hooks move it into the namespace nspawn prepares, so the
# program finds it from its first instruction; a default route through it is a warning.
out=$($NSPAWN start $app --interface e2e-dummy0 -- /bin/sh -c 'ip -o link show e2e-dummy0; exec /bin/sleep 300' 2>&1) || fail "start with --interface: $out"
echo "$out" | grep_q "warning: e2e-dummy0 carries the host's default route" || fail "the default route through the interface was not warned about: $out"
retry 10 bash -c "$NSPAWN logs $app | tr -d '\r' | grep -q 'e2e-dummy0: <'" || fail "the program did not find the interface inside from the start: $($NSPAWN logs $app 2>&1)"
[ ! -e /sys/class/net/e2e-dummy0 ] || fail "the interface is still on the host while the machine runs"
$NSPAWN exec $app -- ip -o link show e2e-dummy0 </dev/null | tr -d '\r' | grep_q "e2e-dummy0: <" || fail "the interface is not inside the running machine"
# An interface given to an app keeps CAP_NET_ADMIN, so the app configures it.
$NSPAWN exec $app -- ip link set e2e-dummy0 up </dev/null 2>&1 | tr -d '\r' | grep -v "^$" && fail "the app cannot bring its interface up"
$NSPAWN exec $app -- ip -o link show e2e-dummy0 </dev/null | tr -d '\r' | grep_q ",UP" || fail "the interface did not come up inside the app: $($NSPAWN exec $app -- ip -o link show e2e-dummy0 </dev/null 2>&1)"
$NSPAWN inspect $app | python3 -c "import json,sys; d = json.load(sys.stdin)[0]; assert d['interfaces'] == ['e2e-dummy0'], d" || fail "inspect does not show the interface"
grep -q '^Wants=sys-subsystem-net-devices-e2e\\x2ddummy0.device' /etc/systemd/system/systemd-nspawn@$app.service.d/nspawn-hooks.conf || fail "the unit does not wait for the device: $(cat /etc/systemd/system/systemd-nspawn@$app.service.d/nspawn-hooks.conf)"
grep -q "^Interface=" /etc/systemd/nspawn/$app.nspawn && fail "an app on the bridge got Interface= in its settings"
$NSPAWN create $app e2e-phys-b --interface e2e-dummy0 -- /bin/sleep 300 >/dev/null || fail "create a second machine naming the interface"
out=$($NSPAWN start e2e-phys-b 2>&1) && fail "a second machine took the interface of a running one"
echo "$out" | grep_q "is set for $app, which is running" || fail "the refused second machine was not explained: $out"
$NSPAWN stop $app >/dev/null || fail "stop the machine with the interface"
[ -e /sys/class/net/e2e-dummy0 ] || fail "the interface did not come back to the host with stop"
ip netns list | grep_q -E "^nspawn-$app( |$)" && fail "the namespace was left behind"
# Killed, the release hook still gives it back: the namespace name keeps the namespace
# alive until the hook is done with it.
$NSPAWN start $app >/dev/null || fail "start again with the remembered interface"
[ ! -e /sys/class/net/e2e-dummy0 ] || fail "the remembered interface was not moved on the second start"
$NSPAWN stop $app --force >/dev/null || fail "stop --force the app with the interface"
[ -e /sys/class/net/e2e-dummy0 ] || fail "the release hook did not give the interface back after stop --force"
$NSPAWN start e2e-phys-b >/dev/null || fail "start the second machine once the first stopped"
[ ! -e /sys/class/net/e2e-dummy0 ] || fail "the second machine did not take the interface"
$NSPAWN rm -f e2e-phys-b >/dev/null || fail "rm -f the machine holding the interface"
retry 5 test -e /sys/class/net/e2e-dummy0 || fail "rm -f did not give the interface back"
# --network none: systemd-nspawn moves it itself (Interface=) and gives it back at exit.
$NSPAWN start $app --network none --interface e2e-dummy0 -- /bin/sh -c 'ip -o link show e2e-dummy0; exec /bin/sleep 300' >/dev/null || fail "start with --network none and --interface"
grep -q "^Interface=e2e-dummy0" /etc/systemd/nspawn/$app.nspawn || fail "the settings file lacks Interface=: $(cat /etc/systemd/nspawn/$app.nspawn)"
retry 10 bash -c "$NSPAWN logs $app | tr -d '\r' | grep -q 'e2e-dummy0: <'" || fail "with --network none the program did not find the interface: $($NSPAWN logs $app 2>&1)"
[ ! -e /sys/class/net/e2e-dummy0 ] || fail "with --network none the interface stayed on the host"
$NSPAWN stop $app >/dev/null || fail "stop the --network none machine"
retry 5 test -e /sys/class/net/e2e-dummy0 || fail "systemd-nspawn did not give the interface back"
# A booted machine on the bridge: Interface= beside Bridge=; its own user namespace owns
# the network namespace, so its root configures the interface.
$NSPAWN pull "$IMAGE" --name e2e-phys-boot --backend overlay --force >/dev/null || fail "pull e2e-phys-boot"
$NSPAWN start e2e-phys-boot --interface e2e-dummy0 >/dev/null || fail "start a booted machine with --interface"
grep -q "^Bridge=" /etc/systemd/nspawn/e2e-phys-boot.nspawn && grep -q "^Interface=e2e-dummy0" /etc/systemd/nspawn/e2e-phys-boot.nspawn || fail "the booted machine's settings lack Bridge= with Interface=: $(cat /etc/systemd/nspawn/e2e-phys-boot.nspawn)"
[ ! -e /sys/class/net/e2e-dummy0 ] || fail "the booted machine did not take the interface"
$NSPAWN exec e2e-phys-boot -- ip link set e2e-dummy0 up </dev/null || fail "the booted machine cannot bring the interface up"
$NSPAWN exec e2e-phys-boot -- ip -o link show e2e-dummy0 </dev/null | tr -d '\r' | grep_q ",UP" || fail "the interface is not up inside the booted machine: $($NSPAWN exec e2e-phys-boot -- ip -o link show e2e-dummy0 </dev/null 2>&1)"
retry 10 bash -c "$NSPAWN exec e2e-phys-boot -- ip -4 -o addr show host0 </dev/null | tr -d '\r' | grep -q ' 10.99.0.'" || fail "the booted machine lost its bridge address to the interface: $($NSPAWN exec e2e-phys-boot -- ip -4 -o addr show </dev/null 2>&1)"
$NSPAWN stop e2e-phys-boot >/dev/null || fail "stop the booted machine"
retry 5 test -e /sys/class/net/e2e-dummy0 || fail "the booted machine did not give the interface back"
$NSPAWN start e2e-phys-boot >/dev/null || fail "start the booted machine again (the interface is remembered)"
[ ! -e /sys/class/net/e2e-dummy0 ] || fail "the remembered interface was not moved on the second start"
# Not stop --force here: killed, systemd-nspawn gives nothing back, and the kernel
# destroys a virtual interface like this one with the namespace (a physical one and a
# wifi phy come back to the host).
$NSPAWN stop e2e-phys-boot >/dev/null || fail "stop the booted machine again"
retry 5 test -e /sys/class/net/e2e-dummy0 || fail "the booted machine did not give the remembered interface back"
$NSPAWN rm e2e-phys-boot >/dev/null || fail "rm e2e-phys-boot"
# What is refused: names the kernel takes not, nspawn's own interfaces, one that is not
# there, and machines without a namespace of their own.
for bad in lo a/b nspawn0 e2e-missing0; do
  out=$($NSPAWN start $app --network bridge --interface $bad -- /bin/sleep 1 2>&1) && fail "--interface $bad was accepted"
done
echo "$out" | grep_q "is not on the host" || fail "a missing interface was not explained: $out"
out=$($NSPAWN start $app --network host --interface e2e-dummy0 -- /bin/sleep 1 2>&1) && fail "--interface was accepted with --network host"
echo "$out" | grep_q "shares the host's network" || fail "--network host was not explained: $out"
$NSPAWN start $app --network bridge --interface none -- /bin/sleep 300 >/dev/null || fail "start with --interface none"
$NSPAWN inspect $app | python3 -c "import json,sys; d = json.load(sys.stdin)[0]; assert d['interfaces'] == [], d" || fail "--interface none did not forget the interface"
[ -e /sys/class/net/e2e-dummy0 ] || fail "--interface none still moved the interface"
$NSPAWN create $app e2e-phys-b --network container:$app --interface e2e-dummy0 -- /bin/sleep 1 >/dev/null || fail "create with container: and --interface"
out=$($NSPAWN start e2e-phys-b 2>&1) && fail "--interface was accepted with --network container:"
echo "$out" | grep_q "give the interface to $app" || fail "--network container: was not explained: $out"
$NSPAWN rm e2e-phys-b >/dev/null || fail "rm e2e-phys-b"
$NSPAWN stop $app >/dev/null || fail "stop"
ip link del e2e-dummy0 || fail "delete the dummy interface"
# A wireless interface moves with its whole phy: iw on the host for an app on the
# bridge, systemd-nspawn itself since 256 where it makes the namespace.
if modprobe mac80211_hwsim radios=1 2>/dev/null; then
  wl=$(ls /sys/devices/virtual/mac80211_hwsim/hwsim0/net/ | head -n 1)
  phy=$(cat /sys/class/net/$wl/phy80211/name)
  [ -n "$wl" ] && [ -n "$phy" ] || fail "no interface of the simulated radio: $wl $phy"
  $NSPAWN start $app --interface $wl -- /bin/sh -c "cat /sys/class/net/$wl/phy80211/name; exec /bin/sleep 300" >/dev/null || fail "start with a wireless interface"
  retry 10 bash -c "$NSPAWN logs $app | tr -d '\r' | grep -qx '$phy'" || fail "the phy is not inside the app: $($NSPAWN logs $app 2>&1)"
  [ ! -e /sys/class/net/$wl ] || fail "the wireless interface stayed on the host"
  $NSPAWN stop $app >/dev/null || fail "stop the app with the wireless interface"
  retry 5 test -e /sys/class/net/$wl || fail "the wireless interface did not come back"
  [ "$(cat /sys/class/net/$wl/phy80211/name)" = "$phy" ] || fail "the wireless interface came back on another phy: $(cat /sys/class/net/$wl/phy80211/name)"
  if [ "$systemd_major" -ge 256 ]; then
    $NSPAWN start $app --network none --interface $wl -- /bin/sh -c "cat /sys/class/net/$wl/phy80211/name; exec /bin/sleep 300" >/dev/null || fail "start with a wireless interface through Interface="
    retry 10 bash -c "$NSPAWN logs $app | tr -d '\r' | grep -qx '$phy'" || fail "the phy is not inside through Interface=: $($NSPAWN logs $app 2>&1)"
    [ ! -e /sys/class/net/$wl ] || fail "through Interface= the wireless interface stayed on the host"
    $NSPAWN stop $app >/dev/null || fail "stop the --network none machine with the wireless interface"
    retry 5 test -e /sys/class/net/$wl || fail "systemd-nspawn did not give the wireless interface back"
  else
    out=$($NSPAWN start $app --network none --interface $wl -- /bin/sleep 1 2>&1) && fail "a wireless interface through Interface= was accepted on systemd $systemd_major"
    echo "$out" | grep_q "takes systemd 256 or newer" || fail "the systemd version was not explained: $out"
  fi
  $NSPAWN start $app --network bridge --interface none -- /bin/sleep 300 >/dev/null || fail "forget the wireless interface"
  $NSPAWN stop $app >/dev/null || fail "stop"
  rmmod mac80211_hwsim || fail "unload mac80211_hwsim"
else
  echo "mac80211_hwsim cannot be loaded here: the wireless part is skipped"
fi

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
# A signal right after a start goes to the program, never to systemd-nspawn's child that
# becomes it: stopped there, the program would not run.
$NSPAWN start $app --restart no -p none -- /bin/sleep 300 >/dev/null || fail "start an app to signal at once"
$NSPAWN kill -s STOP $app >/dev/null || fail "kill -s STOP right after a start"
leader=$(machinectl show $app -p Leader --value)
child=$(cut -d' ' -f1 /proc/$leader/task/$leader/children)
[ "$(cat /proc/$child/comm)" = sleep ] && retry 3 grep -q '^State:.*stopped' /proc/$child/status || fail "a signal right after a start missed the program: $(cat /proc/$child/comm) $(grep ^State: /proc/$child/status)"
$NSPAWN kill -s CONT $app >/dev/null || fail "kill -s CONT"
$NSPAWN stop $app >/dev/null || fail "stop the app signalled at once"
# The program says when its trap is set: a signal before that would end it.
trapper="trap 'echo e2e-got-hup-$nonce' HUP; echo e2e-trap-$nonce; while :; do sleep 1; done"
$NSPAWN start $app --restart always -p none -- /bin/sh -c "$trapper" >/dev/null || fail "start an app that traps SIGHUP"
retry 10 bash -c "$NSPAWN logs $app | grep_q e2e-trap-$nonce" || fail "the program did not set its trap"
[ "$($NSPAWN kill -s HUP $app)" = "$app" ] || fail "kill -s HUP does not print the machine's name"
retry 5 bash -c "$NSPAWN logs $app | grep_q e2e-got-hup-$nonce" || fail "the program did not get SIGHUP"
$NSPAWN ps | grep_q "^ *$app " || fail "SIGHUP, which the program handles, ended the machine"
# As docker's kill: without a stop signal of its own, any signal counts as a stop, so
# the program that SIGUSR1 ends is not brought back by --restart always.
$NSPAWN kill -s USR1 $app >/dev/null || fail "kill -s USR1"
sleep 5
# The program died of the signal, so the unit may end failed; it must not come back.
case "$(systemctl is-active systemd-nspawn@$app.service)" in
  active|activating) fail "kill -s USR1 of a machine without a stop signal did not end it for good" ;;
esac
[ -e /var/lib/nspawn/machines/$app/exit-on-next ] && fail "the release hook left the exit-on-next mark behind"
# With a stop signal of its own, another signal leaves it to the policy, and that one
# ends the machine for good.
$NSPAWN start $app --restart always --stop-signal SIGTERM -p none -- /bin/sh -c "$trapper" >/dev/null || fail "start an app with a stop signal of its own"
retry 10 bash -c "$NSPAWN logs $app | grep_q e2e-trap-$nonce" || fail "the program did not set its trap again"
restarts=$(systemctl show -p NRestarts --value systemd-nspawn@$app.service)
$NSPAWN kill -s USR1 $app >/dev/null || fail "kill -s USR1 with a stop signal"
retry 15 bash -c "[ \"\$(systemctl show -p NRestarts --value systemd-nspawn@$app.service)\" -gt $restarts ]" || fail "a program ended by kill -s USR1 was not restarted by its policy"
retry 15 bash -c "$NSPAWN ps | grep_q '^ *$app '" || fail "the app did not come back after SIGUSR1"
$NSPAWN kill -s TERM $app >/dev/null || fail "kill with the stop signal"
sleep 5
case "$(systemctl is-active systemd-nspawn@$app.service)" in
  active|activating) fail "the stop signal sent by kill did not end the machine for good" ;;
esac
[ -e /var/lib/nspawn/machines/$app/exit-on-next ] && fail "the release hook left the exit-on-next mark behind"
$NSPAWN start $app --stop-signal "" >/dev/null || fail "start after kill -s TERM"
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

step "s6-overlay images: the program as PID 1, and --init either way"
# linuxserver.io's images start s6-overlay's /init, which refuses to run unless it is PID 1;
# the Alpine base also carries busybox's /sbin/init, which must not make it a booted image.
s6dir=/tmp/e2e-s6-init
rm -rf $s6dir && mkdir -p $s6dir
printf '#!/bin/sh\necho e2e-s6-cont-init-%s\n' "$nonce" > $s6dir/10-marker
chmod 0755 $s6dir/10-marker
pid1_of() { $NSPAWN exec "$1" -- cat /proc/1/comm </dev/null 2>/dev/null | tr -d '\r'; }
field_of() { $NSPAWN inspect "$1" | python3 -c "import json,sys; print(json.load(sys.stdin)[0].get('$2'))"; }
# lscr.io answers 429 now and then when the VMs pull at once; a refused pull makes nothing.
retry 3 $NSPAWN run -d --name e2e-s6 -e PUID=1234 -e PGID=1234 -v $s6dir:/custom-cont-init.d:ro lscr.io/linuxserver/baseimage-alpine:3.22 >/dev/null || fail "run of a linuxserver image"
$NSPAWN ps | grep -q "^ *e2e-s6 .* app " || fail "a linuxserver image was not taken for an app: $($NSPAWN ps)"
grep -q '^ProcessTwo' /etc/systemd/nspawn/e2e-s6.nspawn && fail "an s6-overlay image runs under the stub init"
grep -q '^KillSignal=SIGTERM$' /etc/systemd/nspawn/e2e-s6.nspawn || fail "a PID 1 program does not get SIGTERM when its unit stops: $(cat /etc/systemd/nspawn/e2e-s6.nspawn)"
[ "$(field_of e2e-s6 pid1)" = True ] || fail "inspect does not say the program is PID 1"
retry 15 bash -c "[ \"\$($NSPAWN exec e2e-s6 -- cat /proc/1/comm </dev/null 2>/dev/null | tr -d '\r')\" = s6-svscan ]" || fail "s6-svscan is not PID 1: $(pid1_of e2e-s6); $($NSPAWN logs e2e-s6 2>&1 | tail -5)"
retry 15 bash -c "$NSPAWN logs e2e-s6 | grep_q e2e-s6-cont-init-$nonce" || fail "the custom init script did not run: $($NSPAWN logs e2e-s6 | tail -20)"
$NSPAWN logs e2e-s6 | grep_q "can only run as pid 1" && fail "s6 refused to run"
[ "$($NSPAWN exec e2e-s6 -- id -u abc </dev/null | tr -d '\r')" = 1234 ] || fail "PUID did not reach the image's user"
out=$($NSPAWN stop e2e-s6 2>&1) || fail "stop of the s6 machine: $out"
echo "$out" | grep_q "ignored" && fail "s6 did not end on its stop signal: $out"
[ "$(field_of e2e-s6 exit_code)" = 0 ] || fail "the s6 machine did not end with 0: $(field_of e2e-s6 exit_code)"
$NSPAWN restart e2e-s6 >/dev/null || fail "restart of the s6 machine"
retry 15 bash -c "[ \"\$($NSPAWN logs e2e-s6 | grep -c e2e-s6-cont-init-$nonce)\" -ge 2 ]" || fail "the s6 machine did not run its init again after a restart"
# A unit stop, as systemctl or a shutdown gives it, reaches s6 as SIGTERM too.
systemctl stop systemd-nspawn@e2e-s6.service || fail "systemctl stop of the s6 machine"
[ "$(field_of e2e-s6 exit_code)" = 0 ] || fail "a unit stop did not end s6 cleanly: $(field_of e2e-s6 exit_code)"
# --init puts the program under the stub init, where s6 refuses; --init=false gives it back.
$NSPAWN start e2e-s6 --init >/dev/null 2>&1
retry 10 bash -c "$NSPAWN logs e2e-s6 | grep_q 'can only run as pid 1'" || fail "--init did not put s6 under the stub init"
retry 10 bash -c "! $NSPAWN ps | grep_q '^ *e2e-s6 '" || $NSPAWN stop e2e-s6 --force >/dev/null 2>&1
[ "$(field_of e2e-s6 pid1)" = False ] || fail "inspect still says PID 1 after --init"
$NSPAWN start e2e-s6 --init=false >/dev/null || fail "start --init=false"
retry 15 bash -c "[ \"\$($NSPAWN exec e2e-s6 -- cat /proc/1/comm </dev/null 2>/dev/null | tr -d '\r')\" = s6-svscan ]" || fail "--init=false did not make s6 PID 1 again"
$NSPAWN generate e2e-s6 | grep_q -- '--init=false' || fail "generate does not keep --init=false: $($NSPAWN generate e2e-s6)"
$NSPAWN stop e2e-s6 >/dev/null || fail "stop after --init=false"
retry 3 $NSPAWN run -d --name e2e-s6u lscr.io/linuxserver/baseimage-ubuntu:noble >/dev/null || fail "run of the Ubuntu linuxserver base"
retry 15 bash -c "[ \"\$($NSPAWN exec e2e-s6u -- cat /proc/1/comm </dev/null 2>/dev/null | tr -d '\r')\" = s6-svscan ]" || fail "s6-svscan is not PID 1 in the Ubuntu base: $($NSPAWN logs e2e-s6u 2>&1 | tail -5)"
out=$($NSPAWN stop e2e-s6u 2>&1) || fail "stop of the Ubuntu s6 machine: $out"
echo "$out" | grep_q "ignored" && fail "s6 of the Ubuntu base did not end on its stop signal: $out"
$NSPAWN images rm e2e-s6 e2e-s6u >/dev/null || fail "images rm of the s6 machines"
rm -rf $s6dir
# Any app as PID 1: a program without a handler ignores SIGTERM there, as under docker, and
# stop kills it after its timeout.
$NSPAWN create $app e2e-pid1 --init=false -- /bin/sleep 300 >/dev/null || fail "create --init=false"
$NSPAWN start e2e-pid1 >/dev/null || fail "start of a PID 1 program"
retry 10 bash -c "[ \"\$($NSPAWN exec e2e-pid1 -- cat /proc/1/comm </dev/null 2>/dev/null | tr -d '\r')\" = sleep ]" || fail "sleep is not PID 1: $(pid1_of e2e-pid1)"
out=$($NSPAWN stop e2e-pid1 -t 2 2>&1) || fail "stop of a PID 1 program: $out"
echo "$out" | grep_q "ignored SIGTERM for 2 seconds; killing it" || fail "a PID 1 program without a handler was not killed after its timeout: $out"
[ "$(field_of e2e-pid1 exit_code)" = 137 ] || fail "a killed PID 1 program does not report 137: $(field_of e2e-pid1 exit_code)"
$NSPAWN start e2e-pid1 -- /bin/sh -c 'trap "exit 7" TERM; while :; do sleep 1; done' >/dev/null || fail "start of a PID 1 program that traps TERM"
retry 10 bash -c "[ \"\$($NSPAWN exec e2e-pid1 -- cat /proc/1/comm </dev/null 2>/dev/null | tr -d '\r')\" = sh ]" || fail "sh is not PID 1"
$NSPAWN kill -s TERM e2e-pid1 >/dev/null || fail "kill -s TERM of a PID 1 program"
retry 10 bash -c "! $NSPAWN ps | grep_q '^ *e2e-pid1 '" || fail "a PID 1 program that traps TERM did not end on kill"
[ "$(field_of e2e-pid1 exit_code)" = 7 ] || fail "the exit code of a PID 1 program: $(field_of e2e-pid1 exit_code)"
$NSPAWN images rm e2e-pid1 >/dev/null || fail "images rm e2e-pid1"

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
# Every event of a kind carries the same metadata, whatever the action: what an event
# says of a volume or an image must not depend on the record still being there.
image = "docker.io/library/busybox:latest"
for e in events:
    if e["type"] == "machine" and e["name"] == app:
        assert e["attributes"].get("image") == image, e
    if e["type"] == "volume" and e["name"] == "e2evol-events":
        assert e["attributes"].get("path") == "/var/lib/nspawn/volumes/e2evol-events", e
assert all(e["name"] != "e2e-forged" for e in events), "a forged entry was reported"
assert all(e["time"].endswith("Z") for e in events)
PY
out=$(timeout 30 $NSPAWN events --since "$since" --until now --filter name=$app --filter event=die) || fail "events --since --until did not end by itself: $out"
echo "$out" | grep_q "machine die $app (.*exit_code=3" || fail "events --since --until misses the exit: $out"
out=$(timeout 30 $NSPAWN events --since 10m --until 0s --filter name=$app --filter event=die) || fail "events --since 10m --until 0s did not end by itself: $out"
echo "$out" | grep_q "machine die $app (.*exit_code=3" || fail "events with a duration ago misses the exit: $out"
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
# --memory-swap is memory and swap together: equal to --memory leaves no swap, -1 no
# bound on it, and a total without --memory is refused.
$NSPAWN start $app -m 64m --memory-swap 64m -- /bin/sleep 300 >/dev/null || fail "start with --memory-swap equal to --memory"
[ "$(cat $cg/memory.swap.max)" = 0 ] || fail "--memory-swap equal to --memory left swap: $(cat $cg/memory.swap.max)"
$NSPAWN inspect $app | python3 -c "import json,sys; d = json.load(sys.stdin)[0]; assert d['memory_swap'] == 67108864, d" || fail "inspect does not show --memory-swap"
$NSPAWN stop $app >/dev/null || fail "stop the machine without swap"
$NSPAWN start $app --memory-swap -1 -- /bin/sleep 300 >/dev/null || fail "start with --memory-swap -1"
[ "$(cat $cg/memory.max)" = 67108864 ] || fail "--memory-swap -1 changed memory.max: $(cat $cg/memory.max)"
[ "$(cat $cg/memory.swap.max)" = max ] || fail "--memory-swap -1 did not lift the bound on swap: $(cat $cg/memory.swap.max)"
$NSPAWN stop $app >/dev/null || fail "stop the machine with unbounded swap"
out=$($NSPAWN start $app -m 0 --memory-swap 1g -- /bin/sleep 300 2>&1) && fail "--memory-swap without --memory was accepted"
echo "$out" | grep_q "needs --memory" || fail "the refusal of --memory-swap without --memory reads wrong: $out"
out=$($NSPAWN start $app -m 64m --memory-swap 32m -- /bin/sleep 300 2>&1) && fail "--memory-swap below --memory was accepted"
echo "$out" | grep_q "cannot be below --memory" || fail "the refusal of --memory-swap below --memory reads wrong: $out"
$NSPAWN update $app -m 0 --memory-swap 0 >/dev/null || fail "update back to no memory limit"
$NSPAWN inspect $app | python3 -c "import json,sys; d = json.load(sys.stdin)[0]; assert d['memory'] == 0 and d['memory_swap'] == 0, d" || fail "update -m 0 --memory-swap 0 was not remembered"

step "update: the limits of a running machine change at once, the policy with them"
$NSPAWN start $app -m 64m -- /bin/sleep 300 >/dev/null || fail "start before update"
[ "$($NSPAWN update $app -m 128m --cpus 0.5 --pids-limit 50)" = "$app" ] || fail "update of a running machine"
[ "$(cat $cg/memory.max)" = 134217728 ] || fail "update did not change memory.max at once: $(cat $cg/memory.max)"
[ "$(cat $cg/memory.swap.max)" = 134217728 ] || fail "update did not change memory.swap.max at once: $(cat $cg/memory.swap.max)"
$NSPAWN update $app --memory-swap -1 >/dev/null || fail "update --memory-swap -1"
[ "$(cat $cg/memory.swap.max)" = max ] || fail "update --memory-swap -1 did not lift the bound on swap at once: $(cat $cg/memory.swap.max)"
$NSPAWN update $app --memory-swap 0 >/dev/null || fail "update --memory-swap 0"
[ "$(cat $cg/memory.swap.max)" = 134217728 ] || fail "update --memory-swap 0 did not go back to as much swap as memory: $(cat $cg/memory.swap.max)"
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
# A probe that says more than the 4 KiB kept is drained rather than cut off: cut off, it
# would die of SIGPIPE and count as a failure.
$NSPAWN update $app --health-cmd "yes | head -c 300000; test -f /ok" >/dev/null || fail "update to a chatty probe"
chatty_probe_ok() {
  $NSPAWN inspect $app | python3 -c "
import json, sys
d = json.load(sys.stdin)[0]
probes = [p.split(' ', 2) for p in d['health_log']]
assert d['health'] == 'healthy' and probes and probes[-1][1] == '0' and len(probes[-1][2]) == 4096, (d['health'], [(p[1], len(p[2])) for p in probes])"
}
retry 10 chatty_probe_ok 2>/dev/null || fail "a chatty probe was not drained: $($NSPAWN inspect $app | python3 -c 'import json, sys; d = json.load(sys.stdin)[0]; print(d[\"health\"], [(p.split(\" \", 2)[1], len(p.split(\" \", 2)[2])) for p in d[\"health_log\"]])')"
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

step "--depends-on: other machines started first and waited for, by nspawn start and by systemd alike"
dep_since=$(date +%s)
dropin=/etc/systemd/system/systemd-nspawn@e2e-dep-web.service.d/nspawn-hooks.conf
$NSPAWN create $app e2e-dep-db --health-cmd "test -f /ok" --health-interval 1s --health-retries 100 -- /bin/sleep 300 >/dev/null || fail "create the machine depended on"
$NSPAWN create $app e2e-dep-job -- /bin/sh -c "echo job-ran-$nonce" >/dev/null || fail "create the machine that completes"
$NSPAWN create $app e2e-dep-web --depends-on e2e-dep-db:healthy --depends-on e2e-dep-job:completed -- /bin/sleep 300 >/dev/null || fail "create a machine with dependencies"
$NSPAWN inspect e2e-dep-web | python3 -c "import json,sys; d = json.load(sys.stdin)[0]; assert d['depends_on'] == ['e2e-dep-db:healthy', 'e2e-dep-job:completed'], d['depends_on']" || fail "inspect does not show the dependencies"
# The start goes on as long as e2e-dep-db is not healthy, both started by systemd.
$NSPAWN start e2e-dep-web > /tmp/e2e-dep-start.txt 2>&1 &
dep_start=$!
retry 10 bash -c "journalctl -u systemd-nspawn@e2e-dep-web.service --since @$dep_since | grep -q 'e2e-dep-web waits for e2e-dep-db to be healthy'" || fail "the hook does not say what it waits for: $(journalctl -u systemd-nspawn@e2e-dep-web.service --since @$dep_since | tail -5)"
$NSPAWN ps | grep_q "^ *e2e-dep-db .* running" || fail "the machine depended on was not started: $($NSPAWN ps)"
sleep 2
[ "$(systemctl show -p ActiveState --value systemd-nspawn@e2e-dep-web.service)" = activating ] || fail "the machine did not wait for its dependency: $(systemctl show -p ActiveState --value systemd-nspawn@e2e-dep-web.service)"
machinectl show e2e-dep-web >/dev/null 2>&1 && fail "the machine ran before its dependency was healthy"
$NSPAWN exec e2e-dep-db -- touch /ok </dev/null || fail "touch /ok in the machine depended on"
wait $dep_start || fail "start of a machine with dependencies: $(cat /tmp/e2e-dep-start.txt)"
cat /tmp/e2e-dep-start.txt
grep_q "e2e-dep-db started first: e2e-dep-web depends on it" /tmp/e2e-dep-start.txt || fail "start does not say it started e2e-dep-db first"
grep_q "e2e-dep-job ran first: e2e-dep-web waits for it to complete" /tmp/e2e-dep-start.txt || fail "start does not say it ran e2e-dep-job first"
$NSPAWN ps | grep_q "^ *e2e-dep-web .* running" || fail "the machine with dependencies is not running"
retry 5 bash -c "$NSPAWN logs e2e-dep-job | grep -q job-ran-$nonce" || fail "the machine that completes did not run"
# A run that ended well counts: neither a start again nor a restart runs the job twice,
# as compose runs a job once. The unit is only ordered after it.
grep -q "^Wants=systemd-nspawn@e2e-dep-job.service$" $dropin && fail "the unit wants a machine that completes, which runs it at every start"
$NSPAWN stop e2e-dep-web >/dev/null || fail "stop the machine with dependencies"
$NSPAWN start e2e-dep-web > /tmp/e2e-dep-start.txt 2>&1 || fail "start again after the job completed: $(cat /tmp/e2e-dep-start.txt)"
grep_q "e2e-dep-job ran first" /tmp/e2e-dep-start.txt && fail "a start ran a job that had completed again"
$NSPAWN restart e2e-dep-web >/dev/null || fail "restart the machine with dependencies"
systemctl restart systemd-nspawn@e2e-dep-web.service || fail "systemctl restart of the machine with dependencies"
retry 5 bash -c "$NSPAWN ps | grep -q '^ *e2e-dep-web .* running'" || fail "the machine with dependencies does not run after its restarts"
[ "$($NSPAWN logs e2e-dep-job | grep -c job-ran-$nonce)" = 1 ] || fail "the job ran again: $($NSPAWN logs e2e-dep-job)"
grep -q "^Wants=systemd-nspawn@e2e-dep-db.service$" $dropin && grep -q "^After=systemd-nspawn@e2e-dep-job.service$" $dropin && grep -q "await-dependencies %i$" $dropin && grep -q "^TimeoutStartSec=infinity$" $dropin || fail "the drop-in lacks the dependencies: $(cat $dropin)"
# Stopping a dependency leaves the machine alone; removing it is refused.
$NSPAWN stop e2e-dep-db >/dev/null || fail "stop the machine depended on"
$NSPAWN ps | grep_q "^ *e2e-dep-web .* running" || fail "stopping a dependency stopped the machine that depends on it"
out=$($NSPAWN rm -f e2e-dep-db 2>&1) && fail "a machine another one depends on was removed"
echo "$out" | grep_q "e2e-dep-web depends on e2e-dep-db" || fail "the refused removal was not explained: $out"
# Named together, a dependency goes only once what depends on it has: e2e-dep-web runs,
# so rm without -f keeps it, and e2e-dep-db with it.
out=$($NSPAWN rm e2e-dep-db e2e-dep-web 2>&1) && fail "rm of a running machine and its dependency succeeded"
echo "$out" | grep_q "e2e-dep-web stays and needs e2e-dep-db" || fail "the kept dependency was not explained: $out"
$NSPAWN images ls | grep_q "^ *e2e-dep-db " || fail "rm removed the dependency of a machine it kept"
# What is refused before anything changes.
out=$($NSPAWN start e2e-dep-db --depends-on e2e-dep-web 2>&1) && fail "a cycle of dependencies was accepted"
echo "$out" | grep_q "cycle, e2e-dep-db -> e2e-dep-web -> e2e-dep-db" || fail "the cycle was not explained: $out"
out=$($NSPAWN create $app e2e-dep-x --depends-on e2e-dep-x -- /bin/true 2>&1) && fail "a machine depending on itself was created"
echo "$out" | grep_q "cannot depend on itself" || fail "the self dependency was not explained: $out"
out=$($NSPAWN create $app e2e-dep-x --depends-on e2e-nowhere -- /bin/true 2>&1) && fail "a dependency on no machine was accepted"
echo "$out" | grep_q "no machine of nspawn's" || fail "the missing dependency was not explained: $out"
out=$($NSPAWN create $app e2e-dep-x --depends-on e2e-dep-db:ready -- /bin/true 2>&1) && fail "an unknown condition was accepted"
echo "$out" | grep_q "started, healthy or completed" || fail "the unknown condition was not explained: $out"
$NSPAWN images ls | grep_q "^ *e2e-dep-x " && fail "a refused create left a machine"
# A dependency that does not complete fails the start with its exit code, unless optional.
$NSPAWN stop e2e-dep-web >/dev/null || fail "stop the machine with dependencies"
$NSPAWN start e2e-dep-job -- /bin/sh -c "exit 3" >/dev/null 2>&1
out=$($NSPAWN start e2e-dep-web 2>&1) && fail "a machine started although a dependency failed"
echo "$out" | grep_q "e2e-dep-web did not start: e2e-dep-job, which e2e-dep-web waits for to complete, exited with 3" || fail "the failed dependency was not explained: $out"
$NSPAWN start e2e-dep-web --depends-on e2e-dep-db:healthy --depends-on e2e-dep-job:completed:optional > /tmp/e2e-dep-start.txt 2>&1 || fail "start with a failing optional dependency: $(cat /tmp/e2e-dep-start.txt)"
grep_q "warning: e2e-dep-job failed to start; it is optional" /tmp/e2e-dep-start.txt || fail "the failed optional dependency was not reported: $(cat /tmp/e2e-dep-start.txt)"
journalctl -u systemd-nspawn@e2e-dep-web.service --since @$dep_since | grep_q "it is optional, so e2e-dep-web starts anyway" || fail "the hook did not warn of the optional dependency"
# systemd alone, as at boot: the unit pulls in what it depends on.
$NSPAWN stop e2e-dep-web e2e-dep-db >/dev/null || fail "stop both"
timeout 120 systemctl start systemd-nspawn@e2e-dep-web.service || fail "systemctl start of a machine with dependencies"
$NSPAWN ps | grep_q "^ *e2e-dep-db .* running" || fail "systemd did not start the dependency first"
$NSPAWN ps | grep "^ *e2e-dep-db " | grep_q "(healthy)" || fail "systemd started the machine before its dependency was healthy: $($NSPAWN ps)"
retry 5 bash -c "$NSPAWN ps | grep -q '^ *e2e-dep-web .* running'" || fail "the machine started by systemd does not run"
# A start that waits can be stopped: its unit is systemd's by then.
$NSPAWN stop e2e-dep-web >/dev/null || fail "stop the machine started by systemd"
$NSPAWN exec e2e-dep-db -- rm /ok </dev/null || fail "rm /ok"
$NSPAWN restart e2e-dep-db >/dev/null || fail "restart the machine depended on"
retry 5 bash -c "$NSPAWN ps | grep '^ *e2e-dep-db ' | grep -q 'health: starting'" || fail "e2e-dep-db did not start over its healthcheck: $($NSPAWN ps)"
$NSPAWN start e2e-dep-web > /tmp/e2e-dep-start.txt 2>&1 &
dep_start=$!
retry 10 bash -c "[ \"\$(systemctl show -p ActiveState --value systemd-nspawn@e2e-dep-web.service)\" = activating ]" || fail "the machine is not waiting"
out=$(timeout 20 $NSPAWN start e2e-dep-web 2>&1) && fail "a second start of a waiting machine succeeded"
echo "$out" | grep_q "already starting" || fail "the second start was not explained: $out"
$NSPAWN stop e2e-dep-web >/dev/null || fail "stop a machine waiting for its dependencies"
wait $dep_start && fail "a start whose machine was stopped succeeded"
[ "$(systemctl show -p ActiveState --value systemd-nspawn@e2e-dep-web.service)" = activating ] && fail "the waiting start is still there"
# started alone asks for no wait, and none forgets them.
$NSPAWN start e2e-dep-web --depends-on e2e-dep-db >/dev/null || fail "start with a started dependency"
grep -q "^TimeoutStartSec=" $dropin && fail "a started dependency took the time limit off the start"
$NSPAWN stop e2e-dep-web >/dev/null || fail "stop"
$NSPAWN start e2e-dep-web --depends-on none >/dev/null || fail "start with --depends-on none"
grep -q "await-dependencies\|e2e-dep-db" $dropin && fail "--depends-on none left the dependencies in the drop-in: $(cat $dropin)"
$NSPAWN rm -f e2e-dep-db e2e-dep-web e2e-dep-job >/dev/null || fail "rm the machines of the dependencies step"

step "generate: the commands that make machines again, run back to the same machines"
$NSPAWN network create e2e-gn --subnet 10.98.9.0/24 --label team=e2e >/dev/null || fail "network create for generate"
printf 'gen-secret' | $NSPAWN secret create e2e-gen-pw >/dev/null || fail "secret create for generate"
$NSPAWN create $app e2e-gen-db --network e2e-gn --health-cmd true --health-interval 1s -- /bin/sleep 300 >/dev/null || fail "create the machine generate brings along"
$NSPAWN create $app e2e-gen-web --network e2e-gn --network-alias www -p 18097:8080 -e "GREETING=hello there" -e EMPTY= -v e2evol-gen:/data -l "tier=front end" --restart on-failure -m 64m --cpus 0.5 --pids-limit 200 --health-cmd "test -d /data" --health-interval 2s --hostname gen-web --cap-add NET_ADMIN --tmpfs /scratch:size=8m --add-host peer:10.1.1.1 --ulimit nofile=64:128 --stop-signal SIGINT --stop-timeout 4 --secret e2e-gen-pw --depends-on e2e-gen-db:healthy --entrypoint /bin/sh -- -c "echo 'gen ok'; exec /bin/sleep 300" >/dev/null || fail "create a machine with many flags"
gen_keys e2e-gen-web > /tmp/e2e-gen-web.before; gen_keys e2e-gen-db > /tmp/e2e-gen-db.before
$NSPAWN generate e2e-gen-web > /tmp/e2e-gen.sh || fail "generate"
cat /tmp/e2e-gen.sh
grep_q "^# secret e2e-gen-pw is encrypted for this host" /tmp/e2e-gen.sh || fail "generate does not say the secret stays behind"
grep_q "^nspawn network create e2e-gn --subnet 10.98.9.0/24" /tmp/e2e-gen.sh || fail "generate does not make the network"
[ "$(grep -n '^nspawn run -d --name e2e-gen-db ' /tmp/e2e-gen.sh | cut -d: -f1)" -lt "$(grep -n '^nspawn run -d --name e2e-gen-web ' /tmp/e2e-gen.sh | cut -d: -f1)" ] || fail "generate does not make the dependency first"
$NSPAWN generate > /tmp/e2e-gen-all.sh || fail "generate of every machine"
grep_q "^nspawn run -d --name e2e-gen-web " /tmp/e2e-gen-all.sh || fail "generate without names left a machine out"
out=$($NSPAWN generate e2e-nonexistent 2>&1) && fail "generate of a machine nspawn does not have"
echo "$out" | grep_q "is not a machine of nspawn's" || fail "the missing machine was not explained: $out"
# The machines and their network go, the script brings them back.
$NSPAWN rm -f e2e-gen-web e2e-gen-db >/dev/null || fail "rm the machines generate described"
$NSPAWN network rm e2e-gn >/dev/null || fail "network rm e2e-gn"
sed "s#^nspawn #$NSPAWN #" /tmp/e2e-gen.sh | bash > /tmp/e2e-gen-run.txt 2>&1 || fail "the generated script failed: $(cat /tmp/e2e-gen-run.txt)"
$NSPAWN network inspect e2e-gn | python3 -c "import json,sys; d = json.load(sys.stdin)[0]; assert d['subnet'] == '10.98.9.0/24' and d['labels'] == {'team': 'e2e'}, d" || fail "the network came back otherwise"
for m in e2e-gen-web e2e-gen-db; do
  gen_keys $m > /tmp/$m.after
  diff /tmp/$m.before /tmp/$m.after > /tmp/$m.diff || fail "$m came back otherwise: $(cat /tmp/$m.diff)"
done
$NSPAWN ps | grep_q "^ *e2e-gen-web .* running" || fail "the machine generate made again does not run"
retry 5 bash -c "$NSPAWN logs e2e-gen-web | grep -q 'gen ok'" || fail "the command of the machine made again did not run"
$NSPAWN rm -f e2e-gen-web e2e-gen-db >/dev/null || fail "rm the machines made again"
$NSPAWN network rm e2e-gn >/dev/null || fail "network rm e2e-gn"
$NSPAWN secret rm e2e-gen-pw >/dev/null || fail "secret rm e2e-gen-pw"
$NSPAWN volume rm e2evol-gen >/dev/null || fail "volume rm e2evol-gen"
# A machine another one takes its network from runs, flags of its own or not; one that
# depends on it as well gets it started first.
$NSPAWN pull "$IMAGE" --name e2e-gen-own --backend overlay --force >/dev/null || fail "pull the owner of a shared network"
$NSPAWN create $app e2e-gen-side --network container:e2e-gen-own --depends-on e2e-gen-own -- /bin/sleep 300 >/dev/null || fail "create a machine in the owner's network"
$NSPAWN start e2e-gen-side > /tmp/e2e-gen-side.txt 2>&1 || fail "start a machine whose stopped owner it depends on: $(cat /tmp/e2e-gen-side.txt)"
grep_q "e2e-gen-own started first" /tmp/e2e-gen-side.txt || fail "start does not say it started the owner first: $(cat /tmp/e2e-gen-side.txt)"
$NSPAWN ps | grep_q "^ *e2e-gen-own .* running" || fail "the owner was not started first"
$NSPAWN generate e2e-gen-side > /tmp/e2e-gen-side.sh || fail "generate of a machine in another one's network"
cat /tmp/e2e-gen-side.sh
grep_q "^nspawn run -d --name e2e-gen-own " /tmp/e2e-gen-side.sh || fail "the owner without flags of its own is not run"
$NSPAWN rm -f e2e-gen-side e2e-gen-own >/dev/null || fail "rm the machine in a shared network and its owner"
sed "s#^nspawn #$NSPAWN #" /tmp/e2e-gen-side.sh | bash > /tmp/e2e-gen-run.txt 2>&1 || fail "the script of the shared network failed: $(cat /tmp/e2e-gen-run.txt)"
retry 10 bash -c "$NSPAWN ps | grep -q '^ *e2e-gen-side .* running'" || fail "the machine in the shared network does not run again"
$NSPAWN rm -f e2e-gen-side e2e-gen-own >/dev/null || fail "rm the machines of the shared network"

step "restart, pause, unpause and top"
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
# The program comes a moment after the machine's init, which top waits for.
retry 10 bash -c "$NSPAWN top $app > /tmp/e2e-top.txt && grep -q '/bin/sleep 300' /tmp/e2e-top.txt" || fail "top does not list the program: $(cat /tmp/e2e-top.txt)"
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
# volume prune takes every unused volume: it runs only where the unused ones are this
# run's, by their names, since a host may keep volumes of its own. A listing that fails
# counts as one that has them.
own_volumes="e2evol e2evol2 e2evol-free e2evol-home e2evol-etc e2evol-events e2evol-gen"
foreign=$($NSPAWN volume ls --json | python3 -c "import json,sys; own = sys.argv[1].split(); print(' '.join(v['name'] for v in json.load(sys.stdin) if not v['used_by'] and v['name'] not in own))" "$own_volumes") || foreign="the listing failed"
if [ -z "$foreign" ]; then
  $NSPAWN volume prune -f | tee /tmp/e2e-prune.txt || fail "volume prune"
  grep -q "removed e2evol-free" /tmp/e2e-prune.txt || fail "prune left an unused volume"
  [ -d /var/lib/nspawn/volumes/e2evol2 ] || fail "prune removed a volume in use"
else
  echo "unused volumes of the host ($foreign): volume prune -f not run"
  $NSPAWN volume rm e2evol-free >/dev/null || fail "volume rm e2evol-free"
fi
$NSPAWN stop $app >/dev/null || fail "stop the app with e2evol2"
$NSPAWN start $app -v none -- /bin/sleep 300 >/dev/null && $NSPAWN stop $app >/dev/null || fail "start with -v none"
$NSPAWN volume rm e2evol2 e2e-no-such-volume > /tmp/e2e-volrm.txt 2>&1 && fail "volume rm of a missing volume succeeded"
grep -q "removed e2evol2" /tmp/e2e-volrm.txt || fail "volume rm stopped at the missing volume: $(cat /tmp/e2e-volrm.txt)"
grep -q "no volume named e2e-no-such-volume" /tmp/e2e-volrm.txt || fail "a missing volume was not explained: $(cat /tmp/e2e-volrm.txt)"
$NSPAWN volume rm ../images >/dev/null 2>&1 && fail "volume rm reached outside the volumes directory"
# A volume made on first use takes what the image has at its path, owner included:
# busybox's /home belongs to nobody, its /etc has files.
$NSPAWN start $app -v e2evol-home:/home -v e2evol-etc:/etc -- /bin/sleep 300 >/dev/null || fail "start with volumes over image directories"
[ "$(stat -c %u /var/lib/nspawn/volumes/e2evol-home)" = 65534 ] || fail "the volume over /home did not take nobody's ownership: $(stat -c '%u %a' /var/lib/nspawn/volumes/e2evol-home)"
[ -f /var/lib/nspawn/volumes/e2evol-etc/passwd ] || fail "the volume over /etc was not seeded with the image's files"
$NSPAWN exec $app -- sh -c 'grep -q "^root:" /etc/passwd && stat -c %u /home' </dev/null | tr -d '\r' | grep_q "^65534$" || fail "the seeded volumes are not what the machine sees"
$NSPAWN stop $app >/dev/null || fail "stop the app with seeded volumes"
$NSPAWN start $app -v none -- /bin/sleep 300 >/dev/null && $NSPAWN stop $app >/dev/null || fail "start with -v none after the seeded volumes"
$NSPAWN volume rm e2evol-home e2evol-etc >/dev/null || fail "volume rm of the seeded volumes"
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
$NSPAWN events --since "-2min" --until now --filter type=secret | grep_q "secret create e2e-pw (size=7)" || fail "no event for the secret"
$NSPAWN events --since "-2min" --until now --filter type=secret --filter event=remove | grep_q "secret remove e2e-pw (size=7)" || fail "secret remove carries no metadata: $($NSPAWN events --since -2min --until now --filter type=secret | tail -3)"

step "everything at once: the features of 1.3.0, 1.4.0 and 1.10.0 combined, on an app, a sidecar and a booted machine"
$NSPAWN network create e2e-nd --label tier=mix >/dev/null || fail "network create e2e-nd"
$NSPAWN network create e2e-ne --internal >/dev/null || fail "network create e2e-ne"
printf 'mix-secret' | $NSPAWN secret create e2e-pw >/dev/null || fail "secret create for the mix"
mix_since=$(date +%s)
mix_host_ip=$(ip -4 route get 1.1.1.1 | awk '{for (i = 1; i <= NF; i++) if ($i == "src") print $(i + 1); exit}')
addr_on() { $NSPAWN inspect "$1" | python3 -c "import json,sys; print(json.load(sys.stdin)[0]['addresses'].get('$2', ''))"; }
# The app: two networks with aliases, two ports on loopback (the second for the sidecar
# below), a healthcheck, a secret, a read-only root with a tmpfs it writes, a user with a
# group that binds port 80 through a sysctl, a hostname, a stop signal and a restart
# policy, all on one machine.
$NSPAWN create $app e2e-mix-app --network e2e-nd --network e2e-ne --network-alias mixweb --network-alias e2e-ne=inner \
  -p 127.0.0.1:18095:80 -p 127.0.0.1:18097:8080 --health-cmd "wget -qO- http://127.0.0.1/ >/dev/null" --health-interval 1s --health-retries 2 \
  --secret e2e-pw:/run/secrets/token:0444 --read-only --tmpfs /www:size=8m,mode=1777 -u nobody:root \
  --sysctl net.ipv4.ip_unprivileged_port_start=0 --hostname mix-$nonce --add-host peer:10.1.1.9 \
  --stop-signal SIGINT --stop-timeout 2 --restart on-failure -l role=mix \
  -- /bin/sh -c 'cat /run/secrets/token > /www/index.html; exec /bin/httpd -f -p 80 -h /www' >/dev/null || fail "create the mixed app"
$NSPAWN start e2e-mix-app >/dev/null || fail "start the mixed app"
mx() { $NSPAWN exec -u root e2e-mix-app -- /bin/sh -c "$1" </dev/null 2>/dev/null | tr -d '\r'; }
retry 10 bash -c "$NSPAWN ps | grep '^ *e2e-mix-app ' | grep_q '(healthy)'" || fail "the mixed app is not healthy: $($NSPAWN ps | grep e2e-mix-app; $NSPAWN logs e2e-mix-app -n 5)"
retry 5 bash -c "curl -sf -m 2 http://127.0.0.1:18095/ | grep_q mix-secret" || fail "the port on loopback does not serve the secret: $(curl -s -m 2 http://127.0.0.1:18095/)"
curl -sf -m 2 "http://$mix_host_ip:18095/" >/dev/null 2>&1 && fail "the port published on 127.0.0.1 answered on $mix_host_ip"
[ "$(mx hostname)" = "mix-$nonce" ] || fail "hostname of the mixed app: $(mx hostname)"
mx 'cat /etc/hosts' | grep_q "^10.1.1.9 peer$" || fail "--add-host missing in the mixed app: $(mx 'cat /etc/hosts')"
mx 'cat /etc/hosts' | grep_q "mixweb" || fail "the app's own alias is not in its hosts file: $(mx 'cat /etc/hosts')"
mx 'cat /etc/hosts' | grep_q "inner" || fail "the app's alias on the internal network is not in its hosts file: $(mx 'cat /etc/hosts')"
[ "$(mx 'touch /x 2>/dev/null && echo RW || echo RO')" = RO ] || fail "the mixed app's root is writable"
app_nd=$(addr_on e2e-mix-app e2e-nd); app_ne=$(addr_on e2e-mix-app e2e-ne)
[ -n "$app_nd" ] && [ -n "$app_ne" ] && [ "$app_nd" != "$app_ne" ] || fail "the mixed app has no address on each network: $app_nd $app_ne"
$NSPAWN inspect e2e-mix-app | python3 -c "
import json, sys
d = json.load(sys.stdin)[0]
assert d['networks'] == ['e2e-nd', 'e2e-ne'] and d['aliases'] == ['e2e-nd=mixweb', 'e2e-ne=inner'], (d['networks'], d['aliases'])
assert d['ports'] == ['127.0.0.1:18095->80/tcp', '127.0.0.1:18097->8080/tcp'] and d['secrets'] == ['e2e-pw:/run/secrets/token:0444:0:0'], (d['ports'], d['secrets'])
assert d['user'] == 'nobody:root' and d['read_only'] and d['restart'] == 'on-failure' and d['labels']['role'] == 'mix', d
assert d['health'] == 'healthy' and d['healthcheck']['retries'] == 2, (d['health'], d['healthcheck'])" || fail "inspect of the mixed app"
[ "$(mx 'p=$(pidof httpd); awk "/^Uid:/ {print \$2} /^Gid:/ {print \$2}" /proc/$p/status' | tr '\n' ' ')" = "65534 0 " ] || fail "-u nobody:root is not what httpd runs as: $(mx 'p=$(pidof httpd); grep -E "^(Uid|Gid):" /proc/$p/status')"
# A sidecar in the app's network, with a user and group, a read-only root and a tmpfs,
# that waits for the app to be healthy: its port comes out through the app's second
# published port, it reads the app's hosts file, and it has nothing of its own on the host.
$NSPAWN create $app e2e-mix-side --network container:e2e-mix-app --depends-on e2e-mix-app:healthy -u 65534:65534 --read-only --tmpfs /www2:size=4m,mode=1777 -l role=mix \
  -- /bin/sh -c "echo side-$nonce > /www2/index.html; exec /bin/httpd -f -p 8080 -h /www2" >/dev/null || fail "create the mixed sidecar"
$NSPAWN start e2e-mix-side >/dev/null || fail "start the mixed sidecar"
ms() { $NSPAWN exec -u root e2e-mix-side -- /bin/sh -c "$1" </dev/null 2>/dev/null | tr -d '\r'; }
retry 5 bash -c "curl -sf -m 2 http://127.0.0.1:18097/ | grep_q side-$nonce" || fail "the sidecar's port does not come out through the app's published port: $(curl -s -m 2 http://127.0.0.1:18097/)"
[ "$(ms 'ip -4 -o addr show host0' | grep -c " $app_nd/")" = 1 ] || fail "the sidecar does not have the app's address: $(ms 'ip -4 -o addr show')"
ms 'cat /etc/hosts' | grep_q "^10.1.1.9 peer$" || fail "the sidecar does not read the app's hosts file: $(ms 'cat /etc/hosts')"
[ "$(ms 'p=$(pidof httpd); awk "/^Uid:/ {print \$2} /^Gid:/ {print \$2}" /proc/$p/status' | tr '\n' ' ')" = "65534 65534 " ] || fail "the sidecar's httpd does not run as 65534:65534: $(ms 'p=$(pidof httpd); grep -E "^(Uid|Gid):" /proc/$p/status')"
[ "$(ms 'touch /x 2>/dev/null && echo RW || echo RO')" = RO ] || fail "the sidecar's root is writable"
ip -o link show | grep_q "vb-e2e-mix-side" && fail "the sidecar got a veth of its own"
[ "$(addr_on e2e-mix-side e2e-nd)" = "" ] || fail "the sidecar got an address of its own"
$NSPAWN ps | grep "^ *e2e-mix-side " | grep_q " container:e2e-mix-app " || fail "ps does not show the sidecar's network: $($NSPAWN ps | grep e2e-mix-side)"
# The booted machine: the same networks with an alias, a healthcheck on its systemd, the
# secret, a hostname, a tmpfs, a capability dropped, and the sidecar it depends on.
$NSPAWN pull "$IMAGE" --name e2e-mix-boot --backend overlay --force >/dev/null || fail "pull e2e-mix-boot"
$NSPAWN start e2e-mix-boot --network e2e-nd --network e2e-ne --network-alias mixboot --depends-on e2e-mix-side \
  --health-cmd "systemctl is-system-running --wait | grep -Eq '^(running|degraded)\$'" --health-interval 2s --health-timeout 30s --health-retries 3 --health-start-period 40s \
  --secret e2e-pw --hostname mixboot-$nonce --tmpfs /var/tmp:size=16m --cap-drop SYS_MODULE --add-host peer:10.1.1.9 --dns-search mix.test >/dev/null || fail "start the mixed booted machine"
mb() { $NSPAWN exec e2e-mix-boot -- /bin/sh -c "$1" </dev/null 2>/dev/null | tr -d '\r'; }
retry 30 bash -c "$NSPAWN ps | grep '^ *e2e-mix-boot ' | grep_q '(healthy)'" || fail "the mixed booted machine is not healthy: $($NSPAWN ps | grep e2e-mix-boot; $NSPAWN inspect e2e-mix-boot | grep -A3 health_log)"
[ "$(mb 'cat /run/secrets/e2e-pw')" = mix-secret ] || fail "the secret is not readable in the booted machine: $(mb 'ls -la /run/secrets')"
# A minimal image has no hostname binary; the kernel's name is the same thing.
[ "$(mb 'uname -n')" = "mixboot-$nonce" ] || fail "hostname of the booted machine: $(mb 'uname -n')"
mb 'cat /proc/mounts' | grep_q " /var/tmp tmpfs" || fail "--tmpfs missing in the booted machine"
[ $(( 0x$(mb "awk '/^CapBnd:/ {print \$2}' /proc/1/status") & 0x10000 )) = 0 ] || fail "--cap-drop SYS_MODULE left the capability in the booted machine: $(mb 'grep CapBnd /proc/1/status')"
mb 'cat /etc/hosts' | grep_q "^10.1.1.9 peer$" || fail "--add-host missing in the booted machine"
mb 'cat /run/systemd/network/1*-host*.network' | grep_q "^Domains=mix.test$" || fail "--dns-search missing from the booted machine's network files: $(mb 'cat /run/systemd/network/1*-host*.network')"
if [ -n "$(mb 'command -v resolvectl')" ]; then
  retry 10 bash -c "$NSPAWN exec e2e-mix-boot -- resolvectl domain </dev/null 2>/dev/null | grep_q mix.test" || fail "systemd-resolved in the booted machine lacks the search domain: $(mb 'resolvectl domain')"
fi
mb 'getent hosts mixweb' | grep_q "$app_nd" || fail "the booted machine does not resolve the app's alias on e2e-nd: $(mb 'getent hosts mixweb')"
mb 'getent hosts inner' | grep_q "$app_ne" || fail "the booted machine does not resolve the app's alias on e2e-ne: $(mb 'getent hosts inner')"
$NSPAWN exec e2e-mix-boot -- bash -c 'exec 3<>/dev/tcp/mixweb/80; printf "GET / HTTP/1.0\r\n\r\n" >&3; cat <&3' </dev/null | tr -d '\r' | grep_q mix-secret || fail "the booted machine does not reach the app by its alias"
$NSPAWN exec e2e-mix-boot -- bash -c 'exec 3<>/dev/tcp/inner/80; printf "GET / HTTP/1.0\r\n\r\n" >&3; cat <&3' </dev/null | tr -d '\r' | grep_q mix-secret || fail "the booted machine does not reach the app through the internal network"
$NSPAWN exec e2e-mix-boot -- bash -c 'exec 3<>/dev/tcp/mixweb/8080; printf "GET / HTTP/1.0\r\n\r\n" >&3; cat <&3' </dev/null | tr -d '\r' | grep_q side-$nonce || fail "the booted machine does not reach the sidecar through the app's alias"
mx 'cat /etc/hosts' | grep_q " mixboot" || fail "the app's hosts file misses the booted machine's alias: $(mx 'cat /etc/hosts')"
# The app runs as nobody, and so would exec: ping needs root's raw socket.
retry 5 bash -c "$NSPAWN exec -u root e2e-mix-app -- ping -c 1 -W 2 mixboot </dev/null >/dev/null 2>&1" || fail "the app does not reach the booted machine by its alias"
# Operations on top of it all: pause, an update of the healthcheck, restart, top, exec
# with its flags, the logs of both.
$NSPAWN pause e2e-mix-app >/dev/null || fail "pause the mixed app"
$NSPAWN ps | grep "^ *e2e-mix-app " | grep_q " paused " || fail "ps does not show the mixed app paused: $($NSPAWN ps | grep e2e-mix-app)"
$NSPAWN unpause e2e-mix-app >/dev/null || fail "unpause the mixed app"
retry 10 bash -c "$NSPAWN ps | grep '^ *e2e-mix-app ' | grep_q '(healthy)'" || fail "the mixed app is not healthy again after unpause: $($NSPAWN ps | grep e2e-mix-app)"
$NSPAWN update e2e-mix-app --health-retries 3 --health-interval 2s >/dev/null || fail "update the healthcheck of the mixed app"
retry 10 bash -c "$NSPAWN ps | grep '^ *e2e-mix-app ' | grep_q '(healthy)'" || fail "the mixed app is not healthy after update: $($NSPAWN ps | grep e2e-mix-app)"
$NSPAWN restart e2e-mix-app -t 2 | grep_q "restarted e2e-mix-app" || fail "restart the mixed app"
retry 10 bash -c "$NSPAWN ps | grep '^ *e2e-mix-app ' | grep_q '(healthy)'" || fail "the mixed app is not healthy after restart: $($NSPAWN ps | grep e2e-mix-app)"
retry 5 bash -c "curl -sf -m 2 http://127.0.0.1:18095/ | grep_q mix-secret" || fail "the port does not answer after restart"
[ "$(addr_on e2e-mix-app e2e-nd)" = "$app_nd" ] || fail "the address on e2e-nd changed across restart"
# The sidecar kept the namespace the app left: its port is silent until
# it restarts too.
curl -sf -m 2 http://127.0.0.1:18097/ >/dev/null 2>&1 && fail "the sidecar answered from the app's old namespace after the app's restart"
$NSPAWN restart e2e-mix-side >/dev/null || fail "restart the sidecar after the app"
retry 5 bash -c "curl -sf -m 2 http://127.0.0.1:18097/ | grep_q side-$nonce" || fail "the sidecar's port does not come back after its restart"
$NSPAWN top e2e-mix-app | grep_q "httpd" || fail "top does not list httpd: $($NSPAWN top e2e-mix-app)"
[ "$($NSPAWN exec e2e-mix-app -u nobody -w /www -T -- /bin/sh -c 'pwd; id -u' </dev/null | tr -d '\r' | tr '\n' ' ')" = "/www 65534 " ] || fail "exec -u -w -T in the mixed app: $($NSPAWN exec e2e-mix-app -u nobody -w /www -T -- /bin/sh -c 'pwd; id -u' </dev/null)"
$NSPAWN logs e2e-mix-app e2e-mix-boot --until now > /tmp/e2e-mixlogs.txt 2>&1 || fail "logs of both mixed machines"
grep -q "^e2e-mix-boot *| " /tmp/e2e-mixlogs.txt || fail "logs of both carry no names: $(head -3 /tmp/e2e-mixlogs.txt)"
# Stop with the machine's own signal: the exit code follows it, the policy stays quiet,
# nothing of the secret, the health or the port is left behind.
$NSPAWN stop e2e-mix-app -t 2 | grep_q "stopped e2e-mix-app" || fail "stop the mixed app"
sleep 3
$NSPAWN ps | grep_q "^ *e2e-mix-app " && fail "the mixed app came back after stop despite its policy"
$NSPAWN inspect e2e-mix-app | python3 -c "import json,sys; d = json.load(sys.stdin)[0]; assert d['state'] == 'stopped' and d['exit_code'] == 130, (d['state'], d.get('exit_code'))" || fail "the exit code of the mixed app after stop with SIGINT"
[ -e /run/nspawn/secrets/e2e-mix-app ] && fail "the secret of the mixed app was left decrypted"
[ -e /run/nspawn/health/e2e-mix-app.json ] && fail "the health file of the mixed app was left behind"
nft list map ip nspawn addr_ports | grep_q 18095 && fail "the port of the mixed app is still mapped"
$NSPAWN restart e2e-mix-boot -t 5 | grep_q "restarted e2e-mix-boot" || fail "restart the mixed booted machine"
retry 30 bash -c "$NSPAWN ps | grep '^ *e2e-mix-boot ' | grep_q '(healthy)'" || fail "the booted machine is not healthy after restart: $($NSPAWN ps | grep e2e-mix-boot)"
[ "$(mb 'cat /run/secrets/e2e-pw')" = mix-secret ] || fail "the secret is gone after the restart of the booted machine"
$NSPAWN events --since "@$mix_since" --until now --filter name=e2e-mix-app --filter event=health_status | grep_q "status=healthy" || fail "no health_status event for the mixed app"
$NSPAWN stop e2e-mix-boot >/dev/null || fail "stop the mixed booted machine"
out=$($NSPAWN secret rm e2e-pw 2>&1) && fail "the secret was removed while the mixed machines take it"
out=$($NSPAWN rm e2e-mix-app 2>&1) && fail "the app was removed while the sidecar takes its network"
echo "$out" | grep_q "network of e2e-mix-side" || fail "the refused rm of the app was not explained: $out"
out=$($NSPAWN rm e2e-mix-side 2>&1) && fail "the sidecar was removed while the booted machine depends on it"
echo "$out" | grep_q "e2e-mix-boot depends on e2e-mix-side" || fail "the refused rm of the sidecar was not explained: $out"
$NSPAWN stop e2e-mix-side >/dev/null || fail "stop the sidecar"
ip netns list | grep_q -E "^nspawn-e2e-mix-side( |$)" && fail "the sidecar's namespace name was left behind"
# The three down, a start of the booted machine brings up the other two as systemd does
# at boot: the app first, healthy before the sidecar joins its new namespace.
$NSPAWN start e2e-mix-boot > /tmp/e2e-mix-start.txt 2>&1 || fail "start the booted machine with the rest of the mix down: $(cat /tmp/e2e-mix-start.txt)"
grep_q "e2e-mix-side started first: e2e-mix-boot depends on it" /tmp/e2e-mix-start.txt || fail "start does not say it started the sidecar first: $(cat /tmp/e2e-mix-start.txt)"
$NSPAWN ps | grep "^ *e2e-mix-app " | grep_q "(healthy)" || fail "the sidecar started before the app was healthy: $($NSPAWN ps | grep e2e-mix-)"
retry 5 bash -c "curl -sf -m 2 http://127.0.0.1:18097/ | grep_q side-$nonce" || fail "the sidecar started by systemd does not answer through the app's port"
retry 30 bash -c "$NSPAWN ps | grep '^ *e2e-mix-boot ' | grep_q '(healthy)'" || fail "the booted machine is not healthy after the start of the three: $($NSPAWN ps | grep e2e-mix-boot)"
$NSPAWN exec e2e-mix-boot -- bash -c 'exec 3<>/dev/tcp/mixweb/8080; printf "GET / HTTP/1.0\r\n\r\n" >&3; cat <&3' </dev/null | tr -d '\r' | grep_q side-$nonce || fail "the booted machine does not reach the sidecar after the start of the three"
# generate of the booted machine brings along the two it needs, in the order they start;
# with the three and their networks gone, the script makes them again.
for m in e2e-mix-app e2e-mix-side e2e-mix-boot; do gen_keys $m > /tmp/$m.before; done
$NSPAWN generate e2e-mix-boot > /tmp/e2e-mix-gen.sh || fail "generate of the mixed machines"
cat /tmp/e2e-mix-gen.sh
grep_q "^# secret e2e-pw is encrypted for this host" /tmp/e2e-mix-gen.sh || fail "generate does not say the secret of the mix stays behind"
grep_q "^nspawn network create e2e-ne --subnet [0-9./]* --internal$" /tmp/e2e-mix-gen.sh || fail "generate does not make the internal network"
grep_q "^nspawn network create e2e-nd " /tmp/e2e-mix-gen.sh && grep_q "^  --label tier=mix$" /tmp/e2e-mix-gen.sh || fail "generate does not make the labelled network"
mix_line() { grep -n "^nspawn run -d --name $1 " /tmp/e2e-mix-gen.sh | cut -d: -f1; }
[ -n "$(mix_line e2e-mix-app)" ] && [ "$(mix_line e2e-mix-app)" -lt "$(mix_line e2e-mix-side)" ] && [ "$(mix_line e2e-mix-side)" -lt "$(mix_line e2e-mix-boot)" ] || fail "generate does not make the app, the sidecar and the booted machine in that order"
# One rm of the three, the app named first: it goes once the two that need it have.
$NSPAWN rm -f e2e-mix-app e2e-mix-side e2e-mix-boot >/dev/null || fail "rm -f of the three mixed machines"
$NSPAWN network rm e2e-nd e2e-ne >/dev/null || fail "network rm of the mix networks"
sed "s#^nspawn #$NSPAWN #" /tmp/e2e-mix-gen.sh | bash > /tmp/e2e-mix-gen-run.txt 2>&1 || fail "the script of the mixed machines failed: $(cat /tmp/e2e-mix-gen-run.txt)"
for m in e2e-mix-app e2e-mix-side e2e-mix-boot; do
  gen_keys $m > /tmp/$m.after
  diff /tmp/$m.before /tmp/$m.after > /tmp/$m.diff || fail "$m came back otherwise: $(cat /tmp/$m.diff)"
done
retry 10 bash -c "curl -sf -m 2 http://127.0.0.1:18095/ | grep_q mix-secret" || fail "the app made again does not serve the secret on its port"
retry 10 bash -c "curl -sf -m 2 http://127.0.0.1:18097/ | grep_q side-$nonce" || fail "the sidecar made again does not answer through the app's port"
retry 30 bash -c "$NSPAWN ps | grep '^ *e2e-mix-boot ' | grep_q '(healthy)'" || fail "the booted machine made again is not healthy: $($NSPAWN ps | grep e2e-mix-boot)"
$NSPAWN exec e2e-mix-boot -- bash -c 'exec 3<>/dev/tcp/inner/80; printf "GET / HTTP/1.0\r\n\r\n" >&3; cat <&3' </dev/null | tr -d '\r' | grep_q mix-secret || fail "the booted machine made again does not reach the app through the internal network"
$NSPAWN rm -f e2e-mix-app e2e-mix-side e2e-mix-boot >/dev/null || fail "rm -f of the mixed machines made again"
$NSPAWN secret rm e2e-pw >/dev/null || fail "secret rm once the mixed machines are gone"
$NSPAWN network rm e2e-nd e2e-ne >/dev/null || fail "network rm of the mix networks"
ip -o link show | grep_q "nsbr-e2e-n[de]" && fail "a mix network left its bridge"

step "rm: a running machine is refused, rm -f stops it first, named volumes stay"
$NSPAWN start $app -v e2evol:/vol -- /bin/sleep 300 >/dev/null || fail "start the app with e2evol"
out=$($NSPAWN rm $app 2>&1) && fail "rm removed a running machine"
echo "$out" | grep_q "rm --force" || fail "rm of a running machine does not mention --force: $out"
$NSPAWN rm -f $app > /tmp/e2e-rmf.txt 2>&1 || { cat /tmp/e2e-rmf.txt; fail "rm -f of a running machine"; }
grep -q "removed $app" /tmp/e2e-rmf.txt || fail "rm -f did not say it removed $app: $(cat /tmp/e2e-rmf.txt)"
grep -q "volume e2evol kept" /tmp/e2e-rmf.txt || fail "rm did not say the named volume was kept: $(cat /tmp/e2e-rmf.txt)"
[ -d /var/lib/nspawn/volumes/e2evol ] || fail "rm removed a named volume"
$NSPAWN ps -a | grep_q "^ *$app " && fail "$app still listed after rm -f"
$NSPAWN events --since "-2min" --until now --filter name=$app --filter event=remove | grep_q -E "machine remove $app \(created=[0-9]+, image=docker.io/library/busybox:latest\)" || fail "machine remove carries no image: $($NSPAWN events --since -2min --until now --filter name=$app | tail -2)"
[ -e "/var/lib/machines/$app" ] && fail "/var/lib/machines/$app left after rm -f"
systemctl is-failed systemd-nspawn@$app.service >/dev/null 2>&1 && fail "rm -f left the unit failed"
[ -e /etc/systemd/nspawn/$app.nspawn ] && fail "settings file left behind for $app"
ls /etc/systemd/system/ | grep_q "$app" && fail "unit files left behind for $app"

# Nothing else holds the blobs of busybox now: the pull below downloads them again.
$NSPAWN images rm e2e-bb >/dev/null 2>&1
step "progress: a bar follows each download on a terminal, nothing of it in a pipe"
# rm -f collected the blobs of busybox, so this pull downloads them again.
bar='[0-9] [KMG]?i?B/[0-9.]+ [KMG]?i?B'
if grep_q "blob .*: downloading" /tmp/e2e-app.txt; then
  grep_q -E "$bar" /tmp/e2e-app.txt && fail "progress went into a pipe: $(cat /tmp/e2e-app.txt)"
fi
python3 "$(dirname "$0")/terminal.py" $NSPAWN pull "$busybox_ref" --name $app --backend overlay --force </dev/null > /tmp/e2e-progress.txt 2>&1 || fail "pull busybox on a terminal"
tr -d '\r' < /tmp/e2e-progress.txt | grep_q "blob .*: downloading" || fail "the pull after rm -f downloaded nothing: $(tr -d '\r' < /tmp/e2e-progress.txt)"
grep_q -aE "$bar" /tmp/e2e-progress.txt || fail "no progress bar on a terminal: $(tr -d '\r' < /tmp/e2e-progress.txt)"
$NSPAWN rm $app >/dev/null || fail "rm $app after the pull on a terminal"

step "signatures: a hub image is verified before anything is downloaded"
# The hub's build workflow signs every image twice (the project's key and keyless);
# the policy for hub.nspawn.org is built in and requires one of them.
$NSPAWN pull hub.nspawn.org/dnsmasq:latest --name e2e-signed --force > /tmp/e2e-signed.txt 2>&1 || { cat /tmp/e2e-signed.txt; fail "pull of a signed hub image"; }
grep_q "^hub.nspawn.org/dnsmasq:latest: signature verified (key 6wiWMtJZCUkV, keyless https://github.com/nspawn/mkosi-definitions/.github/workflows/mkosi.yml@refs/heads/master)" /tmp/e2e-signed.txt || fail "the pull did not say who signed the image: $(cat /tmp/e2e-signed.txt)"
[ "$(grep -n "signature verified" /tmp/e2e-signed.txt | cut -d: -f1)" = 1 ] || fail "the signature was not checked first: $(cat /tmp/e2e-signed.txt)"
$NSPAWN inspect e2e-signed | python3 -c "
import json, sys
d = json.load(sys.stdin)[0]
assert d['signed_by'].startswith('key 6wiWMtJZCUkV05p2cDF3DrfY9Q1YsYRMLaOYI/PY7Lc=, keyless https://github.com/nspawn/mkosi-definitions/'), d['signed_by']
assert d['signed_at'] > 1790000000, d['signed_at']" || fail "inspect does not show the signature"
$NSPAWN images ls --json | python3 -c "
import json, sys
d = [i for i in json.load(sys.stdin) if i['name'] == 'e2e-signed'][0]
assert d['signed_by'].startswith('key '), d" || fail "images ls --json has no signed_by"
$NSPAWN create e2e-signed e2e-nvc >/dev/null || fail "create from the signed image"
$NSPAWN inspect e2e-nvc | python3 -c "import json, sys; d = json.load(sys.stdin)[0]; assert d['signed_by'].startswith('key ') and d['signed_at'] > 0, d" || fail "a created machine does not inherit its source's signature"
$NSPAWN rm e2e-nvc >/dev/null || fail "rm e2e-nvc"
# An image without a signature is refused, unless --no-verify says so; the dated tags
# older than the signing (2026-09-26) are such images while the hub keeps them.
old=$($NSPAWN --registry hub.nspawn.org hub tags fedora 2>/dev/null | awk -F- '$1 == "44" && NF == 2 && $2 ~ /^[0-9]{8}$/ && $2 < "20260926" {print; exit}')
if [ -z "$old" ]; then
  echo "no unsigned dated tag of fedora left on the hub: the refusal is not checked"
elif $NSPAWN images ls | grep_q "^ *fedora-$old "; then
  echo "fedora-$old is an image of this host's: the refusal is not checked"
else
  # What this run pulls under that name, the cleanup may remove.
  echo "fedora-$old" > /tmp/e2e-dated-image
  out=$($NSPAWN pull hub.nspawn.org/fedora:$old --name e2e-unsigned 2>&1) && fail "an unsigned hub image was pulled"
  echo "$out" | grep_q "hub.nspawn.org/fedora:$old (.*) carries no signature on hub.nspawn.org; the policy requires one (--no-verify pulls it anyway)" || fail "the refusal of an unsigned image reads wrong: $out"
  $NSPAWN images ls | grep_q "^ *e2e-unsigned " && fail "a refused pull left an image behind"
  out=$($NSPAWN create hub.nspawn.org/fedora:$old e2e-nvc 2>&1) && fail "create pulled an unsigned image"
  echo "$out" | grep_q "carries no signature" || fail "create did not explain the refusal: $out"
  out=$($NSPAWN run -d --pull always --name e2e-nv hub.nspawn.org/fedora:$old 2>&1) && fail "run pulled an unsigned image"
  echo "$out" | grep_q "carries no signature" || fail "run did not explain the refusal: $out"
  $NSPAWN pull hub.nspawn.org/fedora:$old --name e2e-unsigned --no-verify > /tmp/e2e-unsigned.txt 2>&1 || { cat /tmp/e2e-unsigned.txt; fail "pull --no-verify"; }
  grep_q "^note: signature verification of hub.nspawn.org/fedora:$old skipped (--no-verify)" /tmp/e2e-unsigned.txt || fail "pull --no-verify did not say so: $(cat /tmp/e2e-unsigned.txt)"
  $NSPAWN inspect e2e-unsigned | python3 -c "import json, sys; d = json.load(sys.stdin)[0]; assert d['signed_by'] == '' and d['signed_at'] == 0, d" || fail "an unverified image claims a signature"
  $NSPAWN images rm e2e-unsigned >/dev/null || fail "images rm e2e-unsigned"
  $NSPAWN create --no-verify hub.nspawn.org/fedora:$old e2e-nvc >/dev/null || fail "create --no-verify"
  $NSPAWN images ls | grep_q "^ *fedora-$old " || fail "create --no-verify did not keep the pulled image under its name"
  $NSPAWN run -d --no-verify --pull always --name e2e-nv hub.nspawn.org/fedora:$old >/dev/null || fail "run --no-verify"
  $NSPAWN stop e2e-nv >/dev/null || fail "stop e2e-nv"
  $NSPAWN rm e2e-nv e2e-nvc >/dev/null || fail "rm e2e-nv e2e-nvc"
  $NSPAWN images rm "fedora-$old" >/dev/null || fail "images rm fedora-$old"
fi
$NSPAWN images rm e2e-signed >/dev/null || fail "images rm e2e-signed"

step "signatures: a policy of the configuration, a key of our own on the test registry"
# busybox goes to the test registry under a repository of this run's own (referrers
# hang from the repository, and the registry is shared with the other hosts of the
# matrix), gets a policy there, and a signature made without cosign: the statement,
# its DSSE envelope signed with openssl, a timestamp from a TSA of ours (a bundle has
# to carry a log entry or a timestamp), the bundle as a referrer of the manifest.
sig=/tmp/e2e-sig; rm -rf $sig; mkdir -p $sig
repo=e2e/sig-$nonce
$NSPAWN pull "$busybox_ref" --name e2e-nv --backend overlay --force >/dev/null 2>&1 || fail "pull busybox for the signing test"
$NSPAWN push e2e-nv --to $repo:1 > /tmp/e2e-pushsig.txt || fail "push busybox to the test registry"
$NSPAWN images rm e2e-nv >/dev/null || fail "images rm e2e-nv"
ca="--cacert $NSPAWN_CA_CERT"
base="https://$NSPAWN_REGISTRY/v2/$repo"
curl -sS $ca -H 'Accept: application/vnd.oci.image.manifest.v1+json' "$base/manifests/1" -o $sig/image.manifest.json || fail "fetch the manifest of $repo:1"
digest="sha256:$(sha256sum $sig/image.manifest.json | cut -d' ' -f1)"
openssl ecparam -name prime256v1 -genkey -noout -out $sig/key.pem
openssl ec -in $sig/key.pem -pubout -out /etc/nspawn/e2e-cosign.pub 2>/dev/null
openssl ec -in $sig/key.pem -pubout -outform DER -out $sig/pub.der 2>/dev/null
cat > $sig/tsa.cnf <<CNF
[req]
distinguished_name = dn
x509_extensions = tsa_ext
prompt = no
[dn]
CN = e2e TSA
[tsa_ext]
extendedKeyUsage = critical,timeStamping
keyUsage = digitalSignature
[tsa]
default_tsa = tsa_config
[tsa_config]
dir = $sig
serial = $sig/tsa.serial
signer_cert = $sig/tsa.crt
signer_key = $sig/tsa.key
certs = $sig/tsa.crt
default_policy = 1.3.6.1.4.1.99999.1
signer_digest = sha256
digests = sha256
crypto_device = builtin
accuracy = secs:1
ordering = no
tsa_name = no
ess_cert_id_chain = no
CNF
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -keyout $sig/tsa.key -out $sig/tsa.crt -days 2 -config $sig/tsa.cnf 2>/dev/null || fail "make a TSA certificate"
echo 01 > $sig/tsa.serial
hint=$(python3 - $sig "${digest#sha256:}" <<'PY'
import base64, hashlib, json, subprocess, sys
d, hexdigest = sys.argv[1], sys.argv[2]
statement = json.dumps({"_type": "https://in-toto.io/Statement/v1", "subject": [{"digest": {"sha256": hexdigest}}], "predicateType": "https://sigstore.dev/cosign/sign/v1", "predicate": {}}).encode()
kind = b"application/vnd.in-toto+json"
pae = b"DSSEv1 " + str(len(kind)).encode() + b" " + kind + b" " + str(len(statement)).encode() + b" " + statement
open(f"{d}/pae.bin", "wb").write(pae)
subprocess.run(["openssl", "dgst", "-sha256", "-sign", f"{d}/key.pem", "-out", f"{d}/sig.der", f"{d}/pae.bin"], check=True)
subprocess.run(["openssl", "ts", "-query", "-data", f"{d}/sig.der", "-sha256", "-cert", "-out", f"{d}/req.tsq"], check=True, capture_output=True)
subprocess.run(["openssl", "ts", "-reply", "-config", f"{d}/tsa.cnf", "-queryfile", f"{d}/req.tsq", "-out", f"{d}/resp.tsr"], check=True, capture_output=True)
b64 = lambda b: base64.b64encode(b).decode()
sig = open(f"{d}/sig.der", "rb").read()
hint = b64(hashlib.sha256(open(f"{d}/pub.der", "rb").read()).digest())
bundle = {"mediaType": "application/vnd.dev.sigstore.bundle.v0.3+json",
          "verificationMaterial": {"publicKey": {"hint": hint}, "tlogEntries": [],
                                   "timestampVerificationData": {"rfc3161Timestamps": [{"signedTimestamp": b64(open(f"{d}/resp.tsr", "rb").read())}]}},
          "dsseEnvelope": {"payload": b64(statement), "payloadType": "application/vnd.in-toto+json", "signatures": [{"sig": b64(sig)}]}}
open(f"{d}/bundle.json", "w").write(json.dumps(bundle))
bad = bytearray(sig); bad[10] ^= 1
bundle["dsseEnvelope"]["signatures"][0]["sig"] = b64(bytes(bad))
open(f"{d}/tampered.json", "w").write(json.dumps(bundle))
print(hint)
PY
) || fail "make the bundle"
[ -s $sig/bundle.json ] || fail "no bundle was made"
printf '{}' > $sig/config.json
# A blob and a manifest to the registry with curl, as any OCI client would.
push_blob() {
  local file=$1 d="sha256:$(sha256sum "$1" | cut -d' ' -f1)" location sep code
  location=$(curl -sS $ca -u tester:s3cret -X POST -D - -o /dev/null "$base/blobs/uploads/" | tr -d '\r' | awk -F': ' 'tolower($1) == "location" {print $2}')
  case "$location" in /*) location="https://$NSPAWN_REGISTRY$location" ;; esac
  case "$location" in *\?*) sep='&' ;; *) sep='?' ;; esac
  code=$(curl -sS $ca -u tester:s3cret -X PUT -H 'Content-Type: application/octet-stream' --data-binary @"$file" -o /dev/null -w '%{http_code}' "$location${sep}digest=$d")
  [ "$code" = 201 ] || { echo "upload of $file answered $code"; return 1; }
  echo "$d"
}
push_signature() { # bundle file -> the digest of its manifest
  local bundle=$1 bd cd m md code
  cd=$(push_blob $sig/config.json) || return 1
  bd=$(push_blob "$bundle") || return 1
  m=$sig/$(basename "$bundle" .json).manifest.json
  python3 - "$m" "$cd" "$bd" "$(stat -c %s "$bundle")" "$digest" "$(stat -c %s $sig/image.manifest.json)" <<'PY'
import json, sys
out, cd, bd, bsize, subject, ssize = sys.argv[1:]
json.dump({"schemaVersion": 2, "mediaType": "application/vnd.oci.image.manifest.v1+json",
           "artifactType": "application/vnd.dev.sigstore.bundle.v0.3+json",
           "config": {"mediaType": "application/vnd.oci.empty.v1+json", "size": 2, "digest": cd},
           "layers": [{"mediaType": "application/vnd.dev.sigstore.bundle.v0.3+json", "size": int(bsize), "digest": bd}],
           "subject": {"mediaType": "application/vnd.oci.image.manifest.v1+json", "size": int(ssize), "digest": subject},
           "annotations": {"dev.sigstore.bundle.content": "dsse-envelope", "dev.sigstore.bundle.predicateType": "https://sigstore.dev/cosign/sign/v1"}},
          open(out, "w"), separators=(",", ":"))
PY
  md="sha256:$(sha256sum "$m" | cut -d' ' -f1)"
  code=$(curl -sS $ca -u tester:s3cret -X PUT -H 'Content-Type: application/vnd.oci.image.manifest.v1+json' --data-binary @"$m" -o /dev/null -w '%{http_code}' "$base/manifests/$md")
  [ "$code" = 201 ] || { echo "push of the signature manifest answered $code"; return 1; }
  echo "$md"
}
policy() { # the service's configuration with a table for the test registry, then a restart
  printf 'registry = "%s"\nca_cert = "%s"\n\n[registries."%s"]\n%s\n' "$NSPAWN_REGISTRY" "$NSPAWN_CA_CERT" "$NSPAWN_REGISTRY" "$1" > /etc/nspawn/e2e.toml
  systemctl stop nspawn.service
}
policy 'keys = ["/etc/nspawn/e2e-cosign.pub"]
rekor = false'
out=$($NSPAWN pull $NSPAWN_REGISTRY/$repo:1 --name e2e-nvc 2>&1) && fail "an unsigned image of a registry with a policy was pulled"
echo "$out" | grep_q "carries no signature on $NSPAWN_REGISTRY; the policy requires one" || fail "the refusal reads wrong: $out"
$NSPAWN create --no-verify $NSPAWN_REGISTRY/$repo:1 e2e-nvc >/dev/null || fail "create --no-verify against a configured policy"
$NSPAWN rm e2e-nvc >/dev/null && $NSPAWN images rm e2e-sig-$nonce-1 >/dev/null || fail "rm e2e-nvc and its image"
policy 'verify = false'
$NSPAWN pull $NSPAWN_REGISTRY/$repo:1 --name e2e-nvc > /tmp/e2e-nvc.txt 2>&1 || { cat /tmp/e2e-nvc.txt; fail "pull with verify = false"; }
grep -q -i "signature" /tmp/e2e-nvc.txt && fail "verify = false still said something of signatures: $(cat /tmp/e2e-nvc.txt)"
$NSPAWN images rm e2e-nvc >/dev/null || fail "images rm e2e-nvc"
policy 'keys = ["/etc/nspawn/e2e-cosign.pub"]
rekor = false'
tampered=$(push_signature $sig/tampered.json) || fail "push the tampered signature"
curl -sS $ca "$base/referrers/$digest" | grep_q "$tampered" || fail "the registry does not list the signature among the referrers"
out=$($NSPAWN pull $NSPAWN_REGISTRY/$repo:1 --name e2e-nvc 2>&1) && fail "an image whose signature does not verify was pulled"
echo "$out" | grep_q "no signature verifies under the policy for $NSPAWN_REGISTRY: ${tampered:7:12}: key ${hint:0:12}: /etc/nspawn/e2e-cosign.pub (" || fail "the failure does not name the bundle and the key: $out"
good=$(push_signature $sig/bundle.json) || fail "push the good signature"
$NSPAWN pull $NSPAWN_REGISTRY/$repo:1 --name e2e-nvc > /tmp/e2e-nvc.txt 2>&1 || { cat /tmp/e2e-nvc.txt; fail "pull of the image once signed"; }
grep -q "^$NSPAWN_REGISTRY/$repo:1: signature verified (key ${hint:0:12})$" /tmp/e2e-nvc.txt || fail "the pull did not say the key signed it: $(cat /tmp/e2e-nvc.txt)"
$NSPAWN inspect e2e-nvc | python3 -c "import json, sys; d = json.load(sys.stdin)[0]; assert d['signed_by'] == 'key $hint' and d['signed_at'] == 0, d" || fail "inspect of the key-signed image"
$NSPAWN images rm e2e-nvc >/dev/null || fail "images rm e2e-nvc"
policy 'keys = ["/etc/nspawn/e2e-cosign.pub"]
rekor = false
required = false'
$NSPAWN pull "$IMAGE" --name e2e-unsigned --backend overlay --force > /tmp/e2e-unsigned.txt 2>&1 || { cat /tmp/e2e-unsigned.txt; fail "pull of an unsigned image under required = false"; }
grep -q "^note: $NSPAWN_REGISTRY/$IMAGE carries no signature; the policy for $NSPAWN_REGISTRY does not require one" /tmp/e2e-unsigned.txt || fail "required = false did not note the missing signature: $(cat /tmp/e2e-unsigned.txt)"
$NSPAWN images rm e2e-unsigned >/dev/null || fail "images rm e2e-unsigned"
printf 'registry = "%s"\nca_cert = "%s"\n' "$NSPAWN_REGISTRY" "$NSPAWN_CA_CERT" > /etc/nspawn/e2e.toml
# The registry may refuse deletions; the repository is this run's alone either way.
for m in 1 $tampered $good; do curl -sS $ca -u tester:s3cret -X DELETE "$base/manifests/$m" -o /dev/null || true; done
rm -f /etc/nspawn/e2e-cosign.pub; rm -rf $sig
systemctl stop nspawn.service

step "pull: several blobs at once, each verified, nothing left behind"
# memcached:alpine has six small layers. The first three transfers begin before any
# ends: their "downloading" lines come first, then each "downloaded" as it completes.
$NSPAWN pull "$memcached_ref" --name e2e-multi --backend overlay --force > /tmp/e2e-multi.txt 2>&1 || { cat /tmp/e2e-multi.txt; fail "pull of an image with several layers"; }
[ "$(grep -c '^blob .*: downloading' /tmp/e2e-multi.txt)" -ge 6 ] || fail "the layers were not all downloaded: $(cat /tmp/e2e-multi.txt)"
[ "$(grep -E '^blob .*: (downloading|downloaded)$' /tmp/e2e-multi.txt | head -3 | grep -c downloading)" = 3 ] || fail "the blobs were not fetched several at a time: $(cat /tmp/e2e-multi.txt)"
[ "$(grep -c '^blob .*: downloaded$' /tmp/e2e-multi.txt)" = "$(grep -c '^blob .*: downloading$' /tmp/e2e-multi.txt)" ] || fail "a download was not reported done: $(cat /tmp/e2e-multi.txt)"
for blob in /var/lib/nspawn/blobs/sha256-*; do
  [ -e "$blob" ] || continue
  [ "sha256-$(sha256sum "$blob" | awk '{print $1}')" = "$(basename "$blob")" ] || fail "blob $(basename "$blob") does not match its digest after a pull of several at once"
done
ls /var/lib/nspawn/blobs/ | grep_q "^\.part-" && fail "part files left after the pull: $(ls -a /var/lib/nspawn/blobs/ | grep '^\.')"
$NSPAWN start e2e-multi -- /bin/sh -c "echo multi-$nonce" >/dev/null 2>&1
retry 10 bash -c "$NSPAWN logs e2e-multi | tr -d '\r' | grep -q multi-$nonce" || fail "the image pulled several blobs at a time does not run: $($NSPAWN logs e2e-multi)"
$NSPAWN rm -f e2e-multi >/dev/null || fail "rm e2e-multi"

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
  $NSPAWN ps 2>&1 >/dev/null | grep_q "warning: the nspawn service runs" && fail "the command and the service disagree on their version: $($NSPAWN ps 2>&1 >/dev/null)"
  for m in ListImages GetImage Generate PullImage CreateMachine PushImage BuildImage RemoveImages SearchImages ListRepositories ListTags ImageDigest ListMachines GetMachine MachineStats StartMachine RunMachine StopMachine KillMachine PauseMachine UnpauseMachine MachineProcesses UpdateMachine Exec Events Shell Logs ListSecrets GetSecret CreateSecret RemoveSecrets ListNetworks GetNetwork CreateNetwork RemoveNetworks PruneNetworks NetworkUp Login Logout RemoveMachines CopyFrom CopyTo ListVolumes CreateVolume RemoveVolumes PruneVolumes AutoUpdate PruneImages DiskUsage ConnectNetwork DisconnectNetwork; do
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
  echo "$out" | grep_q "no machine or image named e2e-nothing-$nonce" || fail "images rm of a name nothing is behind was not refused: $out"
  echo "$out" | grep_q "machine e2e-dbus is running" || fail "images rm stopped at the first failure: $out"
  $B call $M ListMachines b false > /tmp/e2e-lm.txt
  grep -q '"name" s "e2e-dbus"' /tmp/e2e-lm.txt || fail "ListMachines misses the machine"
  grep -q '"machine_path" s "/org/freedesktop/machine1/machine/e2e_2ddbus"' /tmp/e2e-lm.txt || fail "ListMachines has no machined path"
  grep -q '"state" s "running"' /tmp/e2e-lm.txt || fail "ListMachines: not running"
  $B call $M GetMachine s e2e-dbus | grep_q '"state" s "running"' || fail "GetMachine of a running machine"
  $B call $M GetNetwork s bridge | grep_q '"name" s "e2e-dbus"' || fail "GetNetwork misses the machine"
  # Names for shell completion: every user gets them, polkit is not asked; nothing else
  # answers such a caller.
  N="org.nspawn /org/nspawn org.nspawn.Names"
  $B introspect $N > /tmp/e2e-names.txt || fail "org.nspawn.Names not reachable"
  for m in Machines Images References Networks Volumes; do
    grep -q "^\.$m  *method" /tmp/e2e-names.txt || fail "method $m missing from org.nspawn.Names"
  done
  out=$(runuser -u nobody -- busctl --system call $N Machines b false 2>&1) || fail "Names.Machines refused an unprivileged caller: $out"
  echo "$out" | grep_q '"e2e-dbus"' || fail "Names.Machines misses the running machine: $out"
  runuser -u nobody -- busctl --system call $N Images | grep_q '"e2e-dbus"' || fail "Names.Images misses the image"
  runuser -u nobody -- busctl --system call $N Networks | grep_q '"bridge"' || fail "Names.Networks misses bridge"
  out=$(runuser -u nobody -- busctl --system call $M ListMachines b false 2>&1) && fail "ListMachines answered an unprivileged caller: $out"
  bin=$(readlink -f "$NSPAWN")
  if runuser -u nobody -- test -x "$bin"; then
    tab() { runuser -u nobody -- env NSPAWN_COMPLETE=fish "$bin" -- nspawn "$@" | cut -f1; }
    tab stop "" | grep_q -x e2e-dbus || fail "TAB after stop does not offer the running machine to an unprivileged user: $(tab stop "")"
    tab stop "" | grep_q '^-' && fail "TAB after stop offers flags among the names: $(tab stop "")"
    tab stop -- | grep_q -x -- --force || fail "TAB after stop -- does not offer the flags: $(tab stop --)"
    tab start "" | grep_q -x e2e-dbus && fail "TAB after start offers a running machine: $(tab start "")"
    tab rm e2e-d | grep_q -x e2e-dbus || fail "TAB after rm e2e-d does not offer e2e-dbus: $(tab rm e2e-d)"
    tab network rm "" | grep_q -x bridge || fail "TAB after network rm misses bridge: $(tab network rm "")"
    tab secret rm "" | grep_q -v '^-' && fail "TAB after secret rm offers names: $(tab secret rm "")"
  fi
  for shell in bash zsh fish; do
    $NSPAWN completions $shell | grep_q NSPAWN_COMPLETE || fail "the $shell completions do not call nspawn back"
  done
  # The script a package installed, sourced by a user's bash as a TAB would run it.
  if [ "$packaged" = yes ]; then
    out=$(runuser -u nobody -- bash -c 'source /usr/share/bash-completion/completions/nspawn; COMP_WORDS=(nspawn stop ""); COMP_CWORD=2; COMP_TYPE=9; _clap_complete_nspawn nspawn "" stop; printf "%s\n" "${COMPREPLY[@]}"' 2>&1)
    echo "$out" | grep_q -x e2e-dbus || fail "the installed bash completion does not offer the running machine: $out"
  fi
  # Every exec of this run went through Exec; each left a process object behind.
  out=$($NSPAWN exec e2e-dbus -- /bin/sh -c "echo via-bus-$nonce; exit 7" </dev/null); code=$?
  [ "$code" = 7 ] || fail "exec did not propagate the exit code (got $code)"
  echo "$out" | grep_q "via-bus-$nonce" || fail "exec lost the output: $out"
  proc=$($B get-property $M Processes | awk '{print $NF}' | tr -d '"')
  echo "$proc" | grep_q "^/org/nspawn/process/" || fail "no process object after Exec"
  [ "$($B get-property org.nspawn $proc org.nspawn.Process State)" = 's "exited"' ] || fail "the process object did not see the exit"
  [ "$($B get-property org.nspawn $proc org.nspawn.Process ExitStatus)" = "i 7" ] || fail "the process object kept the wrong exit status"
  $B get-property org.nspawn $proc org.nspawn.Process Argv | grep_q "via-bus-$nonce" || fail "the process object has the wrong argv"
  # The service keeps the last hundred exited commands; older ones go with their objects.
  for i in $(seq 1 105); do $NSPAWN exec e2e-dbus -- /bin/true </dev/null >/dev/null 2>&1 || fail "exec $i of many"; done
  retry 3 bash -c "[ \"\$($B get-property $M Processes | awk '{print \$2}')\" -le 105 ]" || fail "the service keeps every exited command ($($B get-property $M Processes | awk '{print $2}') process objects)"
  $B get-property org.nspawn $proc org.nspawn.Process State >/dev/null 2>&1 && fail "an exited command older than the last hundred kept its object"
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
  nft add table ip filter && forward_table_made=yes || fail "cannot make the test table"
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
  nft delete table ip filter && forward_table_made= || fail "cannot remove the test table"
fi

step "a firewall that drops forwarded traffic once machines run: the exception is there ahead of it"
# ufw sets FORWARD to DROP when it is enabled, machines running or not, and docker when
# it is the one that turns IP forwarding on. Only where nothing else owns the table.
dropper=
for p in /usr/bin/dockerd /usr/sbin/dockerd /usr/local/bin/dockerd /snap/bin/docker /usr/sbin/ufw; do
  [ -e "$p" ] && dropper=$p
done
if nft list table ip filter >/dev/null 2>&1; then
  echo "an ip filter table is already there (docker? ufw?): skipping"
elif ! command -v iptables >/dev/null 2>&1; then
  echo "no iptables here: skipping"
elif [ -z "$dropper" ]; then
  # Nothing drops and nothing will: iptables' table is left alone.
  $NSPAWN network up >/dev/null 2>&1 || fail "network up"
  if nft list table ip filter >/dev/null 2>&1; then
    forward_table_made=yes
    fail "nspawn made an iptables table on a host where nothing drops forwarded traffic: $(nft list table ip filter)"
  fi
else
  forward_table_made=yes
  $NSPAWN run -d "$busybox_ref" --name e2e-fwd -- /bin/sleep 300 >/dev/null || fail "run e2e-fwd"
  iptables -S FORWARD | grep_q -- "-i nspawn0 -j ACCEPT" || fail "with $dropper installed, the exception is not in FORWARD before it comes up: $(iptables -S FORWARD)"
  # What docker or ufw does when it comes up: the policy drops, the rules above stay.
  iptables -P FORWARD DROP || fail "cannot set FORWARD's policy"
  gateway=$(ip -4 route show default | awk '{print $3; exit}')
  retry 5 bash -c "$NSPAWN exec -u root e2e-fwd -- ping -c 1 -W 2 $gateway </dev/null >/dev/null 2>&1" || fail "a machine started before FORWARD dropped reaches nothing beyond the bridge"
  $NSPAWN rm -f e2e-fwd >/dev/null || fail "rm e2e-fwd"
  nft delete table ip filter && forward_table_made= || fail "cannot remove the test table"
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
# A rule an interrupted run left would pass for the host's own.
rm -f "$rules"
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
  # A bind source under a directory only root reads, as /etc/letsencrypt/live: compose,
  # which runs as $who, leaves it to the service instead of failing to make it.
  closed=$(mktemp -d /tmp/e2e-closed.XXXXXX); chmod 700 "$closed"; echo "closed-$nonce" > "$closed/cert.pem"
  cbind=$(mktemp -d /tmp/e2e-cbind.XXXXXX); chown "$who" "$cbind"
  printf 'services:\n  w:\n    image: %s\n    command: ["sleep", "300"]\n    volumes: ["%s/cert.pem:/cert.pem:ro"]\n' "$busybox_ref" "$closed" > "$cbind/compose.yaml"
  out=$(cd "$cbind" && sudo -u "$who" $NSPAWN compose -p e2e-cbind up -d 2>&1) || fail "compose up as $who with a bind under a directory only root reads: $out"
  [ "$($NSPAWN exec e2e-cbind-w-1 -- cat /cert.pem </dev/null | tr -d '\r')" = "closed-$nonce" ] || fail "the machine does not see the file under the closed directory"
  [ -f "$closed/cert.pem" ] || fail "the file under the closed directory is not a file any more"
  (cd "$cbind" && sudo -u "$who" $NSPAWN compose -p e2e-cbind down >/dev/null 2>&1) || fail "compose down as $who"
  rm -rf "$closed" "$cbind"
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
for cmd in rm "images rm"; do
  out=$($NSPAWN $cmd e2e-nonexistent 2>&1); rc=$?
  [ $rc -ne 0 ] || fail "$cmd of an unknown name succeeded: $out"
  echo "$out" | grep_q "no machine or image named e2e-nonexistent" || fail "$cmd of an unknown name was not explained: $out"
done
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

step "compose: a project up, reconciled, run, stopped and down, known by the labels of its machines"
rm -rf /tmp/e2e-compose && mkdir -p /tmp/e2e-compose
cat > /tmp/e2e-compose/compose.yaml <<'COMPOSE'
name: e2e-compose
services:
  db:
    image: busybox:1.36
    command: ["sh", "-c", "mkdir -p /www; echo db-ok > /www/index.html; exec httpd -f -p 80 -h /www"]
    healthcheck:
      test: ["CMD-SHELL", "wget -qO- http://127.0.0.1/ | grep -q db-ok"]
      interval: 2s
      retries: 10
  web:
    image: busybox:1.36
    command: ["sh", "-c", "mkdir -p /www; echo web-$$TAG > /www/index.html; exec httpd -f -p 80 -h /www"]
    environment:
      TAG: ${TAG:-one}
      TABBED: "a\tb"
    depends_on:
      db:
        condition: service_healthy
    ports: ["127.0.0.1:18090:80", "18092:18092/udp"]
    volumes: ["data:/data"]
  job:
    image: busybox:1.36
    command: ["echo", "job-done"]
volumes:
  data: {}
COMPOSE
# In the project's directory, as compose finds its file there; not a subshell, whose
# failures would not count.
if pushd /tmp/e2e-compose > /dev/null; then
  $NSPAWN compose up -d > /tmp/e2e-compose.txt 2>&1 || { cat /tmp/e2e-compose.txt; fail "compose up -d"; }
  grep_q " Container e2e-compose-db-1 Healthy " /tmp/e2e-compose.txt || fail "compose up did not wait for db to be healthy: $(cat /tmp/e2e-compose.txt)"
  retry 15 bash -c "curl -fsS http://127.0.0.1:18090/ | grep -q web-one" || fail "the published port of web does not answer"
  $NSPAWN compose exec -T web wget -qO- http://db/ 2>&1 | grep_q db-ok || fail "web does not reach db by its service name"
  # A tab in a variable, as postgres:18-alpine has in its image, reaches the program.
  [ "$($NSPAWN compose exec -T web printenv TABBED)" = "$(printf 'a\tb')" ] || fail "a variable with a tab did not reach the machine as it was"
  $NSPAWN compose ps -a --format json > /tmp/e2e-compose-ps.txt || fail "compose ps --format json"
  python3 -c 'import json; rows = {r["Service"]: r for r in map(json.loads, open("/tmp/e2e-compose-ps.txt"))}; assert rows["db"]["Health"] == "healthy" and rows["web"]["State"] == "running" and rows["job"]["State"] == "exited" and rows["job"]["ExitCode"] == 0, rows' || fail "compose ps does not show the services as they are: $(cat /tmp/e2e-compose-ps.txt)"
  # A udp port, which under SELinux the service must be let bind to look at it.
  grep_q '18092->18092/udp' /tmp/e2e-compose-ps.txt || fail "the udp port of web is not published: $(cat /tmp/e2e-compose-ps.txt)"
  $NSPAWN compose logs job | grep_q "^job-1  | job-done" || fail "compose logs has not the line of job behind its name"
  $NSPAWN inspect e2e-compose-web-1 | python3 -c 'import json,sys; l = json.load(sys.stdin)[0]["labels"]; assert l["com.docker.compose.project"] == "e2e-compose" and l["com.docker.compose.service"] == "web" and l["com.docker.compose.config-hash"], l' || fail "the machine of web lacks the labels of compose"
  $NSPAWN compose up -d > /tmp/e2e-compose.txt 2>&1 || fail "compose up -d again"
  grep_q " Container e2e-compose-web-1 Running " /tmp/e2e-compose.txt || fail "an unchanged service is not left running: $(cat /tmp/e2e-compose.txt)"
  grep_q "Recreate" /tmp/e2e-compose.txt && fail "an unchanged project made machines anew: $(cat /tmp/e2e-compose.txt)"
  TAG=two $NSPAWN compose up -d > /tmp/e2e-compose.txt 2>&1 || fail "compose up -d with a changed variable"
  grep_q " Container e2e-compose-web-1 Recreated " /tmp/e2e-compose.txt || fail "a changed service is not made anew: $(cat /tmp/e2e-compose.txt)"
  grep_q "e2e-compose-db-1 Recreate" /tmp/e2e-compose.txt && fail "an unchanged dependency was made anew"
  retry 15 bash -c "curl -fsS http://127.0.0.1:18090/ | grep -q web-two" || fail "the service made anew does not answer with its new variable"
  out=$(TAG=two $NSPAWN compose run --rm job echo hi-run 2>/dev/null)
  [ "$out" = hi-run ] || fail "compose run --rm: $out"
  TAG=two $NSPAWN compose run --rm job sh -c 'exit 3' > /dev/null 2>&1
  [ $? -eq 3 ] || fail "compose run does not exit with the code of its command"
  $NSPAWN compose stop > /dev/null 2>&1 || fail "compose stop"
  [ -z "$($NSPAWN compose ps -q)" ] || fail "machines still run after compose stop"
  $NSPAWN compose start > /dev/null 2>&1 || fail "compose start"
  retry 15 bash -c "curl -fsS http://127.0.0.1:18090/ | grep -q web-two" || fail "web does not answer after compose start"
  $NSPAWN compose down -v > /tmp/e2e-compose.txt 2>&1 || { cat /tmp/e2e-compose.txt; fail "compose down -v"; }
  [ -z "$($NSPAWN compose ps -a -q)" ] || fail "compose down left machines"
  $NSPAWN network ls | grep_q e2e-compose_default && fail "compose down left its network"
  $NSPAWN volume ls | grep_q e2e-compose_data && fail "compose down -v left its volume"
  $NSPAWN compose ls -a | grep_q e2e-compose && fail "compose ls still lists the project"
  popd > /dev/null || true
fi
# Images pinned to the digest their registry names now, asked through the service with
# the credentials of login: busybox pushed to the test registry under a tag of this run.
repo=e2e/compose-$nonce
$NSPAWN push busybox-1.36 --to $repo:1 > /dev/null 2>&1 || fail "push busybox for compose config --lock-image-digests"
digest="sha256:$(curl -sS --cacert "$NSPAWN_CA_CERT" -H 'Accept: application/vnd.oci.image.manifest.v1+json' "https://$NSPAWN_REGISTRY/v2/$repo/manifests/1" | sha256sum | cut -d' ' -f1)"
printf 'services:\n  app:\n    image: %s/%s:1\n  hub:\n    image: busybox:1.36\n' "$NSPAWN_REGISTRY" "$repo" > /tmp/e2e-compose/digests.yaml
out=$($NSPAWN compose -f /tmp/e2e-compose/digests.yaml -p e2e-digests config --lock-image-digests 2>&1) || fail "compose config --lock-image-digests: $out"
echo "$out" | grep_q -Fx "    image: $NSPAWN_REGISTRY/$repo:1@$digest" || fail "compose config did not pin the image to its digest on the registry ($digest): $out"
echo "$out" | grep_q -E '^    image: docker\.io/library/busybox:1\.36@sha256:[0-9a-f]{64}$' || fail "compose config did not pin an image of Docker Hub: $out"
printf 'services:\n  app:\n    image: %s/%s:missing\n' "$NSPAWN_REGISTRY" "$repo" > /tmp/e2e-compose/digests.yaml
out=$($NSPAWN compose -f /tmp/e2e-compose/digests.yaml -p e2e-digests config --resolve-image-digests 2>&1) && fail "a tag the registry lacks was pinned: $out"
echo "$out" | grep_q -F "failed to resolve digest for $NSPAWN_REGISTRY/$repo:missing" || fail "the missing tag was not explained: $out"

step "Docker API: proxies and dashboards that read docker's socket find the machines"
api() { curl -sS --unix-socket "/run/nspawn/api/$1.sock" "http://docker$2" "${@:3}"; }
$NSPAWN network create e2e-api >/dev/null || fail "network create e2e-api"
backend() {
  $NSPAWN create busybox-1.36 "$1" --network e2e-api -e E2E_ENV=$nonce \
    -l "caddy=http://$1.e2e.test" -l 'caddy.reverse_proxy={{upstreams 80}}' \
    -- /bin/sh -c "mkdir -p /www; echo $1-$nonce > /www/index.html; exec httpd -f -p 80 -h /www" >/dev/null || fail "create $1"
  $NSPAWN start "$1" >/dev/null || fail "start $1"
}
backend e2e-api-web
# caddy-docker-proxy finds its own container and its networks through the API, and
# routes by the labels of the others; with polling an hour away, a backend started later
# reaches it only through the event stream.
$NSPAWN run -d --name e2e-api-caddy --network e2e-api -p 18110:80 -e CADDY_DOCKER_POLLING_INTERVAL=1h \
  -v /var/run/docker.sock:/var/run/docker.sock "$caddy_image" >/dev/null || fail "run caddy-docker-proxy"
[ -S /run/nspawn/api/e2e-api-caddy.sock ] || fail "no Docker API socket for e2e-api-caddy"
[ -d /var/run/docker.sock ] && fail "a directory was made where docker's socket would be"
grep -q '^Wants=nspawn-api@e2e-api-caddy.socket' /etc/systemd/system/systemd-nspawn@e2e-api-caddy.service.d/*.conf || fail "the machine's unit does not want its socket"
[ "$(api e2e-api-caddy /_ping)" = OK ] || fail "_ping on the Docker API: $(api e2e-api-caddy /_ping 2>&1)"
api e2e-api-caddy /v1.51/version | grep_q '"ApiVersion":"1.51"' || fail "version on the Docker API"
api e2e-api-caddy /containers/json | grep_q '"Names":\["/e2e-api-web"\]' || fail "containers/json misses e2e-api-web: $(api e2e-api-caddy /containers/json)"
retry 30 bash -c "curl -fsS -H 'Host: e2e-api-web.e2e.test' http://127.0.0.1:18110/ | grep_q e2e-api-web-$nonce" || fail "caddy did not route to the labelled machine: $($NSPAWN logs e2e-api-caddy 2>&1 | tail -20)"
backend e2e-api-web2
retry 30 bash -c "curl -fsS -H 'Host: e2e-api-web2.e2e.test' http://127.0.0.1:18110/ | grep_q e2e-api-web2-$nonce" || fail "caddy did not pick up a machine started later: $($NSPAWN logs e2e-api-caddy 2>&1 | tail -20)"
# Read-only but for a kill: a write is refused, a signal goes through as nspawn kill.
out=$(api e2e-api-caddy /containers/create -X POST)
code=$(api e2e-api-caddy /containers/create -X POST -o /dev/null -w '%{http_code}')
[ "$code" = 403 ] && echo "$out" | grep_q "read-only" || fail "a write through the Docker API was not refused: $code $out"
[ "$(api e2e-api-caddy '/containers/e2e-api-web2/kill?signal=HUP' -X POST -o /dev/null -w '%{http_code}')" = 204 ] || fail "kill through the Docker API"
sleep 2
$NSPAWN ps | grep_q '^ *e2e-api-web2 ' || fail "the HUP the API sent ended e2e-api-web2"
api e2e-api-caddy /containers/nope/json | grep_q 'No such container: nope' || fail "an unknown container on the Docker API"
# A machine in a user namespace: its root reaches the socket through the idmapped bind,
# sees every machine and their environment, as docker's socket shows them; another user
# cannot connect.
# Under a name of the run's own, which run then finds by its reference.
$NSPAWN pull "$curl_image" --name e2e-api-curl --force >/dev/null || fail "pull curl"
out=$($NSPAWN run --rm --network none -u 0 -v /var/run/docker.sock:/var/run/docker.sock:ro "$curl_image" \
  -- -sS --unix-socket /var/run/docker.sock http://docker/containers/e2e-api-web/json 2>&1) || fail "curl from a machine in a user namespace: $out"
echo "$out" | grep_q "E2E_ENV=$nonce" || fail "inspect through the Docker API lacks the machine's environment: $out"
out=$($NSPAWN run --rm --network none -u 100 -v /var/run/docker.sock:/var/run/docker.sock "$curl_image" \
  -- -sS --unix-socket /var/run/docker.sock http://docker/_ping 2>&1) && fail "a user other than root reached the Docker API: $out"
# A booted machine takes it at /run/docker.sock.
$NSPAWN create "$IMAGE" e2e-api-boot -v /run/docker.sock:/run/docker.sock >/dev/null || fail "create e2e-api-boot"
$NSPAWN start e2e-api-boot >/dev/null || fail "start e2e-api-boot"
retry 10 bash -c "$NSPAWN exec e2e-api-boot -- curl -sS --unix-socket /run/docker.sock http://docker/_ping </dev/null | grep_q OK" || fail "the booted machine does not reach its Docker API"
$NSPAWN stop e2e-api-boot >/dev/null || fail "stop e2e-api-boot"
# The socket goes with the unit, a moment after the machine itself.
retry 10 bash -c "[ ! -e /run/nspawn/api/e2e-api-boot.sock ]" || fail "the socket stayed after the machine stopped"
retry 10 bash -c "[ \"\$(systemctl is-active nspawn-api@e2e-api-boot.socket)\" != active ]" || fail "the socket unit stayed after the machine stopped"
# A unit written before the socket existed (no Wants= of it) still gets its socket when
# systemd starts the machine itself.
sed -i '/nspawn-api@e2e-api-boot.socket/d' /etc/systemd/system/systemd-nspawn@e2e-api-boot.service.d/*.conf
systemctl daemon-reload
systemctl start systemd-nspawn@e2e-api-boot.service || fail "systemctl start of a machine whose unit does not want its socket: $(journalctl -u systemd-nspawn@e2e-api-boot.service -n 20 --no-pager)"
retry 10 bash -c "[ -S /run/nspawn/api/e2e-api-boot.sock ]" || fail "the socket was not opened for a unit that does not want it"
$NSPAWN stop e2e-api-boot >/dev/null || fail "stop e2e-api-boot again"
# Compose: use_api_socket, and the short form with :ro.
mkdir -p /tmp/e2e-compose-api
cat > /tmp/e2e-compose-api/compose.yaml <<'COMPOSE'
services:
  a:
    image: busybox:1.36
    command: ["sleep", "300"]
    use_api_socket: true
  b:
    image: busybox:1.36
    command: ["sleep", "300"]
    volumes: ["/var/run/docker.sock:/var/run/docker.sock:ro"]
COMPOSE
$NSPAWN compose -f /tmp/e2e-compose-api/compose.yaml -p e2e-capi up -d >/dev/null 2>&1 || fail "compose up with the Docker API"
[ -S /run/nspawn/api/e2e-capi-a-1.sock ] && [ -S /run/nspawn/api/e2e-capi-b-1.sock ] || fail "compose's machines have no Docker API socket"
[ -d /var/run/docker.sock ] && fail "compose made a directory where docker's socket would be"
api e2e-capi-a-1 /containers/json | grep_q '"com.docker.compose.project":"e2e-capi"' || fail "the compose labels are not on the Docker API"
$NSPAWN compose -f /tmp/e2e-compose-api/compose.yaml -p e2e-capi down >/dev/null 2>&1 || fail "compose down of e2e-capi"
for m in e2e-api-caddy e2e-api-web e2e-api-web2 e2e-api-boot; do $NSPAWN rm -f $m >/dev/null 2>&1; done
$NSPAWN network rm e2e-api >/dev/null || fail "network rm e2e-api"
$NSPAWN images rm e2e-api-curl >/dev/null 2>&1

step "auto-update: a labelled machine gets the image its tag names now, and goes back when it fails"
repo=e2e/au-$nonce
base="https://$NSPAWN_REGISTRY/v2/$repo"
ref="$NSPAWN_REGISTRY/$repo:1"
$NSPAWN pull "$busybox137_ref" --name e2e-au-137 --force >/dev/null || fail "pull busybox 1.37"
$NSPAWN push busybox-1.36 --to "$ref" >/dev/null 2>&1 || fail "push busybox 1.36 as $ref"
$NSPAWN network create e2e-au-net >/dev/null || fail "network create e2e-au-net"
page='mkdir -p /www; busybox | head -1 > /www/index.html; exec httpd -f -p 80 -h /www'
$NSPAWN run -d --name e2e-au --pull always --network e2e-au-net -p 18120:80 -e AU=yes -v e2e-au-vol:/data \
  --memory 64m --restart unless-stopped -l org.nspawn.auto-update=registry \
  --health-cmd 'wget -qO- http://127.0.0.1/ >/dev/null' --health-interval 1s --health-retries 3 \
  "$ref" /bin/sh -c "$page" >/dev/null || fail "run e2e-au"
$NSPAWN create busybox-1.36 e2e-au-side --network container:e2e-au -- /bin/sleep 300 >/dev/null || fail "create e2e-au-side"
$NSPAWN start e2e-au-side >/dev/null || fail "start e2e-au-side"
retry 15 bash -c "curl -fsS http://127.0.0.1:18120/ | grep_q 'v1.36'" || fail "e2e-au does not serve busybox 1.36"
$NSPAWN exec e2e-au -- /bin/sh -c 'echo kept > /data/f' </dev/null || fail "write into the volume of e2e-au"
before_gen=$($NSPAWN generate e2e-au 2>/dev/null | grep -v '^#')
before_addr=$(addr_of e2e-au)
before_digest=$($NSPAWN inspect e2e-au | python3 -c "import json,sys; print(json.load(sys.stdin)[0]['digest'])")
# Named: without names auto-update takes every labelled machine of the host.
out=$($NSPAWN auto-update --dry-run e2e-au 2>&1) || fail "auto-update --dry-run: $out"
echo "$out" | grep_q "^ *systemd-nspawn@e2e-au.service *e2e-au .* registry *false *$" || fail "auto-update --dry-run does not say e2e-au is current: $out"
# A newer image under the same tag.
$NSPAWN push e2e-au-137 --to "$ref" >/dev/null 2>&1 || fail "push busybox 1.37 as $ref"
out=$($NSPAWN auto-update --dry-run e2e-au 2>&1) || fail "auto-update --dry-run e2e-au: $out"
echo "$out" | grep_q " pending *$" || fail "auto-update --dry-run does not see the newer image: $out"
[ "$($NSPAWN inspect e2e-au | python3 -c "import json,sys; print(json.load(sys.stdin)[0]['digest'])")" = "$before_digest" ] || fail "a dry run changed the machine"
out=$($NSPAWN auto-update e2e-au 2>&1) || fail "auto-update: $out"
echo "$out" | grep_q "e2e-au .* registry *true *$" || fail "auto-update did not update e2e-au: $out"
retry 15 bash -c "curl -fsS http://127.0.0.1:18120/ | grep_q 'v1.37'" || fail "e2e-au does not serve busybox 1.37 after the update"
[ "$($NSPAWN generate e2e-au 2>/dev/null | grep -v '^#')" = "$before_gen" ] || fail "the update changed the options of e2e-au: $($NSPAWN generate e2e-au)"
[ "$(addr_of e2e-au)" = "$before_addr" ] || fail "the update changed the address of e2e-au"
[ "$($NSPAWN exec e2e-au -- cat /data/f </dev/null | tr -d '\r')" = kept ] || fail "the update lost what the volume held"
[ "$($NSPAWN inspect e2e-au | python3 -c "import json,sys; d = json.load(sys.stdin)[0]; print(d['digest'] != '$before_digest' and d['restart'] == 'unless-stopped' and d['memory'] == 67108864)")" = True ] || fail "inspect after the update: $($NSPAWN inspect e2e-au)"
retry 10 bash -c "$NSPAWN exec e2e-au-side -- wget -qO- http://127.0.0.1/ </dev/null | grep_q 'v1.37'" || fail "the machine sharing e2e-au's network did not follow it"
ls /var/lib/nspawn/rollback/e2e-au >/dev/null 2>&1 && fail "the update left its rollback behind"
# A broken image under the tag (an entrypoint that fails): the machine goes back.
manifest=/tmp/e2e-au-manifest.json
curl -sS $ca -u tester:s3cret -H 'Accept: application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json' "$base/manifests/1" -o $manifest || fail "fetch the manifest of $ref"
config=$(python3 -c "import json,sys; print(json.load(open(sys.argv[1]))['config']['digest'])" $manifest)
curl -sS $ca -u tester:s3cret "$base/blobs/$config" -o /tmp/e2e-au-config.json || fail "fetch the config of $ref"
python3 - /tmp/e2e-au-config.json <<'PY'
import json, sys
c = json.load(open(sys.argv[1]))
c["config"]["Entrypoint"] = ["/bin/false"]
json.dump(c, open(sys.argv[1], "w"))
PY
newconfig=$(push_blob /tmp/e2e-au-config.json) || fail "push the broken config"
python3 - $manifest "$newconfig" "$(stat -c %s /tmp/e2e-au-config.json)" <<'PY'
import json, sys
m = json.load(open(sys.argv[1]))
m["config"]["digest"], m["config"]["size"] = sys.argv[2], int(sys.argv[3])
json.dump(m, open(sys.argv[1], "w"))
PY
type=$(python3 -c "import json,sys; print(json.load(open(sys.argv[1])).get('mediaType', 'application/vnd.oci.image.manifest.v1+json'))" $manifest)
code=$(curl -sS $ca -u tester:s3cret -X PUT -H "Content-Type: $type" --data-binary @$manifest -o /dev/null -w '%{http_code}' "$base/manifests/1")
[ "$code" = 201 ] || fail "push of the broken manifest answered $code"
good_digest=$($NSPAWN inspect e2e-au | python3 -c "import json,sys; print(json.load(sys.stdin)[0]['digest'])")
out=$($NSPAWN auto-update e2e-au 2>&1) && fail "auto-update of a broken image succeeded: $out"
echo "$out" | grep_q "rolled back *$" || fail "auto-update of a broken image did not roll back: $out"
echo "$out" | grep_q "exited with code 1" || fail "the rollback does not say why: $out"
[ "$($NSPAWN inspect e2e-au | python3 -c "import json,sys; print(json.load(sys.stdin)[0]['digest'])")" = "$good_digest" ] || fail "e2e-au is not back on its image after the rollback"
retry 15 bash -c "curl -fsS http://127.0.0.1:18120/ | grep_q 'v1.37'" || fail "e2e-au does not serve again after the rollback"
[ "$($NSPAWN exec e2e-au -- cat /data/f </dev/null | tr -d '\r')" = kept ] || fail "the rollback lost the volume"
# The timer comes disabled; its service runs a pass on its own.
systemctl cat nspawn-auto-update.timer >/dev/null 2>&1 || fail "the auto-update timer is not installed"
[ "$(systemctl is-enabled nspawn-auto-update.timer 2>/dev/null)" = disabled ] || fail "the auto-update timer is enabled out of the box"
$NSPAWN push e2e-au-137 --to "$ref" >/dev/null 2>&1 || fail "push busybox 1.37 back as $ref"
# The service updates every labelled machine of the host: only where all are this run's.
labelled=$({ $NSPAWN ps -a -q --filter label=org.nspawn.auto-update && $NSPAWN ps -a -q --filter label=io.containers.autoupdate; } 2>/dev/null) || labelled="the listing failed"
others=$(echo "$labelled" | grep -v -x -e e2e-au -e e2e-au-side | grep -v '^$')
if [ -z "$others" ]; then
  systemctl start nspawn-auto-update.service || fail "nspawn-auto-update.service: $(journalctl -u nspawn-auto-update.service -n 20 --no-pager)"
else
  echo "labelled machines of the host ($(echo $others)): nspawn-auto-update.service not run"
fi
for m in e2e-au-side e2e-au e2e-au-137; do $NSPAWN rm -f $m >/dev/null 2>&1; done
$NSPAWN network rm e2e-au-net >/dev/null 2>&1
$NSPAWN volume rm e2e-au-vol >/dev/null 2>&1

step "docker's other flags and commands: --env-file, --mount, ps --filter and --format, inspect -f, port, wait, network connect, prune and df"
d=/tmp/e2e-cli
rm -rf $d && mkdir -p $d/bind
# run takes a reference: the one of the local busybox-1.36, so that nothing is pulled.
bb136=$($NSPAWN inspect -f '{{.reference}}' busybox-1.36) || fail "no local busybox-1.36"
printf '# comment\nE2E_A=from file\nE2E_B=overridden\n' > $d/env
printf 'e2e.file=yes\n' > $d/labels
echo bound > $d/bind/f
$NSPAWN network create e2e-cli-net >/dev/null || fail "network create e2e-cli-net"
$NSPAWN run -d --name e2e-cli -p 18130:80 --env-file $d/env -e E2E_B=flag --label-file $d/labels \
  --mount type=bind,source=$d/bind,target=/b,readonly --mount source=e2e-cli-vol,target=/v \
  --mount type=tmpfs,target=/t,tmpfs-size=1m "$bb136" \
  -- /bin/sh -c 'mkdir -p /www; echo cli > /www/index.html; exec httpd -f -p 80 -h /www' >/dev/null || fail "run with --env-file, --label-file and --mount"
[ "$($NSPAWN exec e2e-cli -- /bin/sh -c 'echo "$E2E_A/$E2E_B"' </dev/null | tr -d '\r')" = "from file/flag" ] || fail "--env-file and -e did not reach the program"
[ "$($NSPAWN inspect -f '{{index .labels "e2e.file"}}' e2e-cli)" = yes ] || fail "--label-file, or inspect -f: $($NSPAWN inspect -f '{{json .labels}}' e2e-cli)"
[ "$($NSPAWN exec e2e-cli -- cat /b/f </dev/null | tr -d '\r')" = bound ] || fail "the bind of --mount"
$NSPAWN exec e2e-cli -- touch /b/x </dev/null 2>/dev/null && fail "a readonly --mount took a write"
$NSPAWN exec e2e-cli -- /bin/sh -c 'grep -q " /t tmpfs " /proc/mounts && touch /v/x' </dev/null || fail "the volume and tmpfs of --mount"
[ "$($NSPAWN inspect -f '{{.state}} {{len .volumes}}' e2e-cli)" = "running 2" ] || fail "inspect -f with a function: $($NSPAWN inspect -f '{{.state}} {{len .volumes}}' e2e-cli 2>&1)"
$NSPAWN ps -q | grep_q -x e2e-cli || fail "ps -q misses e2e-cli: $($NSPAWN ps -q)"
[ "$($NSPAWN ps -q --filter label=e2e.file=yes)" = e2e-cli ] || fail "ps --filter label: $($NSPAWN ps -q --filter label=e2e.file=yes)"
[ -z "$($NSPAWN ps -q --filter name=^e2e-none-such$)" ] || fail "ps --filter name matched what it should not"
$NSPAWN ps --format '{{.Names}} {{.State}} {{.Label "e2e.file"}}' | grep_q -x 'e2e-cli running yes' || fail "ps --format: $($NSPAWN ps --format '{{.Names}} {{.State}} {{.Label "e2e.file"}}' 2>&1)"
$NSPAWN ps --format 'table {{.Names}}\t{{.Ports}}' | sed -n 1p | grep_q -E '^NAMES +PORTS$' || fail "ps --format table: $($NSPAWN ps --format 'table {{.Names}}\t{{.Ports}}' 2>&1)"
# A port given no IP is published on every IPv4 and, where the host has IPv6, every IPv6
# address, as docker shows it.
if [ "$host_ipv6" = yes ]; then
  cli_ports='0.0.0.0:18130->80/tcp, [::]:18130->80/tcp'
  cli_port=$'80/tcp -> 0.0.0.0:18130\n80/tcp -> [::]:18130'
  cli_port80=$'0.0.0.0:18130\n[::]:18130'
else
  cli_ports='0.0.0.0:18130->80/tcp'
  cli_port='80/tcp -> 0.0.0.0:18130'
  cli_port80='0.0.0.0:18130'
fi
$NSPAWN ps --format json --filter name=e2e-cli | python3 -c "import json,sys; rows = [json.loads(l) for l in sys.stdin]; assert rows[0]['Names'] == 'e2e-cli' and rows[0]['Ports'] == '$cli_ports', rows" || fail "ps --format json"
[ "$($NSPAWN port e2e-cli)" = "$cli_port" ] || fail "port: $($NSPAWN port e2e-cli 2>&1)"
[ "$($NSPAWN port e2e-cli 80)" = "$cli_port80" ] || fail "port with a port: $($NSPAWN port e2e-cli 80 2>&1)"
out=$($NSPAWN port e2e-cli 81 2>&1) && fail "port of a port not published: $out"
echo "$out" | grep_q "no public port '81' published for e2e-cli" || fail "port of a port not published was not explained: $out"
# A running app and a running booted machine join a network and leave it.
$NSPAWN run -d --name e2e-cli-peer --network e2e-cli-net "$bb136" \
  -- /bin/sh -c 'mkdir -p /www; echo peer > /www/index.html; exec httpd -f -p 80 -h /www' >/dev/null || fail "run e2e-cli-peer"
$NSPAWN network connect --alias cliweb e2e-cli-net e2e-cli >/dev/null || fail "network connect of a running app"
retry 5 bash -c "$NSPAWN exec e2e-cli-peer -- wget -qO- http://cliweb/ </dev/null | grep_q cli" || fail "the peer does not reach the connected machine by its alias"
retry 5 bash -c "$NSPAWN exec e2e-cli -- wget -qO- http://e2e-cli-peer/ </dev/null | grep_q peer" || fail "the connected machine does not reach the peer"
$NSPAWN inspect -f '{{json .networks}}' e2e-cli | grep_q e2e-cli-net || fail "inspect does not show the network joined"
out=$($NSPAWN network connect e2e-cli-net e2e-cli 2>&1) && fail "a second connect went through"
echo "$out" | grep_q "already exists in network e2e-cli-net" || fail "a second connect was not explained: $out"
$NSPAWN create "$IMAGE" e2e-cli-boot >/dev/null && $NSPAWN start e2e-cli-boot >/dev/null || fail "start e2e-cli-boot"
$NSPAWN network connect e2e-cli-net e2e-cli-boot >/dev/null || fail "network connect of a running booted machine"
retry 10 bash -c "$NSPAWN exec e2e-cli-boot -- curl -s http://e2e-cli-peer/ </dev/null | grep_q peer" || fail "the booted machine does not reach the peer on the network it joined"
$NSPAWN network disconnect e2e-cli-net e2e-cli >/dev/null || fail "network disconnect"
$NSPAWN exec e2e-cli -- wget -T 3 -qO- http://e2e-cli-peer/ </dev/null >/dev/null 2>&1 && fail "the disconnected machine still reaches the peer"
$NSPAWN inspect -f '{{json .networks}}' e2e-cli | grep_q e2e-cli-net && fail "inspect still shows the network left"
# Leaving the network of the default route hands it to the next one.
$NSPAWN network connect e2e-cli-net e2e-cli >/dev/null || fail "network connect again"
$NSPAWN network disconnect bridge e2e-cli >/dev/null || fail "network disconnect of the network of the default route"
$NSPAWN exec e2e-cli -- ip -4 route show default </dev/null | grep_q "default via" || fail "no default route after leaving the network that had it: $($NSPAWN exec e2e-cli -- ip -4 route </dev/null 2>&1)"
$NSPAWN stop e2e-cli-boot >/dev/null
# wait prints the code a machine ends with; ps finds it by it.
$NSPAWN run -d --name e2e-cli-wait --label e2e.prune=$nonce "$bb136" -- /bin/sh -c 'sleep 2; exit 3' >/dev/null || fail "run e2e-cli-wait"
[ "$(timeout 30 $NSPAWN wait e2e-cli-wait)" = 3 ] || fail "wait did not print the exit code"
$NSPAWN ps -q --filter exited=3 | grep_q -x e2e-cli-wait || fail "ps --filter exited=3"
$NSPAWN ps -q --filter status=exited | grep_q -x e2e-cli-wait || fail "ps --filter status=exited"
# A machine of run -d --rm is gone once it ends, and one with a restart policy runs
# again: wait gives the code of the end all the same.
$NSPAWN run -d --rm --name e2e-cli-wrm "$bb136" -- /bin/sh -c 'sleep 2; exit 4' >/dev/null || fail "run -d --rm e2e-cli-wrm"
[ "$(timeout 30 $NSPAWN wait e2e-cli-wrm)" = 4 ] || fail "wait did not give the code of a run -d --rm machine"
$NSPAWN run -d --restart always --name e2e-cli-wrs "$bb136" -- /bin/sh -c 'sleep 2; exit 5' >/dev/null || fail "run -d --restart always e2e-cli-wrs"
[ "$(timeout 30 $NSPAWN wait e2e-cli-wrs)" = 5 ] || fail "wait did not return at the end of a machine with a restart policy"
$NSPAWN rm -f e2e-cli-wrs >/dev/null 2>&1
# What takes the disk, and its prune: only what no machine uses goes.
out=$($NSPAWN system df) || fail "system df"
for kind in Images Containers "Local Volumes" "Build Cache"; do echo "$out" | grep_q "^ *$kind " || fail "system df misses $kind: $out"; done
$NSPAWN system df -v | grep_q "^Images space usage:" || fail "system df -v"
# The prunes only ever run with a filter that selects what this run made: a host that
# runs the suite has images and machines of its own. An image of the run's own is
# busybox 1.37 with a label, pushed to the test registry.
python3 - "$NSPAWN_REGISTRY" "$NSPAWN_CA_CERT" "$busybox137_ref" "e2e/prune-$nonce" "$nonce" <<'PY' || fail "push of the labelled image"
import base64, hashlib, json, ssl, sys, urllib.request
reg, ca, src, dst, nonce = sys.argv[1:]
ctx = ssl.create_default_context(cafile=ca)
# The test registry's CA has no key usage extension, which Python 3.13 and later
# refuse by default.
ctx.verify_flags &= ~ssl.VERIFY_X509_STRICT
auth = "Basic " + base64.b64encode(b"tester:s3cret").decode()
def req(method, url, data=None, headers={}):
    return urllib.request.urlopen(urllib.request.Request(url, data=data, method=method, headers={"Authorization": auth, **headers}), context=ctx)
def put_blob(data):
    digest = "sha256:" + hashlib.sha256(data).hexdigest()
    location = req("POST", f"https://{reg}/v2/{dst}/blobs/uploads/").headers["Location"]
    if location.startswith("/"):
        location = f"https://{reg}{location}"
    req("PUT", location + ("&" if "?" in location else "?") + "digest=" + digest, data, {"Content-Type": "application/octet-stream"})
    return digest
repo, tag = src[len(reg) + 1:].rsplit(":", 1)
manifest = json.load(req("GET", f"https://{reg}/v2/{repo}/manifests/{tag}", headers={"Accept": "application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json"}))
config = json.load(req("GET", f"https://{reg}/v2/{repo}/blobs/{manifest['config']['digest']}"))
config.setdefault("config", {}).setdefault("Labels", {})["e2e.prune"] = nonce
raw = json.dumps(config).encode()
manifest["config"]["digest"], manifest["config"]["size"] = put_blob(raw), len(raw)
for layer in manifest["layers"]:
    put_blob(req("GET", f"https://{reg}/v2/{repo}/blobs/{layer['digest']}").read())
req("PUT", f"https://{reg}/v2/{dst}/manifests/1", json.dumps(manifest).encode(), {"Content-Type": manifest.get("mediaType", "application/vnd.oci.image.manifest.v1+json")})
PY
$NSPAWN pull "$NSPAWN_REGISTRY/e2e/prune-$nonce:1" --name e2e-cli-img --force >/dev/null || fail "pull e2e-cli-img"
$NSPAWN pull "$busybox137_ref" --name e2e-cli-keep --force >/dev/null || fail "pull e2e-cli-keep"
out=$($NSPAWN images prune -f --filter label=e2e.prune=$nonce) || fail "images prune: $out"
echo "$out" | grep_q "^Total reclaimed space: 0B$" || fail "images prune without -a removed something: $out"
out=$($NSPAWN images prune -a -f --filter label=e2e.prune=$nonce) || fail "images prune -a: $out"
echo "$out" | grep_q "^deleted: e2e-cli-img$" || fail "images prune -a kept the unused image its filter selects: $out"
[ "$(echo "$out" | grep -c '^deleted: ')" = 1 ] || fail "images prune -a removed what its filter does not select: $out"
$NSPAWN images ls | grep_q "^ *e2e-cli-keep " || fail "images prune -a removed an image without the label"
$NSPAWN images ls | grep_q "^ *busybox-1.36 " || fail "images prune -a removed an image machines were made from"
# system prune: the stopped machine and the unused network with the label go; a stopped
# machine and a network without it stay, and so do running machines.
$NSPAWN network create e2e-cli-pnet --label e2e.prune=$nonce >/dev/null || fail "network create e2e-cli-pnet"
out=$($NSPAWN system prune -f --filter label=e2e.prune=$nonce) || fail "system prune: $out"
echo "$out" | grep_q -x e2e-cli-wait || fail "system prune kept the stopped machine its filter selects: $out"
echo "$out" | grep_q -x e2e-cli-pnet || fail "system prune kept the unused network its filter selects: $out"
echo "$out" | grep_q -x e2e-cli-boot && fail "system prune removed a stopped machine without the label: $out"
$NSPAWN ps -a -q | grep_q -x e2e-cli-boot || fail "system prune removed e2e-cli-boot"
$NSPAWN network ls | grep_q e2e-cli-net || fail "system prune removed a network without the label"
$NSPAWN ps -q | grep_q -x e2e-cli || fail "system prune removed a running machine"
for m in e2e-cli e2e-cli-peer e2e-cli-boot e2e-cli-wait e2e-cli-img e2e-cli-keep; do $NSPAWN rm -f $m >/dev/null 2>&1; done
$NSPAWN network rm e2e-cli-net e2e-cli-pnet >/dev/null 2>&1
$NSPAWN volume rm e2e-cli-vol >/dev/null 2>&1
rm -rf $d

step "--ip: a fixed address on a network of the user's, kept across starts"
$NSPAWN network create e2e-ip-net --subnet 10.97.42.0/24 >/dev/null || fail "network create e2e-ip-net"
$NSPAWN create busybox-1.36 e2e-ip --network e2e-ip-net --ip 10.97.42.50 -- /bin/sleep 300 >/dev/null || fail "create --ip"
$NSPAWN start e2e-ip >/dev/null || fail "start e2e-ip"
[ "$(addr_of e2e-ip)" = 10.97.42.50 ] || fail "the address asked for is not the machine's: $(addr_of e2e-ip)"
$NSPAWN exec e2e-ip -- ip -4 addr show host0 </dev/null | grep_q "inet 10.97.42.50/24" || fail "the machine does not have the address it asked for"
$NSPAWN stop e2e-ip >/dev/null && $NSPAWN start e2e-ip >/dev/null || fail "restart of e2e-ip"
[ "$(addr_of e2e-ip)" = 10.97.42.50 ] || fail "the fixed address changed across a restart"
out=$($NSPAWN create busybox-1.36 e2e-ip2 --network e2e-ip-net --ip 10.97.42.50 -- /bin/sleep 300 2>&1) && fail "a second machine took a taken address: $out"
echo "$out" | grep_q "Address already in use" || fail "a taken address was not explained: $out"
# A machine made from e2e-ip cannot take its address either.
out=$($NSPAWN create e2e-ip e2e-ip2 --network e2e-ip-net --ip 10.97.42.50 2>&1) && fail "a machine made from e2e-ip took its address: $out"
echo "$out" | grep_q "is the address of e2e-ip" || fail "the address of the source was not explained: $out"
out=$($NSPAWN create busybox-1.36 e2e-ip2 --network e2e-ip-net --ip 10.97.43.5 -- /bin/sleep 300 2>&1) && fail "an address outside the subnet was taken: $out"
echo "$out" | grep_q "no configured subnet or ip-range contain the IP address 10.97.43.5" || fail "an address outside the subnet was not explained: $out"
out=$($NSPAWN create busybox-1.36 e2e-ip2 --ip 10.99.0.200 -- /bin/sleep 300 2>&1) && fail "an address on the default network was taken: $out"
echo "$out" | grep_q "user specified IP address is supported on user defined networks only" || fail "an address on the default network was not explained: $out"
$NSPAWN network create e2e-ip-net2 --subnet 10.97.43.0/24 >/dev/null || fail "network create e2e-ip-net2"
$NSPAWN network connect --ip 10.97.43.7 e2e-ip-net2 e2e-ip >/dev/null || fail "network connect --ip"
$NSPAWN exec e2e-ip -- ip -4 addr </dev/null | grep_q "inet 10.97.43.7/24" || fail "network connect --ip did not give the address"
$NSPAWN rm -f e2e-ip >/dev/null 2>&1
# compose's ipv4_address.
mkdir -p /tmp/e2e-compose-ip
cat > /tmp/e2e-compose-ip/compose.yaml <<'COMPOSE'
services:
  app:
    image: busybox:1.36
    command: ["sleep", "300"]
    networks:
      fixed:
        ipv4_address: 10.97.44.9
networks:
  fixed:
    ipam:
      config:
        - subnet: 10.97.44.0/24
COMPOSE
out=$($NSPAWN compose -f /tmp/e2e-compose-ip/compose.yaml -p e2e-cip up -d 2>&1) || fail "compose up with ipv4_address: $out"
[ "$($NSPAWN inspect e2e-cip-app-1 | python3 -c "import json,sys; print(json.load(sys.stdin)[0]['addresses'].get('e2e-cip_fixed', ''))")" = 10.97.44.9 ] || fail "compose's ipv4_address was not the machine's: $($NSPAWN inspect e2e-cip-app-1)"
$NSPAWN compose -f /tmp/e2e-compose-ip/compose.yaml -p e2e-cip down >/dev/null 2>&1 || fail "compose down of e2e-cip"
$NSPAWN network rm e2e-ip-net e2e-ip-net2 >/dev/null 2>&1
rm -rf /tmp/e2e-compose-ip

step "CDI: the devices of the host's specs, --gpus, and their hooks before the start"
# Specs as a vendor's tool writes them: device nodes, a library, variables and a hook,
# and an NVIDIA spec whose nodes are /dev/null and /dev/zero. No GPU is needed.
d=/tmp/e2e-cdi
rm -rf $d && mkdir -p $d /etc/cdi /var/run/cdi
bb136=$($NSPAWN inspect -f '{{.reference}}' busybox-1.36) || fail "no local busybox-1.36"
echo "e2e library $nonce" > $d/libe2e.so.1
cat > /usr/local/bin/e2e-cdi-hook <<'HOOK'
#!/usr/bin/python3 -I
# A createContainer hook: the OCI state on stdin, the machine's root in the bundle.
import json, os, sys
state = json.load(sys.stdin)
config = json.load(open(os.path.join(state["bundle"], "config.json")))
root = config["root"]["path"]
lib = os.path.join(root, "usr/lib/e2e")
os.symlink("libe2e.so.1", os.path.join(lib, "libe2e.so"))
seen = {
    "id": state["id"], "status": state["status"], "argv": sys.argv,
    "hook_env": os.environ.get("E2E_HOOK"),
    "lib": open(os.path.join(lib, "libe2e.so.1")).read().strip(),
    "node": os.path.exists(os.path.join(root, "dev/e2e-null")),
    "proc": os.path.exists(os.path.join(root, "proc/self/status")),
}
json.dump(seen, open(os.path.join(root, "e2e-cdi-hook.json"), "w"))
HOOK
chmod 755 /usr/local/bin/e2e-cdi-hook
cat > /etc/cdi/e2e-nspawn.yaml <<SPEC
---
cdiVersion: 0.6.0
kind: e2e.nspawn.test/dev
containerEdits:
  env:
  - E2E_CDI_SPEC=1
devices:
- name: one
  containerEdits:
    env: [E2E_CDI=one]
    deviceNodes:
    - path: /dev/e2e-null
      hostPath: /dev/null
    mounts:
    - hostPath: $d/libe2e.so.1
      containerPath: /usr/lib/e2e/libe2e.so.1
      options: [ro, nosuid, nodev, bind]
    hooks:
    - hookName: createContainer
      path: /usr/local/bin/e2e-cdi-hook
      args: [e2e-cdi-hook, one]
      env: [E2E_HOOK=yes]
- name: two
  containerEdits:
    env: [E2E_CDI=two]
    deviceNodes:
    - path: /dev/e2e-zero
      hostPath: /dev/zero
      permissions: r
- name: made
  containerEdits:
    deviceNodes:
    - path: /dev/e2e-made
      type: c
      major: 1
      minor: 5
- name: failing
  containerEdits:
    hooks:
    - hookName: createContainer
      path: /bin/false
SPEC
printf '%s\n' '{"cdiVersion":"0.5.0","kind":"nvidia.com/gpu","containerEdits":{"env":["NVIDIA_VISIBLE_DEVICES=void"]},"devices":[' \
  '{"name":"0","containerEdits":{"deviceNodes":[{"path":"/dev/nvidia0","hostPath":"/dev/null"}]}},' \
  '{"name":"1","containerEdits":{"deviceNodes":[{"path":"/dev/nvidia1","hostPath":"/dev/zero"}]}},' \
  '{"name":"all","containerEdits":{"deviceNodes":[{"path":"/dev/nvidia0","hostPath":"/dev/null"},{"path":"/dev/nvidia1","hostPath":"/dev/zero"}]}}]}' \
  > /var/run/cdi/e2e-nspawn-gpu.json
printf 'cdiVersion: 0.6.0\nkind: e2e.nspawn.test/bad\nbogus: 1\ndevices:\n- name: x\n  containerEdits: {env: [A=1]}\n' > /etc/cdi/e2e-nspawn-bad.yaml
# An app: the node, the library, the variables, and what the hook saw and left.
out=$($NSPAWN run --rm --device e2e.nspawn.test/dev=one "$bb136" -- /bin/sh -c \
  'echo "env $E2E_CDI_SPEC/$E2E_CDI"; test -c /dev/e2e-null && echo node; cat /usr/lib/e2e/libe2e.so.1; echo "link $(readlink /usr/lib/e2e/libe2e.so)"; cat /e2e-cdi-hook.json' 2>&1) || fail "run with a CDI device: $out"
echo "$out" | grep_q -x "env 1/one" || fail "the variables of the spec and the device are not the program's: $out"
echo "$out" | grep_q -x node || fail "the CDI device node is missing: $out"
echo "$out" | grep_q -x "e2e library $nonce" || fail "the CDI mount is missing: $out"
echo "$out" | grep_q -x "link libe2e.so.1" || fail "what the hook made in the root is missing: $out"
echo "$out" | tail -1 | python3 -c "
import json, sys
seen = json.load(sys.stdin)
assert seen['status'] == 'creating' and seen['id'].startswith('busybox-1.36-'), seen
assert seen['argv'] == ['/usr/local/bin/e2e-cdi-hook', 'one'] or seen['argv'][1:] == ['one'], seen
assert seen['hook_env'] == 'yes' and seen['lib'] == 'e2e library $nonce', seen
assert seen['node'] and seen['proc'], seen
" || fail "the hook did not see the machine as a runtime shows it: $(echo "$out" | tail -1)"
# A node the host lacks is made with the spec's numbers; the access rules go to the
# unit (the template allows /dev/zero fully anyway, so the kernel is not asked here).
out=$($NSPAWN run --rm --device e2e.nspawn.test/dev=two --device e2e.nspawn.test/dev=made "$bb136" -- /bin/sh -c \
  'echo "env $E2E_CDI"; head -c 4 /dev/e2e-zero | wc -c; head -c 4 /dev/e2e-made | wc -c' 2>&1)
echo "$out" | grep_q -x "env two" || fail "the device's variable: $out"
[ "$(echo "$out" | grep -cx 4)" = 2 ] || fail "the CDI nodes cannot be read: $out"
$NSPAWN create busybox-1.36 e2e-cdi-perm --device e2e.nspawn.test/dev=two -- /bin/sleep 300 >/dev/null && $NSPAWN start e2e-cdi-perm >/dev/null || fail "start e2e-cdi-perm"
systemctl show -p DeviceAllow systemd-nspawn@e2e-cdi-perm.service | grep_q "/dev/zero r\b" || fail "the r of the spec is not the unit's: $(systemctl show -p DeviceAllow systemd-nspawn@e2e-cdi-perm.service)"
$NSPAWN rm -f e2e-cdi-perm >/dev/null 2>&1
# --gpus: the NVIDIA spec's devices, as docker's flag names them.
out=$($NSPAWN run --rm --gpus all "$bb136" -- /bin/sh -c 'echo "$NVIDIA_VISIBLE_DEVICES"; for n in 0 1; do test -c /dev/nvidia$n && echo nvidia$n; done' 2>&1) || fail "run --gpus all: $out"
echo "$out" | grep_q -x void && echo "$out" | grep_q -x nvidia1 || fail "--gpus all: $out"
out=$($NSPAWN run --rm --gpus 1 "$bb136" -- /bin/sh -c 'for n in 0 1; do test -c /dev/nvidia$n && echo nvidia$n; done' 2>&1)
echo "$out" | grep_q -x nvidia0 && ! echo "$out" | grep_q -x nvidia1 || fail "--gpus 1: $out"
out=$($NSPAWN run --rm --gpus '"device=1"' "$bb136" -- test -c /dev/nvidia1 2>&1) || fail "--gpus device=1: $out"
out=$($NSPAWN run --rm --gpus 3 "$bb136" -- true 2>&1) && fail "--gpus 3 ran with two GPUs: $out"
echo "$out" | grep_q "unresolvable CDI devices nvidia.com/gpu=2" || fail "--gpus 3 was not explained: $out"
out=$($NSPAWN run --rm --device e2e.nspawn.test/dev=nope "$bb136" -- true 2>&1) && fail "a device the specs lack: $out"
echo "$out" | grep_q "unresolvable CDI devices e2e.nspawn.test/dev=nope" || fail "a device the specs lack was not explained: $out"
out=$($NSPAWN run --rm --device e2e.nspawn.test/bad=x "$bb136" -- true 2>&1) && fail "a device of a spec with an unknown field: $out"
echo "$out" | grep_q "unknown field" || fail "a spec with an unknown field was not explained: $out"
# A hook that fails stops the start.
$NSPAWN create busybox-1.36 e2e-cdi-bad --device e2e.nspawn.test/dev=failing -- /bin/sleep 300 >/dev/null || fail "create e2e-cdi-bad"
out=$($NSPAWN start e2e-cdi-bad 2>&1) && fail "a failing CDI hook let the machine start: $out"
{ echo "$out"; journalctl -u systemd-nspawn@e2e-cdi-bad.service --since "-2min" --no-pager 2>/dev/null; } | grep_q "createContainer hook /bin/false of the CDI specs failed" || fail "the failing hook was not explained: $out"
$NSPAWN rm -f e2e-cdi-bad >/dev/null 2>&1
# A booted machine, and what inspect and generate say.
$NSPAWN create "$IMAGE" e2e-cdi-boot --device e2e.nspawn.test/dev=one --gpus 1 >/dev/null && $NSPAWN start e2e-cdi-boot >/dev/null || fail "start a booted machine with CDI devices"
retry 10 bash -c "$NSPAWN exec e2e-cdi-boot -- /bin/sh -c 'test -c /dev/e2e-null && test -c /dev/nvidia0 && cat /usr/lib/e2e/libe2e.so.1 && readlink /usr/lib/e2e/libe2e.so' </dev/null | grep_q libe2e.so.1" || fail "the booted machine misses its CDI devices"
$NSPAWN inspect -f '{{json .devices}} {{json .gpus}}' e2e-cdi-boot | grep_q '\["e2e.nspawn.test/dev=one"\] \["count=1"\]' || fail "inspect: $($NSPAWN inspect -f '{{json .devices}} {{json .gpus}}' e2e-cdi-boot)"
$NSPAWN generate e2e-cdi-boot | grep_q -- "--device e2e.nspawn.test/dev=one" || fail "generate misses the CDI device: $($NSPAWN generate e2e-cdi-boot)"
$NSPAWN generate e2e-cdi-boot | grep_q -- "--gpus count=1" || fail "generate misses --gpus: $($NSPAWN generate e2e-cdi-boot)"
$NSPAWN rm -f e2e-cdi-boot >/dev/null 2>&1
# compose's gpus and CDI names in devices.
mkdir -p $d/compose
cat > $d/compose/compose.yaml <<'COMPOSE'
services:
  ml:
    image: busybox:1.36
    command: ["sleep", "300"]
    devices:
      - e2e.nspawn.test/dev=two
    gpus: all
COMPOSE
out=$($NSPAWN compose -f $d/compose/compose.yaml -p e2e-cdi up -d 2>&1) || fail "compose up with gpus: $out"
$NSPAWN exec e2e-cdi-ml-1 -- /bin/sh -c 'test -c /dev/nvidia1 && test -c /dev/e2e-zero' </dev/null || fail "compose's gpus and CDI devices did not reach the machine"
$NSPAWN compose -f $d/compose/compose.yaml -p e2e-cdi down >/dev/null 2>&1 || fail "compose down of e2e-cdi"
# Without a spec of the vendor, docker's own words.
rm -f /var/run/cdi/e2e-nspawn-gpu.json
out=$($NSPAWN run --rm --gpus all "$bb136" -- true 2>&1) && fail "--gpus without a spec ran: $out"
echo "$out" | grep_q 'could not select device driver "" with capabilities: \[\[gpu\]\]' || fail "--gpus without a spec was not explained: $out"
rm -f /etc/cdi/e2e-nspawn.yaml /etc/cdi/e2e-nspawn-bad.yaml /usr/local/bin/e2e-cdi-hook
rm -rf $d

$NSPAWN images rm busybox-1.36 > /dev/null 2>&1

step "build: Dockerfiles built as the reference builds them, the cache, the sandbox"
# Each case of tests/build was built by docker in tests/build/oracle.sh; here nspawn
# builds it from the same bases (copies in the test registry) and the images must have
# the same settings, history and layer contents.
cases="$(dirname "$0")/build/cases"
# What the host's build cache held before: a prune at the end runs only when nothing.
cache_before=$($NSPAWN builder du --json 2>/dev/null | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))' 2>/dev/null) || cache_before=unknown
golden="$(dirname "$0")/build/golden"
for dir in "$cases"/*/; do
  name=$(basename "$dir")
  work=/tmp/e2e-build-$name
  rm -rf $work && cp -a "$dir" $work
  # The modes git gives the files, as the reference saw them, whatever the umask the
  # tree was unpacked with.
  find $work -type d -exec chmod 0755 {} +
  find $work -type f -perm -u=x -exec chmod 0755 {} +
  find $work -type f ! -perm -u=x -exec chmod 0644 {} +
  args=()
  if [ -f $work/args ]; then
    while read -r line; do args+=($(echo "$line" | sed "s#@CASE@#$work#g")); done < $work/args
  fi
  if [ -f $work/expect-error ]; then
    out=$($NSPAWN build -t e2e-case-$name --build-arg BUSYBOX="$busybox137_ref" --build-arg ALPINE="$alpine322_ref" "${args[@]}" $work 2>&1) && fail "build of $name went through"
    wanted=$(sed 's/^ERROR: failed to build: failed to solve: //' "$golden/$name.error")
    echo "$out" | grep_q -F "$wanted" || fail "build of $name failed otherwise: $out"
  else
    $NSPAWN build -t e2e-case-$name --build-arg BUSYBOX="$busybox137_ref" --build-arg ALPINE="$alpine322_ref" "${args[@]}" $work > /tmp/e2e-build.txt 2>&1 || { cat /tmp/e2e-build.txt; fail "build of $name"; continue; }
    python3 -I "$(dirname "$0")/build/describe.py" store /var/lib/nspawn e2e-case-$name --bases "$golden/bases.json" > /tmp/e2e-build-$name.json || fail "describe $name"
    out=$(python3 -I "$(dirname "$0")/build/describe.py" compare "$golden/$name.json" /tmp/e2e-build-$name.json) || fail "the image of $name differs from the reference's:
$out"
  fi
  rm -rf $work
done
# The same build again comes from the cache whole, with the same image.
work=/tmp/e2e-build-layers && rm -rf $work && cp -a "$cases/layers" $work
id1=$($NSPAWN build -q -t e2e-case-layers --build-arg BUSYBOX="$busybox137_ref" $work 2>/dev/null) || fail "quiet build"
out=$($NSPAWN build -t e2e-case-layers --build-arg BUSYBOX="$busybox137_ref" $work 2>&1) || fail "second build: $out"
echo "$out" | grep_q CACHED || fail "the second build took nothing from the cache: $out"
id2=$($NSPAWN build -q -t e2e-case-layers --build-arg BUSYBOX="$busybox137_ref" $work 2>/dev/null) || fail "third build"
[ "$id1" = "$id2" ] || fail "a build taken from the cache gave another image: $id1 $id2"
# A file of the context that changes builds again from the step that copies it: to
# contents of this run, which no earlier run left in the cache.
echo changed-$nonce > $work/files/top.txt
out=$($NSPAWN build -t e2e-case-layers --build-arg BUSYBOX="$busybox137_ref" $work 2>&1) || fail "build after a change: $out"
echo "$out" | grep -A1 'COPY files/ ./' | grep_q CACHED && fail "the changed file came from the cache: $out"
id3=$($NSPAWN build -q -t e2e-case-layers --build-arg BUSYBOX="$busybox137_ref" $work 2>/dev/null)
[ "$id3" != "$id1" ] || fail "a changed context gave the same image"
out=$($NSPAWN build --no-cache -t e2e-case-layers --build-arg BUSYBOX="$busybox137_ref" $work 2>&1) || fail "--no-cache: $out"
echo "$out" | grep_q CACHED && fail "--no-cache took steps from the cache"
rm -rf $work
# The image runs, and goes to a registry and back.
# An exec form CMD is not expanded: echo gets $GREETING as it is written.
[ "$($NSPAWN run --rm e2e-case-settings 2>/dev/null)" = 'serve $GREETING' ] || fail "the built image does not run its entrypoint: $($NSPAWN run --rm e2e-case-settings 2>&1)"
$NSPAWN push e2e-case-multistage --to "$NSPAWN_REGISTRY/e2e/built-$nonce:1" >/dev/null 2>&1 || fail "push of a built image"
$NSPAWN images rm e2e-case-multistage >/dev/null || fail "images rm of a built image"
$NSPAWN pull "$NSPAWN_REGISTRY/e2e/built-$nonce:1" --name e2e-case-multistage >/dev/null || fail "pull of a pushed built image"
[ "$($NSPAWN run --rm "$NSPAWN_REGISTRY/e2e/built-$nonce:1" 2>/dev/null)" = built ] || fail "the pulled built image does not run: $($NSPAWN run --rm "$NSPAWN_REGISTRY/e2e/built-$nonce:1" 2>&1)"
# ADD of a URL: checked against --checksum, and taken by its contents, so that what the
# server has now is fetched again.
work=/tmp/e2e-build-url && rm -rf $work && mkdir -p $work/served
echo fetched-one-$nonce > $work/served/file.txt
python3 -m http.server --bind 127.0.0.1 18199 --directory $work/served >/dev/null 2>&1 &
served=$!
sum=$(sha256sum $work/served/file.txt | cut -d' ' -f1)
printf 'ARG BUSYBOX\nFROM ${BUSYBOX}\nADD --checksum=sha256:%s http://127.0.0.1:18199/file.txt /fetched.txt\nCMD ["cat", "/fetched.txt"]\n' $sum > $work/Dockerfile
retry 10 bash -c "curl -sf http://127.0.0.1:18199/file.txt >/dev/null" || fail "the local HTTP server did not come up"
$NSPAWN build -t e2e-build-url --build-arg BUSYBOX="$busybox137_ref" $work >/dev/null 2>&1 || fail "ADD of a URL with its checksum"
[ "$($NSPAWN run --rm e2e-build-url 2>/dev/null)" = fetched-one-$nonce ] || fail "the image does not have what the URL served"
printf 'ARG BUSYBOX\nFROM ${BUSYBOX}\nADD http://127.0.0.1:18199/file.txt /fetched.txt\nCMD ["cat", "/fetched.txt"]\n' > $work/Dockerfile
$NSPAWN build -t e2e-build-url --build-arg BUSYBOX="$busybox137_ref" $work >/dev/null 2>&1 || fail "ADD of a URL"
echo fetched-two-$nonce > $work/served/file.txt
out=$($NSPAWN build -t e2e-build-url --build-arg BUSYBOX="$busybox137_ref" $work 2>&1) || fail "ADD of a URL whose contents changed: $out"
echo "$out" | grep -A1 'ADD http' | grep_q CACHED && fail "a URL whose contents changed came from the cache: $out"
[ "$($NSPAWN run --rm e2e-build-url 2>/dev/null)" = fetched-two-$nonce ] || fail "the image does not have what the URL serves now"
printf 'ARG BUSYBOX\nFROM ${BUSYBOX}\nADD --checksum=sha256:%064d http://127.0.0.1:18199/file.txt /fetched.txt\n' 0 > $work/Dockerfile
out=$($NSPAWN build -t e2e-build-url --build-arg BUSYBOX="$busybox137_ref" $work 2>&1) && fail "ADD with a wrong checksum went through"
echo "$out" | grep_q "digest mismatch" || fail "a wrong checksum was not explained: $out"
kill $served 2>/dev/null; wait $served 2>/dev/null
rm -rf $work
# A step runs confined: no kexec, no module loading, root without the host's devices.
work=/tmp/e2e-build-sandbox && rm -rf $work && mkdir -p $work
printf 'ARG BUSYBOX\nFROM ${BUSYBOX}\nRUN id -u > /uid && (insmod /x.ko 2>/dev/null; echo $? > /insmod) && ls /dev > /devs && (ping -c1 -W2 10.0.0.1 >/dev/null 2>&1; true)\n' > $work/Dockerfile
$NSPAWN build -t e2e-build-sandbox --build-arg BUSYBOX="$busybox137_ref" $work >/dev/null 2>&1 || fail "build of the sandbox case"
[ "$($NSPAWN run --rm e2e-build-sandbox cat /uid 2>/dev/null)" = 0 ] || fail "a step does not run as root"
$NSPAWN run --rm e2e-build-sandbox cat /devs 2>/dev/null | grep_q '^sda$\|^vda$\|^nvme' && fail "a step saw the host's disks"
# --network none and host.
printf 'ARG BUSYBOX\nFROM ${BUSYBOX}\nRUN --network=none ip -o link | grep -v " lo:" | wc -l > /links\n' > $work/Dockerfile
$NSPAWN build -t e2e-build-sandbox --build-arg BUSYBOX="$busybox137_ref" $work >/dev/null 2>&1 || fail "RUN --network=none"
[ "$($NSPAWN run --rm e2e-build-sandbox cat /links 2>/dev/null)" = 0 ] || fail "RUN --network=none had an interface"
printf 'ARG BUSYBOX\nFROM ${BUSYBOX}\nRUN --network=host true\n' > $work/Dockerfile
# Built with --allow first, so that the step is in the cache: the entitlement is asked
# for all the same.
$NSPAWN build --allow network.host -t e2e-build-sandbox --build-arg BUSYBOX="$busybox137_ref" $work >/dev/null 2>&1 || fail "RUN --network=host with --allow, into the cache"
out=$($NSPAWN build -t e2e-build-sandbox --build-arg BUSYBOX="$busybox137_ref" $work 2>&1) && fail "RUN --network=host without --allow: $out"
echo "$out" | grep_q "network.host is not allowed" || fail "RUN --network=host without --allow was not explained: $out"
$NSPAWN build --allow network.host -t e2e-build-sandbox --build-arg BUSYBOX="$busybox137_ref" $work >/dev/null 2>&1 || fail "RUN --network=host with --allow"
# A failed build leaves no mount, namespace or directory behind.
grep -q '/var/lib/nspawn/builds/df-' /proc/self/mountinfo && fail "a build left a mount: $(grep /var/lib/nspawn/builds/ /proc/self/mountinfo)"
ls /var/lib/nspawn/builds/ 2>/dev/null | grep_q '^df-' && fail "a build left its directory: $(ls /var/lib/nspawn/builds/)"
ip netns list | grep_q -F 'nspawn-.build-' && fail "a build left its network namespace: $(ip netns list)"
rm -rf $work
# Two builds at once, each with its own steps, both whole.
work=/tmp/e2e-build-two && rm -rf $work && mkdir -p $work/a $work/b
for x in a b; do printf 'ARG BUSYBOX\nFROM ${BUSYBOX}\nRUN echo %s-%s > /which && sleep 2\nCMD ["cat", "/which"]\n' $x $nonce > $work/$x/Dockerfile; done
$NSPAWN build -t e2e-build-two-a --build-arg BUSYBOX="$busybox137_ref" $work/a > /tmp/e2e-build-a.txt 2>&1 &
pa=$!
$NSPAWN build -t e2e-build-two-b --build-arg BUSYBOX="$busybox137_ref" $work/b > /tmp/e2e-build-b.txt 2>&1 &
pb=$!
wait $pa || fail "the first of two builds at once: $(cat /tmp/e2e-build-a.txt)"
wait $pb || fail "the second of two builds at once: $(cat /tmp/e2e-build-b.txt)"
[ "$($NSPAWN run --rm e2e-build-two-a 2>/dev/null)" = "a-$nonce" ] || fail "the first of two builds at once made another image"
[ "$($NSPAWN run --rm e2e-build-two-b 2>/dev/null)" = "b-$nonce" ] || fail "the second of two builds at once made another image"
# A build whose client goes away stops, and leaves nothing behind.
# The sleep's own number tells it apart: the shell may run it in its own place.
nap=3${nonce: -5}
printf 'ARG BUSYBOX\nFROM ${BUSYBOX}\nRUN echo cancel-%s && sleep %s\n' $nonce $nap > $work/a/Dockerfile
$NSPAWN build -t e2e-build-cancel --build-arg BUSYBOX="$busybox137_ref" $work/a > /tmp/e2e-build-cancel.txt 2>&1 &
pc=$!
for i in $(seq 1 60); do grep -q "cancel-$nonce" /tmp/e2e-build-cancel.txt && break; sleep 1; done
grep -q "cancel-$nonce" /tmp/e2e-build-cancel.txt || fail "the build to cancel did not start: $(cat /tmp/e2e-build-cancel.txt)"
kill -TERM $pc; wait $pc 2>/dev/null
for i in $(seq 1 30); do pgrep -f "cancel-$nonce|sleep $nap" >/dev/null || break; sleep 1; done
pgrep -af "cancel-$nonce|sleep $nap" && fail "the step of a cancelled build still runs"
[ -z "$(systemctl list-units --all --no-legend 'nspawn-build-*.scope' 2>/dev/null)" ] || fail "a cancelled build left the scope of its step: $(systemctl list-units --all --no-legend 'nspawn-build-*.scope')"
for i in $(seq 1 10); do ls /var/lib/nspawn/builds/ 2>/dev/null | grep_q '^df-' || break; sleep 1; done
ls /var/lib/nspawn/builds/ 2>/dev/null | grep_q '^df-' && fail "a cancelled build left its directory: $(ls /var/lib/nspawn/builds/)"
grep -q '/var/lib/nspawn/builds/df-' /proc/self/mountinfo && fail "a cancelled build left a mount"
ip netns list | grep_q -F 'nspawn-.build-' && fail "a cancelled build left its network namespace"
$NSPAWN images ls | grep_q '^ *e2e-build-cancel ' && fail "a cancelled build made an image"
rm -rf $work
# compose: a service without an image is built by up as PROJECT-SERVICE, from the
# cache by compose build; pull_policy build builds at every up; --build rebuilds;
# --push sends a named one; down --rmi local takes the images the project built.
cb=e2e-cb-$nonce
work=/tmp/e2e-compose-build && rm -rf $work && mkdir -p $work/app
printf 'ARG BUSYBOX\nFROM ${BUSYBOX}\nARG GREETING\nCOPY msg /msg\nRUN echo "$GREETING" > /greeting\nCMD ["sh", "-c", "cat /msg /greeting; exec sleep 600"]\n' > $work/app/Dockerfile
echo from-context > $work/app/msg
cat > $work/compose.yaml <<COMPOSE
name: $cb
services:
  web:
    build:
      context: ./app
      args:
        BUSYBOX: $busybox137_ref
        GREETING: hello-build
      labels:
        e2e.build: "yes"
  tagged:
    image: $NSPAWN_REGISTRY/e2e/cb-$nonce:1
    pull_policy: build
    build:
      context: ./app
      dockerfile_inline: |
        ARG BUSYBOX
        FROM \$\${BUSYBOX}
        CMD ["sh", "-c", "echo inline; exec sleep 600"]
      args:
        BUSYBOX: $busybox137_ref
COMPOSE
out=$($NSPAWN compose -f $work/compose.yaml up --no-build -d 2>&1) && fail "compose up --no-build with an image to build: $out"
echo "$out" | grep_q "compose build makes it" || fail "compose up --no-build was not explained: $out"
$NSPAWN compose -f $work/compose.yaml up -d > /tmp/e2e-cb.txt 2>&1 || fail "compose up of services that build: $(cat /tmp/e2e-cb.txt)"
$NSPAWN images ls | grep_q "^ *$cb-web " || fail "compose up did not build $cb-web: $($NSPAWN images ls)"
for i in $(seq 1 20); do $NSPAWN logs $cb-web-1 2>/dev/null | grep_q hello-build && break; sleep 1; done
$NSPAWN logs $cb-web-1 2>&1 | grep_q from-context || fail "the built service does not run its image: $($NSPAWN logs $cb-web-1 2>&1)"
$NSPAWN logs $cb-web-1 2>&1 | grep_q hello-build || fail "the build args of the service were not given"
[ "$($NSPAWN inspect -f '{{index .labels "e2e.build"}}' $cb-web)" = yes ] || fail "the build labels are not on the image"
[ "$($NSPAWN inspect -f '{{index .labels "com.docker.compose.project"}}' $cb-web)" = $cb ] || fail "the image has no project label"
# An image the project built under a name of its own is nspawn's, under the configured
# registry: nspawn run takes it by that name, and compose shows the name the file gives.
[ "$($NSPAWN inspect -f '{{.reference}}' $cb-web)" = "$NSPAWN_REGISTRY/$cb-web:latest" ] || fail "the built image is not under the registry: $($NSPAWN inspect -f '{{.reference}}' $cb-web)"
[ "$($NSPAWN run --rm --pull never $cb-web -- cat /greeting 2>/dev/null | tr -d '\r')" = hello-build ] || fail "nspawn run does not take the image compose built by its name"
[ "$($NSPAWN compose -f $work/compose.yaml ps --format '{{.Image}}' web)" = $cb-web ] || fail "compose ps does not show the built image as the project names it: $($NSPAWN compose -f $work/compose.yaml ps --format '{{.Image}}' web)"
$NSPAWN compose -f $work/compose.yaml images web | grep_q "^$cb-web-1 *$cb-web *latest " || fail "compose images does not show the built image as the project names it: $($NSPAWN compose -f $work/compose.yaml images web)"
for i in $(seq 1 20); do $NSPAWN logs $cb-tagged-1 2>/dev/null | grep_q inline && break; sleep 1; done
$NSPAWN logs $cb-tagged-1 2>&1 | grep_q inline || fail "dockerfile_inline was not built"
created=$($NSPAWN inspect -f '{{.created}}' $cb-web-1)
out=$($NSPAWN compose -f $work/compose.yaml build 2>&1) || fail "compose build: $out"
echo "$out" | grep_q CACHED || fail "compose build took nothing from the cache: $out"
out=$($NSPAWN compose -f $work/compose.yaml up -d 2>&1) || fail "compose up again: $out"
echo "$out" | grep_q "Image $NSPAWN_REGISTRY/e2e/cb-$nonce:1 Built" || fail "pull_policy build did not build at up: $out"
[ "$($NSPAWN inspect -f '{{.created}}' $cb-web-1)" = "$created" ] || fail "an image built the same recreated its machine"
echo changed > $work/app/msg
$NSPAWN compose -f $work/compose.yaml up -d --build >/dev/null 2>&1 || fail "compose up --build"
for i in $(seq 1 20); do $NSPAWN logs $cb-web-1 2>/dev/null | grep_q changed && break; sleep 1; done
$NSPAWN logs $cb-web-1 2>&1 | grep_q changed || fail "compose up --build did not rebuild and recreate"
$NSPAWN compose -f $work/compose.yaml build --push tagged >/dev/null 2>&1 || fail "compose build --push"
$NSPAWN pull "$NSPAWN_REGISTRY/e2e/cb-$nonce:1" --name e2e-cb-pulled >/dev/null || fail "the pushed image is not in the registry"
$NSPAWN images rm e2e-cb-pulled >/dev/null
$NSPAWN compose -f $work/compose.yaml ps -a --format '{{.Name}}' | grep_q "^$cb-web$" && fail "compose ps lists the built image as a machine"
$NSPAWN compose -f $work/compose.yaml down --rmi local >/dev/null 2>&1 || fail "compose down --rmi local"
$NSPAWN images ls | grep_q "^ *$cb-web \|^ *e2e-cb-$nonce-1 " && fail "down --rmi local left a built image: $($NSPAWN images ls)"
rm -rf $work
# The cache: listed, and pruned.
$NSPAWN builder du | grep_q '^Total:' || fail "builder du"
if [ "$cache_before" = 0 ]; then
  out=$($NSPAWN builder prune -a -f) || fail "builder prune: $out"
  echo "$out" | grep_q '^Total reclaimed space:' || fail "builder prune said nothing: $out"
  [ -z "$($NSPAWN builder du --json | python3 -c 'import json,sys; print(len(json.load(sys.stdin)) or "")')" ] || fail "builder prune -a left cache"
else
  echo "the host's build cache had entries before: builder prune -a not run"
fi
for i in $($NSPAWN images ls | awk '$1 ~ /^e2e-case-|^e2e-build-/ {print $1}'); do $NSPAWN images rm $i >/dev/null 2>&1; done

# What the review of the code found, checked where it shows: machines, networks, builds,
# compose and the service.
rv=$busybox137_ref
rm -rf /tmp/e2e-rv && mkdir -p /tmp/e2e-rv

step "review: names that do not exist are refused before anything is made"
out=$($NSPAWN create $rv e2e-rv-nonet --network e2e-rv-fronted 2>&1) && fail "create with a network that does not exist: $out"
echo "$out" | grep_q "e2e-rv-fronted" || fail "create did not name the missing network: $out"
$NSPAWN images ls | grep_q "^ *e2e-rv-nonet " && fail "a refused create left a machine"
out=$($NSPAWN create $rv e2e-rv-side --network container:e2e-rv-ghost 2>&1) && fail "create in the namespace of no machine: $out"
echo "$out" | grep_q "not a machine of nspawn's" || fail "create container: of no machine said: $out"
out=$($NSPAWN logs e2e-rv-nope 2>&1) && fail "logs of a name nothing has: $out"
echo "$out" | grep_q "no machine named e2e-rv-nope" || fail "logs of a name nothing has said: $out"

step "review: inspect prints what it finds, ps formats a row's Label through a variable"
$NSPAWN run -d --name e2e-rv-web -l rvkey=rvval $rv -- sleep 600 >/dev/null || fail "run e2e-rv-web"
$NSPAWN inspect e2e-rv-web e2e-rv-nope > /tmp/e2e-rv/inspect.txt 2>/tmp/e2e-rv/inspect.err && fail "inspect of a name nothing has succeeded"
python3 -c 'import json; d = json.load(open("/tmp/e2e-rv/inspect.txt")); assert [m["name"] for m in d] == ["e2e-rv-web"], d' || fail "inspect did not print the machine it found: $(cat /tmp/e2e-rv/inspect.txt)"
grep_q e2e-rv-nope /tmp/e2e-rv/inspect.err || fail "inspect did not say which name it did not find"
$NSPAWN inspect -f '' e2e-rv-web | python3 -c 'import json,sys; json.load(sys.stdin)' || fail "inspect -f '' does not print the JSON"
out=$($NSPAWN inspect -f '{{.x' e2e-rv-web 2>&1) && fail "a template that does not parse was taken"
echo "$out" | grep_q "template parsing error:" || fail "inspect's template error lacks docker's words: $out"
[ "$($NSPAWN ps --filter name=e2e-rv-web --format '{{$c := .}}{{$c.Label "rvkey"}}')" = rvval ] || fail "ps --format took no Label through a variable"

step "review: cp writes where a destination link points and keeps the link; a device is refused"
$NSPAWN exec e2e-rv-web -- sh -c 'echo old > /var/target; ln -s /var/target /etc/rvlink' </dev/null || fail "make a link in e2e-rv-web"
echo new > /tmp/e2e-rv/cpf
$NSPAWN cp /tmp/e2e-rv/cpf e2e-rv-web:/etc/rvlink || fail "cp onto a link"
[ "$($NSPAWN exec e2e-rv-web -- cat /var/target </dev/null)" = new ] || fail "cp did not write where the link points"
$NSPAWN exec e2e-rv-web -- test -L /etc/rvlink </dev/null || fail "cp replaced the link in the machine"
echo old > /tmp/e2e-rv/hosttarget && ln -sf /tmp/e2e-rv/hosttarget /tmp/e2e-rv/hostlink
$NSPAWN cp e2e-rv-web:/var/target /tmp/e2e-rv/hostlink || fail "cp onto a host link"
{ [ "$(cat /tmp/e2e-rv/hosttarget)" = new ] && [ -L /tmp/e2e-rv/hostlink ]; } || fail "cp on the host did not write where the link points"
out=$($NSPAWN cp /tmp/e2e-rv/cpf e2e-rv-web:/dev/null 2>&1) && fail "cp onto a device succeeded"
echo "$out" | grep_q "device" || fail "cp onto a device said: $out"

step "review: a machine made anew under the name of one that ended is created, not exited"
$NSPAWN run -d --name e2e-rv-ex $rv -- sh -c 'exit 3' >/dev/null || fail "run e2e-rv-ex"
retry 15 bash -c "$NSPAWN ps -a --format '{{.Names}} {{.Status}}' | grep_q '^e2e-rv-ex Exited (3)'" || fail "e2e-rv-ex did not end with 3: $($NSPAWN ps -a)"
$NSPAWN rm e2e-rv-ex >/dev/null || fail "rm e2e-rv-ex"
$NSPAWN create $rv e2e-rv-ex >/dev/null || fail "create e2e-rv-ex anew"
[ "$($NSPAWN ps -a --format '{{.Names}} {{.State}}' | awk '$1 == "e2e-rv-ex" {print $2}')" = created ] || fail "a machine made anew shows the end of the one before: $($NSPAWN ps -a)"

step "review: a start refused in its preparation leaves no secret decrypted and no namespace"
printf 'rv-secret' | $NSPAWN secret create e2e-rv-pw >/dev/null || fail "secret create e2e-rv-pw"
$NSPAWN create $rv e2e-rv-sec --secret e2e-rv-pw -u "root:e2e-nogroup-$nonce" -- sleep 600 >/dev/null || fail "create e2e-rv-sec"
$NSPAWN start e2e-rv-sec >/dev/null 2>&1 && fail "a start with a group the image lacks succeeded"
[ -e /run/nspawn/secrets/e2e-rv-sec ] && fail "a refused start left its secrets decrypted"
ip netns list | grep_q "nspawn-e2e-rv-sec" && fail "a refused start left its network namespace"

step "review: run of what docker takes: variables with dots and digits, a tmpfs a user writes, a probe as the user"
out=$($NSPAWN run --rm -e discovery.type=single-node -e 1A=x $rv -- env 2>&1)
echo "$out" | grep_q '^discovery.type=single-node$' || fail "a variable with a dot did not reach the program: $out"
echo "$out" | grep_q '^1A=x$' || fail "a variable starting with a digit did not reach the program: $out"
$NSPAWN run --rm --tmpfs /scratch -u 1000 $rv -- touch /scratch/x || fail "a user cannot write to a bare --tmpfs"
$NSPAWN run -d --name e2e-rv-hc -u 1000 --health-cmd 'id -u > /tmp/hc' --health-interval 1s $rv -- sleep 600 >/dev/null || fail "run e2e-rv-hc"
retry 15 bash -c "[ \"\$($NSPAWN exec e2e-rv-hc -- cat /tmp/hc </dev/null 2>/dev/null)\" = 1000 ]" || fail "the probe did not run as the machine's user"
$NSPAWN rm -f e2e-rv-hc >/dev/null 2>&1

step "review: a refused start keeps the restart policy; exec into a paused machine fails at once"
$NSPAWN create $rv e2e-rv-pol --restart always -- sleep 600 >/dev/null || fail "create e2e-rv-pol"
$NSPAWN start e2e-rv-pol --restart no -v /nonexistent-$nonce:/x >/dev/null 2>&1 && fail "a start with a volume that does not exist succeeded"
[ "$($NSPAWN inspect -f '{{.restart}}' e2e-rv-pol)" = always ] || fail "a refused start changed the restart policy: $($NSPAWN inspect -f '{{.restart}}' e2e-rv-pol)"
$NSPAWN pause e2e-rv-web >/dev/null || fail "pause e2e-rv-web"
out=$(timeout 15 $NSPAWN exec e2e-rv-web -- true </dev/null 2>&1) && fail "exec into a paused machine succeeded"
echo "$out" | grep_q -i "pause" || fail "exec into a paused machine said: $out"
$NSPAWN unpause e2e-rv-web >/dev/null || fail "unpause e2e-rv-web"

step "review: run -i hands the program a pipe, not the terminal it was typed on"
out=$(pyrun $NSPAWN run -i --rm $rv -- readlink /proc/self/fd/0 </dev/null 2>&1 | tr -d '\r')
echo "$out" | grep_q "pipe:" || fail "run -i gave the program something else than a pipe: $out"

step "review: a call that allows no interactive authorization is refused at once"
if command -v busctl >/dev/null 2>&1 && id nobody >/dev/null 2>&1; then
  out=$(timeout 20 runuser -u nobody -- busctl --system --allow-interactive-authorization=no call org.nspawn /org/nspawn org.nspawn.Manager ListImages 2>&1) && fail "an unprivileged call was allowed: $out"
  # Without polkit on the bus only root is authorized, whatever the call allows.
  why="did not allow interactive authorization"
  { busctl --system call org.freedesktop.DBus /org/freedesktop/DBus org.freedesktop.DBus ListNames
    busctl --system call org.freedesktop.DBus /org/freedesktop/DBus org.freedesktop.DBus ListActivatableNames; } 2>/dev/null \
    | grep_q '"org.freedesktop.PolicyKit1"' || why="polkit is not on this bus"
  echo "$out" | grep_q "$why" || fail "the refusal does not say why: $out"
fi

step "review: the release hook cleans up with a configuration broken since the start"
$NSPAWN run -d --rm --name e2e-rv-rel -p 18197:80 --secret e2e-rv-pw $rv -- httpd -f -p 80 >/dev/null || fail "run e2e-rv-rel"
retry 10 bash -c "[ -d /run/nspawn/secrets/e2e-rv-rel ]" || fail "e2e-rv-rel has no secrets decrypted"
cp /etc/nspawn/e2e.toml /tmp/e2e-rv/e2e.toml
echo "e2e_bogus_key = 1" >> /etc/nspawn/e2e.toml
since=$(date +%s)
systemctl stop systemd-nspawn@e2e-rv-rel.service
# The removal of the --rm machine goes by the same broken file, in a unit of its own.
retry 30 bash -c "[ ! -e /var/lib/nspawn/images/e2e-rv-rel.json ]" || fail "the --rm machine was not removed with the configuration broken"
cp /tmp/e2e-rv/e2e.toml /etc/nspawn/e2e.toml
journalctl -u systemd-nspawn@e2e-rv-rel.service --since "@$since" | grep_q "going by the settings of it that are still valid" || fail "the release hook did not go by what of the configuration is still valid"
[ -e /run/nspawn/secrets/e2e-rv-rel ] && fail "the release hook left the secrets decrypted"
ip netns list | grep_q "nspawn-e2e-rv-rel" && fail "the release hook left the network namespace"
nft list map ip nspawn ports 2>/dev/null | grep_q " 18197 " && fail "the release hook left the published port"

step "review: builds: a hard link of a symlink, a label without a value, bad tags and heredoc paths before any step"
mkdir -p /tmp/e2e-rv/build
printf 'FROM %s\nRUN ln -s /bin/busybox /var/s && ln /var/s /var/h\n' "$rv" > /tmp/e2e-rv/build/Dockerfile
$NSPAWN build -t e2e-rv-hl:1 --label rvflag /tmp/e2e-rv/build > /tmp/e2e-rv/build.txt 2>&1 || fail "build of a hard link to a symlink: $(tail -20 /tmp/e2e-rv/build.txt)"
[ "$($NSPAWN run --rm e2e-rv-hl:1 -- sh -c 'test -L /var/h && echo link' 2>/dev/null)" = link ] || fail "the hard link of a symlink is not a symlink in the image"
$NSPAWN inspect e2e-rv-hl-1 | python3 -c 'import json,sys; assert json.load(sys.stdin)[0]["labels"]["rvflag"] == ""' || fail "a label without a value is not on the image"
out=$($NSPAWN build -t e2e-rv-ok:1 -t E2E-Bad:2 /tmp/e2e-rv/build 2>&1) && fail "a build with a bad tag succeeded"
echo "$out" | grep_q "lowercase" || fail "the bad tag was not named: $out"
$NSPAWN images ls | grep_q "^ *e2e-rv-ok-1 " && fail "a refused build made an image"
printf 'FROM %s\nCOPY <</etc/e2e-rv-%s /dest\nnot here\n/etc/e2e-rv-%s\n' "$rv" "$nonce" "$nonce" > /tmp/e2e-rv/build/Dockerfile.heredoc
$NSPAWN build -f /tmp/e2e-rv/build/Dockerfile.heredoc -t e2e-rv-hd:1 /tmp/e2e-rv/build >/dev/null 2>&1 && fail "a heredoc named by a path was taken"
[ -e "/etc/e2e-rv-$nonce" ] && fail "a heredoc wrote on the host"
# A socket in the context goes as an empty file, and what follows it still arrives.
python3 -c "import socket; s = socket.socket(socket.AF_UNIX); s.bind('/tmp/e2e-rv/build/a.sock')"
echo after > /tmp/e2e-rv/build/z
printf 'FROM %s\nCOPY . /ctx\nRUN test -f /ctx/a.sock && test -f /ctx/z\nRUN true\nRUN true\nRUN printf "a\\0b\\n"\n' "$rv" > /tmp/e2e-rv/build/Dockerfile.ctx
$NSPAWN build -f /tmp/e2e-rv/build/Dockerfile.ctx -t e2e-rv-ctx:1 /tmp/e2e-rv/build > /tmp/e2e-rv/build.txt 2>&1 || fail "a context with a socket, steps that change nothing, a NUL in the output: $(tail -20 /tmp/e2e-rv/build.txt)"
$NSPAWN run --rm e2e-rv-ctx:1 -- true || fail "an image of repeated empty layers does not run"
$NSPAWN ps >/dev/null || fail "the service is gone after the build"

step "review: builds: two cache mounts of one id locked, host-gateway, URLs by name and checksum"
mkdir -p /tmp/e2e-rv/http && echo url-content > "/tmp/e2e-rv/http/a b"
python3 -m http.server 18199 --bind 127.0.0.1 --directory /tmp/e2e-rv/http >/dev/null 2>&1 &
http_pid=$!
sleep 1
sum=$(sha256sum "/tmp/e2e-rv/http/a b" | cut -d' ' -f1)
cat > /tmp/e2e-rv/build/Dockerfile.url <<DOCKERFILE
FROM $rv
RUN --mount=type=cache,id=e2e-rv-c,target=/a,sharing=locked --mount=type=cache,id=e2e-rv-c,target=/b,sharing=locked true
RUN grep -q gw /etc/hosts
ADD --checksum=sha256:$sum http://127.0.0.1:18199/a%20b /x/
DOCKERFILE
$NSPAWN build -f /tmp/e2e-rv/build/Dockerfile.url --add-host gw:host-gateway -t e2e-rv-url:1 /tmp/e2e-rv/build > /tmp/e2e-rv/build.txt 2>&1 || fail "cache mounts, host-gateway and ADD of a URL: $(tail -20 /tmp/e2e-rv/build.txt)"
[ "$($NSPAWN run --rm e2e-rv-url:1 -- cat '/x/a b' 2>/dev/null)" = url-content ] || fail "ADD of a URL did not name the file by the URL's path"
printf 'FROM %s\nADD http://127.0.0.1:18199/a%%20b /one\nADD --checksum=sha256:%064d http://127.0.0.1:18199/a%%20b /two\n' "$rv" 0 > /tmp/e2e-rv/build/Dockerfile.sum
out=$($NSPAWN build -f /tmp/e2e-rv/build/Dockerfile.sum -t e2e-rv-sum:1 /tmp/e2e-rv/build 2>&1) && fail "a wrong checksum was taken"
echo "$out" | grep_q -i "digest\|checksum" || fail "the wrong checksum was not said: $(echo "$out" | tail -5)"
kill $http_pid 2>/dev/null

step "review: compose: a restart takes what depends on it with restart: true, runs post_start, and run leaves nothing"
mkdir -p /tmp/e2e-rv/rvr
cat > /tmp/e2e-rv/rvr/compose.yaml <<COMPOSE
name: e2e-rvr
services:
  db:
    image: $rv
    command: ["sleep", "600"]
    volumes: ["data:/data"]
  web:
    image: $rv
    command: ["sleep", "600"]
    depends_on:
      db: {condition: service_started, restart: true}
    post_start:
      - command: ["sh", "-c", "echo started >> /var/started"]
  job:
    image: $rv
    command: ["true"]
    cap_add: [SYS_PTRACE]
    volumes: ["data:/data"]
volumes:
  data: {}
COMPOSE
$NSPAWN compose -f /tmp/e2e-rv/rvr/compose.yaml up -d > /tmp/e2e-rv/rvr.txt 2>&1 || fail "compose up of e2e-rvr: $(cat /tmp/e2e-rv/rvr.txt)"
started() { $NSPAWN exec e2e-rvr-web-1 -- cat /var/started </dev/null 2>/dev/null | wc -l; }
retry 10 bash -c "[ \"\$($NSPAWN exec e2e-rvr-web-1 -- cat /var/started </dev/null 2>/dev/null | wc -l)\" = 1 ]" || fail "post_start did not run at up: $(started)"
$NSPAWN compose -f /tmp/e2e-rv/rvr/compose.yaml restart db >/dev/null 2>&1 || fail "compose restart db"
[ "$(started)" = 2 ] || fail "restart of db did not restart web with its post_start: $(started)"
$NSPAWN compose -f /tmp/e2e-rv/rvr/compose.yaml restart --no-deps db >/dev/null 2>&1 || fail "compose restart --no-deps db"
[ "$(started)" = 2 ] || fail "restart --no-deps of db restarted web: $(started)"
# ls: running ones without -a, exited too with it; the name filter a regular expression.
$NSPAWN compose ls --filter 'name=^e2e-rvr$' > /tmp/e2e-rv/ls.txt || fail "compose ls --filter"
grep_q '^e2e-rvr  *running(2) ' /tmp/e2e-rv/ls.txt || fail "compose ls counts what does not run: $(cat /tmp/e2e-rv/ls.txt)"
$NSPAWN compose ls -a --filter 'name=^e2e-rvr$' | grep_q 'exited(1)' || fail "compose ls -a leaves the job out"
# run: -v of a volume of the project (one the service uses: the others are left out
# of a run's project), dotenv files, caps added to the service's.
$NSPAWN compose -f /tmp/e2e-rv/rvr/compose.yaml run --rm -v data:/backup job sh -c 'echo rv > /backup/f' >/dev/null 2>&1 || fail "compose run -v data:/backup"
[ "$($NSPAWN exec e2e-rvr-db-1 -- cat /data/f </dev/null 2>/dev/null)" = rv ] || fail "run -v data:/backup did not write into the project's volume"
printf 'export A="x y"\n# a comment\nB=2\n' > /tmp/e2e-rv/rvr/env.txt
out=$($NSPAWN compose -f /tmp/e2e-rv/rvr/compose.yaml run --rm --env-from-file /tmp/e2e-rv/rvr/env.txt job sh -c 'echo "$A|$B"' 2>/dev/null)
[ "$out" = "x y|2" ] || fail "run --env-from-file was not read as a dotenv file: $out"
caps=$($NSPAWN compose -f /tmp/e2e-rv/rvr/compose.yaml run --rm --cap-add NET_ADMIN job grep CapEff /proc/self/status 2>/dev/null | awk '{print $2}')
python3 -c "c = int('$caps', 16); assert c >> 12 & 1 and c >> 19 & 1, hex(c)" || fail "run --cap-add did not keep the service's cap_add: $caps"
$NSPAWN compose -f /tmp/e2e-rv/rvr/compose.yaml run --rm job true >/dev/null 2>&1 || fail "compose run --rm job"
$NSPAWN compose -f /tmp/e2e-rv/rvr/compose.yaml down -v >/dev/null 2>&1 || fail "compose down right after run --rm"
$NSPAWN ps -a | grep_q 'e2e-rvr-job-run' && fail "compose run --rm left its one-off"
$NSPAWN network ls | grep_q e2e-rvr_default && fail "compose down right after run --rm left the network"

step "review: compose: one image written twice is pulled once; network labels; links; a second IPv4 subnet"
mkdir -p /tmp/e2e-rv/rvn
# A repository of this run's own: compose pull keeps the image it pulls, as docker does.
rvn_repo=e2e-rvn-$nonce
$NSPAWN pull "$rv" --name e2e-rvn-src --force >/dev/null 2>&1 && $NSPAWN push e2e-rvn-src --to "$NSPAWN_REGISTRY/$rvn_repo:latest" >/dev/null 2>&1 || fail "push $rvn_repo"
$NSPAWN images rm e2e-rvn-src >/dev/null 2>&1
cat > /tmp/e2e-rv/rvn/compose.yaml <<COMPOSE
name: e2e-rvn
services:
  a:
    image: $NSPAWN_REGISTRY/$rvn_repo
    command: ["sleep", "600"]
    pull_policy: always
  b:
    image: $NSPAWN_REGISTRY/$rvn_repo:latest
    command: ["sleep", "600"]
    pull_policy: always
    links: ["a:alpha"]
networks:
  default:
    labels:
      rvnet: "yes"
COMPOSE
out=$($NSPAWN compose -f /tmp/e2e-rv/rvn/compose.yaml pull 2>&1) || fail "compose pull of one image written twice: $out"
[ "$(echo "$out" | grep -c 'Pulled')" = 1 ] || fail "one image written twice was pulled more than once: $out"
$NSPAWN compose -f /tmp/e2e-rv/rvn/compose.yaml up -d >/dev/null 2>&1 || fail "compose up of e2e-rvn"
$NSPAWN network inspect e2e-rvn_default | python3 -c 'import json,sys; d = json.load(sys.stdin)[0]; assert d["labels"]["rvnet"] == "yes", d' || fail "the network has not the file's labels"
$NSPAWN exec e2e-rvn-b-1 -- grep -q alpha /etc/hosts </dev/null || fail "the alias of links does not name the linked service"
$NSPAWN compose -f /tmp/e2e-rv/rvn/compose.yaml images -q | grep_q -E '^[0-9a-f]{64}$' || fail "compose images -q does not print whole IDs"
$NSPAWN compose -f /tmp/e2e-rv/rvn/compose.yaml down >/dev/null 2>&1 || fail "compose down of e2e-rvn"
$NSPAWN images rm "$rvn_repo" >/dev/null 2>&1 || fail "images rm $rvn_repo"
cat > /tmp/e2e-rv/rvn/two.yaml <<COMPOSE
name: e2e-rvn2
services:
  a: {image: $rv, command: ["sleep", "600"]}
networks:
  default:
    ipam:
      config: [{subnet: 10.231.1.0/24}, {subnet: 10.231.2.0/24}]
COMPOSE
out=$($NSPAWN compose -f /tmp/e2e-rv/rvn/two.yaml up -d 2>&1) && fail "a network of two IPv4 subnets was made: $out"
echo "$out" | grep_q "second IPv4 subnet" || fail "the second subnet was not said: $out"
$NSPAWN network ls | grep_q e2e-rvn2_default && fail "a refused project made its network"

step "review: a tree deeper than the copy goes is refused, and rm and volume rm still take it"
$NSPAWN volume create e2e-rv3-deep >/dev/null || fail "volume create e2e-rv3-deep"
$NSPAWN run -d --name e2e-rv3-deep -v e2e-rv3-deep:/v $rv -- sleep 600 >/dev/null || fail "run e2e-rv3-deep"
deep='for top in /deep /v/deep; do mkdir -p $top && cd $top || exit 1; mkdir x; i=0; while [ $i -lt 3000 ]; do mkdir t && mv x t/d && mv t x || break; i=$((i + 1)); done; echo $i; done'
depths=$(timeout 300 $NSPAWN exec e2e-rv3-deep -- sh -c "$deep" </dev/null 2>/dev/null | tr -d '\r' | tr '\n' ' ')
echo "depths made: $depths"
if [ "${depths%% *}" -gt 2048 ] 2>/dev/null; then
  # 2048 levels make a path longer than the host takes: the host's end gives up first,
  # with the error of the name, as docker's would.
  out=$(timeout 300 $NSPAWN cp e2e-rv3-deep:/deep /tmp/e2e-rv/deep-out 2>&1) && fail "a copy of a tree deeper than 2048 succeeded"
  echo "$out" | grep -qE "more than 2048 directories deep|File name too long" || fail "the deep copy said: $(echo "$out" | tail -3 | cut -c1-300)"
else
  fail "the machine could not make a deep tree: $depths"
fi
timeout 60 $NSPAWN ps >/dev/null || fail "the service does not answer after the deep copy"
timeout 300 $NSPAWN rm -f e2e-rv3-deep >/dev/null || fail "rm -f of a machine with a deep tree"
[ -e /var/lib/nspawn/machines/e2e-rv3-deep ] && fail "rm left the deep machine's directory"
timeout 300 $NSPAWN volume rm e2e-rv3-deep >/dev/null || fail "volume rm of a deep volume"
[ -e /var/lib/nspawn/volumes/e2e-rv3-deep ] && fail "volume rm left the deep volume"
timeout 60 $NSPAWN ps >/dev/null || fail "the service does not answer after removing deep trees"
rm -rf /tmp/e2e-rv/deep-out

step "review: an attached run gives its own run's exit code, and rm clears a failed unit"
$NSPAWN run --name e2e-rv3-t $rv -- sh -c 'exit 3' >/dev/null 2>&1; rc=$?
[ "$rc" = 3 ] || fail "run of a program that exits 3 gave $rc"
$NSPAWN rm e2e-rv3-t >/dev/null || fail "rm e2e-rv3-t"
systemctl is-failed --quiet systemd-nspawn@e2e-rv3-t.service && fail "rm left the unit of the failed machine failed"
out=$($NSPAWN run --name e2e-rv3-t $rv -- sh -c 'sleep 3; echo done' 2>/dev/null); rc=$?
{ [ "$rc" = 0 ] && [ "$out" = done ]; } || fail "a run under the name of one that exited 3 gave $rc: $out"
$NSPAWN rm -f e2e-rv3-t >/dev/null 2>&1

step "review: a detached exec and a dropped output outlive a restart of the service"
$NSPAWN run -d --name e2e-rv3-ex $rv -- sleep 600 >/dev/null || fail "run e2e-rv3-ex"
$NSPAWN exec -d e2e-rv3-ex -- sleep 601 </dev/null || fail "exec -d into e2e-rv3-ex"
$NSPAWN run --name e2e-rv3-dr --log-driver none $rv -- sh -c 'while :; do echo x; sleep 1; done' >/dev/null 2>&1 &
client=$!
retry 15 bash -c "$NSPAWN ps --format '{{.Names}}' | grep_q '^e2e-rv3-dr\$'" || fail "e2e-rv3-dr did not start"
kill -9 $client 2>/dev/null
sleep 2
systemctl restart nspawn.service || fail "restart the service"
timeout 30 $NSPAWN ps >/dev/null || fail "the service does not answer after its restart with a detached exec running"
retry 10 bash -c "$NSPAWN top e2e-rv3-ex | grep_q 'sleep 601'" || fail "the detached exec did not outlive the service's restart"
sleep 5
[ "$($NSPAWN ps --filter name=e2e-rv3-dr -q)" = e2e-rv3-dr ] || fail "the machine whose client went away ended with the service's restart"
systemctl is-active --quiet nspawn-drain-e2e-rv3-dr.service || fail "no unit reads the output of e2e-rv3-dr once the service restarted"
$NSPAWN stop e2e-rv3-dr >/dev/null || fail "stop e2e-rv3-dr"
retry 10 bash -c "! systemctl is-active --quiet nspawn-drain-e2e-rv3-dr.service" || fail "the drain unit outlived its machine"
$NSPAWN rm -f e2e-rv3-ex e2e-rv3-dr >/dev/null 2>&1

step "review: a healthcheck runner ends with its machine; none for a machine without one"
$NSPAWN run -d --name e2e-rv3-hc --health-cmd true --health-interval 1s $rv -- sh -c 'sleep 4' >/dev/null || fail "run e2e-rv3-hc"
retry 15 bash -c "$NSPAWN ps -a --format '{{.Names}} {{.State}}' | grep_q '^e2e-rv3-hc exited'" || fail "e2e-rv3-hc did not end"
retry 5 bash -c "! systemctl is-active --quiet nspawn-health-e2e-rv3-hc.service" || fail "the health runner outlived its machine"
$NSPAWN rm e2e-rv3-hc >/dev/null || fail "rm e2e-rv3-hc"
$NSPAWN run -d --name e2e-rv3-hc $rv -- sleep 30 >/dev/null || fail "run e2e-rv3-hc anew"
sleep 2
systemctl is-active --quiet nspawn-health-e2e-rv3-hc.service && fail "a machine without a healthcheck has a runner"
[ -z "$($NSPAWN inspect -f '{{.health}}' e2e-rv3-hc 2>/dev/null | grep -v '<no value>')" ] || fail "a machine without a healthcheck shows a health: $($NSPAWN inspect -f '{{.health}}' e2e-rv3-hc)"
$NSPAWN rm -f e2e-rv3-hc >/dev/null 2>&1
mkdir -p /tmp/e2e-rv/hc
printf 'services:\n  web:\n    image: %s\n    command: ["sleep", "600"]\n    healthcheck:\n      test: ["CMD", "true"]\n      timeout: 500us\n' "$rv" > /tmp/e2e-rv/hc/compose.yaml
out=$($NSPAWN compose -p e2e-rv3h -f /tmp/e2e-rv/hc/compose.yaml up -d 2>&1) && fail "a healthcheck timeout under 1ms was taken"
echo "$out" | grep_q "Timeout in Healthcheck cannot be less than 1ms" || fail "the short healthcheck timeout said: $out"
$NSPAWN compose -p e2e-rv3h -f /tmp/e2e-rv/hc/compose.yaml down >/dev/null 2>&1

step "review: exec into a machine that froze its own cgroup fails at once"
$NSPAWN run -d --name e2e-rv3-frz $rv -- sleep 600 >/dev/null || fail "run e2e-rv3-frz"
if $NSPAWN exec e2e-rv3-frz -- sh -c 'mkdir /sys/fs/cgroup/e2e && echo 1 > /sys/fs/cgroup/e2e/cgroup.procs && echo 1 > /sys/fs/cgroup/e2e/cgroup.freeze' </dev/null >/dev/null 2>&1; then
  start=$(date +%s)
  out=$(timeout 30 $NSPAWN exec e2e-rv3-frz -- true </dev/null 2>&1) && fail "exec into a frozen cgroup succeeded"
  [ $(( $(date +%s) - start )) -lt 15 ] || fail "exec into a frozen cgroup took $(( $(date +%s) - start )) seconds"
  echo "$out" | grep_q "froze the cgroup" || fail "exec into a frozen cgroup said: $out"
  leader=$($NSPAWN inspect -f '{{.leader}}' e2e-rv3-frz)
  cg=$(sed -n 's/^0:://p' "/proc/$leader/cgroup")
  echo 0 > "/sys/fs/cgroup$cg/cgroup.freeze" || fail "thaw /sys/fs/cgroup$cg"
  timeout 30 $NSPAWN exec e2e-rv3-frz -- true </dev/null || fail "exec after the thaw"
else
  echo "the machine cannot write its cgroup here: the frozen exec check is skipped"
fi
$NSPAWN rm -f e2e-rv3-frz >/dev/null 2>&1

step "review: builds: private cache mounts are each build's own, and a cache of another owner is another cache"
mkdir -p /tmp/e2e-rv/cache
for n in 1 2; do
  printf 'FROM %s\nRUN echo build-%s\nRUN --mount=type=cache,id=e2e-rv3-p-%s,target=/c,sharing=private sh -c '"'"'echo $$ > /c/owner; sleep 3; echo "mine=$$ seen=$(cat /c/owner)"'"'"'\n' "$rv" "$n" "$nonce" > /tmp/e2e-rv/cache/Dockerfile.$n
  $NSPAWN build --no-cache --progress plain -f /tmp/e2e-rv/cache/Dockerfile.$n -t e2e-rv3-p$n:1 /tmp/e2e-rv/cache > /tmp/e2e-rv/cache/out.$n 2>&1 &
done
wait
for n in 1 2; do
  line=$(grep -o 'mine=[0-9]* seen=[0-9]*' /tmp/e2e-rv/cache/out.$n | tail -1)
  [ -n "$line" ] && [ "${line#mine=}" != "$line" ] && [ "$(echo "$line" | sed 's/mine=\([0-9]*\) seen=\([0-9]*\)/\1/')" = "$(echo "$line" | sed 's/mine=\([0-9]*\) seen=\([0-9]*\)/\2/')" ] \
    || fail "build $n did not see its own private cache: $line $(tail -5 /tmp/e2e-rv/cache/out.$n)"
done
printf 'FROM %s\nRUN --mount=type=cache,id=e2e-rv3-own-%s,target=/c2 touch /c2/root-made\nUSER 1000\nRUN --mount=type=cache,id=e2e-rv3-own-%s,target=/c2,uid=1000 touch /c2/x\nUSER 0\nRUN --mount=type=cache,id=e2e-rv3-two-%s,target=/a,sharing=private --mount=type=cache,id=e2e-rv3-two-%s,target=/b,sharing=locked sh -c "touch /a/f && test -f /b/f"\n' "$rv" "$nonce" "$nonce" "$nonce" "$nonce" > /tmp/e2e-rv/cache/Dockerfile.own
timeout 300 $NSPAWN build -f /tmp/e2e-rv/cache/Dockerfile.own -t e2e-rv3-own:1 /tmp/e2e-rv/cache > /tmp/e2e-rv/cache/out.own 2>&1 || fail "a cache mount of uid 1000 after one of root, one cache in two mounts of a step: $(tail -10 /tmp/e2e-rv/cache/out.own)"
$NSPAWN images rm e2e-rv3-p1-1 e2e-rv3-p2-1 e2e-rv3-own-1 >/dev/null 2>&1

step "review: compose: jobs run after what they depend on, with their env_file; scheduled ones are refused"
mkdir -p /tmp/e2e-rv/jobs
cat > /tmp/e2e-rv/jobs/compose.yaml <<COMPOSE
services:
  web:
    image: $rv
    command: ["sleep", "600"]
    env_file: missing.env
jobs:
  seed:
    image: $rv
    command: ["sh", "-c", "date +%s > /data/seeded"]
    volumes: ["data:/data"]
    triggers: {manual: true}
  migrate:
    image: $rv
    env_file: migrate.env
    volumes: ["data:/data"]
    depends_on: [seed]
    triggers: {manual: true}
volumes:
  data: {}
COMPOSE
echo "DATABASE_URL=postgres://e2e@db/e2e" > /tmp/e2e-rv/jobs/migrate.env
out=$(cd /tmp/e2e-rv/jobs && timeout 300 $NSPAWN compose -p e2e-rv3j run --rm migrate sh -c 'echo "url=$DATABASE_URL"; cat /data/seeded' 2>&1) || fail "compose run of a job: $out"
echo "$out" | grep_q "^url=postgres://e2e@db/e2e" || fail "the job did not get its env_file: $out"
echo "$out" | grep_q "^[0-9]\{10,\}" || fail "the job's dependency did not run first: $out"
sed -i "/^jobs:\$/a\\  nightly:\\n    image: $rv\\n    triggers: {schedule: [{cron: \"0 3 * * *\"}]}" /tmp/e2e-rv/jobs/compose.yaml
# up reads the env files of every service it makes, as docker compose does: web's has to
# be there for up to get as far as the jobs.
: > /tmp/e2e-rv/jobs/missing.env
out=$(cd /tmp/e2e-rv/jobs && $NSPAWN compose -p e2e-rv3j up -d 2>&1) && fail "up with a scheduled job succeeded"
echo "$out" | grep_q "scheduled jobs are not supported in this version: nightly" || fail "the scheduled job said: $out"
(cd /tmp/e2e-rv/jobs && $NSPAWN compose -p e2e-rv3j down -v >/dev/null 2>&1)

step "review: compose: a bind volume with o: bind,ro is read-only inside; other bind options are refused"
mkdir -p /tmp/e2e-rv/robind/src
cat > /tmp/e2e-rv/robind/compose.yaml <<COMPOSE
services:
  web:
    image: $rv
    command: ["sleep", "600"]
    volumes: ["ro:/ro"]
volumes:
  ro:
    driver_opts: {type: none, o: "bind,ro", device: /tmp/e2e-rv/robind/src}
COMPOSE
(cd /tmp/e2e-rv/robind && $NSPAWN compose -p e2e-rv3ro up -d >/dev/null 2>&1) || fail "compose up of a read-only bind volume"
$NSPAWN exec e2e-rv3ro-web-1 -- touch /ro/x </dev/null 2>/dev/null && fail "a bind volume with o: bind,ro is writable inside"
[ -e /tmp/e2e-rv/robind/src/x ] && fail "a write reached the host through a read-only bind volume"
(cd /tmp/e2e-rv/robind && $NSPAWN compose -p e2e-rv3ro down >/dev/null 2>&1)
sed -i 's/o: "bind,ro"/o: "bind,noexec"/' /tmp/e2e-rv/robind/compose.yaml
out=$(cd /tmp/e2e-rv/robind && $NSPAWN compose -p e2e-rv3ro up -d 2>&1) && fail "a bind volume with o: bind,noexec was taken"
echo "$out" | grep_q "noexec" || fail "the refused bind option was not named: $out"
(cd /tmp/e2e-rv/robind && $NSPAWN compose -p e2e-rv3ro down >/dev/null 2>&1)

step "review: compose: a secret from the environment is kept while its value is, replaced when it changes, and carries no digest"
mkdir -p /tmp/e2e-rv/sec
cat > /tmp/e2e-rv/sec/compose.yaml <<COMPOSE
services:
  web:
    image: $rv
    command: ["sleep", "600"]
    secrets: [pw]
secrets:
  pw:
    environment: E2E_RV3_PW
COMPOSE
(cd /tmp/e2e-rv/sec && E2E_RV3_PW=first $NSPAWN compose -p e2e-rv3s up -d >/dev/null 2>&1) || fail "compose up with a secret from the environment"
created1=$($NSPAWN secret inspect e2e-rv3s_secret_pw 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["created"])')
sleep 1
(cd /tmp/e2e-rv/sec && E2E_RV3_PW=first $NSPAWN compose -p e2e-rv3s up -d >/dev/null 2>&1) || fail "compose up again with the same secret"
created2=$($NSPAWN secret inspect e2e-rv3s_secret_pw 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["created"])')
[ -n "$created1" ] && [ "$created1" = "$created2" ] || fail "the same secret was written again: $created1 $created2"
(cd /tmp/e2e-rv/sec && E2E_RV3_PW=second $NSPAWN compose -p e2e-rv3s up -d >/dev/null 2>&1) || fail "compose up with a new secret value"
created3=$($NSPAWN secret inspect e2e-rv3s_secret_pw 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["created"])')
[ "$created3" != "$created2" ] || fail "a new secret value did not replace the secret"
$NSPAWN secret inspect e2e-rv3s_secret_pw | grep_q -i "sha256\|compose.content" && fail "the secret carries something derived from its content: $($NSPAWN secret inspect e2e-rv3s_secret_pw)"
# One that cannot be read back (a key gone with the host it was made on) is replaced.
printf 'not a credential' > /var/lib/nspawn/secrets/e2e-rv3s_secret_pw.cred
(cd /tmp/e2e-rv/sec && E2E_RV3_PW=second $NSPAWN compose -p e2e-rv3s up -d >/dev/null 2>&1) || fail "compose up over a secret that does not decrypt"
(cd /tmp/e2e-rv/sec && E2E_RV3_PW=second $NSPAWN compose -p e2e-rv3s down >/dev/null 2>&1)
$NSPAWN secret rm e2e-rv3s_secret_pw >/dev/null 2>&1

step "review: compose: a machine in another one's network is made again with it"
mkdir -p /tmp/e2e-rv/vpn
cat > /tmp/e2e-rv/vpn/compose.yaml <<COMPOSE
services:
  vpn:
    image: $rv
    command: ["sleep", "600"]
    environment: {GEN: "1"}
  torrent:
    image: $rv
    command: ["sleep", "600"]
    network_mode: service:vpn
COMPOSE
(cd /tmp/e2e-rv/vpn && $NSPAWN compose -p e2e-rv3v up -d >/dev/null 2>&1) || fail "compose up of vpn and torrent"
t1=$($NSPAWN inspect -f '{{.created}}' e2e-rv3v-torrent-1)
sed -i 's/GEN: "1"/GEN: "2"/' /tmp/e2e-rv/vpn/compose.yaml
sleep 1
(cd /tmp/e2e-rv/vpn && $NSPAWN compose -p e2e-rv3v up -d >/dev/null 2>&1) || fail "compose up after vpn changed"
t2=$($NSPAWN inspect -f '{{.created}}' e2e-rv3v-torrent-1)
[ "$t1" != "$t2" ] || fail "torrent was not made again with the vpn it shares a network with"
a=$($NSPAWN exec e2e-rv3v-vpn-1 -- cat /proc/net/fib_trie </dev/null 2>/dev/null | md5sum)
b=$($NSPAWN exec e2e-rv3v-torrent-1 -- cat /proc/net/fib_trie </dev/null 2>/dev/null | md5sum)
[ "$a" = "$b" ] || fail "torrent is not in vpn's new network namespace"
(cd /tmp/e2e-rv/vpn && $NSPAWN compose -p e2e-rv3v down >/dev/null 2>&1)

step "review: two machines of one host port started by systemd at once: one runs, the other is refused"
$NSPAWN create $rv e2e-rv3-pa -p 18198:80 -- httpd -f -p 80 >/dev/null || fail "create e2e-rv3-pa"
$NSPAWN create $rv e2e-rv3-pb -p 18198:80 -- httpd -f -p 80 >/dev/null 2>&1 || fail "create e2e-rv3-pb"
systemctl start systemd-nspawn@e2e-rv3-pa.service systemd-nspawn@e2e-rv3-pb.service >/dev/null 2>&1
sleep 3
active=0
for m in e2e-rv3-pa e2e-rv3-pb; do systemctl is-active --quiet systemd-nspawn@$m.service && active=$((active + 1)); done
[ "$active" = 1 ] || fail "$active of the two machines of host port 18198 run"
journalctl -u systemd-nspawn@e2e-rv3-pa.service -u systemd-nspawn@e2e-rv3-pb.service --since "-2min" | grep_q "host port 18198/tcp is already published by" || fail "the refused machine did not say the port is taken"
$NSPAWN rm -f e2e-rv3-pa e2e-rv3-pb >/dev/null 2>&1

step "review: disconnecting a booted machine of a long name takes its port off that bridge only"
$NSPAWN network create e2e-rv3-n2 >/dev/null || fail "network create e2e-rv3-n2"
$NSPAWN pull "$IMAGE" --name e2e-rv3-bootlongname --backend overlay --force >/dev/null || fail "pull e2e-rv3-bootlongname"
$NSPAWN start e2e-rv3-bootlongname >/dev/null || fail "start e2e-rv3-bootlongname"
$NSPAWN run -d --name e2e-rv3-n2peer --network e2e-rv3-n2 $rv -- sleep 600 >/dev/null || fail "run e2e-rv3-n2peer"
$NSPAWN network connect e2e-rv3-n2 e2e-rv3-bootlongname >/dev/null || fail "network connect e2e-rv3-bootlongname"
br=$($NSPAWN network inspect e2e-rv3-n2 | python3 -c "import json,sys; print(json.load(sys.stdin)[0]['interface'])")
before=$(ip -o link show master "$br" | wc -l)
[ "$before" = 2 ] || fail "the bridge of e2e-rv3-n2 has $before ports, not 2"
# Its primary network, whose port systemd-nspawn named: found by the namespace its peer
# is in.
primary=$(ip -o link show master nspawn0 | wc -l)
$NSPAWN network disconnect bridge e2e-rv3-bootlongname >/dev/null || fail "network disconnect of the booted machine's primary network"
[ "$(ip -o link show master nspawn0 | wc -l)" = $((primary - 1)) ] || fail "its port on nspawn0 stayed: $(ip -o link show master nspawn0)"
[ "$(ip -o link show master "$br" | wc -l)" = 2 ] || fail "the other bridge lost a port"
peer=$(addr_of e2e-rv3-n2peer)
$NSPAWN exec e2e-rv3-n2peer -- ip -4 -o addr </dev/null | grep_q "$peer" || fail "the peer lost its address on the bridge"
$NSPAWN rm -f e2e-rv3-bootlongname e2e-rv3-n2peer >/dev/null 2>&1
$NSPAWN network rm e2e-rv3-n2 >/dev/null 2>&1

step "review: a volume another running machine uses is not filled from an image; a free one is"
# /etc/network, which busybox fills with directories: at /etc the mount points of the
# machine's hosts and resolv.conf would land in the volume that is not filled.
$NSPAWN volume create e2e-rv3-vol >/dev/null || fail "volume create e2e-rv3-vol"
$NSPAWN run -d --name e2e-rv3-va -v e2e-rv3-vol:/srv $rv -- sleep 600 >/dev/null || fail "run e2e-rv3-va"
out=$($NSPAWN run -d --name e2e-rv3-vb -v e2e-rv3-vol:/etc/network $rv -- sleep 600 2>&1) || fail "run e2e-rv3-vb: $out"
echo "$out" | grep_q "is in use by e2e-rv3-va, which runs: it is not filled" || fail "the second machine did not say why the volume stays empty: $out"
[ -z "$(ls -A /var/lib/nspawn/volumes/e2e-rv3-vol 2>/dev/null)" ] || fail "a volume in use was filled from an image: $(ls -A /var/lib/nspawn/volumes/e2e-rv3-vol)"
$NSPAWN rm -f e2e-rv3-va e2e-rv3-vb >/dev/null 2>&1
$NSPAWN run -d --name e2e-rv3-vc -v e2e-rv3-vol:/etc/network $rv -- sleep 600 >/dev/null || fail "run e2e-rv3-vc"
$NSPAWN exec e2e-rv3-vc -- test -d /etc/network/if-up.d </dev/null || fail "a free empty volume was not filled from the image"
$NSPAWN rm -f e2e-rv3-vc >/dev/null 2>&1
$NSPAWN volume rm e2e-rv3-vol >/dev/null 2>&1

step "review: a machine made by reference takes no backend from another machine of it that ran"
# A reference of this run alone, so that no image of it is kept here.
$NSPAWN pull $rv --name e2e-rv3-src --force >/dev/null || fail "pull e2e-rv3-src"
ref="$NSPAWN_REGISTRY/e2e/rv3-ref:$nonce"
$NSPAWN push e2e-rv3-src --to "$ref" >/dev/null || fail "push $ref"
$NSPAWN images rm e2e-rv3-src >/dev/null 2>&1
$NSPAWN pull "$ref" --name e2e-rv3-fa --backend flat --force >/dev/null || fail "pull $ref as a flat machine"
$NSPAWN start e2e-rv3-fa -- sleep 300 >/dev/null || fail "start e2e-rv3-fa"
$NSPAWN create "$ref" e2e-rv3-fb -- sleep 300 >/dev/null || fail "create e2e-rv3-fb by reference"
[ "$($NSPAWN inspect -f '{{.backend}}' e2e-rv3-fb)" = overlay ] || fail "create by reference took the flat backend of a machine that ran: $($NSPAWN inspect -f '{{.backend}}' e2e-rv3-fb)"
$NSPAWN run -d --name e2e-rv3-fc "$ref" -- sleep 300 >/dev/null || fail "run e2e-rv3-fc by reference"
[ "$($NSPAWN inspect -f '{{.backend}}' e2e-rv3-fc)" = overlay ] || fail "run by reference took the flat backend of a machine that ran: $($NSPAWN inspect -f '{{.backend}}' e2e-rv3-fc)"
$NSPAWN rm -f e2e-rv3-fa e2e-rv3-fb e2e-rv3-fc >/dev/null 2>&1

step "review: an update cut short keeps its rollback until --force or rm lets it go"
# The state an update cut short leaves: the old record in rollback/, none in images/.
cut_short() {
  mkdir -p /var/lib/nspawn/rollback/e2e-rv3-rb
  cp /var/lib/nspawn/images/e2e-rv3-rb.json /var/lib/nspawn/rollback/e2e-rv3-rb/record.json
  cp /var/lib/nspawn/manifests/e2e-rv3-rb.json /var/lib/nspawn/rollback/e2e-rv3-rb/manifest.json
  rm -f /var/lib/nspawn/images/e2e-rv3-rb.json /var/lib/nspawn/manifests/e2e-rv3-rb.json
}
$NSPAWN pull $rv --name e2e-rv3-rb --force >/dev/null || fail "pull e2e-rv3-rb"
cut_short
out=$($NSPAWN pull $rv --name e2e-rv3-rb 2>&1) && fail "a pull over a name with a rollback succeeded"
echo "$out" | grep_q -- "--force lets it go" || fail "the pull over a rollback said: $out"
$NSPAWN pull $rv --name e2e-rv3-rb --force >/dev/null || fail "pull --force over a rollback"
[ -e /var/lib/nspawn/rollback/e2e-rv3-rb ] && fail "pull --force left the rollback"
cut_short
$NSPAWN rm e2e-rv3-rb >/dev/null 2>&1 || fail "rm of a name with only a rollback"
[ -e /var/lib/nspawn/rollback/e2e-rv3-rb ] && fail "rm left the rollback"
rm -rf /var/lib/nspawn/rollback/e2e-rv3-rb

step "review: builds: a step's flood of output is clipped once, and a failing one shows its tail"
mkdir -p /tmp/e2e-rv/clip
cat > /tmp/e2e-rv/clip/Dockerfile <<DOCKERFILE
FROM $rv
RUN head -c 3000000 /dev/zero | tr '\0' 'x' | fold -w 100
DOCKERFILE
# --no-cache: a host whose build cache the suite does not prune would give the step from
# it, without a line of output.
$NSPAWN build --no-cache --progress plain -t e2e-rv3-clip:1 /tmp/e2e-rv/clip > /tmp/e2e-rv/clip/out 2>&1 || fail "a build of a step with 3 MB of output: $(tail -5 /tmp/e2e-rv/clip/out)"
[ "$(grep -c 'output clipped, log limit' /tmp/e2e-rv/clip/out)" = 1 ] || fail "the flood was not clipped once: $(grep -c 'output clipped' /tmp/e2e-rv/clip/out)"
cat > /tmp/e2e-rv/clip/Dockerfile.fail <<DOCKERFILE
FROM $rv
RUN yes | head -n 400000; exit 3
DOCKERFILE
$NSPAWN build --progress plain -f /tmp/e2e-rv/clip/Dockerfile.fail -t e2e-rv3-clipf:1 /tmp/e2e-rv/clip > /tmp/e2e-rv/clip/out.fail 2>&1 && fail "a step that exits 3 built"
grep_q 'output clipped, log limit' /tmp/e2e-rv/clip/out.fail || fail "the failing flood was not clipped"
grep_q 'did not complete successfully: exit code: 3' /tmp/e2e-rv/clip/out.fail || fail "the failure was not said: $(tail -5 /tmp/e2e-rv/clip/out.fail)"
awk '/output clipped/ {seen = 1} seen && /(^| )y$/ {found = 1} END {exit !found}' /tmp/e2e-rv/clip/out.fail || fail "the failing step's tail after the clip is not shown"
$NSPAWN images rm e2e-rv3-clip-1 >/dev/null 2>&1

step "review: builds: the client's -f in a context from stdin, an ambiguous stdin, and buildx's warnings"
mkdir -p /tmp/e2e-rv/bc/ctx
printf 'FROM %s\nRUN echo prod > /built\n' "$rv" > /tmp/e2e-rv/bc/ctx/Dockerfile.prod
tar -c -C /tmp/e2e-rv/bc/ctx . | $NSPAWN build -f ./Dockerfile.prod -t e2e-rv3-bc:1 - > /tmp/e2e-rv/bc/out 2>&1 || fail "a context from stdin with -f ./Dockerfile.prod: $(tail -5 /tmp/e2e-rv/bc/out)"
out=$(cd /tmp/e2e-rv/bc/ctx && $NSPAWN build -f Dockerfile.prod -t e2e-rv3-bc2:1 - < Dockerfile.prod 2>&1) && fail "a Dockerfile on stdin with -f was taken"
echo "$out" | grep_q "ambiguous Dockerfile source" || fail "the ambiguous Dockerfile said: $out"
$NSPAWN build -m 2g --squash -f /tmp/e2e-rv/bc/ctx/Dockerfile.prod -t e2e-rv3-bc3:1 /tmp/e2e-rv/bc/ctx > /tmp/e2e-rv/bc/out3 2>&1 || fail "a build with buildx's ignored flags: $(tail -5 /tmp/e2e-rv/bc/out3)"
grep_q "squash is removed with BuildKit" /tmp/e2e-rv/bc/out3 || fail "the squash warning was not printed"
$NSPAWN images rm e2e-rv3-bc-1 e2e-rv3-bc3-1 >/dev/null 2>&1

step "review: builds: a base named by a sha512 digest is refused; ADD of a URL takes the unescaped name"
printf 'FROM %s@sha512:%0128d\n' "${rv%:*}" 0 > /tmp/e2e-rv/bc/Dockerfile.sha512
out=$($NSPAWN build -f /tmp/e2e-rv/bc/Dockerfile.sha512 -t e2e-rv3-512:1 /tmp/e2e-rv/bc 2>&1) && fail "a base by a sha512 digest was taken"
echo "$out" | grep_q "an image is taken by a sha256 digest only" || fail "the sha512 base said: $out"
mkdir -p /tmp/e2e-rv/http3 && echo pkg > "/tmp/e2e-rv/http3/pkg[1].txt"
python3 -m http.server 18200 --bind 127.0.0.1 --directory /tmp/e2e-rv/http3 >/dev/null 2>&1 &
http3_pid=$!
sleep 1
printf 'FROM %s\nADD http://127.0.0.1:18200/pkg%%5B1%%5D.txt /opt/\n' "$rv" > /tmp/e2e-rv/bc/Dockerfile.url
$NSPAWN build -f /tmp/e2e-rv/bc/Dockerfile.url -t e2e-rv3-url:1 /tmp/e2e-rv/bc > /tmp/e2e-rv/bc/out.url 2>&1 || fail "ADD of an escaped URL: $(tail -5 /tmp/e2e-rv/bc/out.url)"
[ "$($NSPAWN run --rm e2e-rv3-url:1 -- cat '/opt/pkg[1].txt' 2>/dev/null)" = pkg ] || fail "ADD of pkg%5B1%5D.txt did not make /opt/pkg[1].txt"
kill $http3_pid 2>/dev/null
$NSPAWN images rm e2e-rv3-url-1 >/dev/null 2>&1

step "review: Docker API: bad escapes, health filters, kill signals and names answered as docker answers"
$NSPAWN run -d --name e2e-rv3-api -v /var/run/docker.sock:/var/run/docker.sock $rv -- sleep 600 >/dev/null || fail "run e2e-rv3-api"
sock=/run/nspawn/api/e2e-rv3-api.sock
retry 10 test -S $sock || fail "no Docker API socket for e2e-rv3-api"
# docker's error bodies end with a newline: the code follows on the same line.
out=$(curl -sS -w ' %{http_code}' --unix-socket $sock 'http://docker/containers/json?all=%zz' | tr -d '\n')
echo "$out" | grep_q 'invalid URL escape \\"%zz\\".* 400$' || fail "a bad escape said: $out"
out=$(curl -sS -G --unix-socket $sock --data-urlencode 'filters={"health":{"healthy":false}}' http://docker/containers/json)
[ "$out" = "[]" ] || fail "a health filter of a false value listed: $out"
out=$(curl -sS -w ' %{http_code}' -X POST --unix-socket $sock 'http://docker/containers/e2e-rv3-api/kill?signal=32' | tr -d '\n')
echo "$out" | grep_q 'cannot kill container: e2e-rv3-api: the linux daemon does not support signal 32.* 400$' || fail "kill with signal 32 said: $out"
out=$(curl -sS -w ' %{http_code}' -X POST --unix-socket $sock 'http://docker/containers/e2e-rv3-nope/kill' | tr -d '\n')
echo "$out" | grep_q 'cannot kill container: e2e-rv3-nope: No such container: e2e-rv3-nope.* 404$' || fail "kill of a missing name said: $out"
$NSPAWN rm -f e2e-rv3-api >/dev/null 2>&1

step "review: events: no kill of a machine between restarts; each die has its run's own code"
since=$(date +%s)
$NSPAWN run -d --name e2e-rv3-loop --restart always $rv -- sh -c 'sleep 1; exit 1' >/dev/null || fail "run e2e-rv3-loop"
retry 15 bash -c "$NSPAWN ps --format '{{.Names}} {{.State}}' | grep_q '^e2e-rv3-loop restarting'" || echo "note: e2e-rv3-loop was not seen between restarts"
$NSPAWN kill e2e-rv3-loop >/dev/null 2>&1
sleep 2
$NSPAWN rm -f e2e-rv3-loop >/dev/null 2>&1
$NSPAWN run --name e2e-rv3-die $rv -- sh -c 'exit 1' >/dev/null 2>&1
$NSPAWN start e2e-rv3-die -- sleep 600 >/dev/null || fail "start e2e-rv3-die"
$NSPAWN kill e2e-rv3-die >/dev/null || fail "kill e2e-rv3-die"
sleep 3
out=$(timeout 30 $NSPAWN events --since "@$since" --until now --filter name=e2e-rv3-die --filter event=die --json)
python3 -c '
import json, sys
codes = [json.loads(l)["attributes"].get("exit_code") for l in sys.argv[1].splitlines() if l.strip()]
assert codes[:2] == ["1", "137"], codes' "$out" || fail "the dies of e2e-rv3-die do not carry their own codes: $out"
$NSPAWN rm -f e2e-rv3-die >/dev/null 2>&1

step "review: compose: links make the linked machine anew with its alias; flags docker refuses together"
mkdir -p /tmp/e2e-rv/links
cat > /tmp/e2e-rv/links/compose.yaml <<COMPOSE
services:
  web:
    image: $rv
    command: ["sleep", "600"]
  db:
    image: $rv
    command: ["sleep", "600"]
COMPOSE
(cd /tmp/e2e-rv/links && $NSPAWN compose -p e2e-rv3l up -d >/dev/null 2>&1) || fail "compose up of e2e-rv3l"
python3 - /tmp/e2e-rv/links/compose.yaml <<'PY'
import sys
p = sys.argv[1]
lines = open(p).read().splitlines(True)
i = lines.index('  web:\n')
lines.insert(i + 3, '    links: ["db:database"]\n')
open(p, 'w').write(''.join(lines))
PY
(cd /tmp/e2e-rv/links && $NSPAWN compose -p e2e-rv3l up -d >/dev/null 2>&1) || fail "compose up after adding links"
retry 5 bash -c "$NSPAWN exec e2e-rv3l-web-1 -- grep -q database /etc/hosts </dev/null" || fail "web does not resolve the link's alias"
out=$(cd /tmp/e2e-rv/links && $NSPAWN compose -p e2e-rv3l up -d --always-recreate-deps --no-recreate 2>&1) && fail "--always-recreate-deps with --no-recreate was taken"
echo "$out" | grep_q -- "--always-recreate-deps and --no-recreate are incompatible" || fail "the incompatible flags said: $out"
(cd /tmp/e2e-rv/links && $NSPAWN compose -p E2E-RV3L down >/dev/null 2>&1) || fail "compose -p in capitals down"
$NSPAWN ps -a --format '{{.Names}}' | grep_q '^e2e-rv3l-' && fail "down of the project named in capitals left its machines"

step "review: compose: formats of ps and images, logs --since, orphans, and names of one machine"
mkdir -p /tmp/e2e-rv/fmt
cat > /tmp/e2e-rv/fmt/compose.yaml <<COMPOSE
services:
  my_db:
    image: $rv
    command: ["sleep", "600"]
COMPOSE
(cd /tmp/e2e-rv/fmt && $NSPAWN compose -p e2e-rv3f up -d >/dev/null 2>&1) || fail "compose up of e2e-rv3f"
out=$(cd /tmp/e2e-rv/fmt && $NSPAWN compose -p e2e-rv3f ps --format '{{.Name}} {{.Label "com.docker.compose.service"}}')
[ "$out" = "e2e-rv3f-my_db-1 my_db" ] || [ "$out" = "e2e-rv3f-my-db-1 my_db" ] || fail "compose ps --format with Label printed: $out"
out=$(cd /tmp/e2e-rv/fmt && $NSPAWN compose -p e2e-rv3f images --format '{{.Repository}}' 2>&1) && fail "compose images took a template"
echo "$out" | grep_q 'format value "{{.Repository}}" could not be parsed: parsing failed' || fail "compose images --format TEMPLATE said: $out"
# sed reads to the end: head would close the pipe before nspawn wrote its last line,
# which pipefail counts.
(cd /tmp/e2e-rv/fmt && $NSPAWN compose -p e2e-rv3f images --format JSON) | sed -n 2p | grep_q '^    "' || fail "compose images --format JSON is not the indented map"
out=$(cd /tmp/e2e-rv/fmt && $NSPAWN compose -p e2e-rv3f logs --since bogus 2>&1) && fail "logs --since bogus succeeded"
echo "$out" | grep_q 'invalid value for "since": failed to parse value as time or duration: "bogus"' || fail "logs --since bogus said: $out"
cat >> /tmp/e2e-rv/fmt/compose.yaml <<COMPOSE
  my-db:
    image: $rv
    command: ["sleep", "600"]
COMPOSE
out=$(cd /tmp/e2e-rv/fmt && $NSPAWN compose -p e2e-rv3f up -d my-db 2>&1) && fail "two services of one machine name were taken"
echo "$out" | grep_q "my_db" && echo "$out" | grep_q "my-db" || fail "the clash did not name both services: $out"
printf 'services:\n  other:\n    image: %s\n    command: ["sleep", "600"]\n' "$rv" > /tmp/e2e-rv/fmt/other.yaml
(cd /tmp/e2e-rv/fmt && COMPOSE_REMOVE_ORPHANS=1 $NSPAWN compose -p e2e-rv3f -f other.yaml down --remove-orphans=false >/dev/null 2>&1)
$NSPAWN ps -a --format '{{.Names}}' | grep_q '^e2e-rv3f-my' || fail "down --remove-orphans=false removed the orphan the variable named"
(cd /tmp/e2e-rv/fmt && $NSPAWN compose -p e2e-rv3f down --remove-orphans >/dev/null 2>&1)
for m in $($NSPAWN ps -a 2>/dev/null | awk '$1 ~ /^e2e-rv3f-/ {print $1}'); do $NSPAWN rm -f "$m" >/dev/null 2>&1; done

step "review: compose kill ends a service between its restarts"
mkdir -p /tmp/e2e-rv/kill
printf 'services:\n  flap:\n    image: %s\n    command: ["sh", "-c", "sleep 1; exit 1"]\n    restart: always\n' "$rv" > /tmp/e2e-rv/kill/compose.yaml
(cd /tmp/e2e-rv/kill && $NSPAWN compose -p e2e-rv3k up -d >/dev/null 2>&1) || fail "compose up of e2e-rv3k"
sleep 4
(cd /tmp/e2e-rv/kill && $NSPAWN compose -p e2e-rv3k kill >/dev/null 2>&1)
sleep 4
systemctl is-active --quiet systemd-nspawn@e2e-rv3k-flap-1.service && fail "compose kill left the restarting service running"
(cd /tmp/e2e-rv/kill && $NSPAWN compose -p e2e-rv3k down >/dev/null 2>&1)

step "review: machines: stop of a flapping machine always ends it; unless-stopped stays off after stop and update"
$NSPAWN run -d --name e2e-rv3-flap --restart always $rv -- true >/dev/null || fail "run e2e-rv3-flap"
for i in 1 2 3 4 5; do
  $NSPAWN start e2e-rv3-flap >/dev/null 2>&1
  $NSPAWN stop e2e-rv3-flap >/dev/null 2>&1 || fail "stop $i of a flapping machine failed"
done
sleep 3
systemctl is-active --quiet systemd-nspawn@e2e-rv3-flap.service && fail "the flapping machine runs after stop"
$NSPAWN rm -f e2e-rv3-flap >/dev/null 2>&1
$NSPAWN run -d --name e2e-rv3-us --restart unless-stopped $rv -- sleep 600 >/dev/null || fail "run e2e-rv3-us"
$NSPAWN stop e2e-rv3-us >/dev/null || fail "stop e2e-rv3-us"
$NSPAWN update e2e-rv3-us --restart unless-stopped >/dev/null || fail "update e2e-rv3-us"
[ "$(systemctl is-enabled systemd-nspawn@e2e-rv3-us.service 2>/dev/null)" = enabled ] && fail "update enabled a machine stopped with unless-stopped"
$NSPAWN rm -f e2e-rv3-us >/dev/null 2>&1

step "review: store: an empty record does not stop other machines; df counts what a machine wrote"
# A machine that has its address: a new one is refused, as one record unread might hold
# the address it would get.
$NSPAWN run -d --name e2e-rv3-ok $rv -- sh -c 'head -c 2000000 /dev/zero > /big; sleep 600' >/dev/null || fail "run e2e-rv3-ok"
$NSPAWN stop e2e-rv3-ok >/dev/null || fail "stop e2e-rv3-ok"
: > /var/lib/nspawn/images/e2e-rv3-broken.json
$NSPAWN start e2e-rv3-ok >/dev/null || fail "a machine does not start next to an empty record"
out=$($NSPAWN run -d --name e2e-rv3-new $rv -- sleep 600 2>&1) && fail "a new machine was given an address next to an empty record"
echo "$out" | grep_q "e2e-rv3-broken.json cannot be read" || fail "the new machine next to an empty record said: $out"
rm -f /var/lib/nspawn/images/e2e-rv3-broken.json
$NSPAWN rm -f e2e-rv3-new >/dev/null 2>&1
sleep 2
$NSPAWN system df --json | python3 -c '
import json, sys
rows = [r for r in json.load(sys.stdin).get("container_rows", []) if r.get("name") == "e2e-rv3-ok"]
assert rows and rows[0]["size"] > 0, rows' || fail "system df counts nothing written by e2e-rv3-ok"
$NSPAWN rm -f e2e-rv3-ok >/dev/null 2>&1

step "review: cp: a host link to a directory is copied as the directory; a file with a slash is refused; dot paths"
$NSPAWN run -d --name e2e-rv3-cp $rv -- sleep 600 >/dev/null || fail "run e2e-rv3-cp"
mkdir -p /tmp/e2e-rv/cp3/real && echo in > /tmp/e2e-rv/cp3/real/f && ln -sfn real /tmp/e2e-rv/cp3/lnk && echo f > /tmp/e2e-rv/cp3/file
$NSPAWN exec e2e-rv3-cp -- mkdir -p /srv </dev/null
$NSPAWN cp /tmp/e2e-rv/cp3/lnk/ e2e-rv3-cp:/srv/ || fail "cp of a link to a directory with a trailing slash"
$NSPAWN exec e2e-rv3-cp -- sh -c 'test -d /srv/lnk && test ! -L /srv/lnk && test -f /srv/lnk/f' </dev/null || fail "the link with a slash was not copied as the directory"
out=$($NSPAWN cp /tmp/e2e-rv/cp3/file/ e2e-rv3-cp:/srv/ 2>&1) && fail "cp of a file named with a trailing slash succeeded"
echo "$out" | grep_q -i "not a directory" || fail "cp of file/ said: $out"
mkdir -p /tmp/e2e-rv/cp3/out
$NSPAWN cp e2e-rv3-cp:/etc/./ /tmp/e2e-rv/cp3/out || fail "cp of /etc/./ into a directory"
[ -f /tmp/e2e-rv/cp3/out/passwd ] || fail "cp of /etc/./ did not put the contents of /etc in the directory"
# A destination link reached through another link resolves inside the machine, as
# docker's daemon resolves it.
$NSPAWN exec e2e-rv3-cp -- sh -c 'mkdir -p /var/www/app/releases/5 /var/www/app/shared/storage && ln -s releases/5 /var/www/app/current && ln -s ../../shared/storage /var/www/app/releases/5/storage' </dev/null || fail "make the release layout"
$NSPAWN cp /tmp/e2e-rv/cp3/file e2e-rv3-cp:/var/www/app/current/storage || fail "cp through a link to a directory reached through a link"
$NSPAWN exec e2e-rv3-cp -- test -f /var/www/app/shared/storage/file </dev/null || fail "the copy did not land where the links lead"
$NSPAWN rm -f e2e-rv3-cp >/dev/null 2>&1

step "review: run, create and start refuse what docker refuses: a mount point twice, an app with nothing to run"
out=$($NSPAWN run -d --name e2e-rv3-dup -v e2e-rv3-dupvol:/data --tmpfs /data// $rv -- sleep 60 2>&1) && fail "a volume and a tmpfs on one place were taken"
echo "$out" | grep_q "Duplicate mount point: /data" || fail "the mount point twice said: $out"
$NSPAWN ps -a -q | grep_q '^e2e-rv3-dup$' && fail "a refused run left a machine"
out=$($NSPAWN run -d --name e2e-rv3-dup -v /tmp:/x -v /var/tmp:/x/ $rv -- sleep 60 2>&1) && fail "two volumes on one place were taken"
echo "$out" | grep_q "Duplicate mount point: /x" || fail "two volumes on one place said: $out"
$NSPAWN volume rm e2e-rv3-dupvol >/dev/null 2>&1
out=$($NSPAWN run -d --name e2e-rv3-noc --entrypoint "" $rv 2>&1) && fail "an app with nothing to run was made"
echo "$out" | grep_q "no command specified" || fail "an app with nothing to run said: $out"
$NSPAWN ps -a -q | grep_q '^e2e-rv3-noc$' && fail "a refused run left a machine"
[ "$($NSPAWN run --rm --entrypoint "" $rv echo hi 2>/dev/null)" = hi ] || fail "--entrypoint \"\" with arguments did not run them"
# An image with neither an entrypoint nor a cmd runs the command given to run.
mkdir -p /tmp/e2e-rv/nocmd
# busybox's binary needs its libc, which the image keeps in /lib (/lib64 a link to it).
printf 'FROM %s AS b\nFROM scratch\nCOPY --from=b /bin/busybox /busybox\nCOPY --from=b /lib /lib\nCOPY --from=b /lib64 /lib64\n' "$rv" > /tmp/e2e-rv/nocmd/Dockerfile
$NSPAWN build -t e2e-rv3-nocmd:1 /tmp/e2e-rv/nocmd > /tmp/e2e-rv/nocmd/out 2>&1 || fail "build of an image without a cmd: $(tail -5 /tmp/e2e-rv/nocmd/out)"
[ "$($NSPAWN run --rm e2e-rv3-nocmd:1 -- /busybox echo hi 2>/dev/null)" = hi ] || fail "run of an image without a cmd did not run the command given"
out=$($NSPAWN run --rm e2e-rv3-nocmd:1 2>&1) && fail "run of an image without a cmd and none given succeeded"
echo "$out" | grep_q "no command specified" || fail "run of an image with nothing to run said: $out"
$NSPAWN images rm e2e-rv3-nocmd-1 >/dev/null 2>&1
$NSPAWN run -d --name e2e-rv3-ep --entrypoint /bin/sh $rv >/dev/null 2>&1
[ "$($NSPAWN inspect -f '{{json .cmd}}' e2e-rv3-ep)" = "[]" ] || fail "an entrypoint given alone keeps the image's cmd: $($NSPAWN inspect -f '{{json .cmd}}' e2e-rv3-ep)"
$NSPAWN rm -f e2e-rv3-ep >/dev/null 2>&1
mkdir -p /tmp/e2e-rv/ep
cat > /tmp/e2e-rv/ep/compose.yaml <<COMPOSE
services:
  web:
    image: $rv
    entrypoint: []
COMPOSE
(cd /tmp/e2e-rv/ep && $NSPAWN compose -p e2e-rv3e up -d >/dev/null 2>&1) || fail "compose up of an empty entrypoint"
[ "$($NSPAWN inspect -f '{{json .command}}' e2e-rv3e-web-1)" = '["sh"]' ] || fail "an empty entrypoint of the file did not keep the image's command: $($NSPAWN inspect -f '{{json .command}}' e2e-rv3e-web-1)"
(cd /tmp/e2e-rv/ep && $NSPAWN compose -p e2e-rv3e down >/dev/null 2>&1)

step "review: ps: docker's Command and Status, --no-trunc, and the Docker API's Command"
$NSPAWN run --name e2e-rv3-ps $rv -- sh -c 'echo a; exit 3' >/dev/null 2>&1
$NSPAWN run -d --name e2e-rv3-ps2 -v /var/run/docker.sock:/var/run/docker.sock $rv -- sh -c 'echo a; sleep 3600' >/dev/null || fail "run e2e-rv3-ps2"
$NSPAWN ps --filter ' ' >/dev/null 2>&1 && fail "ps took a filter of blanks"
status=$($NSPAWN ps -a --filter 'name=^e2e-rv3-ps$' --format '{{.Status}}')
echo "$status" | grep_q '^Exited (3) [0-9a-zA-Z ]* ago$' || fail "the status of a machine that exited 3 is: $status"
# docker's Ellipsis cuts to 20 columns, U+2026 the last of them.
ell=$(printf '\342\200\246')
[ "$($NSPAWN ps --filter name=e2e-rv3-ps2 --format '{{.Command}}')" = "\"sh -c 'echo a; slee$ell\"" ] || fail "ps cut the command as: $($NSPAWN ps --filter name=e2e-rv3-ps2 --format '{{.Command}}')"
[ "$($NSPAWN ps --no-trunc --filter name=e2e-rv3-ps2 --format '{{.Command}}')" = "\"sh -c 'echo a; sleep 3600'\"" ] || fail "ps --no-trunc printed: $($NSPAWN ps --no-trunc --filter name=e2e-rv3-ps2 --format '{{.Command}}')"
retry 10 test -S /run/nspawn/api/e2e-rv3-ps2.sock || fail "no Docker API socket for e2e-rv3-ps2"
curl -sS --unix-socket /run/nspawn/api/e2e-rv3-ps2.sock 'http://docker/containers/json?filters=%7B%22name%22%3A%5B%22e2e-rv3-ps2%22%5D%7D' | python3 -c '
import json, sys
c = json.load(sys.stdin)[0]
assert c["Command"] == "sh -c '"'"'echo a; sleep 3600'"'"'", c["Command"]' || fail "the Docker API lists the command otherwise"
$NSPAWN rm -f e2e-rv3-ps e2e-rv3-ps2 >/dev/null 2>&1

step "review: the image's own getent is bound from inside its root, not through its links"
mkdir -p /tmp/e2e-rv/ge
cat > /tmp/e2e-rv/ge/Dockerfile <<'DOCKERFILE'
FROM busybox-placeholder
RUN rm -rf /usr && ln -s / /usr && printf '#!/bin/sh\necho image-getent\n' > /bin/getent && chmod 755 /bin/getent
USER nobody
DOCKERFILE
sed -i "s#busybox-placeholder#$rv#" /tmp/e2e-rv/ge/Dockerfile
$NSPAWN build -t e2e-rv3-ge:1 /tmp/e2e-rv/ge > /tmp/e2e-rv/ge/out 2>&1 || fail "build e2e-rv3-ge: $(tail -5 /tmp/e2e-rv/ge/out)"
# By its path: systemd-nspawn gives a root without /sbin one leading to usr/sbin, which
# with /usr a link to / leads to itself, and a lookup on PATH ends there.
out=$($NSPAWN run --rm e2e-rv3-ge:1 -- /bin/cat /run/nspawn/getent 2>/dev/null)
echo "$out" | grep_q "image-getent" || fail "the getent bound in the machine is not the image's: $(echo "$out" | head -c 200 | od -c | head -3)"
$NSPAWN images rm e2e-rv3-ge-1 >/dev/null 2>&1

step "review: a negative stop timeout waits for the program, as docker's -1"
trap_cmd='trap "sleep 12; exit 7" TERM; while :; do sleep 1; done'
$NSPAWN run -d --name e2e-rv3-nt --stop-timeout -1 $rv -- sh -c "$trap_cmd" >/dev/null || fail "run --stop-timeout -1"
$NSPAWN inspect e2e-rv3-nt | python3 -c 'import json,sys; assert json.load(sys.stdin)[0]["stop_timeout"] == 2**64 - 1' || fail "--stop-timeout -1 is not kept as none"
$NSPAWN generate e2e-rv3-nt | grep_q -- "--stop-timeout=-1" || fail "generate does not give --stop-timeout=-1 back"
sleep 2
start=$(date +%s)
$NSPAWN stop e2e-rv3-nt >/dev/null || fail "stop e2e-rv3-nt"
[ $(( $(date +%s) - start )) -ge 11 ] || fail "stop did not wait for the program's own end"
$NSPAWN ps -a --filter name=e2e-rv3-nt --format '{{.Status}}' | grep_q '^Exited (7)' || fail "the program did not end by itself: $($NSPAWN ps -a --filter name=e2e-rv3-nt --format '{{.Status}}')"
$NSPAWN rm -f e2e-rv3-nt >/dev/null 2>&1
$NSPAWN run -d --name e2e-rv3-nt2 $rv -- sh -c "$trap_cmd" >/dev/null || fail "run e2e-rv3-nt2"
sleep 2
$NSPAWN stop -t -1 e2e-rv3-nt2 >/dev/null || fail "stop -t -1"
$NSPAWN ps -a --filter name=e2e-rv3-nt2 --format '{{.Status}}' | grep_q '^Exited (7)' || fail "stop -t -1 did not wait: $($NSPAWN ps -a --filter name=e2e-rv3-nt2 --format '{{.Status}}')"
$NSPAWN rm -f e2e-rv3-nt2 >/dev/null 2>&1
mkdir -p /tmp/e2e-rv/nt
printf 'services:\n  slow:\n    image: %s\n    command: ["sh", "-c", %s]\n' "$rv" "$(python3 -c 'import json,sys; print(json.dumps(sys.argv[1]))' "$trap_cmd")" > /tmp/e2e-rv/nt/compose.yaml
(cd /tmp/e2e-rv/nt && $NSPAWN compose -p e2e-rv3n up -d >/dev/null 2>&1) || fail "compose up of e2e-rv3n"
sleep 2
(cd /tmp/e2e-rv/nt && $NSPAWN compose -p e2e-rv3n stop -t -1 >/dev/null 2>&1) || fail "compose stop -t -1"
$NSPAWN ps -a --filter name=e2e-rv3n-slow-1 --format '{{.Status}}' | grep_q '^Exited (7)' || fail "compose stop -t -1 did not wait: $($NSPAWN ps -a --filter name=e2e-rv3n-slow-1 --format '{{.Status}}')"
(cd /tmp/e2e-rv/nt && $NSPAWN compose -p e2e-rv3n down >/dev/null 2>&1)

if command -v mkosi >/dev/null 2>&1 && [ -f "${ctx:-}/mkosi.conf" ]; then
  step "review: an mkosi build the service's end cut short leaves nothing; a kept output stays"
  $NSPAWN build -t e2e-rv3-mk:1 "$ctx" > /tmp/e2e-rv/mk.out 2>&1 &
  mk_pid=$!
  retry 60 bash -c 'ls -d /var/lib/nspawn/builds/mkosi-* >/dev/null 2>&1' || fail "no mkosi build directory appeared"
  systemctl restart nspawn.service
  wait $mk_pid 2>/dev/null
  timeout 30 $NSPAWN ps >/dev/null || fail "the service does not answer after its restart"
  ls -d /var/lib/nspawn/builds/mkosi-* >/dev/null 2>&1 && fail "the cut short mkosi build left its output: $(ls -d /var/lib/nspawn/builds/mkosi-*)"
  $NSPAWN images rm e2e-rv3-mk-1 >/dev/null 2>&1
fi

step "review: volume rm takes a btrfs subvolume made inside the volume"
if [ "$(stat -f -c %T /var/lib/nspawn 2>/dev/null)" = btrfs ] && command -v btrfs >/dev/null 2>&1; then
  $NSPAWN volume create e2e-rv3-sv >/dev/null || fail "volume create e2e-rv3-sv"
  btrfs subvolume create /var/lib/nspawn/volumes/e2e-rv3-sv/sub >/dev/null || fail "btrfs subvolume create"
  touch /var/lib/nspawn/volumes/e2e-rv3-sv/sub/f
  $NSPAWN volume rm e2e-rv3-sv >/dev/null || fail "volume rm of a volume that holds a btrfs subvolume"
  [ -e /var/lib/nspawn/volumes/e2e-rv3-sv ] && fail "volume rm left the volume with a subvolume"
else
  echo "/var/lib/nspawn is not on btrfs here: the subvolume check is skipped"
fi

step "review: CDI: a node whose numbers changed is made again by systemd's own start; a 'u' node cannot be opened"
mkdir -p /etc/cdi
rv3_spec() {
  cat > /etc/cdi/e2e-rv3.yaml <<SPEC
---
cdiVersion: 0.6.0
kind: e2e-rv3.nspawn.test/dev
devices:
- name: made
  containerEdits:
    deviceNodes:
    - path: /dev/e2e-rv3-made
      type: c
      major: 1
      minor: $1
- name: unbuffered
  containerEdits:
    deviceNodes:
    - path: /dev/e2e-rv3-u
      type: u
      major: 240
      minor: 0
SPEC
}
rv3_spec 3
$NSPAWN run -d --name e2e-rv3-cdi --restart always --device e2e-rv3.nspawn.test/dev=made --device e2e-rv3.nspawn.test/dev=unbuffered $rv -- sleep 600 >/dev/null || fail "run e2e-rv3-cdi"
[ "$($NSPAWN exec e2e-rv3-cdi -- stat -c '%t:%T' /dev/e2e-rv3-made </dev/null | tr -d '\r')" = 1:3 ] || fail "the made node has not /dev/null's numbers"
# Numbers no other rule lets the machine open (systemd's DevicePolicy=closed lets it open
# /dev/null's and the rest of the standard ones): a 'u' node gets no rule of its own.
out=$($NSPAWN exec e2e-rv3-cdi -- cat /dev/e2e-rv3-u </dev/null 2>&1) && fail "a 'u' node could be opened"
echo "$out" | grep_q -i "not permitted" || fail "opening a 'u' node said: $out"
$NSPAWN stop e2e-rv3-cdi >/dev/null || fail "stop e2e-rv3-cdi"
rv3_spec 5
systemctl start systemd-nspawn@e2e-rv3-cdi.service || fail "systemd's own start after the node's numbers changed: $(journalctl -u systemd-nspawn@e2e-rv3-cdi.service -n 5 --no-pager)"
retry 15 bash -c "$NSPAWN exec e2e-rv3-cdi -- stat -c '%t:%T' /dev/e2e-rv3-made </dev/null | grep_q '^1:5'" || fail "the made node has not the new numbers"
$NSPAWN exec e2e-rv3-cdi -- head -c 4 /dev/e2e-rv3-made </dev/null | od -An -tx1 | grep_q "00 00 00 00" || fail "the made node of /dev/zero's numbers cannot be read"
$NSPAWN rm -f e2e-rv3-cdi >/dev/null 2>&1 || fail "rm e2e-rv3-cdi"
[ -e /run/systemd/system.control/systemd-nspawn@e2e-rv3-cdi.service.d/50-DeviceAllow.conf ] && fail "rm left the runtime DeviceAllow drop-in"
rm -f /etc/cdi/e2e-rv3.yaml

step "review: a start held in systemd's queue and stopped there leaves nothing of its preparation"
$NSPAWN create $rv e2e-rv3-qdb --health-cmd 'test -f /tmp/ok' --health-interval 1s -- sh -c 'sleep 20; touch /tmp/ok; sleep 600' >/dev/null || fail "create e2e-rv3-qdb"
$NSPAWN create $rv e2e-rv3-qweb --depends-on e2e-rv3-qdb:healthy -- sleep 600 >/dev/null || fail "create e2e-rv3-qweb"
$NSPAWN start e2e-rv3-qweb >/tmp/e2e-rv/qweb.out 2>&1 &
qstart=$!
retry 15 bash -c "systemctl list-jobs --no-legend | grep_q 'systemd-nspawn@e2e-rv3-qweb.service'" || echo "note: e2e-rv3-qweb's start was not seen in the queue"
$NSPAWN stop e2e-rv3-qweb >/dev/null 2>&1
wait $qstart 2>/dev/null
sleep 2
ip netns list | grep_q "nspawn-e2e-rv3-qweb" && fail "a start stopped in the queue left its network namespace"
[ -e /run/nspawn/secrets/e2e-rv3-qweb ] && fail "a start stopped in the queue left its secrets"
$NSPAWN rm -f e2e-rv3-qweb e2e-rv3-qdb >/dev/null 2>&1

step "review: generate says to push an image built here, its build's record gone"
mkdir -p /tmp/e2e-rv/gen
printf 'FROM %s\nRUN echo built > /built\n' "$rv" > /tmp/e2e-rv/gen/Dockerfile
$NSPAWN build -t e2e-rv3-built:1 /tmp/e2e-rv/gen >/dev/null 2>&1 || fail "build e2e-rv3-built"
$NSPAWN create e2e-rv3-built:1 e2e-rv3-fromb -- sleep 600 >/dev/null || fail "create e2e-rv3-fromb"
$NSPAWN images rm e2e-rv3-built-1 >/dev/null 2>&1 || fail "images rm e2e-rv3-built-1"
$NSPAWN generate e2e-rv3-fromb 2>&1 | grep_q -i "push" || fail "generate does not say the image was built here: $($NSPAWN generate e2e-rv3-fromb 2>&1 | head -5)"
$NSPAWN rm -f e2e-rv3-fromb >/dev/null 2>&1

step "review: compose: COMPOSE_REMOVE_ORPHANS of .env, restart of what restart: true leaves out, Ctrl+C of up SERVICE"
mkdir -p /tmp/e2e-rv/orph
cat > /tmp/e2e-rv/orph/compose.yaml <<COMPOSE
services:
  db:
    image: $rv
    command: ["sleep", "600"]
  web:
    image: $rv
    command: ["sleep", "600"]
    depends_on:
      db: {condition: service_started, restart: true}
  gone:
    image: $rv
    command: ["sleep", "600"]
COMPOSE
(cd /tmp/e2e-rv/orph && $NSPAWN compose -p e2e-rv3o up -d >/dev/null 2>&1) || fail "compose up of e2e-rv3o"
# restart of web while db, which it depends on with restart: true, is stopped: web alone.
(cd /tmp/e2e-rv/orph && $NSPAWN compose -p e2e-rv3o stop db >/dev/null 2>&1)
(cd /tmp/e2e-rv/orph && timeout 60 $NSPAWN compose -p e2e-rv3o restart web >/dev/null 2>&1) || fail "compose restart web waited for db, which it does not restart"
# Ctrl+C of an attached up of web stops web, not db.
(cd /tmp/e2e-rv/orph && $NSPAWN compose -p e2e-rv3o up -d db >/dev/null 2>&1)
(cd /tmp/e2e-rv/orph && exec $NSPAWN compose -p e2e-rv3o up web >/dev/null 2>&1) &
upweb=$!
retry 15 bash -c "$NSPAWN ps --format '{{.Names}}' | grep_q '^e2e-rv3o-web-1\$'" || fail "the attached up did not start web"
sleep 2
kill -INT $upweb 2>/dev/null
wait $upweb 2>/dev/null
retry 15 bash -c "! $NSPAWN ps --format '{{.Names}}' | grep_q '^e2e-rv3o-web-1\$'" || fail "Ctrl+C did not stop web"
$NSPAWN ps --format '{{.Names}}' | grep_q '^e2e-rv3o-db-1$' || fail "Ctrl+C of up web stopped db as well"
# An orphan goes at up when .env asks it.
python3 - /tmp/e2e-rv/orph/compose.yaml <<'PY'
import sys
p = sys.argv[1]
s = open(p).read()
s = s[:s.index('  gone:')]
open(p, 'w').write(s)
PY
echo "COMPOSE_REMOVE_ORPHANS=true" > /tmp/e2e-rv/orph/.env
(cd /tmp/e2e-rv/orph && $NSPAWN compose -p e2e-rv3o up -d >/dev/null 2>&1) || fail "compose up with COMPOSE_REMOVE_ORPHANS in .env"
$NSPAWN ps -a --format '{{.Names}}' | grep_q '^e2e-rv3o-gone-1$' && fail "up did not take COMPOSE_REMOVE_ORPHANS from .env"
(cd /tmp/e2e-rv/orph && $NSPAWN compose -p e2e-rv3o down >/dev/null 2>&1)

step "review: compose: start --wait by name alone takes a one-shot others wait to complete"
mkdir -p /tmp/e2e-rv/once
cat > /tmp/e2e-rv/once/compose.yaml <<COMPOSE
services:
  migrate:
    image: $rv
    command: ["true"]
  web:
    image: $rv
    command: ["sleep", "600"]
    depends_on:
      migrate: {condition: service_completed_successfully}
COMPOSE
(cd /tmp/e2e-rv/once && $NSPAWN compose -p e2e-rv3w up -d >/dev/null 2>&1) || fail "compose up of e2e-rv3w"
(cd /tmp/e2e-rv/once && $NSPAWN compose -p e2e-rv3w stop >/dev/null 2>&1)
(cd / && timeout 120 $NSPAWN compose -p e2e-rv3w start --wait >/dev/null 2>&1) || fail "start --wait by name alone refused the one-shot web waits for"
(cd /tmp/e2e-rv/once && $NSPAWN compose -p e2e-rv3w down >/dev/null 2>&1)

step "review: compose: run of a job takes the project's volume named with -v"
mkdir -p /tmp/e2e-rv/jobv
cat > /tmp/e2e-rv/jobv/compose.yaml <<COMPOSE
services:
  db:
    image: $rv
    command: ["sleep", "600"]
    volumes: ["pgdata:/data"]
jobs:
  backup:
    image: $rv
    triggers: {manual: true}
volumes:
  pgdata: {}
COMPOSE
(cd /tmp/e2e-rv/jobv && timeout 120 $NSPAWN compose -p e2e-rv3b run --rm -v pgdata:/x backup true >/dev/null 2>&1) || fail "compose run of a job with the project's volume"
$NSPAWN volume ls --json | python3 -c 'import json,sys; assert all(v["name"] != "pgdata" for v in json.load(sys.stdin))' || fail "the job took a volume of its own, not the project's"
(cd /tmp/e2e-rv/jobv && $NSPAWN compose -p e2e-rv3b down -v >/dev/null 2>&1)
$NSPAWN volume rm e2e-rv3b_pgdata pgdata >/dev/null 2>&1

step "compose: a service others depend on is made anew or removed as with docker"
mkdir -p /tmp/e2e-rv/deps
cat > /tmp/e2e-rv/deps/compose.yaml <<COMPOSE
services:
  db:
    image: $rv
    command: ["sleep", "600"]
    environment: {GEN: "1"}
  web:
    image: $rv
    command: ["sleep", "600"]
    depends_on: [db]
COMPOSE
(cd /tmp/e2e-rv/deps && $NSPAWN compose -p e2e-rv4d up -d >/dev/null 2>&1) || fail "compose up of e2e-rv4d"
$NSPAWN inspect e2e-rv4d-web-1 | python3 -c 'import json,sys; d = json.load(sys.stdin)[0]; assert not d.get("depends_on"), d.get("depends_on")' || fail "a compose machine carries --depends-on"
created() { $NSPAWN inspect "$1" | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["created"])'; }
web_created=$(created e2e-rv4d-web-1)
db_created=$(created e2e-rv4d-db-1)
sleep 1
sed -i 's/GEN: "1"/GEN: "2"/' /tmp/e2e-rv/deps/compose.yaml
out=$(cd /tmp/e2e-rv/deps && $NSPAWN compose -p e2e-rv4d up -d 2>&1) || fail "up that makes db anew under a running web: $out"
[ "$(created e2e-rv4d-db-1)" != "$db_created" ] || fail "db was not made anew"
[ "$(created e2e-rv4d-web-1)" = "$web_created" ] || fail "web was made anew with db, which docker compose leaves alone"
systemctl is-active --quiet systemd-nspawn@e2e-rv4d-web-1.service || fail "web stopped when db was made anew"
out=$(cd /tmp/e2e-rv/deps && $NSPAWN compose -p e2e-rv4d rm -s -f db 2>&1) || fail "rm of a service another one depends on: $out"
$NSPAWN images ls | grep_q "^ *e2e-rv4d-db-1 " && fail "rm left db"
systemctl is-active --quiet systemd-nspawn@e2e-rv4d-web-1.service || fail "web stopped when db was removed"
(cd /tmp/e2e-rv/deps && $NSPAWN compose -p e2e-rv4d down >/dev/null 2>&1) || fail "compose down of e2e-rv4d"

step "compose exec and hooks run as the service's user, as docker compose's do"
mkdir -p /tmp/e2e-rv/user
cat > /tmp/e2e-rv/user/compose.yaml <<COMPOSE
services:
  app:
    image: $rv
    command: ["sleep", "600"]
    user: "65534"
    post_start:
      - command: ["sh", "-c", "id -u > /tmp/hook-uid"]
COMPOSE
(cd /tmp/e2e-rv/user && $NSPAWN compose -p e2e-rv4u up -d >/dev/null 2>&1) || fail "compose up of a service with a user"
[ "$(cd /tmp/e2e-rv/user && $NSPAWN compose -p e2e-rv4u exec -T app id -u | tr -d '\r')" = 65534 ] || fail "compose exec without --user does not run as the service's user"
[ "$(cd /tmp/e2e-rv/user && $NSPAWN compose -p e2e-rv4u exec -T --user root app id -u | tr -d '\r')" = 0 ] || fail "compose exec --user root does not run as root"
[ "$($NSPAWN exec -u root e2e-rv4u-app-1 -- cat /tmp/hook-uid </dev/null | tr -d '\r')" = 65534 ] || fail "a post_start hook without a user does not run as the service's user"
(cd /tmp/e2e-rv/user && $NSPAWN compose -p e2e-rv4u down >/dev/null 2>&1)

$NSPAWN rm -f e2e-rv-web e2e-rv-ex e2e-rv-sec e2e-rv-pol e2e-rv-rel >/dev/null 2>&1
for i in e2e-rv-hl-1 e2e-rv-ctx-1 e2e-rv-url-1; do $NSPAWN images rm $i >/dev/null 2>&1; done
$NSPAWN secret rm e2e-rv-pw >/dev/null 2>&1
rm -rf /tmp/e2e-rv

step "build: a step another build ran again leaves this build's steps after it cached"
# Two Dockerfiles share a step whose result differs each time it runs. After one of them
# runs it again with --no-cache, the other is still taken whole from the cache, as with
# BuildKit: what it was made on comes with it.
d=/tmp/e2e-rv/share && mkdir -p $d
for n in a b; do
  printf 'FROM %s\nRUN date +%%s%%N > /stamp\nRUN echo %s-%s > /%s\n' "$rv" "$n" "$nonce" "$n" > $d/Dockerfile.$n
done
$NSPAWN build -q -f $d/Dockerfile.a -t e2e-share-a:1 $d >/dev/null 2>&1 || fail "build of the first Dockerfile"
idb=$($NSPAWN build -q -f $d/Dockerfile.b -t e2e-share-b:1 $d 2>/dev/null) || fail "build of the second Dockerfile"
stamp=$($NSPAWN run --rm e2e-share-b:1 -- cat /stamp 2>/dev/null)
$NSPAWN build -q --no-cache -f $d/Dockerfile.a -t e2e-share-a:1 $d >/dev/null 2>&1 || fail "--no-cache build of the first Dockerfile"
out=$($NSPAWN build --progress plain -f $d/Dockerfile.b -t e2e-share-b:1 $d 2>&1) || fail "build of the second Dockerfile again: $out"
[ "$(echo "$out" | grep -c CACHED)" = 2 ] || fail "the second Dockerfile was not taken from the cache: $out"
[ "$($NSPAWN build -q -f $d/Dockerfile.b -t e2e-share-b:1 $d 2>/dev/null)" = "$idb" ] || fail "the second Dockerfile gave another image"
[ -n "$stamp" ] && [ "$($NSPAWN run --rm e2e-share-b:1 -- cat /stamp 2>/dev/null)" = "$stamp" ] || fail "the second image lost the step it was made on: $stamp"
$NSPAWN images rm e2e-share-a-1 e2e-share-b-1 >/dev/null 2>&1

step "run and create take a local image of a name as docker reads it when the registry's is not here"
# Which of the two goes first, both being here, the unit tests say: their local names
# would be the same.
d=/tmp/e2e-rv/names && mkdir -p $d
printf 'FROM %s\nRUN echo docker-%s > /which\n' "$rv" "$nonce" > $d/Dockerfile.docker
$NSPAWN build -q -f $d/Dockerfile.docker -t docker.io/library/e2e-names-$nonce:1 $d >/dev/null 2>&1 || fail "build of the Docker Hub named image"
[ "$($NSPAWN run --rm --pull never e2e-names-$nonce:1 -- cat /which 2>/dev/null | tr -d '\r')" = docker-$nonce ] || fail "run did not take the local image of Docker Hub's name"
$NSPAWN create e2e-names-$nonce:1 e2e-names-m >/dev/null 2>&1 || fail "create did not take the local image of Docker Hub's name"
[ "$($NSPAWN inspect -f '{{.reference}}' e2e-names-m)" = "docker.io/library/e2e-names-$nonce:1" ] || fail "create took another image: $($NSPAWN inspect -f '{{.reference}}' e2e-names-m)"
$NSPAWN rm -f e2e-names-m >/dev/null 2>&1
out=$($NSPAWN run --rm --pull never e2e-names-none-$nonce:1 -- true 2>&1) && fail "run --pull never of a name nothing has: $out"
echo "$out" | grep_q "no local image of $NSPAWN_REGISTRY/e2e-names-none-$nonce:1" || fail "run --pull never of a missing image was not explained: $out"
for m in $($NSPAWN images ls | awk -v n="e2e-names-$nonce" '$1 ~ n {print $1}'); do $NSPAWN images rm "$m" >/dev/null 2>&1; done

step "an app's own network takes docker's sysctls: another user binds a low port, --sysctl wins and takes a value with a space, the host keeps its own"
$NSPAWN run -d --name e2e-sc -u 65534 $rv -- sleep 600 >/dev/null || fail "run e2e-sc"
sc() { $NSPAWN exec e2e-sc -- /bin/sh -c "$1" </dev/null 2>&1 | tr -d '\r'; }
[ "$(sc 'cat /proc/sys/net/ipv4/ping_group_range' | tr -s ' \t' ' ')" = "0 2147483647" ] || fail "ping_group_range of an app: $(sc 'cat /proc/sys/net/ipv4/ping_group_range')"
[ "$(sc 'cat /proc/sys/net/ipv4/ip_unprivileged_port_start')" = 0 ] || fail "ip_unprivileged_port_start of an app: $(sc 'cat /proc/sys/net/ipv4/ip_unprivileged_port_start')"
[ "$(sc 'id -u')" = 65534 ] || fail "exec did not run as the app's user: $(sc 'id -u')"
# The listener lets go of the output of exec, which waits for it, and goes once seen.
listen='nc -l -p 80 </dev/null >/dev/null 2>&1 & p=$!; sleep 1; netstat -ltn; kill $p'
sc "$listen" | grep_q ":80 " || fail "a user other than root cannot bind port 80: $(sc "$listen")"
$NSPAWN rm -f e2e-sc >/dev/null 2>&1
$NSPAWN run -d --name e2e-sc --sysctl net.ipv4.ip_unprivileged_port_start=1024 --sysctl 'net.ipv4.ip_local_port_range=40000 50000' $rv -- sleep 600 >/dev/null || fail "run e2e-sc with --sysctl"
[ "$(sc 'cat /proc/sys/net/ipv4/ip_unprivileged_port_start')" = 1024 ] || fail "--sysctl did not win over docker's default: $(sc 'cat /proc/sys/net/ipv4/ip_unprivileged_port_start')"
[ "$(sc 'cat /proc/sys/net/ipv4/ip_local_port_range' | tr -s ' \t' ' ')" = "40000 50000" ] || fail "a --sysctl value with a space: $(sc 'cat /proc/sys/net/ipv4/ip_local_port_range')"
$NSPAWN rm -f e2e-sc >/dev/null 2>&1
# Inside, the range reads as the machine's user namespace maps it: the host's is what
# shows whether an app on the host's network changed it.
host_range=$(tr -s ' \t' ' ' < /proc/sys/net/ipv4/ping_group_range)
host_ports=$(cat /proc/sys/net/ipv4/ip_unprivileged_port_start)
$NSPAWN run --rm --network host $rv -- true >/dev/null 2>&1 || fail "run of an app on the host's network"
[ "$(tr -s ' \t' ' ' < /proc/sys/net/ipv4/ping_group_range)" = "$host_range" ] || fail "the host's ping_group_range changed"
[ "$(cat /proc/sys/net/ipv4/ip_unprivileged_port_start)" = "$host_ports" ] || fail "the host's ip_unprivileged_port_start changed"

step "published ports answer on the host's IPv6 addresses through the port proxy; 0.0.0.0 and [IP] keep a port to one family"
if [ "$host_ipv6" != yes ] || ! python3 -c 'import socket; socket.socket(socket.AF_INET6).bind(("::1", 0))' 2>/dev/null; then
  echo "this host has no IPv6 loopback: skipped"
else
  web6='mkdir -p /www; echo "$0" > /www/index.html; exec /bin/httpd -f -p 80 -h /www'
  get6() { curl -g -sf -m 3 "$1"; }
  $NSPAWN run -d --name e2e-v6 -p 18140:80 -p 0.0.0.0:18141:80 -p '[::1]:18142:80' $rv -- /bin/sh -c "$web6" v6-$nonce >/dev/null || fail "run e2e-v6 with ports of both families"
  retry 10 bash -c "curl -g -sf -m 3 'http://[::1]:18140/' | grep_q v6-$nonce" || fail "a port given no IP does not answer on [::1]: $(systemctl status nspawn-proxy-e2e-v6.service --no-pager 2>&1 | tail -5)"
  [ "$(systemctl is-active nspawn-proxy-e2e-v6.service)" = active ] || fail "the port proxy of e2e-v6 is not active"
  get6 http://127.0.0.1:18140/ | grep_q v6-$nonce || fail "the IPv4 side of a port given no IP"
  get6 http://127.0.0.1:18141/ | grep_q v6-$nonce || fail "0.0.0.0 does not answer on IPv4"
  get6 'http://[::1]:18141/' >/dev/null && fail "0.0.0.0 answered on IPv6"
  get6 'http://[::1]:18142/' | grep_q v6-$nonce || fail "[::1] does not answer on [::1]"
  get6 http://127.0.0.1:18142/ >/dev/null && fail "[::1] answered on IPv4"
  # Through an address of an interface, as a client on the network comes.
  link=$(ip -6 -o addr show scope link | awk '$2 != "lo" {split($4, a, "/"); print a[1] "%" $2; exit}')
  if [ -n "$link" ]; then
    get6 "http://[${link/\%/%25}]:18140/" | grep_q v6-$nonce || fail "the port does not answer on the link-local address $link"
  fi
  $NSPAWN port e2e-v6 | grep_q -x '80/tcp -> \[::\]:18140' || fail "port does not show the IPv6 side: $($NSPAWN port e2e-v6 2>&1)"
  $NSPAWN port e2e-v6 | grep_q -x '80/tcp -> \[::1\]:18142' || fail "port does not show [::1]: $($NSPAWN port e2e-v6 2>&1)"
  $NSPAWN inspect e2e-v6 | python3 -c "import json,sys; d = json.load(sys.stdin)[0]; assert d['ports'] == ['18140->80/tcp', '0.0.0.0:18141->80/tcp', '[::1]:18142->80/tcp'], d['ports']" || fail "inspect does not keep the ports as given"
  $NSPAWN generate e2e-v6 | grep_q -- "--publish '\[::1\]:18142:80'" || fail "generate does not give the IPv6 port back: $($NSPAWN generate e2e-v6 2>&1 | grep publish)"
  # Taken on IPv6 by another machine, or by a service of the host there.
  out=$($NSPAWN run -d --name e2e-v6b -p '[::]:18140:80' $rv -- sleep 600 2>&1) && fail "a port another machine takes on IPv6 was published again"
  echo "$out" | grep_q "host port 18140/tcp is already published by e2e-v6" || fail "the clash on IPv6 was not explained: $out"
  python3 -c 'import socket, time
s = socket.socket(socket.AF_INET6); s.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 1)
s.bind(("::", 18144)); s.listen(); time.sleep(30)' &
  v6_service=$!
  sleep 1
  out=$($NSPAWN run -d --name e2e-v6c -p 18144:80 $rv -- sleep 600 2>&1) && fail "a port a host service takes on IPv6 was published"
  echo "$out" | grep_q "host port 18144/tcp is in use by a service on the host" || fail "the host service on IPv6 was not explained: $out"
  kill $v6_service 2>/dev/null; wait $v6_service 2>/dev/null
  # Created and not running, as docker leaves the container of a refused run.
  $NSPAWN ps -a --format '{{.Names}} {{.State}}' | grep_q -x 'e2e-v6c created' || fail "a refused run did not leave its machine created: $($NSPAWN ps -a --format '{{.Names}} {{.State}}' | grep e2e-v6c)"
  # UDP, the replies from the address written to; a port below 1024.
  $NSPAWN run -d --name e2e-v6u -p 18143:5353/udp $rv -- udpsvd -E 0 5353 cat >/dev/null || fail "run a UDP echo"
  retry 10 python3 -c 'import socket
s = socket.socket(socket.AF_INET6, socket.SOCK_DGRAM); s.settimeout(2)
s.sendto(b"udp-over-v6", ("::1", 18143)); data, peer = s.recvfrom(100)
assert data == b"udp-over-v6" and peer[0] == "::1", (data, peer)' || fail "a UDP port does not answer on [::1]"
  $NSPAWN run -d --name e2e-v6low -p 999:80 $rv -- /bin/sh -c "$web6" low-$nonce >/dev/null || fail "run with a port below 1024"
  retry 10 bash -c "curl -g -sf -m 3 'http://[::1]:999/' | grep_q low-$nonce" || fail "a port below 1024 does not answer on [::1]"
  # A restart, by the policy and by hand, brings the IPv6 side back.
  $NSPAWN restart e2e-v6 >/dev/null || fail "restart e2e-v6"
  retry 10 bash -c "curl -g -sf -m 3 'http://[::1]:18140/' | grep_q v6-$nonce" || fail "the IPv6 side is not back after restart"
  $NSPAWN rm -f e2e-v6low >/dev/null
  $NSPAWN run -d --name e2e-v6low --restart on-failure -p 999:80 $rv -- /bin/sh -c "$web6" low-$nonce >/dev/null || fail "run with a restart policy"
  retry 10 bash -c "curl -g -sf -m 3 'http://[::1]:999/' | grep_q low-$nonce" || fail "the restart-policy machine does not answer on IPv6"
  leader=$(machinectl show e2e-v6low -p Leader --value)
  kill -KILL $(pgrep -P "$leader") || fail "cannot kill the program of e2e-v6low"
  retry 15 bash -c "[ \"\$(systemctl show -p NRestarts --value systemd-nspawn@e2e-v6low.service)\" -ge 1 ]" || fail "on-failure did not restart e2e-v6low"
  retry 15 bash -c "curl -g -sf -m 3 'http://[::1]:999/' | grep_q low-$nonce" || fail "the IPv6 side is not back after a restart of the policy"
  # Leaving the primary network moves the IPv6 side with the IPv4 one.
  $NSPAWN network create e2e-v6net >/dev/null || fail "network create e2e-v6net"
  $NSPAWN run -d --name e2e-v6d --network bridge --network e2e-v6net -p 18145:80 $rv -- /bin/sh -c "$web6" moved-$nonce >/dev/null || fail "run on two networks"
  retry 10 bash -c "curl -g -sf -m 3 'http://[::1]:18145/' | grep_q moved-$nonce" || fail "e2e-v6d does not answer on IPv6"
  $NSPAWN network disconnect bridge e2e-v6d >/dev/null || fail "leave the primary network"
  retry 10 bash -c "curl -g -sf -m 3 'http://[::1]:18145/' | grep_q moved-$nonce" || fail "the IPv6 side does not follow the new primary network"
  get6 http://127.0.0.1:18145/ | grep_q moved-$nonce || fail "the IPv4 side does not follow the new primary network"
  # Stop and rm leave nothing listening and no proxy.
  $NSPAWN stop e2e-v6 >/dev/null || fail "stop e2e-v6"
  get6 'http://[::1]:18140/' >/dev/null && fail "the IPv6 side outlived the machine"
  retry 10 bash -c "! systemctl is-active -q nspawn-proxy-e2e-v6.service" || fail "the port proxy outlived its machine"
  for m in e2e-v6 e2e-v6b e2e-v6c e2e-v6u e2e-v6low e2e-v6d; do $NSPAWN rm -f $m >/dev/null 2>&1; done
  $NSPAWN network rm e2e-v6net >/dev/null 2>&1
  python3 -c 'import socket
for p in (18140, 18142, 18145, 999):
    s = socket.socket(socket.AF_INET6); s.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 1)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1); s.bind(("::", p)); s.listen(); s.close()' || fail "a port of the step is still listened on over IPv6 after rm"
  systemctl list-units --all --no-legend 'nspawn-proxy-e2e-v6*' | grep_q . && fail "a port proxy unit of the step is left: $(systemctl list-units --all --no-legend 'nspawn-proxy-e2e-v6*')"
fi

step "compose config: the binary prints what docker compose printed for the cases"
python3 "$(dirname "$0")/compose/binary.py" "$NSPAWN" || fail "nspawn compose config differs from docker compose on the cases"

if [ "$(getenforce 2>/dev/null)" = Enforcing ] && [ -r "$audit_dir/audit.log" ]; then
  # The current log and those it rotated into, from the start of the run on: the
  # kernel's denials (AVC) and those of systemd and the bus (USER_AVC), which say the
  # same things in their own order.
  denials=$(cat "$audit_dir"/audit.log* | python3 -c '
import collections, re, sys
since = float(sys.argv[1])
seen = collections.Counter()
for line in sys.stdin:
    if not re.match(r"type=(USER_)?AVC ", line) or "denied" not in line:
        continue
    at = re.search(r"audit\((\d+(\.\d+)?):", line)
    if not at or float(at.group(1)) < since:
        continue
    if not re.search(r"scontext=system_u:system_r:nspawn_t:|tcontext=\S*:nspawn_[a-z_]*_t:", line):
        continue
    field = lambda key: key + "=" + (re.search(key + r"=(\S+)", line) or [None, "?"])[1]
    perms = re.search(r"denied\s+\{([^}]*)\}", line)
    comm = re.search(r"comm=\"([^\"]*)\"", line)
    seen[" ".join([
        "{ %s }" % (perms.group(1).strip() if perms else "?"),
        "comm=%s" % (comm.group(1) if comm else "?"),
        field("scontext"), field("tcontext"), field("tclass"),
    ])] += 1
for what, count in sorted(seen.items()):
    print(count, what)
' "$audit_start")
  [ -z "$denials" ] || fail "SELinux denied the service:
$denials"
fi

echo
if [ "$failures" = 0 ]; then echo "ALL OK"; else echo "$failures FAILURE(S)"; exit 1; fi
