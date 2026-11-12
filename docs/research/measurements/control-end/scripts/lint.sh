#!/bin/sh
# Lints the harness for every target Shards supports (resources/supported-targets.txt), from
# a Mac as scripts/lint lints the workspace: macOS natively, Linux through zig, Windows through
# cargo-xwin with LLVM's clang-cl; then runs its light tests. Run from the harness's directory
# of a Shards checkout, with zig on PATH (scripts/install-zig).
#
#   scripts/lint.sh [TARGET...]
set -u
here=$(cd "$(dirname "$0")/.." && pwd)
root=$(cd "$here/../../../.." && pwd)
llvm=${LLVM_BIN:-$(brew --prefix llvm 2>/dev/null || true)/bin}
cd "$here"
failed=0
for target in ${*:-$(cat "$root/resources/supported-targets.txt")}; do
	var=$(printf %s "$target" | tr - _)
	case $target in
	*-linux-gnu | *-linux-musl)
		env "CC_$var=$root/scripts/zig-cc" "AR_$var=$root/scripts/zig-ar" \
			ZIG_TARGET="${target%%-*}-linux-${target##*-}" \
			cargo clippy --all-targets --target "$target" -- -D warnings
		;;
	*-windows-msvc)
		PATH="$llvm:$PATH" cargo xwin clippy --all-targets --target "$target" -- -D warnings
		;;
	*)
		cargo clippy --all-targets --target "$target" -- -D warnings
		;;
	esac
	status=$?
	echo "LINT $target: exit $status"
	[ $status -eq 0 ] || failed=1
done
cargo test
status=$?
echo "TEST: exit $status"
[ $status -eq 0 ] || failed=1
exit $failed
