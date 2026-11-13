#!/bin/bash
# The fix's unit tests on Linux x86_64: the VM process's and shards-init's whole suites, then
# the workload's tests 200 times and the guest's end 300 times, on a loaded host as it is.
cd ~/g121-tmp/to-guest/src
export CARGO_TARGET_DIR=$HOME/g121-tmp/to-guest/target
log=../logs/${1:-linux}.log
echo "# $(git rev-parse --short=12 HEAD); $(date -u +%FT%TZ) $(uname -srm); $(rustc --version); load $(cut -d' ' -f1-3 /proc/loadavg)" > $log
nice -n 10 cargo test -p shards-vm-process --bin shards-vm --profile test-release -j 12 2>&1 | grep -E "^test .*(FAILED|signal|dial|frame|exec_whose|guest_that|only_execs)|test result|panicked" >> $log; echo "## shards-vm (all) exit ${PIPESTATUS[0]}" >> $log
nice -n 10 cargo test -p shards-init --profile test-release -j 12 2>&1 | grep -E "^test .*(FAILED|signals_end)|test result|panicked" >> $log; echo "## shards-init (all) exit ${PIPESTATUS[0]}" >> $log
bin=$(ls -t $CARGO_TARGET_DIR/test-release/deps/shards_vm-* | grep -v '\.d$' | head -1)
pass=0; fail=0; for i in $(seq 1 200); do timeout 60 $bin workload:: -q > /tmp/tg-f.out 2>&1 && pass=$((pass+1)) || { fail=$((fail+1)); grep -E "panicked|FAILED" /tmp/tg-f.out | head -3 >> $log; }; done
echo "## workload:: x200: pass $pass fail $fail" >> $log
ibin=$(ls -t $CARGO_TARGET_DIR/test-release/deps/shards_init-* | grep -v '\.d$' | head -1)
pass=0; fail=0; for i in $(seq 1 300); do timeout 60 $ibin signals_end_tests -q > /tmp/tg-f.out 2>&1 && pass=$((pass+1)) || { fail=$((fail+1)); grep -E "panicked|FAILED" /tmp/tg-f.out | head -3 >> $log; }; done
echo "## signals_end_tests x300: pass $pass fail $fail" >> $log
echo "# done $(date -u +%FT%TZ)" >> $log
