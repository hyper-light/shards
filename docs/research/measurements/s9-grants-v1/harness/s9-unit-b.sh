#!/bin/bash
# Light unit tests of step 9's change (branch p/s9-grants-v1): the crates it touches, and
# the shards binary's tests of the code it touches; helpers skipped (no VM process built).
cd ~/g121-tmp/s9-src || exit 2
git fetch -q origin p/s9-grants-v1 && git checkout -q -B p/s9-grants-v1 origin/p/s9-grants-v1 || exit 3
export CARGO_TARGET_DIR=$HOME/g121-tmp/s9-target SHARDS_HELPERS=skip
echo "== $(git log --oneline -1) $(date -u +%FT%TZ)"
st=0
run() {
  echo "== $*"
  out=$(cargo test "$@" 2>&1); s=$?
  printf '%s\n' "$out" | grep -E "^test result|FAILED|panicked|^error|failures:|^---- " | head -40
  [ $s = 0 ] || { st=1; printf '%s\n' "$out" | grep -A 15 "panicked" | head -60; }
}
for c in shards-dns shards-agentfile shards-net shards-api shards-init; do run -p "$c"; done
run -p shards --bin shards -- api_run:: agentfile:: daemon::commands networks::
echo "== done $(date -u +%FT%TZ) status $st"
exit $st
