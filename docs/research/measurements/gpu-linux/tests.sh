#!/usr/bin/env bash
# tests/gpu.rs (D201, D208) and a run's /dev/shm (D156) on a Linux host with GPUs, RUNS
# times each, the host's GPUs, drivers and load kept beside each run's verdict and time:
# the record D208's "Tested" cites. Builds the GPU's renderer first (scripts/gpu-libs)
# where OUT has none, and fetches the pinned zig where the host's is not 0.16.0.
#   docs/research/measurements/gpu-linux/tests.sh [RUNS]   (RUNS 3 by default)
# OUT (target/gpu-linux by default) receives host.txt, gpu-N.log and shm-N.log; JOBS (8
# by default) bounds cargo's jobs, for hosts others share.
set -euo pipefail
runs=${1:-3}
repo=$(cd "$(dirname "$0")/../../../.." && pwd)
cd "$repo"
out=${OUT:-$repo/target/gpu-linux}
mkdir -p "$out"
export CARGO_BUILD_JOBS=${JOBS:-8}
if [ "$(zig version 2>/dev/null || true)" != 0.16.0 ]; then
	PATH="$(scripts/install-zig):$PATH"
fi
[ -e "$out/gpu-libs/librender_server.a" ] || scripts/gpu-libs "$out/gpu-libs"
export SHARDS_GPU_LIBS=$out/gpu-libs SHARDS_REQUIRE_VMS=1
{
	date -u +%FT%TZ
	git rev-parse HEAD
	uname -srm
	grep PRETTY_NAME /etc/os-release || true
	grep -m1 'model name' /proc/cpuinfo || true
	lspci -nnk | grep -A3 -E 'VGA|3D|Display' || true
	ls -l /dev/dri /dev/nvidia* /dev/udmabuf /dev/kvm 2>&1 || true
	cat /proc/driver/nvidia/version 2>/dev/null || true
	vulkaninfo --summary 2>&1 | grep -E 'deviceName|driverName|driverInfo|apiVersion' || true
	uptime
} >"$out/host.txt"
cat "$out/host.txt"
test() {
	local name=$1 n=$2
	shift 2
	local start=$SECONDS status=0
	cargo test --locked --profile test-release -p shards "$@" -- --nocapture --test-threads 1 \
		>"$out/$name-$n.log" 2>&1 || status=$?
	echo "$name $n: exit $status, $((SECONDS - start)) s, load $(cut -d' ' -f1-3 /proc/loadavg)"
	grep -a -E '^SKIP|deviceName|^test result' "$out/$name-$n.log" | sed 's/^/  /' || true
}
for n in $(seq 1 "$runs"); do
	test gpu "$n" --test gpu
	test shm "$n" --test containers a_microvm_takes_its_ipc_hostname_cgroups_umask_and_writable_layer
done
