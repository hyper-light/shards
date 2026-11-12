#!/bin/bash
# scripts/lint's clippy for the two Windows targets (cargo-xwin, LLVM's clang-cl and llvm-lib,
# NASM), on the Mac Studio, in ~/to-guest/src at the branch's commit, the workspace less
# shards-vmm's own targets: scripts/lint stops before the rest at main's own error in
# shards-vmm's test target (crates/vmm/src/platform/windows_process.rs l. 544, from
# cacbd8883), which nothing of this change builds. shards-vmm's library is linted still, as a
# dependency of the rest.
cd ~/to-guest/src || exit 1
export CARGO_TARGET_DIR=$HOME/to-guest-target SHARDS_HELPERS=skip
command -v zig > /dev/null || PATH="$(ls -d ~/to-guest/tools/zig-*-0.16.0 | head -1):$PATH"
llvm=$(brew --prefix llvm)/bin
log=docs/research/measurements/signal-link/logs/lint-windows.log
echo "# cargo xwin clippy --workspace --exclude shards-vmm --all-targets -- -D warnings; $(git rev-parse --short=12 HEAD); $(date -u +%FT%TZ) $(uname -srm); $(rustc --version); $(cargo clippy --version)" > $log
for t in x86_64-pc-windows-msvc aarch64-pc-windows-msvc; do
	PATH="$llvm:$PATH" nice -n 10 cargo xwin clippy --workspace --exclude shards-vmm --all-targets --target "$t" -- -D warnings > /tmp/tg-wlint.out 2>&1
	rc=$?
	echo "## $t exit $rc" >> $log
	grep -E "^(warning|error)(\[[A-Za-z0-9_]+\])?: " -A 8 /tmp/tg-wlint.out | grep -vE "aws-lc-sys|^\s+(Compiling|Checking)" | head -40 >> $log
	grep -E "Finished" /tmp/tg-wlint.out >> $log
done
echo "# done $(date -u +%FT%TZ)" >> $log
cat $log
