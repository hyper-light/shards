#!/bin/sh
# Vulkan programs in a guest given a Linux host's GPUs (D201, PM M384): llama.cpp's
# test-backend-ops, in test mode, checks each matrix multiplication case on each Venus
# device against the CPU's result; then in perf mode times them, and llama-bench runs
# TinyLlama on each. The image is E6's (../venus/Dockerfile, D144's M218), with 8 compiler
# jobs (Dockerfile here), built in a microVM of the build's own; or IMAGE, one built
# elsewhere and loaded (`shards load`).
#   [IMAGE=NAME] compute.sh SHARDS OUT_DIR
# On Windows, from Git Bash or MSYS2, whose shells would make the guest's paths (/llama.cpp)
# Windows paths of their own before shards sees them (PM M386).
set -eu
export MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*'
shards=$1 out=$2
mkdir -p "$out"
here=$(cd "$(dirname "$0")" && pwd)
# The build's context as Windows names it there, where shards takes no path of the shell's.
if command -v cygpath >/dev/null; then here=$(cygpath -m "$here"); fi
{ uname -a; nvidia-smi --query-gpu=name,driver_version --format=csv,noheader 2>/dev/null || true; } >"$out/host.txt"
image=${IMAGE:-shards-e6:1}
[ -n "${IMAGE:-}" ] || "$shards" build -q -t "$image" "$here" >/dev/null
guest() { "$shards" run --rm --gpus all -m 8g "$image" "$@"; }
guest vulkaninfo --summary >"$out/vulkaninfo.txt" 2>&1
for device in Vulkan0 Vulkan1; do
	guest /llama.cpp/build/bin/test-backend-ops test -o MUL_MAT -b "$device" >"$out/test-$device.txt" 2>&1 || true
	guest /llama.cpp/build/bin/test-backend-ops perf -o MUL_MAT -b "$device" >"$out/perf-$device.txt" 2>&1 || true
done
# Each device alone (ggml's Vulkan backend's own variable).
for n in 0 1; do
	"$shards" run --rm --gpus all -m 8g -e GGML_VK_VISIBLE_DEVICES=$n "$image" \
		/llama.cpp/build/bin/llama-bench -m /model.gguf -ngl 99 -r 5 -o json >"$out/bench-Vulkan$n.json" 2>"$out/bench-Vulkan$n.err" || true
done
for device in Vulkan0 Vulkan1; do
	printf '%s: ' "$device"
	grep -E '[0-9]+/[0-9]+ tests passed|Backend .*: (OK|FAIL)' "$out/test-$device.txt" | tail -2 | tr '\n' ' '
	echo
done
