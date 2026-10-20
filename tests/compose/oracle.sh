#!/bin/sh
# Records what docker compose itself does with the cases nspawn compose is checked
# against, the release src/compose/support names, built from its source in a Go
# container (docker is all it needs on the host):
#
#   tests/compose/oracle.sh build
#   tests/compose/oracle.sh record [CASES_DIR]      (default: tests/compose/cases)
#
# A case is a directory with compose files and a `runs` file: each line the arguments
# after `docker compose`, split on spaces with no quoting and no globbing, `config`
# when there is no such file. Each run writes
# expected/N.stdout, expected/N.stderr and expected/N.status. The case runs at
# /compose/NAME, with HOME=/home/user and the variables of its `env` file, if any, and
# its `stdin` file, if any, on standard input.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
oracle="$root/target/compose-oracle"
tag=$(sed -n 's/^pub const DOCKER_COMPOSE: &str = "\(.*\)";/\1/p' "$root/src/compose/support/mod.rs")

build() {
    mkdir -p "$oracle/src" "$oracle/gomod" "$oracle/gocache"
    if [ ! -d "$oracle/src/.git" ]; then
        git -C "$oracle/src" init -q
    fi
    git -C "$oracle/src" fetch -q --depth 1 https://github.com/docker/compose "refs/tags/$tag:refs/tags/$tag"
    git -C "$oracle/src" checkout -q "$tag"
    go=$(awk '$1 == "go" { print $2; exit }' "$oracle/src/go.mod")
    docker run --rm --user "$(id -u):$(id -g)" \
        -v "$oracle/src:/src:ro" -v "$oracle/gomod:/gomod" -v "$oracle/gocache:/gocache" \
        -v "$oracle:/out" -w /src -e HOME=/tmp -e GOMODCACHE=/gomod -e GOCACHE=/gocache \
        -e GOFLAGS=-mod=mod -e GOTOOLCHAIN=auto -e CGO_ENABLED=0 \
        "golang:${go%.*}" go build -trimpath -o /out/docker-compose ./cmd
    echo "built docker compose $tag in $oracle/docker-compose"
}

record() {
    cases=$(cd "${1:-$here/cases}" && pwd)
    [ -x "$oracle/docker-compose" ] || { echo "run $0 build first" >&2; exit 1; }
    for dir in "$cases"/*/; do
        dir=${dir%/}
        name=$(basename "$dir")
        rm -rf "$dir/expected"
        mkdir -p "$dir/expected"
        if [ -f "$dir/runs" ]; then runs=$(cat "$dir/runs"); else runs=config; fi
        n=0
        echo "$runs" | while IFS= read -r args; do
            # The arguments split on spaces, never globbed.
            set -f
            [ -n "$args" ] || continue
            n=$((n + 1))
            env_args=""
            if [ -f "$dir/env" ]; then
                env_args=$(sed -e '/^#/d' -e '/^$/d' "$dir/env" | tr '\n' ' ')
            fi
            status=0
            stdin=/dev/null
            [ ! -f "$dir/stdin" ] || stdin="$dir/stdin"
            # docker compose has no limit on what aliases expand to: a file that
            # expands without end takes the memory it gets, and no more than this.
            # shellcheck disable=SC2086
            docker run --rm -i --network none --memory 2g --memory-swap 2g \
                --user "$(id -u):$(id -g)" \
                -v "$oracle/docker-compose:/usr/local/bin/docker-compose:ro" \
                -v "$dir:/compose/$name:ro" -w "/compose/$name" \
                debian:stable-slim env -i HOME=/home/user PATH=/usr/local/bin:/usr/bin:/bin $env_args \
                docker-compose $args <"$stdin" >"$dir/expected/$n.stdout" 2>"$dir/expected/$n.stderr" || status=$?
            echo "$status" >"$dir/expected/$n.status"
        done
        echo "$name"
    done
}

case "${1:-}" in
    build) build ;;
    record) shift; record "$@" ;;
    *) echo "usage: $0 build | record [CASES_DIR]" >&2; exit 2 ;;
esac
