#!/usr/bin/env bash
# Light gates for the hex-upstream-v1 port (implementer scope, R15): format, strict clippy and
# unit tests of the crates the ported Hex patches touch, on x86_64 Linux. Run from the
# repository root on the assigned kiln runner, never while amphora-bench runs:
#   docs/research/measurements/hex-upstream-v1/gate.sh LABEL STEP...
# STEP: fmt | policy | clippy-api | clippy-vmm | clippy-vmm-gpu | clippy-shards | clippy-musl
#       | test-api | test-abi | test-gpu-device | test-shards-unit | check
# Each step's whole output goes to $HOME/g121-tmp/hex-upstream-v1/LABEL/STEP.log; this prints
# one line per step: its exit status and the log's last summary lines.
set -u
label=${1:?label}
shift
if pgrep -x amphora-bench >/dev/null; then
	echo "amphora-bench is running: not starting" >&2
	exit 75
fi
export TMPDIR=$HOME/g121-tmp
export PATH=$HOME/.local/share/shards-zig/zig-x86_64-linux-0.16.0:$PATH
export SHARDS_HELPERS=skip
root=$(pwd)
gnu=x86_64-unknown-linux-gnu
musl=x86_64-unknown-linux-musl
zig_env() {
	local var
	var=$(printf %s "$1" | tr - _)
	echo "CC_$var=$root/scripts/zig-cc AR_$var=$root/scripts/zig-ar ZIG_TARGET=x86_64-linux-${1##*-}"
}
out=$HOME/g121-tmp/hex-upstream-v1/$label
mkdir -p "$out"
run() {
	local step=$1
	shift
	local log=$out/$step.log
	{ echo "\$ $*"; git rev-parse HEAD; git status --porcelain | head -20; } >"$log"
	systemd-run --user --scope -q -p MemoryMax=24G env $(zig_env $gnu) "$@" >>"$log" 2>&1
	local status=$?
	printf '%-18s exit %s  ' "$step" "$status"
	grep -E '^test result:|^error(\[|:)|warning: unused|FAILED|panicked' "$log" | tail -4 | tr '\n' ' '
	echo
}
for step in "$@"; do
	case $step in
	fmt) run fmt cargo fmt --all --check ;;
	policy) run policy python3 scripts/check-development-policy ;;
	check) run check cargo check -j4 --target $gnu -p shards-api -p shards-abi -p shards-vmm --all-targets ;;
	clippy-api) run clippy-api cargo clippy -j4 --target $gnu -p shards-api -p shards-abi --all-targets -- -D warnings ;;
	clippy-vmm) run clippy-vmm cargo clippy -j4 --target $gnu -p shards-vmm --all-targets -- -D warnings ;;
	clippy-vmm-gpu) run clippy-vmm-gpu cargo clippy -j4 --target $gnu -p shards-vmm --features gpu --all-targets -- -D warnings ;;
	clippy-shards) run clippy-shards cargo clippy -j4 --target $gnu -p shards --all-targets -- -D warnings ;;
	clippy-musl) run clippy-musl env $(zig_env $musl) cargo clippy -j4 --target $musl -p shards-abi -p shards-init --all-targets -- -D warnings ;;
	test-api) run test-api cargo test --no-fail-fast -j4 --target $gnu -p shards-api ;;
	test-abi) run test-abi cargo test --no-fail-fast -j4 --target $gnu -p shards-abi ;;
	test-gpu-device) run test-gpu-device cargo test --no-fail-fast -j4 --target $gnu -p shards-vmm --lib devices::virtio::gpu:: ;;
	test-shards-unit) run test-shards-unit cargo test --no-fail-fast -j4 --target $gnu -p shards --bin shards ;;
	*) echo "unknown step $step" >&2 ;;
	esac
done
