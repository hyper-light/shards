#!/bin/bash
# scripts/lint for the four Linux targets (zig as their C compiler), from adacachy.
cd ~/g121-tmp/to-guest/src
export CARGO_TARGET_DIR=$HOME/g121-tmp/to-guest/target-lint
log=../logs/lint-linux.log
echo "# scripts/lint x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu x86_64-unknown-linux-musl aarch64-unknown-linux-musl; $(git log --oneline -1 | cut -c1-12) + the fix; $(date -u +%FT%TZ) $(uname -srm); $(rustc --version); $(cargo clippy --version); zig $(zig version)" > $log
nice -n 10 scripts/lint x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu x86_64-unknown-linux-musl aarch64-unknown-linux-musl > /tmp/tg-lint.out 2>&1
rc=$?
grep -E "^== |^(warning|error)" -A 8 /tmp/tg-lint.out | grep -vE "^\s+(Compiling|Checking)" | head -120 >> $log
echo "## scripts/lint (4 Linux targets) exit $rc" >> $log
echo "# done $(date -u +%FT%TZ)" >> $log
