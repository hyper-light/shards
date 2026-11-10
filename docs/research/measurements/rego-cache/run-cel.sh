#!/usr/bin/env bash
set -eu
: "${GOCACHE:?private experiment cache}"
: "${GOMODCACHE:?private experiment module cache}"
: "${SHARDS_REGO_CEL_DIR:?new private CEL source}"
record=${1:?evidence directory}
mkdir -p "$record"
record=$(cd "$record" && pwd)
export GOTOOLCHAIN=go1.26.1 GOWORK=off GOMAXPROCS=4 GOFLAGS=-p=4
go mod download -json github.com/google/cel-go@v0.27.0 > "$record/module.json"
module=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["Dir"])' "$record/module.json")
if [ ! -d "$SHARDS_REGO_CEL_DIR" ]; then
 cp -a "$module" "$SHARDS_REGO_CEL_DIR"
 chmod -R u+w "$SHARDS_REGO_CEL_DIR"
fi
cp docs/research/measurements/rego-cache/cel_probe_test.go "$SHARDS_REGO_CEL_DIR/cel/shards_cache_probe_test.go"
sha256sum docs/research/measurements/rego-cache/cel_probe_test.go "$SHARDS_REGO_CEL_DIR"/{cel/options.go,interpreter/optimizations.go,common/types/string.go} > "$record/source.sha256"
(cd "$SHARDS_REGO_CEL_DIR" && go test -mod=readonly -c -o "$record/cel-probe.test" ./cel) > "$record/build.log" 2>&1
sha256sum "$record/cel-probe.test" > "$record/binary.sha256"
/usr/bin/time -v -o "$record/probe.resources" "$record/cel-probe.test" -test.run '^TestShardsCompilationOwnership$' -test.v > "$record/probe.log" 2>&1
printf '0\n' > "$record/probe.exit"
(cd "$SHARDS_REGO_CEL_DIR" && go list -mod=readonly -m -json all) > "$record/modules.jsonl"
printf 'CEL_OWNERSHIP_PROBE_COMPLETE\n'
