#!/bin/sh
# Copies here, from a release of docker compose, the two things src/compose/support
# decides on: the schema of the Compose Specification that release validates files
# with (from the compose-go it is built with) and the reference of its commands, as
# its command line itself writes it.
#
#   src/compose/upstream/fetch.sh v5.6.0
#
# The tests of src/compose/support then name every attribute, command and option the
# table does not decide on, or decides on but the release no longer has.
set -eu

tag=${1:?usage: fetch.sh vX.Y.Z, a release of docker compose}
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# repository ref: a shallow clone of one tag, in $work/<name>
clone() {
    dir="$work/${1#*/}"
    git init -q "$dir"
    git -C "$dir" fetch -q --depth 1 "https://github.com/$1" "refs/tags/$2:refs/tags/$2"
    git -C "$dir" checkout -q "$2"
    echo "$1 $2 $(git -C "$dir" rev-parse HEAD)"
}

compose=$(clone docker/compose "$tag")
go=$(awk '$1 == "github.com/compose-spec/compose-go/v2" { print $2 }' "$work/compose/go.mod")
[ -n "$go" ] || { echo "no compose-go in the go.mod of docker compose $tag" >&2; exit 1; }
compose_go=$(clone compose-spec/compose-go "$go")

# The attribute paths compose-go itself lists (schema.AttributePaths), which the tests
# check their own walk of the schema against: run in a Go container, as docker is what
# the website of nspawn needs already.
mkdir "$work/paths"
cat > "$work/paths/main.go" <<'GO'
package main

import (
	"fmt"

	"github.com/compose-spec/compose-go/v2/schema"
)

func main() {
	for _, path := range schema.AttributePaths() {
		fmt.Println(path)
	}
}
GO
cat > "$work/paths/go.mod" <<GO
module paths

go 1.24

require github.com/compose-spec/compose-go/v2 $go

replace github.com/compose-spec/compose-go/v2 => ../compose-go
GO
docker run --rm --user "$(id -u):$(id -g)" -v "$work:/work" -w /work/paths \
    -e HOME=/tmp -e GOFLAGS=-mod=mod -e GOTOOLCHAIN=auto \
    golang:1.25 go run . > "$work/attribute-paths.txt"

# The command reference as the release's own cobra tree makes it: the pages under
# docs/reference are made the same way but kept by hand, and some outlive their
# command.
rm -f "$work/compose/docs/reference"/*.yaml
gover=$(awk '$1 == "go" { print $2; exit }' "$work/compose/go.mod")
docker run --rm --user "$(id -u):$(id -g)" -v "$work:/work" -w /work/compose \
    -e HOME=/tmp -e GOFLAGS=-mod=mod -e GOTOOLCHAIN=auto \
    "golang:${gover%.*}" go run ./docs/yaml/main >/dev/null

rm -rf "$here/compose" "$here/compose-go"
mkdir -p "$here/compose/reference" "$here/compose-go"
cp "$work/compose/LICENSE" "$work/compose/NOTICE" "$here/compose/"
cp "$work/compose/docs/reference"/docker_compose*.yaml "$here/compose/reference/"
cp "$work/compose-go/LICENSE" "$work/compose-go/NOTICE" "$work/compose-go/schema/compose-spec.json" \
    "$work/attribute-paths.txt" "$here/compose-go/"
printf '%s\n%s\n' "$compose" "$compose_go" > "$here/SOURCE"
cat "$here/SOURCE"
