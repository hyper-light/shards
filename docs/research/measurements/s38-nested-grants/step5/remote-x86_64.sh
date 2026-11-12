#!/bin/bash
# Step 5's light compile gate on adacachy (Linux x86_64): the files arrive as a tar on stdin.
set -u
D=~/s38-coord-step5
rm -rf "$D"; mkdir -p "$D"; tar -x -m -C "$D" 2>/dev/null
cd "$D" || exit 1
export CARGO_TARGET_DIR=~/s38-coord-step5-target SHARDS_HELPERS=skip
scope="systemd-run --user --scope -q -p MemoryMax=16G -p CPUQuota=400%"
echo "host $(uname -srm) rustc $(rustc -V | cut -d' ' -f2) at $(date -u +%FT%TZ)"
$scope cargo test -p shards --test nesting --no-run -j4 > test-no-run.log 2>&1; echo "nesting_no_run_exit=$?"; grep -E '^(error|warning)|Executable' test-no-run.log | head -20
$scope cargo clippy -p shards --test nesting -j4 -- -D warnings > clippy-nesting.log 2>&1; echo "clippy_nesting_exit=$?"; grep -E '^(error|warning)' -A6 clippy-nesting.log | head -60
export CC_x86_64_unknown_linux_musl=$D/scripts/zig-cc AR_x86_64_unknown_linux_musl=$D/scripts/zig-ar ZIG_TARGET=x86_64-linux-musl
$scope cargo clippy -p shards-testguest --target x86_64-unknown-linux-musl --all-targets -j4 -- -D warnings > clippy-testguest.log 2>&1; echo "clippy_testguest_x86_64_musl_exit=$?"; grep -E '^(error|warning)' -A6 clippy-testguest.log | head -60
$scope cargo build -p shards-testguest --profile guest --target x86_64-unknown-linux-musl -j4 > build-testguest.log 2>&1; echo "build_testguest_x86_64_musl_exit=$?"; tail -2 build-testguest.log
mkdir -p ~/s38-coord-step5-logs && cp *.log ~/s38-coord-step5-logs/
