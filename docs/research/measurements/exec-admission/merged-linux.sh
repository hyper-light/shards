#!/bin/bash
# The merged tree (origin/main merged in): the changed crates' tests and clippy again.
cd ~/g121-tmp/xa-exec/src
export CARGO_TARGET_DIR=$HOME/g121-tmp/xa-exec/target
log=../logs/merged-linux.log
echo "# merged tree $(cat ../MERGED_REV 2>/dev/null); $(date -u +%FT%TZ) $(uname -srm); $(rustc --version); load $(cut -d' ' -f1-3 /proc/loadavg)" > $log
nice -n 10 cargo test -p shards-vm-process --bin shards-vm --profile test-release -j 12 2>&1 | grep -E "test result|panicked|FAILED" >> $log; echo "## shards-vm (all) exit ${PIPESTATUS[0]}" >> $log
nice -n 10 cargo test -p shards-init --profile test-release -j 12 2>&1 | grep -E "test result|panicked|FAILED" >> $log; echo "## shards-init (all) exit ${PIPESTATUS[0]}" >> $log
nice -n 10 cargo test -p shards-vmm --lib --profile test-release -j 12 -- vsock 2>&1 | grep -E "test result|panicked|FAILED" >> $log; echo "## shards-vmm vsock exit ${PIPESTATUS[0]}" >> $log
bin=$(ls -t $CARGO_TARGET_DIR/test-release/deps/shards_vm-* | grep -v '\.d$' | head -1)
pass=0; fail=0; for i in $(seq 1 100); do timeout 60 $bin workload:: -q > /tmp/xa-f.out 2>&1 && pass=$((pass+1)) || { fail=$((fail+1)); grep -E "panicked" /tmp/xa-f.out | head -3 >> $log; }; done
echo "## workload:: x100: pass $pass fail $fail" >> $log
echo "# done $(date -u +%FT%TZ)" >> $log
sed -i 's|log=../logs/lint-final.log|log=../logs/lint-merged.log|' ~/g121-tmp/xa-exec/scripts/lint.sh
bash ~/g121-tmp/xa-exec/scripts/lint.sh
