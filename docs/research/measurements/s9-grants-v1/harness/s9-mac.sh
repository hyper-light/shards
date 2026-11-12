#!/bin/bash
# Step 9 (p/s9-grants-v1) on the Mac Studio: scripts/lint for the seven supported targets,
# then the light unit tests of the crates the change touches, on aarch64-apple-darwin.
# Clippy and these tests read none of the guest binaries build.rs embeds, so empty
# placeholders stand for shards-init and shards-server (SHARDS_INIT_BINARY/SERVER_BINARY).
set -u
base=$HOME/g121-tmp/s9-lint
mkdir -p "$base" && cd "$base" || exit 2
[ -d src ] || git clone -q https://github.com/hyper-light/shards.git src || exit 2
cd src && git fetch -q origin p/s9-grants-v1 && git checkout -q -B p/s9-grants-v1 FETCH_HEAD || exit 3
echo "== $(git log --oneline -1) $(date -u +%FT%TZ)"
zigdir=$(TMPDIR=$base scripts/install-zig 2>"$base/zig.err") || { cat "$base/zig.err"; exit 4; }
export PATH="$zigdir:$PATH"
: > "$base/fake-init"; : > "$base/fake-server"
export SHARDS_INIT_BINARY=$base/fake-init SHARDS_SERVER_BINARY=$base/fake-server CARGO_TARGET_DIR=$base/target
scripts/lint > "$base/lint.log" 2>&1; lint=$?
echo "== lint exit $lint"
grep -E "^== |^error|^warning: [^a]" "$base/lint.log" | grep -v "aws-lc" | head -60
export SHARDS_HELPERS=skip
t=0
for c in shards-dns shards-agentfile shards-net shards-api; do
  out=$(cargo test -p "$c" 2>&1); s=$?; [ $s = 0 ] || t=1
  echo "== test $c exit $s"; printf '%s\n' "$out" | grep -E "^test result|FAILED|panicked" | head -20
done
out=$(cargo test -p shards --bin shards -- api_run:: agentfile:: daemon::commands networks:: 2>&1); s=$?; [ $s = 0 ] || t=1
echo "== test shards (bin) exit $s"; printf '%s\n' "$out" | grep -E "^test result|FAILED|panicked" | head -20
echo "== done $(date -u +%FT%TZ) lint $lint tests $t"
