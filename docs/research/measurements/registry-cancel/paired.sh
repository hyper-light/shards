#!/usr/bin/env bash
set -eu
record=${1:?evidence directory}
mkdir -p "$record"
record=$(cd "$record" && pwd)
source=crates/registry/src/http.rs
cp "$source" "$record/http.rs.before"
trap 'cp "$record/http.rs.before" "$source"' EXIT
python3 docs/research/measurements/registry-cancel/legacy.py
git diff -- "$source" > "$record/legacy.patch"
set +e
bash docs/research/measurements/registry-cancel/run.sh "$record/legacy"
result=$?
set -e
printf '%s\n' "$result" > "$record/legacy.exit"
[ "$result" -eq 101 ]
cp "$record/http.rs.before" "$source"
bash docs/research/measurements/registry-cancel/run.sh "$record/event"
