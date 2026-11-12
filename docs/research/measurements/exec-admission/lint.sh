#!/bin/bash
# Clippy of the changed packages for every supported target, and their unit tests here.
cd ~/g121-tmp/xa-exec/src
export CARGO_TARGET_DIR=$HOME/g121-tmp/xa-exec/target-lint SHARDS_HELPERS=skip
log=../logs/lint-merged.log
echo "# clippy -D warnings of shards-vmm, shards-vm-process, shards-init, --all-targets; $(date -u +%FT%TZ) $(uname -srm); $(rustc --version); $(cargo clippy --version)" > $log
cargo fmt --all --check >> $log 2>&1; echo "## fmt exit $?" >> $log
for t in $(grep -v apple-darwin resources/supported-targets.txt); do
  var=$(printf %s "$t" | tr - _)
  env "CC_$var=$PWD/scripts/zig-cc" "AR_$var=$PWD/scripts/zig-ar" ZIG_TARGET="${t%%-*}-linux-${t##*-}" \
    nice -n 10 cargo clippy -j 12 -p shards-vmm -p shards-vm-process -p shards-init --all-targets --target "$t" -- -D warnings > /tmp/xa-clippy.out 2>&1
  rc=$?
  echo "## $t exit $rc" >> $log
  grep -E "^(warning|error)" -A 6 /tmp/xa-clippy.out | head -40 >> $log
done
echo "# done $(date -u +%FT%TZ)" >> $log
