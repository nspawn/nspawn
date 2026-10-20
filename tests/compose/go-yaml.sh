#!/bin/sh
# Records what go-yaml, the release docker compose's go.mod names, makes of inputs, for
# the ports of it in src/compose to be checked against:
#
#   tests/compose/oracle.sh build              (once: fetches the Go modules)
#
#   tests/compose/go-yaml.sh events DIR
#   NSPAWN_YAML_EVENTS=DIR cargo test parser_matches -- --ignored
#
# The events its parser makes of every file under DIR, with their marks and errors,
# into FILE.events, for src/compose/libyaml.rs.
#
#   tests/compose/go-yaml.sh emit DIR
#   NSPAWN_YAML_EMIT=DIR cargo test emitter_matches -- --ignored
#
# The YAML its Encoder (as compose-go sets it up) and yaml.Marshal write of every
# FILE.json under DIR, into FILE.json.yaml and FILE.json.yaml4, for
# src/compose/emitter.rs. It runs in a Go container, as oracle.sh does.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
oracle="$root/target/compose-oracle"
usage="usage: $0 events DIR | emit DIR"
mode=${1:?$usage}
dir=$(cd "${2:?$usage}" && pwd)
case "$mode" in
    events) test=TestEvents package=./internal/libyaml ;;
    emit) test=TestEmit package=. ;;
    *) echo "$usage" >&2; exit 2 ;;
esac

[ -f "$oracle/src/go.mod" ] || { echo "run $here/oracle.sh build first" >&2; exit 1; }
version=$(awk '$1 == "go.yaml.in/yaml/v4" { print $2; exit }' "$oracle/src/go.mod")
module="$oracle/gomod/go.yaml.in/yaml/v4@$version"
[ -d "$module" ] || { echo "go-yaml $version is not in $oracle/gomod" >&2; exit 1; }
go=$(awk '$1 == "go" { print $2; exit }' "$oracle/src/go.mod")

# The module cache is read-only: a copy takes the tests that write what is compared,
# the parser's from inside its package, to reach its unexported fields.
work="$oracle/go-yaml"
rm -rf "$work"
cp -r "$module" "$work"
chmod -R u+w "$work"

cat >"$work/internal/libyaml/zz_events_test.go" <<'GO'
package libyaml

import (
	"bytes"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestEvents(t *testing.T) {
	err := filepath.Walk(os.Getenv("INPUT_DIR"), func(path string, info os.FileInfo, err error) error {
		if err != nil || info.IsDir() || strings.HasSuffix(path, ".events") {
			return err
		}
		data, err := os.ReadFile(path)
		if err != nil {
			return err
		}
		return os.WriteFile(path+".events", []byte(events(data)), 0o644)
	})
	if err != nil {
		t.Fatal(err)
	}
}

func eventMark(m Mark) string {
	return fmt.Sprintf("%d:%d:%d", m.Line, m.Column, m.Index)
}

// The input read as compose-go reads a file, through an io.Reader.
func events(data []byte) string {
	p := NewParser()
	p.SetInputReader(bytes.NewReader(data))
	var out strings.Builder
	for {
		var e Event
		err := p.Parse(&e)
		if err == io.EOF {
			break
		}
		if err != nil {
			out.WriteString("ERR " + err.Error() + "\n")
			break
		}
		out.WriteString(eventMark(e.StartMark) + "-" + eventMark(e.EndMark) + " " + formatEvent(&e))
		if e.Type == SCALAR_EVENT {
			fmt.Fprintf(&out, " pi=%v qi=%v", e.Implicit, e.quoted_implicit)
		}
		out.WriteString("\n")
		if e.Type == STREAM_END_EVENT {
			break
		}
	}
	return out.String()
}
GO

cat >"$work/zz_emit_test.go" <<'GO'
package yaml_test

import (
	"bytes"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"go.yaml.in/yaml/v4"
)

func TestEmit(t *testing.T) {
	err := filepath.Walk(os.Getenv("INPUT_DIR"), func(path string, info os.FileInfo, err error) error {
		if err != nil || info.IsDir() || !strings.HasSuffix(path, ".json") {
			return err
		}
		data, err := os.ReadFile(path)
		if err != nil {
			return err
		}
		var v any
		if err := json.Unmarshal(data, &v); err != nil {
			return err
		}
		// As compose-go writes a project.
		var buf bytes.Buffer
		enc := yaml.NewEncoder(&buf)
		enc.SetIndent(2)
		if err := enc.Encode(v); err != nil {
			return err
		}
		if err := os.WriteFile(path+".yaml", buf.Bytes(), 0o644); err != nil {
			return err
		}
		out, err := yaml.Marshal(v)
		if err != nil {
			return err
		}
		return os.WriteFile(path+".yaml4", out, 0o644)
	})
	if err != nil {
		t.Fatal(err)
	}
}
GO

docker run --rm --network none --memory 2g --user "$(id -u):$(id -g)" \
    -v "$work:/src" -v "$oracle/gocache:/gocache" -v "$dir:/data" -w /src \
    -e HOME=/tmp -e GOCACHE=/gocache -e GOFLAGS=-mod=mod -e GOTOOLCHAIN=local \
    -e INPUT_DIR=/data \
    "golang:${go%.*}" go test -run "^$test\$" -count=1 "$package"
