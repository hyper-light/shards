#!/usr/bin/env bash
# E6 (docs/research/gpu-accelerators.md, D144): Venus in a guest against MoltenVK on this
# Mac, the same code on each side: Vulkan matrix multiplication (llama.cpp's
# test-backend-ops), llama.cpp's tokens/s (llama-bench, each repetition kept), and the
# steps of host-visible memory (blobs.c). Both sides are built first, then measured in
# ROUNDS rounds, each side in turn, so that a host busy with other work weighs on both
# alike; each round records the host's load. compare.py reads OUT_DIR.
#   run.sh OUT_DIR [ROUNDS]
# SHARDS names the shards that runs the guest's side (one that carries shards-vm-gpu, or
# with SHARDS_VM_BINARY naming it, and SHARDS_KERNEL and SHARDS_INIT where the gpu kernel
# is not pinned). The host's side needs cmake, glslc and SPIRV-Headers (brew install
# cmake shaderc spirv-headers). cold-memory.sh measures the renderer's memory.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
out=${1:?usage: run.sh OUT_DIR [ROUNDS]}
rounds=${2:-3}
mkdir -p "$out"
out=$(cd "$out" && pwd)
shards=${SHARDS:-shards}
llama=8345f333951c661d166b00e6f9362e553768f292 # v0.6.0, as the Dockerfile's
mvk_url=https://github.com/KhronosGroup/MoltenVK/releases/download/v1.4.2/MoltenVK-macos.tar
mvk_sha256=f95765a6229cb7b915990a2890ce12ebe36a730b021545d3d52ae69ce4c4024e
model_url=https://huggingface.co/TheBloke/TinyLlama-1.1B-Chat-v1.0-GGUF/resolve/main/tinyllama-1.1b-chat-v1.0.Q4_0.gguf
model_sha256=da3087fb14aede55fde6eb81a0e55e886810e43509ec82ecdc7aa5d62a03b556

{
    uname -a
    sw_vers
    sysctl -n machdep.cpu.brand_string hw.memsize
    git -C "$here" rev-parse HEAD
} >"$out/host.txt"

# The guest's side: its image.
"$shards" build -q -t shards-e6:1 "$here" >/dev/null
# 8 GiB: llama.cpp's tests allocate past a run's default memory, which is fixed at boot.
guest() { "$shards" run --rm --gpus all -m 8g shards-e6:1 "$@"; }

# The host's side, through MoltenVK alone.
work=$out/host
mkdir -p "$work/mvk"
curl -fsSL --retry 3 -o "$work/mvk.tar" "$mvk_url"
echo "$mvk_sha256  $work/mvk.tar" | shasum -a 256 -c --quiet -
tar -xf "$work/mvk.tar" -C "$work/mvk"
mvk=$work/mvk/MoltenVK/MoltenVK
git init -q "$work/llama.cpp"
git -C "$work/llama.cpp" fetch -q --depth 1 https://github.com/ggml-org/llama.cpp "$llama"
git -C "$work/llama.cpp" checkout -q FETCH_HEAD
cmake -S "$work/llama.cpp" -B "$work/build" -DGGML_VULKAN=ON -DGGML_METAL=OFF -DGGML_BLAS=OFF \
    -DLLAMA_CURL=OFF -DCMAKE_BUILD_TYPE=Release -DCMAKE_PREFIX_PATH="$(brew --prefix)" \
    -DCMAKE_CXX_FLAGS="-I$(brew --prefix spirv-headers)/include" \
    -DVulkan_INCLUDE_DIR="$mvk/include" -DVulkan_LIBRARY="$mvk/dynamic/dylib/macOS/libMoltenVK.dylib" \
    -DVulkan_GLSLC_EXECUTABLE="$(command -v glslc)" >/dev/null
cmake --build "$work/build" -j --target llama-bench test-backend-ops >/dev/null
cc -O2 -I"$mvk/include" -o "$work/blobs" "$here/blobs.c" "$mvk/dynamic/dylib/macOS/libMoltenVK.dylib" \
    -Wl,-rpath,"$mvk/dynamic/dylib/macOS"
curl -fsSL --retry 3 -o "$work/model.gguf" "$model_url"
echo "$model_sha256  $work/model.gguf" | shasum -a 256 -c --quiet -
export MVK_CONFIG_LOG_LEVEL=1
host() { "$@"; }

# Each round, each benchmark on each side in turn.
for r in $(seq "$rounds"); do
    echo "round $r load $(sysctl -n vm.loadavg)" >>"$out/load.txt"
    guest /blobs 200 >"$out/guest-blobs-$r.txt"
    host "$work/blobs" 200 >"$out/host-blobs-$r.txt"
    guest /llama.cpp/build/bin/test-backend-ops perf -o MUL_MAT -b Vulkan0 >"$out/guest-mul-mat-$r.txt"
    host "$work/build/bin/test-backend-ops" perf -o MUL_MAT -b Vulkan0 >"$out/host-mul-mat-$r.txt"
    guest /llama.cpp/build/bin/llama-bench -m /model.gguf -ngl 99 -r 5 -o json >"$out/guest-bench-$r.json"
    host "$work/build/bin/llama-bench" -m "$work/model.gguf" -ngl 99 -r 5 -o json >"$out/host-bench-$r.json"
    echo "round $r done, load $(sysctl -n vm.loadavg)" >>"$out/load.txt"
done
