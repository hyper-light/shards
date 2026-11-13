#!/bin/bash
# Goal 71's CI cluster 9, light checks only (no VM): engine_ring's non-VM tests at the
# commit given, green; then each red patch applied (expected to fail), the tree restored
# after each; shards-api's pause test; clippy for the two crates touched (review.md "Cures,
# goal 71", logs/r23). Run from a directory holding it, this directory's cure-g71-red-*.patch
# and ring.bundle, a git bundle of the commits over origin/main (branch p/g71-ring-v1):
#   bash cure-g71-light.sh <commit>
set -u
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"
here=$(cd "$(dirname "$0")" && pwd)
sha=$1
work=$HOME/g121-tmp/g71-ring
mkdir -p "$work/logs" "$work/ph"
: > "$work/ph/init"
: > "$work/ph/server"
if [ ! -d "$work/src/.git" ]; then
	git clone -q --filter=blob:none --no-checkout https://github.com/hyper-light/shards "$work/src" || exit 10
fi
cd "$work/src" || exit 11
git fetch -q --filter=blob:none origin main || exit 14
git fetch -q "$here/ring.bundle" "p/g71-ring-v1" || exit 12
git reset -q --hard && git checkout -q --detach "$sha" || exit 13
echo "== host: $(hostname) $(sw_vers -productVersion) $(uname -m), $(sysctl -n hw.ncpu) CPUs; $(rustc -V); at $(git log --oneline -1 | cut -c1-90)"
export SHARDS_HELPERS=skip SHARDS_INIT_BINARY="$work/ph/init" SHARDS_SERVER_BINARY="$work/ph/server"
export CARGO_TARGET_DIR="$work/target" CARGO_INCREMENTAL=0
tests="a_settle_of the_held_connections_wait"
run() {
	local name=$1
	shift
	echo "== $name: cargo $* ($(date -u +%H:%M:%SZ))"
	nice -n 10 cargo "$@" >"$work/logs/$name.txt" 2>&1
	local rc=$?
	echo "exit $rc ($(date -u +%H:%M:%SZ))"
	grep -a -E '^(test |test result|running [0-9])|^error|^warning: |panicked at|^(thread|  left|  right)|Stalled|refused|^---- ' "$work/logs/$name.txt" | grep -v -E '^warning: (aws-lc-sys|shards@)' | tail -40
	return $rc
}
run green test -p shards --profile test-release --test engine_ring -- $tests
for patch in red-main-window red-sizes-shared red-held-attached-still; do
	git apply "$here/cure-g71-$patch.patch" || { echo "$patch: did not apply"; continue; }
	run "$patch" test -p shards --profile test-release --test engine_ring -- $tests
	git checkout -q -- crates/shards/tests/engine_ring.rs
done
git status --short
run api-pause test -p shards-api --profile test-release --lib a_pause_weighs
run clippy clippy -p shards-api -p shards --all-targets -- -D warnings
echo "== done $(date -u +%H:%M:%SZ)"
