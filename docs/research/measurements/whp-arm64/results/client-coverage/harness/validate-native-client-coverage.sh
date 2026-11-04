#!/bin/bash
set -u
export PATH=/opt/homebrew/opt/llvm/bin:/Users/adalundhe/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin
export CARGO_BUILD_JOBS=2
cd /Users/adalundhe/Projects/shards/.claude/worktrees/g121-wa64 || exit 1
kit=/Users/adalundhe/g121-a4-winarm
cargo fmt --all > "$kit/client-coverage-fmt.log" 2>&1
status=$?
git diff HEAD > "$kit/client-coverage-source.patch"
if [ "$status" = 0 ]; then
    scripts/lint > "$kit/client-coverage-lint.log" 2>&1
    status=$?
fi
if [ "$status" = 0 ]; then
    unset SHARDS_HELPERS
    export CC_aarch64_unknown_linux_musl="$PWD/scripts/zig-cc"
    export AR_aarch64_unknown_linux_musl="$PWD/scripts/zig-ar"
    export ZIG_TARGET=aarch64-linux-musl
    cargo build --locked -p shards-testguest --profile guest --target aarch64-unknown-linux-musl -j 2 > "$kit/client-coverage-guest-build.log" 2>&1
    status=$?
fi
if [ "$status" = 0 ]; then
    export RUSTFLAGS='-C target-feature=+crt-static'
    cargo xwin test --locked --workspace --profile test-release --target aarch64-pc-windows-msvc --no-run --message-format=json -j 2 > "$kit/build-client-coverage-full.jsonl" 2> "$kit/build-client-coverage-full.log"
    status=$?
fi
git diff HEAD > "$kit/client-coverage-source-final.patch"
if ! cmp -s "$kit/client-coverage-source.patch" "$kit/client-coverage-source-final.patch"; then status=2; fi
printf '%s\n' "$status" > "$kit/validate-native-client-coverage-status.txt"
exit "$status"
