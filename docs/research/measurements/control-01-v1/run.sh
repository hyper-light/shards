#!/bin/bash
# control-01's focused tests: red at the tests' commit, green at the merged fix, two mutations.
set -u
W=$HOME/g121-tmp/control01
mkdir -p $W
df -h $HOME | tail -1
if [ ! -d $W/shards/.git ]; then
  for i in 1 2 3 4 5 6; do git clone -q --filter=blob:none https://github.com/hyper-light/shards.git $W/shards && break; rm -rf $W/shards; sleep 30; done
fi
cd $W/shards || exit 2
for i in 1 2 3 4 5 6; do git fetch -q origin main && break; sleep 30; done
git fetch -q "$OLDPWD/c.bundle" "refs/heads/p/control01-v1:refs/heads/p/control01-v1" || exit 3
export CARGO_INCREMENTAL=0 CARGO_TARGET_DIR=$W/target
T="image::session::tests::"
run() { echo "=== $1"; shift; perl -e 'alarm 900; exec @ARGV' "$@" 2>&1 | grep -E "^test |test result|panicked|left:|right:|error(\[|:)" ; echo "exit ${PIPESTATUS[0]}"; }
git checkout -q d7b62bd7d || exit 4
run "RED (tests on the old pump)" cargo test -q -p shards-api --lib ${T}output_ -- --nocapture
git checkout -q p/control01-v1 || exit 5
git log --oneline -1
run "GREEN new tests" cargo test -p shards-api --lib ${T}output_
run "GREEN session tests" cargo test -p shards-api --lib image::session
F=crates/api/src/image/session.rs
cp $F /tmp/control01-session.rs
perl -0pi -e 's/\(flow\.ended\.is_some\(\) \|\| s\.answered\)\.then_some\(None\)/(flow.ended.is_some() || s.end.is_some() || s.answered).then_some(None)/' $F
git diff --stat
run "MUTANT 1 (run's end ends the pipe)" cargo test -p shards-api --lib ${T}output_
cp /tmp/control01-session.rs $F; touch $F
perl -0pi -e 's/granting = false;/return;/' $F
git diff --stat
run "MUTANT 2 (refused credit ends the pump)" cargo test -p shards-api --lib ${T}output_
cp /tmp/control01-session.rs $F; touch $F
git status --short
run "CLIPPY" cargo clippy -p shards-api --all-targets -- -D warnings
