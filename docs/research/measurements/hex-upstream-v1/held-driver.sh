#!/usr/bin/env bash
# Hex's GPU patch 0008 (R09/R10), on a Linux host with an NVIDIA GPU and its Vulkan driver:
# a render-server context opens the host's Vulkan loader and makes an instance, closing both as
# it ends; unless something holds the driver loaded, each context loads it anew, and NVIDIA's
# libnvidia-tls (DF_STATIC_TLS) takes static TLS each time. Builds the renderer's libraries
# (scripts/gpu-libs, once) and shards-vmm's GPU lib tests twice:
#   red:   virgl.rs as committed (HEAD) with g0008-tests-only.patch, Hex's test alone;
#   green: the working tree's virgl.rs, the renderer holding the driver (HeldDriver).
# Runs the test RUNS times with each, and the renderer's own test once with green.
# Restores the working tree's virgl.rs before it ends. From the repository root:
#   docs/research/measurements/hex-upstream-v1/held-driver.sh LABEL [RUNS]
set -u
label=${1:?label}
runs=${2:-5}
if pgrep -x amphora-bench >/dev/null; then echo "amphora-bench is running: not starting" >&2; exit 75; fi
export TMPDIR=$HOME/g121-tmp
mkdir -p "$TMPDIR"
root=$HOME/g121-tmp/hex-upstream-v1
out=$root/$label
mkdir -p "$out"
libs=$root/gpu-libs-$(git rev-parse --short HEAD:scripts/gpu-libs)
if [ ! -f "$libs/librender_server.a" ]; then
	scripts/gpu-libs "$libs" >"$out/gpu-libs.log" 2>&1 || { echo "gpu-libs failed"; tail -20 "$out/gpu-libs.log"; exit 1; }
fi
export SHARDS_GPU_LIBS=$libs
source=crates/vmm/src/devices/virtio/gpu/virgl.rs
cp "$source" "$out/green-virgl.rs"
git show HEAD:"$source" > "$out/red-virgl.rs"
patch -s "$out/red-virgl.rs" < docs/research/measurements/hex-upstream-v1/g0008-tests-only.patch || exit 1
restore() { cp "$out/green-virgl.rs" "$source"; touch "$source"; }
trap restore EXIT
{ git rev-parse HEAD; uname -r; nvidia-smi --query-gpu=name,driver_version --format=csv,noheader; ls /usr/share/vulkan/icd.d/; sha256sum "$out"/*-virgl.rs "$libs"/*.a; } > "$out/identity.txt" 2>&1
test=devices::virtio::gpu::virgl::tests::a_contexts_instance_loads_no_static_tls_driver_anew_while_the_renderer_holds_it
for kind in red green; do
	cp "$out/$kind-virgl.rs" "$source"
	touch "$source"
	bin=$(systemd-run --user --scope -q -p MemoryMax=24G cargo test -j4 --target x86_64-unknown-linux-gnu -p shards-vmm --features gpu --lib --no-run --message-format=json 2>"$out/$kind-build.stderr" |
		python3 -c 'import json,sys
for l in sys.stdin:
    try: m=json.loads(l)
    except Exception: continue
    if m.get("reason")=="compiler-artifact" and m.get("executable") and m["target"]["kind"]==["lib"]: print(m["executable"])' | tail -1)
	if [ -z "$bin" ]; then echo "$kind: build failed"; grep -E '^error' -A6 "$out/$kind-build.stderr" | head -30; continue; fi
	cp "$bin" "$out/$kind.bin"
	pass=0
	for i in $(seq 1 "$runs"); do
		if "$out/$kind.bin" --exact "$test" --nocapture > "$out/$kind-run$i.log" 2>&1; then pass=$((pass + 1)); fi
	done
	echo "$kind: $pass of $runs runs passed"
	grep -h -E 'SKIP|loaded .* anew|unloaded|test result' "$out/$kind-run1.log" | head -5
done
"$out/green.bin" --exact devices::virtio::gpu::virgl::tests::the_renderer_offers_venus_and_holds_its_blobs --nocapture > "$out/green-renderer.log" 2>&1
echo "green renderer test: exit $? $(grep -h 'test result' "$out/green-renderer.log")"
