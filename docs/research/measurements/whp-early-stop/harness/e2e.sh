#!/bin/bash
# engine_ring's E2E on jupiter, as ROOT's ring-pins r16 ran it (one test at a time), from
# ~/g121-whp-stop-src at its checkout: usage
#   bash e2e.sh TAG ROUNDS [TEST_BINARY_ARGS...]
# Each round's output kept in ~/g121-whp-stop-logs/e2e-TAG-R.log; the test binary built once.
set -u
tag=${1:?tag}; rounds=${2:?rounds}
shift 2
export PATH="$HOME/tools/zig-x86_64-windows-0.16.0:$HOME/tools/nasm-3.02:/c/Users/corph/.cargo/bin:/usr/bin:$PATH"
export CARGO_TARGET_DIR=$HOME/g121-whp-stop-target
export SHARDS_REQUIRE_VMS=1
# Every log of a failing test's directory shown whole (testdir's show_logs).
export SHARDS_TEST_LOG_SHOWN=${SHARDS_TEST_LOG_SHOWN:-100000000}
logs=$HOME/g121-whp-stop-logs
cd "$HOME/g121-whp-stop-src" || exit 1
git log -1 --format='HEAD %H' | tee "$logs/e2e-$tag-head.txt"
test -z "$(git status --porcelain)" || { echo "dirty tree"; git status --porcelain | head; }
began=$(date +%s)
cargo test --locked --profile test-release -p shards --test engine_ring --no-run > "$logs/e2e-$tag-build.log" 2>&1
echo "build exit $? in $(( $(date +%s) - began )) s"
for r in $(seq 1 "$rounds"); do
  t0=$(date +%s)
  cargo test --locked --profile test-release -p shards --test engine_ring -- --test-threads 1 --nocapture "$@" > "$logs/e2e-$tag-$r.log" 2>&1
  rc=$?
  echo "round $r exit $rc in $(( $(date +%s) - t0 )) s: $(grep -a -E '^test result' "$logs/e2e-$tag-$r.log" | tail -1)"
  grep -a -E '^test .* \.\.\. (FAILED|ok)|stopped before|status Some' "$logs/e2e-$tag-$r.log" | head -12
done
