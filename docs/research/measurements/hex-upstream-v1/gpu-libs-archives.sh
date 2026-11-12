#!/usr/bin/env bash
# Hex's GPU patch 0007 (R09/R10): builds the renderer's libraries with this tree's
# scripts/gpu-libs into a fresh directory, says each archive's kind by its first bytes
# ("!<arch>" whole, "!<thin>" a thin archive, whose members name files elsewhere), then links
# shards-vmm's GPU lib tests against them and runs the GPU tests that need a host GPU.
#   docs/research/measurements/hex-upstream-v1/gpu-libs-archives.sh LABEL
set -u
label=${1:?label}
if pgrep -x amphora-bench >/dev/null; then echo "amphora-bench is running: not starting" >&2; exit 75; fi
export TMPDIR=$HOME/g121-tmp
out=$HOME/g121-tmp/hex-upstream-v1/$label
libs=$out/gpu-libs
rm -rf "$libs"
mkdir -p "$out"
{ git rev-parse HEAD; git status --porcelain | head; sha256sum scripts/gpu-libs resources/gpu/*; meson --version 2>/dev/null; ar --version | head -1; } > "$out/identity.txt" 2>&1
if ! scripts/gpu-libs "$libs" > "$out/gpu-libs.log" 2>&1; then echo "gpu-libs failed"; tail -15 "$out/gpu-libs.log"; exit 1; fi
for a in "$libs"/*.a; do
	printf '%s: %s, %s bytes\n' "$(basename "$a")" "$(head -c 7 "$a")" "$(stat -c %s "$a")"
done
export SHARDS_GPU_LIBS=$libs
systemd-run --user --scope -q -p MemoryMax=24G cargo test -j4 --target x86_64-unknown-linux-gnu -p shards-vmm --features gpu --lib --no-run > "$out/link.log" 2>&1
echo "link of shards-vmm's GPU tests: exit $?"
grep -E "failed to open object file|could not find|error:|undefined reference" "$out/link.log" | head -5
