#!/bin/bash
# control-01 follow-up's focused tests: red at the test's commit, green at the fix, two mutations.
set -u
W=$HOME/g121-tmp/control01
mkdir -p $W
df -h $HOME | tail -1
if [ ! -d $W/shards/.git ]; then
  for i in 1 2 3 4 5 6; do git clone -q --filter=blob:none https://github.com/hyper-light/shards.git $W/shards && break; rm -rf $W/shards; sleep 30; done
fi
cd $W/shards || exit 2
for i in 1 2 3 4 5 6; do git fetch -q origin main && break; sleep 30; done
git fetch -q "$OLDPWD/c.bundle" "refs/heads/p/control01-eof-v1:refs/heads/p/control01-eof-v1" || exit 3
: > $W/no-guest
export CARGO_INCREMENTAL=0 CARGO_TARGET_DIR=$W/target SHARDS_HELPERS=skip SHARDS_INIT_BINARY=$W/no-guest SHARDS_SERVER_BINARY=$W/no-guest
T="api_run::hosted::tests::"
run() { echo "=== $1"; shift; perl -e 'alarm 3000; exec @ARGV' "$@" 2>&1 | grep -E "^test |test result|panicked|left:|right:|^error|^warning: unused" ; echo "exit ${PIPESTATUS[0]}"; }
git checkout -q 53faf0804 || exit 4
run "RED (test on the old host)" cargo test -p shards --bin shards ${T}bytes_held_past
git checkout -q p/control01-eof-v1 || exit 5
git log --oneline -1
run "GREEN new test" cargo test -p shards --bin shards ${T}bytes_held_past
run "GREEN api_run's hosted tests" cargo test -p shards --bin shards ${T}
F=crates/shards/src/api_run.rs
cp $F /tmp/control01-api_run.rs
perl -0pi -e 's/(fn cut\(&self\) -> io::Result<\(\)> \{\n)/$1            return Ok(());\n/' $F
git diff --stat
run "MUTANT 1 (no cut at the capture's end)" cargo test -p shards --bin shards ${T}bytes_held_past
cp /tmp/control01-api_run.rs $F; touch $F
perl -0pi -e 's/(                if !flows\.ended\.get\(&key\)\.copied\(\)\.unwrap_or\(false\) \{\n)/$1                    flows.waiting.remove(&key);\n/' $F
git diff --stat
run "MUTANT 2 (cut drops what it held)" cargo test -p shards --bin shards ${T}bytes_held_past
cp /tmp/control01-api_run.rs $F; touch $F
git status --short
run "CLIPPY" cargo clippy -p shards --all-targets -- -D warnings
rm -rf $W/target
