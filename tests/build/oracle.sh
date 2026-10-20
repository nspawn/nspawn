#!/bin/sh
# Records what the reference builder makes of the cases, which nspawn's builder is
# checked against on the test hosts:
#
#   tests/build/oracle.sh record OUT_DIR
#
# Each case under cases/ is built in a docker-in-docker container of the release the
# builder follows (docker is all it needs on the host), saved, and described by
# describe.py into golden/CASE.json: the image's settings, its history and every entry
# of every layer. A case with an expect-error file must fail; the last error line is
# kept in golden/CASE.error. The container is limited to 2 GiB of memory.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
name=nspawn-build-oracle
image=docker:28.5.1-dind

record() {
    out=$(mkdir -p "$1" && cd "$1" && pwd)
    # A container of an earlier record writes where that one asked, and builds the cases
    # of the tree it was made for. Its binds alone: the image's /var/lib/docker is a
    # volume of its own.
    mounts=$(docker inspect $name --format '{{range .Mounts}}{{if or (eq .Destination "/build") (eq .Destination "/out")}}{{.Destination}}={{.Source}} {{end}}{{end}}' 2>/dev/null || true)
    if [ -n "$mounts" ] && [ "$mounts" != "/build=$here /out=$out " ] && [ "$mounts" != "/out=$out /build=$here " ]; then
        docker rm -f -v $name >/dev/null
        mounts=
    fi
    if [ -z "$mounts" ]; then
        docker run -d --privileged --name $name --memory 2g --memory-swap 2g \
            -v "$here:/build:ro" -v "$out:/out" $image >/dev/null
    else
        docker start $name >/dev/null
    fi
    trap 'docker stop $name >/dev/null 2>&1 || true' EXIT
    docker exec -e OUT=/out $name sh /build/scenario.sh
    docker exec $name chown -R "$(id -u):$(id -g)" /out
    rm -rf "$here/golden"
    mkdir -p "$here/golden"
    python3 -I "$here/describe.py" bases "$out/bases.tar" > "$here/golden/bases.json"
    for tar in "$out"/cases/*.tar; do
        case_name=$(basename "$tar" .tar)
        python3 -I "$here/describe.py" image "$tar" --bases "$here/golden/bases.json" \
            > "$here/golden/$case_name.json"
    done
    for err in "$out"/cases/*.error; do
        [ -e "$err" ] && cp "$err" "$here/golden/"
    done
    cp "$out/docker-version.txt" "$here/"
}

case "${1:-}" in
    record) record "${2:?an output directory}" ;;
    *) echo "usage: $0 record OUT_DIR" >&2; exit 2 ;;
esac
