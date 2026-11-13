#!/bin/bash
# After lint.sh stopped at x86_64-pc-windows-msvc on another change's clippy error
# (crates/vmm/src/platform/windows_process.rs:544, a test's unsafe block, cacbd8883): the
# targets scripts/lint had yet to reach, then this change's own crates (shards, shards-ipc)
# on both Windows targets, as scripts/lint lints a Windows target: usage
#   bash lint2.sh WORK
set -u
work=${1:?work}
src=$work/src
cd "$src" || exit 1
git log -1 --format='HEAD %H'
test -z "$(git status --porcelain)" || { echo "dirty tree"; git status --porcelain; exit 1; }
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"
zbin=$(ls -d "$work"/zig/zig-*-0.16.0 2>/dev/null | head -1)
[ -n "$zbin" ] && export PATH="$zbin:$PATH"
echo "zig $(zig version); $(rustc --version)"
export CARGO_TARGET_DIR=$work/lint-target SHARDS_HELPERS=skip
llvm=$(brew --prefix llvm)/bin
t0=$(date +%s)
sh scripts/lint x86_64-unknown-linux-musl aarch64-unknown-linux-musl > "$work/logs/lint-musl.log" 2>&1
echo "LINT-MUSL-EXIT $? in $(( $(date +%s) - t0 )) s"
grep -a -E '^== |^(warning|error)(\[|:)' "$work/logs/lint-musl.log" | grep -v aws-lc | sort | uniq -c | head
for target in x86_64-pc-windows-msvc aarch64-pc-windows-msvc; do
  t0=$(date +%s)
  PATH="$llvm:$PATH" cargo xwin clippy -p shards -p shards-ipc --all-targets --target "$target" -- -D warnings > "$work/logs/lint-$target.log" 2>&1
  echo "CLIPPY-$target-EXIT $? in $(( $(date +%s) - t0 )) s (shards, shards-ipc)"
  grep -a -E '^(warning|error)(\[|:)|^   --> ' "$work/logs/lint-$target.log" | grep -v aws-lc | sort | uniq -c | head
done
t0=$(date +%s)
sh scripts/lint aarch64-pc-windows-msvc > "$work/logs/lint-a64win.log" 2>&1
echo "LINT-aarch64-pc-windows-msvc-EXIT $? in $(( $(date +%s) - t0 )) s (the workspace)"
grep -a -E '^(warning|error)(\[|:)|^   --> ' "$work/logs/lint-a64win.log" | grep -v aws-lc | sort | uniq -c | head
