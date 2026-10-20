#!/bin/sh
# Records what docker's own API answers and what the tools that read the docker socket
# ask of it, which nspawn's read-only API is checked against:
#
#   tests/dockerapi/oracle.sh record OUT_DIR
#
# It runs scenario.sh in a docker-in-docker container of the release the API version
# follows (docker is all it needs on the host), then copies golden/ and requests/ from
# OUT_DIR here. The container is limited to 2 GiB of memory.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
name=nspawn-dockerapi-oracle
image=docker:28.5.1-dind

record() {
    out=$(mkdir -p "$1" && cd "$1" && pwd)
    if ! docker inspect $name >/dev/null 2>&1; then
        docker run -d --privileged --name $name --memory 2g --memory-swap 2g \
            -v "$here:/scenario:ro" -v "$out:/out" $image >/dev/null
    else
        docker start $name >/dev/null
    fi
    docker exec -e OUT=/out $name sh /scenario/scenario.sh
    docker exec $name chown -R "$(id -u):$(id -g)" /out
    rm -rf "$here/golden" "$here/requests"
    cp -r "$out/golden" "$out/requests" "$here/"
    # What tells of the host the oracle ran on goes; the shapes are what the tests read.
    python3 -I "$here/sanitize.py" "$here/golden"
    cp "$out/images.txt" "$out/docker-version.txt" "$here/"
    docker stop $name >/dev/null
}

case "${1:-}" in
    record) record "${2:?an output directory}" ;;
    *) echo "usage: $0 record OUT_DIR" >&2; exit 2 ;;
esac
