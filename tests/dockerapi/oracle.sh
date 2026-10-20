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
    # A container of an earlier record writes where that one asked, and runs the
    # scenario of the tree it was made for. Its binds alone: the image's /var/lib/docker
    # is a volume of its own.
    mounts=$(docker inspect $name --format '{{range .Mounts}}{{if or (eq .Destination "/scenario") (eq .Destination "/out")}}{{.Destination}}={{.Source}} {{end}}{{end}}' 2>/dev/null || true)
    if [ -n "$mounts" ] && [ "$mounts" != "/out=$out /scenario=$here " ] && [ "$mounts" != "/scenario=$here /out=$out " ]; then
        docker rm -f -v $name >/dev/null
        mounts=
    fi
    if [ -z "$mounts" ]; then
        docker run -d --privileged --name $name --memory 2g --memory-swap 2g \
            -v "$here:/scenario:ro" -v "$out:/out" $image >/dev/null
    else
        docker start $name >/dev/null
    fi
    trap 'docker stop $name >/dev/null 2>&1 || true' EXIT
    docker exec -e OUT=/out $name sh /scenario/scenario.sh
    docker exec $name chown -R "$(id -u):$(id -g)" /out
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
