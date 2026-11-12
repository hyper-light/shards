#!/bin/bash
# Runs each mutation (mutate.py) in a copy of src (mut/), with a target directory of its own,
# and the tests that guard it; the copy is synced again, every mutated file touched, before the
# next (rsync keeps mtimes, and cargo would keep the last mutant's build).
cd ~/g121-tmp/to-guest
export CARGO_TARGET_DIR=$HOME/g121-tmp/to-guest/target-mut
log=logs/mutations.log
echo "# mutations of the fix, $(cd src && git rev-parse --short=12 HEAD); $(date -u +%FT%TZ) $(uname -srm); $(cd src && rustc --version); load $(cut -d' ' -f1-3 /proc/loadavg)" > $log
run() { # name, then cargo test's arguments
  name=$1; shift
  rsync -a --delete --exclude /target src/ mut/
  touch mut/crates/shards/src/workload.rs mut/crates/shards/src/workload/hellos.rs mut/crates/init/src/run.rs
  python3 scripts/mutate.py mut "$name" >> $log 2>&1 || { echo "## $name: NOT APPLIED" >> $log; return; }
  echo "## $name: cargo test $*" >> $log
  (cd mut && timeout 900 nice -n 10 cargo test --profile test-release -j 12 "$@" 2>&1) | grep -E "^test .*FAILED|test result|panicked|the exec did not fail|left:|right:" | head -30 >> $log
  echo "## $name exit ${PIPESTATUS[0]}" >> $log
}
for m in HM1-failed-send-told-sent HM2-lost-link-takes-frames HM3-no-watch HM4-taken-unwatched \
         HM5-loss-fails-no-exec HM6-end-taken-for-a-loss HM7-end-keeps-a-loss HM8-no-coalescing \
         HM9-exec-frames-unchecked HM10-any-signal-number HM11-every-wait-fails; do
  run $m -p shards-vm-process --bin shards-vm -- workload::
done
run GM1-no-end-said -p shards-init -- signals_end_tests
run GM2-connect-not-waited -p shards-init -- signals_end_tests
echo "# done $(date -u +%FT%TZ)" >> $log
