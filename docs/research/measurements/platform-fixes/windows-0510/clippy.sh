#!/bin/bash
# platform-05..10 takeover: cargo xwin clippy of the crates it touches, Windows x64 and arm64.
set -u
sha=$1
base=$HOME/g121-tmp/tkplatwin
mkdir -p "$base"
if [ ! -d "$base/src/.git" ]; then
  for i in 1 2 3 4 5 6; do git clone -q --filter=blob:none https://github.com/hyper-light/shards.git "$base/src" && break; rm -rf "$base/src"; sleep 30; done
fi
cd "$base/src" || exit 2
for i in 1 2 3 4 5 6; do git fetch -q origin p/tk-platwin-v1 && break; sleep 30; done
git checkout -q -f --detach "$sha" || exit 2
echo "== $(git log --oneline -1) $(date -u +%FT%TZ)"
export CARGO_TARGET_DIR=$base/target CARGO_INCREMENTAL=0 SHARDS_HELPERS=skip
llvm=$(brew --prefix llvm)/bin
status=0
for target in x86_64-pc-windows-msvc aarch64-pc-windows-msvc; do
  PATH="$llvm:$PATH" nice -n 10 cargo xwin clippy -j 6 -p shards-vmm -p shards-ipc -p shards-registry -p shards-api -p shards --all-targets --target "$target" -- -D warnings > "$base/clippy-$target.log" 2>&1
  code=$?
  [ $code -ne 0 ] && status=1
  echo "== clippy $target exit $code"
  grep -v 'aws-lc-sys@' "$base/clippy-$target.log" | grep -E '^(error|warning)|^ *--> ' | head -60
  tail -3 "$base/clippy-$target.log"
done
echo "== done $(date -u +%FT%TZ) status $status"
exit $status
