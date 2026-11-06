#!/bin/sh
# Records what docker's own API answers and what the tools that read the docker socket
# ask of it, which nspawn's read-only API is checked against:
#
#   tests/dockerapi/oracle.sh record OUT_DIR
#
# It runs scenario.sh in a docker-in-docker machine of the release the API version
# follows (nspawn is all it needs on the host), then copies golden/ and requests/ from
# OUT_DIR here. The machine is limited to 2 GiB of memory, swap included.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
name=nspawn-dockerapi-oracle
image=docker.io/library/docker:28.5.1-dind

# The service lets administrators manage machines; the files of the record belong to
# whoever runs this, so anyone but root asks through sudo.
nspawn() {
    if [ "$(id -u)" = 0 ]; then command nspawn "$@"; else sudo nspawn "$@"; fi
}

record() {
    out=$(mkdir -p "$1" && cd "$1" && pwd)
    # docker cannot keep its layers on the overlay that is the machine's root, so
    # /var/lib/docker is a named volume, the machine's own.
    binds="$here:/scenario:ro $out:/out $name:/var/lib/docker"
    # A machine of an earlier record writes where that one asked, and runs the scenario
    # of the tree it was made for: it goes, and its volume with it.
    mounts=$(nspawn inspect $name -f '{{join .volumes " "}}' 2>/dev/null || true)
    if [ -n "$mounts" ] && [ "$mounts" != "$binds" ]; then
        nspawn rm -f $name >/dev/null
        nspawn volume rm $name >/dev/null
        mounts=
    fi
    if [ -z "$mounts" ]; then
        nspawn run -d --name $name --privileged --memory 2g --memory-swap 2g \
            -v "$here:/scenario:ro" -v "$out:/out" -v "$name:/var/lib/docker" $image >/dev/null
    else
        nspawn start $name >/dev/null
    fi
    trap 'nspawn stop $name >/dev/null 2>&1 || true' EXIT
    # dind starts docker only after moving every process of the machine's cgroup into a
    # child cgroup, and the process exec runs a command through sits in that cgroup,
    # outside the machine's PID namespace and out of dind's reach, until the command
    # ends: docker is waited for with commands that end at once, then the scenario runs.
    tries=0
    until nspawn exec -T $name docker info >/dev/null 2>&1; do
        tries=$((tries + 1))
        if [ $tries -ge 60 ]; then
            echo "docker did not start in $name: nspawn logs $name" >&2
            exit 1
        fi
        sleep 1
    done
    nspawn exec -T -e OUT=/out $name sh /scenario/scenario.sh
    nspawn exec -T $name chown -R "$(id -u):$(id -g)" /out
    # Made aside first: the recorded files are replaced only by a whole new record.
    new=$(mktemp -d "$here/.record.XXXXXX")
    cp -r "$out/golden" "$out/requests" "$new/"
    # What tells of the host the oracle ran on goes; the shapes are what the tests read.
    python3 -I "$here/sanitize.py" "$new/golden"
    rm -rf "$here/golden" "$here/requests"
    mv "$new/golden" "$new/requests" "$here/"
    rmdir "$new"
    cp "$out/images.txt" "$out/docker-version.txt" "$here/"
}

case "${1:-}" in
    record) record "${2:?an output directory}" ;;
    *) echo "usage: $0 record OUT_DIR" >&2; exit 2 ;;
esac
