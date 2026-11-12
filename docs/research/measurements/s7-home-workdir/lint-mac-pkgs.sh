#!/bin/bash
# bash lint-mac-pkgs.sh REV TARGETS PKG...: clippy of PKGs, all targets, warnings denied, for
# each of TARGETS (comma-separated), with scripts/lint's per-target compilers (zig for Linux,
# cargo-xwin with LLVM for Windows), at REV of p/s7-part2-v1.
set -u
rev=$1 targets=$2; shift 2
base=$HOME/g121-tmp/s7p2
cd "$base/shards" || exit 2
git fetch -q origin p/s7-part2-v1 p/s7-part2-land main && git checkout -q --detach "$rev" || exit 3
echo "== $(git log --oneline -1) $(date -u +%FT%TZ)"
export PATH="$base/zig-aarch64-macos-0.16.0:$PATH" SHARDS_HELPERS=skip
: > "$base/fake-init"; : > "$base/fake-server"
export SHARDS_INIT_BINARY=$base/fake-init SHARDS_SERVER_BINARY=$base/fake-server CARGO_TARGET_DIR=$base/target
root=$PWD
llvm=$(brew --prefix llvm 2>/dev/null)/bin
args=""; for p in "$@"; do args="$args -p $p"; done
for target in ${targets//,/ }; do
  var=$(printf %s "$target" | tr - _)
  echo "== $target:$args"
  case $target in
  *-linux-gnu | *-linux-musl)
    env "CC_$var=$root/scripts/zig-cc" "AR_$var=$root/scripts/zig-ar" ZIG_TARGET="${target%%-*}-linux-${target##*-}" \
      cargo clippy $args --all-targets --target "$target" -- -D warnings > "$base/pkgs-$target.log" 2>&1 ;;
  *-windows-msvc)
    PATH="$llvm:$PATH" cargo xwin clippy $args --all-targets --target "$target" -- -D warnings > "$base/pkgs-$target.log" 2>&1 ;;
  *)
    cargo clippy $args --all-targets --target "$target" -- -D warnings > "$base/pkgs-$target.log" 2>&1 ;;
  esac
  s=$?
  grep -E "^error|^warning: [^a]" "$base/pkgs-$target.log" | grep -v "aws-lc\|SHARDS_GPU_LIBS" | head -12
  echo "clippy $target exit=$s"
done
echo "== done $(date -u +%FT%TZ)"
