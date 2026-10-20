#!/bin/sh
# Runs inside the oracle container: builds every case, saves the images and the bases.
set -eu
for i in $(seq 1 60); do docker info >/dev/null 2>&1 && break; sleep 1; done
docker version --format '{{.Server.Version}}' > "$OUT/docker-version.txt"
rm -rf "$OUT/cases" && mkdir -p "$OUT/cases"
bases=""
for base in $(cat /build/bases.txt); do
    docker pull -q "$base" >/dev/null
    bases="$bases $base"
done
docker save -o "$OUT/bases.tar" $bases
for dir in /build/cases/*/; do
    case_name=$(basename "$dir")
    work=$(mktemp -d)
    cp -a "$dir/." "$work/"
    args=""
    [ -f "$work/args" ] && args=$(sed "s#@CASE@#$work#g" "$work/args" | tr '\n' ' ')
    if [ -f "$work/expect-error" ]; then
        if docker build --progress plain -t "case-$case_name" $args "$work" > "$work/.log" 2>&1; then
            echo "$case_name: the build was expected to fail" >&2
            exit 1
        fi
        grep '^ERROR: ' "$work/.log" | tail -1 > "$OUT/cases/$case_name.error"
    else
        docker build --progress plain -t "case-$case_name" $args "$work" > "$work/.log" 2>&1 \
            || { cat "$work/.log" >&2; exit 1; }
        docker save -o "$OUT/cases/$case_name.tar" "case-$case_name"
    fi
    rm -rf "$work"
done
