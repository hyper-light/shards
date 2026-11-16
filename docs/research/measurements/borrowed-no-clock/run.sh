#!/usr/bin/env bash
# The no-clock borrowed connect's light runs (D233), from a clone at the commit under test:
# its tests and the borrowed ones, red (today's behavior through the new signature), green,
# each guard-removal mutant (each must fail), and clippy. Run from the repository's root.
set -u
export CARGO_INCREMENTAL=0 SHARDS_HELPERS=skip
here=docs/research/measurements/borrowed-no-clock
filter=client::borrowed_tests
tests() { cargo test -p shards-api --lib "$filter" -- --test-threads=1 2>&1 | grep -E '^test |test result|panicked|^error' ; return "${PIPESTATUS[0]}"; }
for m in red-today m1-unwatched m2-end-unchecked m3-start-unchecked; do
  git apply "$here/mutants/$m.patch" || { echo "APPLY FAILED $m"; exit 1; }
  touch crates/api/src/client.rs crates/api/src/taken.rs
  echo "=== $m (must fail)"; tests; echo "=== $m exit $?"
  git apply -R "$here/mutants/$m.patch"
  touch crates/api/src/client.rs crates/api/src/taken.rs
done
echo "=== green"; tests; echo "=== green exit $?"
echo "=== clippy"; cargo clippy -p shards-api --all-targets -- -D warnings 2>&1 | tail -15; echo "=== clippy exit ${PIPESTATUS[0]}"
