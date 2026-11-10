#!/usr/bin/env bash
set -eu
: "${CARGO_TARGET_DIR:?use the lane private Rust target}"
: "${GOCACHE:?use the lane private Go build cache}"
: "${GOMODCACHE:?use the lane private Go module cache}"
: "${SHARDS_REGO_OPA_DIR:?use the lane private OPA source copy}"
record=${1:?evidence directory}
mkdir -p "$record"
record=$(cd "$record" && pwd)
rego_root=$PWD
export GOTOOLCHAIN=go1.26.1 GOWORK=off GOMAXPROCS=4 GOFLAGS=-p=4
export SHARDS_REGO_CACHE_CASES="$rego_root/docs/research/measurements/rego-cache/cases.json"
git rev-parse HEAD > "$record/source.txt"
sha256sum crates/rego/src/funcs/regex.rs docs/research/measurements/rego-cache/{cases.json,cache_probe_test.go} > "$record/source.sha256"
uname -a > "$record/environment.txt"
rustc --version >> "$record/environment.txt"
go version >> "$record/environment.txt"
go mod download -json github.com/open-policy-agent/opa@v1.14.1 > "$record/opa-module.json"
module_source=$(python3 - "$record/opa-module.json" <<'PY'
import json,sys
print(json.load(open(sys.argv[1]))['Dir'])
PY
)
if [ ! -d "$SHARDS_REGO_OPA_DIR" ]; then cp -a "$module_source" "$SHARDS_REGO_OPA_DIR"; fi
cmp "$module_source/v1/topdown/regex.go" "$SHARDS_REGO_OPA_DIR/v1/topdown/regex.go"
cmp "$module_source/v1/topdown/regex_template.go" "$SHARDS_REGO_OPA_DIR/v1/topdown/regex_template.go"
chmod u+w "$SHARDS_REGO_OPA_DIR/v1/topdown"
cp docs/research/measurements/rego-cache/cache_probe_test.go "$SHARDS_REGO_OPA_DIR/v1/topdown/shards_cache_probe_test.go"
sha256sum "$SHARDS_REGO_OPA_DIR/v1/topdown/regex.go" "$SHARDS_REGO_OPA_DIR/v1/topdown/regex_template.go" > "$record/opa-source.sha256"
(cd "$SHARDS_REGO_OPA_DIR" && go test -mod=readonly -c -o "$record/opa-probe.test" ./v1/topdown) > "$record/opa-build.log" 2>&1
cargo test -j 4 --profile test-release -p shards-rego --lib rego_cache_probe --no-run --message-format=json > "$record/rust-build.jsonl" 2> "$record/rust-build.log"
executable=$(python3 - "$record/rust-build.jsonl" <<'PY'
import json,sys
for line in open(sys.argv[1]):
    event=json.loads(line)
    if event.get('reason')=='compiler-artifact' and event.get('executable') and event.get('profile',{}).get('test'):
        print(event['executable'])
PY
)
/usr/bin/time -v -o "$record/opa.resources" "$record/opa-probe.test" -test.run '^TestShardsCacheProbe$' -test.v > "$record/opa.log" 2>&1
printf '0\n' > "$record/opa.exit"
/usr/bin/time -v -o "$record/shards.resources" "$executable" --exact funcs::regex::tests::rego_cache_probe --ignored --nocapture --test-threads=1 > "$record/shards.log" 2>&1
printf '0\n' > "$record/shards.exit"
python3 docs/research/measurements/rego-cache/compare.py "$record" > "$record/report.json"
set +e
SHARDS_REGO_CACHE_STRICT=1 "$record/opa-probe.test" -test.run '^TestShardsCacheProbe$' -test.v > "$record/opa-strict.log" 2>&1
opa_code=$?
SHARDS_REGO_CACHE_STRICT=1 "$executable" --exact funcs::regex::tests::rego_cache_probe --ignored --nocapture --test-threads=1 > "$record/shards-strict.log" 2>&1
shards_code=$?
set -e
printf '%s\n' "$opa_code" > "$record/opa-strict.exit"
printf '%s\n' "$shards_code" > "$record/shards-strict.exit"
[ "$opa_code" -eq 1 ] && [ "$shards_code" -eq 101 ]
cmp "$module_source/v1/topdown/regex.go" "$SHARDS_REGO_OPA_DIR/v1/topdown/regex.go"
cmp "$module_source/v1/topdown/regex_template.go" "$SHARDS_REGO_OPA_DIR/v1/topdown/regex_template.go"
cat "$record/report.json"
