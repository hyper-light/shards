#!/bin/bash
# On the Mac Studio (kiln's adas-mac-studio.local-2), in kiln's synced copy of the worktree:
# scripts/lint for macOS and the two Windows targets (cargo-xwin, LLVM, NASM), then the VM
# process's tests (every workload test among them) on macOS arm64, the workload's 200 times.
export CARGO_TARGET_DIR=$HOME/to-guest-target
log=docs/research/measurements/signal-link/logs/mac.log
mkdir -p "$(dirname "$log")"
echo "# $(git log --oneline -1 | cut -c1-12) + the fix (synced); $(date -u +%FT%TZ) $(uname -srm) $(sw_vers -productVersion); $(rustc --version); $(cargo clippy --version)" > $log
nice -n 10 scripts/lint aarch64-apple-darwin x86_64-pc-windows-msvc aarch64-pc-windows-msvc > /tmp/tg-lint.out 2>&1
rc=$?
grep -E "^== |^(warning|error)" -A 8 /tmp/tg-lint.out | grep -vE "^\s+(Compiling|Checking)" | head -120 >> $log
echo "## scripts/lint (macOS, 2 Windows targets) exit $rc" >> $log
nice -n 10 cargo test -p shards-vm-process --bin shards-vm --profile test-release -j 16 2>&1 | grep -E "^test .*(FAILED|signal|dial|frame|exec_whose|guest_that|only_execs)|test result|panicked" >> $log
echo "## shards-vm (all) exit ${PIPESTATUS[0]}" >> $log
bin=$(ls -t $CARGO_TARGET_DIR/test-release/deps/shards_vm-* | grep -vE '\.(d|dSYM)$' | head -1)
pass=0; fail=0; for i in $(seq 1 200); do $bin workload:: -q > /tmp/tg-f.out 2>&1 && pass=$((pass+1)) || { fail=$((fail+1)); grep -E "panicked|FAILED" /tmp/tg-f.out | head -3 >> $log; }; done
echo "## workload:: x200: pass $pass fail $fail" >> $log
echo "# done $(date -u +%FT%TZ)" >> $log
cat $log
