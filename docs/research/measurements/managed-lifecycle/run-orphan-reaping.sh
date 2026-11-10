#!/usr/bin/env bash
set -euo pipefail
set -o noclobber

mode=${1:?usage: run-orphan-reaping.sh green|actor-only NEW_ABSOLUTE_PREFIX}
evidence=${2:?a fresh absolute evidence prefix is required}
: "${DOCKER_CONFIG:?provide an empty private test-only Docker configuration}"
case "$evidence" in /*) ;; *) exit 2 ;; esac
case "$mode" in green|actor-only) ;; *) exit 2 ;; esac
test -z "$(git status --porcelain)"
for suffix in log exit identity source.patch; do test ! -e "$evidence.$suffix"; done
mkdir -p "$(dirname "$evidence")"
trap 'git restore --source=HEAD -- crates/init/src/managed.rs' EXIT
if [ "$mode" = actor-only ]; then
    git apply docs/research/measurements/managed-lifecycle/reap-actor-only-mutation.patch
fi
git diff -- crates/init/src/managed.rs > "$evidence.source.patch"
{
    git rev-parse HEAD
    uname -a
    id
    rustc -Vv
    cargo -V
    printf 'mode=%s\nCARGO_TARGET_DIR=%s\n' "$mode" "${CARGO_TARGET_DIR:-target}"
} > "$evidence.identity"
set +e
cargo test --locked -j4 -p shards --profile test-release --test managed_lifecycle \
    repeated_orphan_crashes_are_reaped_while_a_mixed_neighbor_progresses \
    -- --exact --nocapture > "$evidence.log" 2>&1
status=$?
set -e
printf '%s\n' "$status" > "$evidence.exit"
tail -65 "$evidence.log"
exit "$status"
