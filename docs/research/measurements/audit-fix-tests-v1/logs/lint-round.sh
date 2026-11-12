#!/bin/sh
# Lint round (Linux runner, worktree root): clippy -D warnings for the touched test targets
# and the test guest, on the glibc host and on both guest targets (musl, through the pinned
# zig 0.16.0 that scripts/install-zig fetches and checks by digest), and the guest build.
. docs/research/measurements/audit-fix-tests-v1/logs/runner-prelude.sh
out=$(mktemp)
git rev-parse HEAD
git status --short
step() {
	echo "== $1"
	shift
	echo "\$ $*"
	"$@" >"$out" 2>&1
	status=$?
	grep -v '^warning: aws-lc-sys\|^   Compiling \|^    Checking ' "$out"
	echo "[exit $status]"
}
step clippy-shards-tests cargo clippy -p shards -j2 --test managed_output --test managed_network --test managed_limits --test managed_cleanup --test build -- -D warnings
step clippy-testguest-host cargo clippy -p shards-testguest -j2 --all-targets -- -D warnings
zig=$(scripts/install-zig) || exit 1
PATH="$zig:$PATH"
export PATH
echo "zig $(zig version) at $zig"
root=$(pwd)
for arch in x86_64 aarch64; do
	target=$arch-unknown-linux-musl
	var=$(printf %s "$target" | tr - _)
	step "clippy-testguest-$target" env "CC_$var=$root/scripts/zig-cc" "AR_$var=$root/scripts/zig-ar" ZIG_TARGET="$arch-linux-musl" cargo clippy -p shards-testguest -j2 --all-targets --target "$target" -- -D warnings
done
step guest-build-x86_64-musl env "CC_x86_64_unknown_linux_musl=$root/scripts/zig-cc" "AR_x86_64_unknown_linux_musl=$root/scripts/zig-ar" ZIG_TARGET=x86_64-linux-musl cargo build -p shards-testguest -j2 --profile guest --target x86_64-unknown-linux-musl
rm -f "$out"
