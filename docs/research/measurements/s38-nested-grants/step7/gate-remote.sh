#!/bin/bash
# gate-remote.sh TARGET: nesting.rs's light gate (D234, step 7), on a kiln runner. The files of
# one revision arrive as a tar on stdin (`git archive REV | kiln run --no-sync --stdin`, as
# gate.sh sends them). TARGET is the triple the test is built and linted for:
# x86_64-unknown-linux-gnu natively on Linux x86_64 (adacachy2), or aarch64-unknown-linux-musl
# cross-built on a Mac, as common::linux_tests builds it (zig for C, rust-lld to link).
# No VM runs here. Every directory it makes is removed as it ends (R16). It exits with the
# number of checks that failed.
set -u
# rustup's proxies, where a runner's non-login shell does not have them on PATH.
PATH=$HOME/.cargo/bin:$PATH
fail=0
target=$1
base=~/g121-tmp/s38-docs-$target
src=$base/src
rm -rf "$base"
mkdir -p "$src" "$base/target"
tar -x -m -C "$src" 2>/dev/null
cd "$src" || exit 1
export CARGO_TARGET_DIR=$base/target SHARDS_HELPERS=skip
unset DYLD_FALLBACK_LIBRARY_PATH DYLD_LIBRARY_PATH
if ! command -v zig >/dev/null 2>&1; then
	PATH=$(sh scripts/install-zig):$PATH
fi
case $target in
*-linux-musl)
	var=$(echo "$target" | tr - _)
	export "CC_$var=$src/scripts/zig-cc" "AR_$var=$src/scripts/zig-ar" ZIG_TARGET="${target%%-*}-linux-musl"
	;;
esac
if command -v systemd-run >/dev/null 2>&1; then
	run() { systemd-run --user --scope -q -p MemoryMax=16G -p CPUQuota=800% nice -n 10 "$@"; }
else
	run() { nice -n 10 "$@"; }
fi
echo "host $(uname -srm), $(rustc -V), $(zig version 2>/dev/null | sed 's/^/zig /'), target $target, at $(date -u +%FT%TZ)"
run cargo test -p shards --test nesting --no-run --target "$target" -j8 >no-run.log 2>&1
e=$?; echo "nesting_no_run_exit=$e"; [ $e = 0 ] || fail=$((fail + 1))
grep -aE '^(error|warning: unused)|Executable tests/nesting' no-run.log | head -20
run cargo clippy -p shards --test nesting --target "$target" -j8 -- -D warnings >clippy.log 2>&1
e=$?; echo "clippy_nesting_exit=$e"; [ $e = 0 ] || fail=$((fail + 1))
grep -aE '^(error|warning)' -A8 clippy.log | grep -av 'aws-lc-sys@' | head -60
tail -3 clippy.log
cargo fmt --all --check >fmt.log 2>&1
e=$?; echo "fmt_exit=$e"; [ $e = 0 ] || fail=$((fail + 1))
head -40 fmt.log
echo "finished at $(date -u +%FT%TZ), $fail failed"
cd ~ && rm -rf "$base" && echo "removed $base"
exit $fail
