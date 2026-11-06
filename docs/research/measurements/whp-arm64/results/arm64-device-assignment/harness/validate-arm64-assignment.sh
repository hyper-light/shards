#!/bin/bash
set -u
export PATH=/opt/homebrew/opt/llvm/bin:/Users/adalundhe/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin
cd /Users/adalundhe/Projects/shards/.claude/worktrees/g121-wa64 || exit 1
kit=/Users/adalundhe/g121-a4-winarm
cargo fmt -p shards-vmm > "$kit/arm64-assignment-fmt.log" 2>&1
status=$?
git diff HEAD -- crates/vmm/src/hv/whp_arm64 > "$kit/arm64-assignment-source.patch"
if [ "$status" = 0 ]; then
    SHARDS_HELPERS=skip cargo xwin clippy --locked -p shards-vmm --all-targets --target aarch64-pc-windows-msvc -j 2 -- -D warnings > "$kit/arm64-assignment-lint.log" 2>&1
    status=$?
fi
if [ "$status" = 0 ]; then
    RUSTFLAGS='-C target-feature=+crt-static' cargo xwin test --locked -p shards-vmm --lib --profile test-release --target aarch64-pc-windows-msvc --no-run --message-format=json -j 2 > "$kit/arm64-assignment-build.jsonl" 2> "$kit/arm64-assignment-build.log"
    status=$?
fi
git diff HEAD -- crates/vmm/src/hv/whp_arm64 > "$kit/arm64-assignment-source-final.patch"
if ! cmp -s "$kit/arm64-assignment-source.patch" "$kit/arm64-assignment-source-final.patch"; then status=2; fi
printf '%s\n' "$status" > "$kit/validate-arm64-assignment-status.txt"
exit "$status"
