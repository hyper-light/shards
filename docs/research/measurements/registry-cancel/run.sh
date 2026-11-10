#!/usr/bin/env bash
set -eu
: "${CARGO_TARGET_DIR:?use a private target directory}"
record=${1:?evidence directory}
mkdir -p "$record"
record=$(cd "$record" && pwd)
cc -shared -fPIC -Wall -Wextra -Werror -o "$record/block.so" docs/research/measurements/registry-cancel/block.c -ldl
cargo test -j 4 --profile test-release -p shards-registry --lib registry_cancel_blocked_workers --no-run > "$record/build.log" 2>&1
set +e
for mode in resolve dial; do
    LD_PRELOAD="$record/block.so" SHARDS_CANCEL_MODE="$mode" SHARDS_CANCEL_SAMPLES="${SHARDS_CANCEL_SAMPLES:-20}" cargo test -j 4 --profile test-release -p shards-registry --lib registry_cancel_blocked_workers -- --exact http::tests::registry_cancel_blocked_workers --ignored --test-threads=1 --nocapture > "$record/$mode.log" 2>&1
    code=$?
    printf '%s\n' "$code" > "$record/$mode.exit"
    cat "$record/$mode.log"
done
