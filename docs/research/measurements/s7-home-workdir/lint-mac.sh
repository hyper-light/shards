#!/bin/bash
# Step 7 part 2 on the Mac Studio: scripts/lint for the seven supported targets at REV of
# p/s7-part2-v1, then the light tests of CRATES on aarch64-apple-darwin. Clippy and these
# tests read none of the guest binaries build.rs embeds: empty placeholders stand for them.
# zig 0.16.0 is scripts/install-zig's, or a tarball already here that its pinned digest
# verifies.
set -u
rev=$1; shift
base=$HOME/g121-tmp/s7p2
pin=b23d70deaa879b5c2d486ed3316f7eaa53e84acf6fc9cc747de152450d401489
mkdir -p "$base" && cd "$base/shards" || exit 2
git fetch -q origin p/s7-part2-v1 main && git checkout -q --detach "$rev" || exit 3
echo "== $(git log --oneline -1) $(date -u +%FT%TZ)"
zigdir=$base/zig-aarch64-macos-0.16.0
if [ ! -x "$zigdir/zig" ]; then
  for t in "$base"/zig-aarch64-macos-0.16.0.tar.xz "$HOME"/g121-tmp/s9-lint/zig-aarch64-macos-0.16.0.tar.xz; do
    if [ -f "$t" ] && echo "$pin  $t" | shasum -a 256 -c - >/dev/null 2>&1; then
      tar -xJf "$t" -C "$base" && break
    fi
  done
fi
[ -x "$zigdir/zig" ] || zigdir=$(TMPDIR=$base scripts/install-zig 2>"$base/zig.err") || { cat "$base/zig.err"; exit 4; }
echo "== zig $("$zigdir/zig" version) at $zigdir"
export PATH="$zigdir:$PATH"
: > "$base/fake-init"; : > "$base/fake-server"
export SHARDS_INIT_BINARY=$base/fake-init SHARDS_SERVER_BINARY=$base/fake-server CARGO_TARGET_DIR=$base/target
scripts/lint > "$base/lint-$rev.log" 2>&1; lint=$?
echo "== lint exit $lint"
grep -E "^== |^error|^warning: [^a]" "$base/lint-$rev.log" | grep -v "aws-lc" | head -60
export SHARDS_HELPERS=skip
t=0
for c in "$@"; do
  case $c in
    shards:*) out=$(cargo test -p shards --bin shards -- ${c#shards:} 2>&1) ;;
    *) out=$(cargo test -p "$c" 2>&1) ;;
  esac
  s=$?; [ $s = 0 ] || t=1
  echo "== test $c exit $s"; printf '%s\n' "$out" | grep -E "^test result|FAILED|panicked" | head -20
done
echo "== done $(date -u +%FT%TZ) lint $lint tests $t"
