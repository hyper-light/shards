#!/usr/bin/env bash
set -eu
: "${CARGO_TARGET_DIR:?use a private target directory}"
record=${1:?evidence directory}
mkdir -p "$record"
record=$(cd "$record" && pwd)
git rev-parse HEAD > "$record/source.txt"
sha256sum crates/registry/src/http.rs block.c 2>/dev/null > "$record/source.sha256" || sha256sum crates/registry/src/http.rs docs/research/measurements/registry-cancel/block.c > "$record/source.sha256"
uname -a > "$record/environment.txt"
rustc --version >> "$record/environment.txt"
cc -shared -fPIC -Wall -Wextra -Werror -o "$record/block.so" docs/research/measurements/registry-cancel/block.c -ldl
cargo test -j 4 --profile test-release -p shards-registry --lib registry_cancel_blocked_workers --no-run --message-format=json > "$record/build.jsonl" 2> "$record/build.log"
executable=$(python3 - "$record/build.jsonl" <<'PY'
import json,sys
for line in open(sys.argv[1]):
    event=json.loads(line)
    if event.get('reason') == 'compiler-artifact' and event.get('executable') and event.get('profile',{}).get('test'):
        print(event['executable'])
PY
)
status=0
set +e
for mode in resolve dial; do
    /usr/bin/time -v -o "$record/$mode.resources" env LD_PRELOAD="$record/block.so" SHARDS_CANCEL_MODE="$mode" SHARDS_CANCEL_SAMPLES="${SHARDS_CANCEL_SAMPLES:-20}" "$executable" --exact http::tests::registry_cancel_blocked_workers --ignored --test-threads=1 --nocapture > "$record/$mode.log" 2>&1
    code=$?
    printf '%s\n' "$code" > "$record/$mode.exit"
    if [ "$code" -ne 0 ]; then status=$code; fi
    cat "$record/$mode.log"
done
exit "$status"
