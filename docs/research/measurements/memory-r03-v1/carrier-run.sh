#!/bin/bash
# Carrier for the memory R03 lane: applies patch over BASE in a blobless clone kept under
# ~/g121-tmp/mem-r03, then runs the commands given. Usage: run.sh PATCH -- CMD...
set -uo pipefail
CARRIER=$(cd "$(dirname "$0")" && pwd)
PATCH="$CARRIER/$1"; shift; [ "$1" = "--" ] && shift
BASE=$(cat "$CARRIER/base")
W=$HOME/g121-tmp/mem-r03
mkdir -p "$HOME/g121-tmp"
if [ ! -d "$W/.git" ]; then
  for i in 1 2 3 4 5 6; do
    git clone -q --filter=blob:none git@github.com:hyper-light/shards.git "$W" && break
    rm -rf "$W"; sleep 30
  done
fi
cd "$W" || exit 90
if ! git cat-file -e "$BASE^{commit}" 2>/dev/null; then
  for i in 1 2 3 4 5 6; do git fetch -q origin main && break; sleep 30; done
fi
git checkout -q -f --detach "$BASE" || exit 91
git clean -fdq -e target
git apply --whitespace=nowarn "$PATCH" || exit 92
export CARGO_INCREMENTAL=0 CARGO_TARGET_DIR="$W/target"
echo "== tree $(git rev-parse HEAD) + $(basename "$PATCH") ($( (shasum -a 256 "$PATCH" 2>/dev/null || sha256sum "$PATCH") | cut -c1-16)) on $(hostname) $(uname -sm)"
rc=0
for cmd in "$@"; do
  echo "== $cmd"
  if [ "$(uname -s)" = Linux ]; then nice -n 10 bash -c "$cmd" 2>&1 | tail -n 120; else bash -c "$cmd" 2>&1 | tail -n 120; fi
  r=${PIPESTATUS[0]}; echo "== exit $r"; [ $r -ne 0 ] && rc=$r
done
exit $rc
