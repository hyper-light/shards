#!/usr/bin/env bash
# g71-hand's carrier: shards-server's focused unit tests and clippy for D139's hand refusals,
# run on this Linux runner in a blobless clone of hyper-light/shards and a cold target of its
# own under ~/g121-tmp/g71-hand.
#   run.sh red BASE PATCH       BASE with PATCH (the red test alone) applied
#   run.sh green BUNDLE COMMIT  COMMIT, fetched from BUNDLE (commits over origin/main), or
#                               from origin/main where BUNDLE is -
#   run.sh clean                removes ~/g121-tmp/g71-hand
set -uo pipefail
mode=${1:?mode}
here=$(cd "$(dirname "$0")" && pwd)
W="$HOME/g121-tmp/g71-hand"
src="$W/src"
export PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH"
export CARGO_TARGET_DIR="$W/target" CARGO_INCREMENTAL=0
if [ "$mode" = clean ]; then
  rm -rf "$W"
  echo "removed $W: $(ls -d "$W" 2>&1)"
  exit 0
fi
mkdir -p "$W"
if [ ! -d "$src/.git" ]; then
  git clone -q --filter=blob:none --no-checkout https://github.com/hyper-light/shards "$src" || exit 90
fi
cd "$src" || exit 91
git fetch -q origin main || exit 92
case "$mode" in
  red)
    git checkout -q --force --detach "$2" || exit 93
    git clean -fdq
    git apply "$here/$3" || exit 94
    ;;
  green)
    if [ "$2" != - ]; then git fetch -q "$here/$2" "+refs/heads/*:refs/carrier/*" || exit 95; fi
    git checkout -q --force --detach "$3" || exit 93
    git clean -fdq
    ;;
  *) echo "no mode $mode"; exit 2 ;;
esac
echo "mode=$mode HEAD=$(git rev-parse HEAD) origin/main=$(git rev-parse origin/main)"
git status --short
echo "host=$(hostname) $(uname -srm) $(rustc --version) zig $(zig version) df: $(df -h "$W" | tail -1)"
filters=(broker hand refus program)
quiet() { grep -v '^warning: aws-lc-sys'; }
echo "--- cargo test -p shards-server --locked --target x86_64-unknown-linux-gnu -j 8 -- ${filters[*]} --nocapture"
nice -n 10 cargo test -p shards-server --locked --target x86_64-unknown-linux-gnu -j 8 -- "${filters[@]}" --nocapture 2>&1 | quiet
echo "test_gnu_exit=${PIPESTATUS[0]}"
if [ "$mode" = green ]; then
  echo "--- cargo clippy -p shards-server --locked --target x86_64-unknown-linux-gnu -j 8 --all-targets -- -D warnings"
  nice -n 10 cargo clippy -p shards-server --locked --target x86_64-unknown-linux-gnu -j 8 --all-targets -- -D warnings 2>&1 | quiet
  echo "clippy_gnu_exit=${PIPESTATUS[0]}"
  echo "--- cargo clippy -p shards-server --locked --target x86_64-unknown-linux-musl -j 8 --all-targets -- -D warnings (CC/AR scripts/zig-cc, scripts/zig-ar; ZIG_TARGET=x86_64-linux-musl)"
  CC_x86_64_unknown_linux_musl="$src/scripts/zig-cc" AR_x86_64_unknown_linux_musl="$src/scripts/zig-ar" \
    ZIG_TARGET=x86_64-linux-musl nice -n 10 cargo clippy -p shards-server --locked \
    --target x86_64-unknown-linux-musl -j 8 --all-targets -- -D warnings 2>&1 | quiet
  echo "clippy_musl_exit=${PIPESTATUS[0]}"
  echo "--- cargo fmt -p shards-server -- --check"
  cargo fmt -p shards-server -- --check 2>&1
  echo "fmt_exit=$?"
fi
