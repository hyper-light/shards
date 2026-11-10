#!/usr/bin/env bash
set -eu
: "${CARGO_TARGET_DIR:?private experiment target}"
: "${GOCACHE:?private experiment Go build cache}"
: "${GOMODCACHE:?private experiment Go module cache}"
: "${SHARDS_REGO_OPA_DIR:?private experiment OPA source}"
record=${1:?evidence directory}
mkdir -p "$record"
record=$(cd "$record" && pwd)
export GOTOOLCHAIN=go1.26.1 GOWORK=off GOMAXPROCS=4 GOFLAGS=-p=4
export SHARDS_REGO_EXPERIMENT_INPUT="$record/inputs.json"
python3 docs/research/measurements/rego-cache/make-workloads.py "$SHARDS_REGO_EXPERIMENT_INPUT"
git rev-parse HEAD > "$record/source.txt"
sha256sum crates/rego/src/{eval.rs,funcs/mod.rs,funcs/regex.rs} docs/research/measurements/rego-cache/{experiment.rs,cache_perf_test.go,make-workloads.py} > "$record/source.sha256"
uname -a > "$record/environment.txt"
rustc --version >> "$record/environment.txt"
go version >> "$record/environment.txt"
printf 'UNPROFILED=%s\n' "${SHARDS_REGO_UNPROFILED:-0}" >> "$record/environment.txt"
date -u >> "$record/environment.txt"
cat /proc/loadavg >> "$record/environment.txt"
lscpu >> "$record/environment.txt"
cp docs/research/measurements/rego-cache/cache_perf_test.go "$SHARDS_REGO_OPA_DIR/v1/topdown/shards_cache_perf_test.go"
(cd "$SHARDS_REGO_OPA_DIR" && go test -mod=readonly -c -o "$record/opa-perf.test" ./v1/topdown) > "$record/opa-build.log" 2>&1
touch crates/rego/src/eval.rs crates/rego/src/funcs/mod.rs crates/rego/src/funcs/regex.rs docs/research/measurements/rego-cache/experiment.rs
cargo clean -p shards-rego --profile test-release > "$record/rust-clean.log" 2>&1
cargo test -j 4 --profile test-release -p shards-rego --lib rego_cache_experiment --no-run --message-format=json > "$record/rust-build.jsonl" 2> "$record/rust-build.log"
executable=$(python3 - "$record/rust-build.jsonl" <<'PY'
import json,sys
for line in open(sys.argv[1]):
    event=json.loads(line)
    if event.get('reason')=='compiler-artifact' and event.get('executable') and event.get('profile',{}).get('test'):
        assert event.get('fresh') is False, 'test artifact was reused instead of rebuilt'
        print(event['executable'])
PY
)
cp "$executable" "$record/shards-perf.test"
executable="$record/shards-perf.test"
sha256sum "$executable" "$record/opa-perf.test" > "$record/binaries.sha256"
for workload in constant_hot dynamic_regex dynamic_template dynamic_delimiter failed_compilation; do
    export SHARDS_REGO_EXPERIMENT_WORKLOAD="$workload"
    /usr/bin/time -v -o "$record/opa-$workload.resources" "$record/opa-perf.test" -test.run '^TestShardsCachePerformance$' -test.v > "$record/opa-$workload.log" 2>&1
    printf '0\n' > "$record/opa-$workload.exit"
    for mode in current uncached program; do
        SHARDS_REGO_EXPERIMENT_MODE="$mode" /usr/bin/time -v -o "$record/$mode-$workload.resources" "$executable" --exact eval::regex_cache_experiment::rego_cache_experiment --ignored --nocapture --test-threads=1 > "$record/$mode-$workload.log" 2>&1
        printf '0\n' > "$record/$mode-$workload.exit"
    done
done
unset SHARDS_REGO_EXPERIMENT_WORKLOAD
for scenario in ordinary_template template_ordinary angle_square square_angle; do
    SHARDS_REGO_EXPERIMENT_MIXED="$scenario" "$record/opa-perf.test" -test.run '^TestShardsCacheMixedPolicy$' -test.v > "$record/opa-mixed-$scenario.log" 2>&1
    printf '0\n' > "$record/opa-mixed-$scenario.exit"
done
SHARDS_REGO_EXPERIMENT_MODE=program SHARDS_REGO_EXPERIMENT_WORKLOAD=constant_hot SHARDS_REGO_EXPERIMENT_CHECKS=1 "$executable" --exact eval::regex_cache_experiment::rego_cache_experiment --ignored --nocapture --test-threads=1 > "$record/shards-checks.log" 2>&1
printf '0\n' > "$record/shards-checks.exit"
"$record/opa-perf.test" -test.run '^TestShardsCacheErrorOrder$' -test.v > "$record/opa-errors.log" 2>&1
printf '0\n' > "$record/opa-errors.exit"
cat /proc/loadavg >> "$record/environment.txt"
printf 'EXPERIMENT_CASES_COMPLETED\n'
