#!/bin/bash
# Runs each mutation in a copy of src (mut/), its own target directory, and the tests that
# guard it; every mutated file restored and touched before the next (rsync keeps mtimes).
cd ~/g121-tmp/xa-exec
export CARGO_TARGET_DIR=$HOME/g121-tmp/xa-exec/target-mut
log=logs/mutations.log
echo "# mutations of the fix; $(date -u +%FT%TZ) $(uname -srm); $(cd src && rustc --version)" > $log
run() { # name package-args filter
  name=$1; shift
  rsync -a --delete --exclude /target src/ mut/
  touch mut/crates/shards/src/workload/hellos.rs mut/crates/init/src/run.rs
  python3 scripts/mutate.py mut "$name" >> $log 2>&1 || { echo "## $name: NOT APPLIED" >> $log; return; }
  echo "## $name: cargo test $*" >> $log
  (cd mut && timeout 600 nice -n 10 cargo test --profile test-release -j 8 "$@" 2>&1) | grep -E "^test |test result|panicked|assertion|left:|right:|^\s+(left|right)" | head -30 >> $log
  echo "## $name exit ${PIPESTATUS[0]}" >> $log
}
run HM1-busy-refuses -p shards-vm-process --bin shards-vm -- workload::
run HM2-arrivals-go-with-a-waiter -p shards-vm-process --bin shards-vm -- workload::
run HM3-newest-evicted -p shards-vm-process --bin shards-vm -- workload::
run GM1-no-kill -p shards-init -- exec_link_tests
run GM2-not-told -p shards-init -- exec_link_tests
run GM3-no-generation-guard -p shards-init -- exec_link_tests
run GM4-old-finished -p shards-init -- exec_link_tests
run GM5-fix-reverted -p shards-init -- exec_link_tests
echo "# done $(date -u +%FT%TZ)" >> $log
