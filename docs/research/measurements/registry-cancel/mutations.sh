#!/usr/bin/env bash
set -eu
record=${1:?evidence directory}
mkdir -p "$record"
record=$(cd "$record" && pwd)
source=crates/registry/src/http.rs
cp "$source" "$record/http.rs.before"
trap 'cp "$record/http.rs.before" "$source"' EXIT
for mutation in completion cancel; do
    cp "$record/http.rs.before" "$source"
    python3 docs/research/measurements/registry-cancel/mutations.py "$mutation"
    git diff -- "$source" > "$record/$mutation.patch"
    action=cancel
    if [ "$mutation" = completion ]; then action=complete; fi
    set +e
    SHARDS_CANCEL_ACTION="$action" SHARDS_CANCEL_SAMPLES=1 bash docs/research/measurements/registry-cancel/run.sh "$record/$mutation"
    result=$?
    set -e
    printf '%s\n' "$result" > "$record/$mutation.exit"
    [ "$result" -eq 101 ]
done
cp "$record/http.rs.before" "$source"
SHARDS_CANCEL_ACTION=complete bash docs/research/measurements/registry-cancel/run.sh "$record/restored-completion"
bash docs/research/measurements/registry-cancel/run.sh "$record/restored-cancel"
