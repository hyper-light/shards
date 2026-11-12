#!/bin/bash
# Builds shards (test-release) on jupiter, from a partial clone of its own over HTTPS, at
# COMMIT of BRANCH (default main), into a target outside any synced copy; keeps the binary in
# ~/g121-whp-stop-bin/<commit's first 9>/: usage
#   bash build.sh COMMIT [BRANCH]
set -u
want=${1:?commit}
src=$HOME/g121-whp-stop-src
logs=$HOME/g121-whp-stop-logs
mkdir -p "$logs"
export GIT_CONFIG_COUNT=2 GIT_CONFIG_KEY_0=gc.auto GIT_CONFIG_VALUE_0=0 GIT_CONFIG_KEY_1=maintenance.auto GIT_CONFIG_VALUE_1=false
# Source over HTTPS from GitHub (a branch pushed there), a fresh partial clone of its own.
ref=${2:-main}
if [ ! -d "$src/.git" ]; then
  for i in 1 2 3; do
    rm -rf "$src"
    git clone -q --filter=blob:none --no-checkout https://github.com/hyper-light/shards.git "$src" && break
    sleep 20
  done
fi
for i in 1 2 3; do git -C "$src" fetch -q origin "+refs/heads/$ref:refs/remotes/origin/$ref" && break; sleep 20; done
git -C "$src" checkout -q --detach "$want" || exit 1
git -C "$src" log -1 --format='HEAD %H %s' | cut -c1-120
test -z "$(git -C "$src" status --porcelain)" || { echo "dirty tree"; git -C "$src" status --porcelain | head; exit 1; }
export PATH="$HOME/tools/zig-x86_64-windows-0.16.0:$HOME/tools/nasm-3.02:/c/Users/corph/.cargo/bin:/usr/bin:$PATH"
export CARGO_TARGET_DIR=$HOME/g121-whp-stop-target
cd "$src" || exit 1
began=$(date +%s)
cargo build --locked --profile test-release -p shards --bin shards > "$logs/build-$want.log" 2>&1
rc=$?
echo "build exit $rc in $(( $(date +%s) - began )) s"
tail -15 "$logs/build-$want.log"
ls -la "$CARGO_TARGET_DIR/test-release/shards.exe" 2>&1
if [ $rc -eq 0 ]; then
  mkdir -p "$HOME/g121-whp-stop-bin/${want:0:9}"
  cp "$CARGO_TARGET_DIR/test-release/shards.exe" "$HOME/g121-whp-stop-bin/${want:0:9}/shards.exe"
  sha256sum "$HOME/g121-whp-stop-bin/${want:0:9}/shards.exe"
fi
