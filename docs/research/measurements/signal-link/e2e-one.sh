#!/bin/bash
# One E2E test of tests/<file>.rs on adacachy's KVM, in ~/g121-tmp/to-guest2/src as it stands:
# argv[1] the file, argv[2] the test's name, argv[3] the log's name.
cd ~/g121-tmp/to-guest2/src || exit 1
export PATH="$(cat ../zigdir):$PATH" CARGO_TARGET_DIR=$HOME/g121-tmp/to-guest2/target
file=$1 name=$2 out=../logs/$3.out log=../logs/$3.log
echo "# tests/$file.rs $name: $(git rev-parse --short=12 HEAD), $(git status --short | wc -l) files changed ($(git status --short | awk '{print $2}' | tr '\n' ' ')); $(date -u +%FT%TZ) $(uname -srm); $(rustc --version); load $(cut -d' ' -f1-3 /proc/loadavg)" > $log
t0=$(date +%s)
nice -n 10 cargo test --profile test-release -p shards --test "$file" -- "$name" --exact --nocapture --test-threads 1 > $out 2>&1
rc=$?
grep -E "^test |panicked|did not exit|left:|right:|assertion|test result" $out | head -40 >> $log
echo "## exit $rc after $(( $(date +%s) - t0 )) s, $(date -u +%T)" >> $log
