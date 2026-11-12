#!/bin/bash
# bash test-mac.sh REV CRATE...: the light tests of CRATEs at REV on this Mac (shards:FILTER for
# the shards binary's tests), guest binaries stood in for by empty files.
set -u
rev=$1; shift
base=$HOME/g121-tmp/s7p2
cd "$base/shards" || exit 2
git fetch -q origin p/s7-part2-v1 p/s7-part2-land main && git checkout -q --detach "$rev" || exit 3
echo "== $(git log --oneline -1) $(date -u +%FT%TZ) $(uname -sm)"
: > "$base/fake-init"; : > "$base/fake-server"
export SHARDS_INIT_BINARY=$base/fake-init SHARDS_SERVER_BINARY=$base/fake-server CARGO_TARGET_DIR=$base/target SHARDS_HELPERS=skip
t=0
for c in "$@"; do
  case $c in
    shards:*) out=$(cargo test -p shards --bin shards -- ${c#shards:} 2>&1) ;;
    *) out=$(cargo test -p "$c" 2>&1) ;;
  esac
  s=$?; [ $s = 0 ] || t=1
  echo "== test $c exit $s"; printf '%s\n' "$out" | grep -E "^test result|FAILED|panicked" | head -20
done
echo "== done $(date -u +%FT%TZ) tests $t"
