#!/usr/bin/env bash
# Run only in an isolated checkout admitted by the coordinator/runner guard.
set -euo pipefail
set -o noclobber

mode=${1:?usage: run-tenant-observation.sh green|fatal EVIDENCE_PREFIX}
evidence=${2:?a new evidence prefix is required}
: "${DOCKER_CONFIG:?set a private test-only Docker configuration directory}"
case "$mode" in
    green|fatal) ;;
    *) printf 'unknown mode: %s\n' "$mode" >&2; exit 2 ;;
esac
test -z "$(git status --porcelain)"
for suffix in log exit identity source.patch; do
    test ! -e "$evidence.$suffix"
done
mkdir -p "$(dirname "$evidence")"

restore_source() {
    git restore -- crates/init/src/managed.rs crates/init/src/domains.rs crates/shards/src/vm_run.rs
}
trap restore_source EXIT
campaign=docs/research/measurements/managed-lifecycle
git apply "$campaign/tenant-events-directory.patch"
git apply "$campaign/tenant-console-observation.patch"
if [ "$mode" = fatal ]; then
    git apply "$campaign/tenant-observation-fatal-mutation.patch"
fi
git diff --binary -- crates/init/src/managed.rs crates/init/src/domains.rs crates/shards/src/vm_run.rs > "$evidence.source.patch"
{
    git rev-parse HEAD
    uname -a
    rustc -Vv
    cargo -V
    id
    printf 'mode=%s\n' "$mode"
    printf 'CARGO_TARGET_DIR=%s\n' "${CARGO_TARGET_DIR:-target}"
    git diff --stat
} > "$evidence.identity"

# Four build jobs match this lane's admitted CPU envelope; this is no product limit.
set +e
cargo test -j4 -p shards --profile test-release --test managed_lifecycle \
    a_tenant_observation_fault_leaves_a_mixed_sibling_and_workload_alive \
    -- --ignored --exact --nocapture > "$evidence.log" 2>&1
result=$?
set -e
printf '%s\n' "$result" > "$evidence.exit"
tail -70 "$evidence.log"
exit "$result"
