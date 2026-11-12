#!/bin/bash
# The final tree's unit tests on Linux x86_64, then every mutation again.
cd ~/g121-tmp/xa-exec/src
export CARGO_TARGET_DIR=$HOME/g121-tmp/xa-exec/target
log=../logs/final-linux.log
echo "# final tree, Linux unit tests; $(date -u +%FT%TZ) $(uname -srm); $(rustc --version); load $(cut -d' ' -f1-3 /proc/loadavg)" > $log
nice -n 10 cargo test -p shards-vm-process --bin shards-vm --profile test-release -j 12 2>&1 | grep -E "test result|panicked|FAILED" >> $log; echo "## shards-vm (all) exit ${PIPESTATUS[0]}" >> $log
nice -n 10 cargo test -p shards-init --profile test-release -j 12 2>&1 | grep -E "test result|panicked|FAILED" >> $log; echo "## shards-init (all) exit ${PIPESTATUS[0]}" >> $log
nice -n 10 cargo test -p shards-vmm --lib --profile test-release -j 12 -- vsock 2>&1 | grep -E "test result|panicked|FAILED" >> $log; echo "## shards-vmm vsock exit ${PIPESTATUS[0]}" >> $log
bin=$(ls -t $CARGO_TARGET_DIR/test-release/deps/shards_vm-* | grep -v '\.d$' | head -1)
pass=0; fail=0; for i in $(seq 1 200); do timeout 60 $bin workload:: -q > /tmp/xa-f.out 2>&1 && pass=$((pass+1)) || { fail=$((fail+1)); grep -E "panicked" /tmp/xa-f.out | head -3 >> $log; }; done
echo "## workload:: x200: pass $pass fail $fail" >> $log
ibin=$(ls -t $CARGO_TARGET_DIR/test-release/deps/shards_init-* | grep -v '\.d$' | head -1)
pass=0; fail=0; for i in $(seq 1 300); do timeout 60 $ibin exec_link_tests -q > /tmp/xa-f.out 2>&1 && pass=$((pass+1)) || { fail=$((fail+1)); grep -E "panicked" /tmp/xa-f.out | head -3 >> $log; }; done
echo "## exec_link_tests x300: pass $pass fail $fail" >> $log
echo "# done $(date -u +%FT%TZ)" >> $log
bash ~/g121-tmp/xa-exec/scripts/mutations.sh
