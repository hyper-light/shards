#!/bin/sh
# M22 (docs/research/platform-measurements.md): templated `shards run` with the threads on
# its request path at default QoS and at user-interactive QoS, interleaved. qos.patch adds
# the switch (SHARDS_M22_QOS=1): the main thread from `shards run` on, the workload relay
# and the virtio-vsock worker. It is built in a temporary worktree, so the measured code is
# this revision's plus the switch.
#
#   run.sh TEMPLATES PAIRS KERNEL INIT IMAGE [--load]
#
# SHARDS_HOME must hold IMAGE (`shards pull`); this records KERNEL and INIT as its guest
# and replaces its templates. Each template is saved, restored once, then run PAIRS times
# in each mode. --load keeps every CPU busy with `yes` meanwhile. macOS only.
set -eu

templates=$1 pairs=$2 kernel=$3 init=$4 image=$5 load=${6:-}
repo=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
tree=$(mktemp -d)
yes_pids=""
cleanup() {
	[ -n "$yes_pids" ] && kill $yes_pids 2>/dev/null
	git -C "$repo" worktree remove --force "$tree" 2>/dev/null || true
}
trap cleanup EXIT INT TERM
git -C "$repo" worktree add --detach "$tree" HEAD >/dev/null
git -C "$tree" apply "$repo/docs/research/measurements/service-qos/qos.patch"
CARGO_TARGET_DIR="$repo/target/m22" cargo build --release --quiet -p shards --manifest-path "$tree/Cargo.toml"
shards="$repo/target/m22/shards-m22"
cp "$repo/target/m22/release/shards" "$shards"
codesign --entitlements "$repo/resources/hvf.entitlements" --force -s - "$shards" 2>/dev/null
"$shards" guest use --kernel "$kernel" --init "$init" >/dev/null
if [ "$load" = --load ]; then
	i=0
	while [ $i -lt "$(getconf _NPROCESSORS_ONLN)" ]; do
		yes >/dev/null &
		yes_pids="$yes_pids $!"
		i=$((i + 1))
	done
fi
python3 - "$shards" "$image" "$templates" "$pairs" <<'PY'
import json, os, shutil, sys, time
shards, image, templates, pairs = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4])
home = os.environ["SHARDS_HOME"]

def run(qos):
    env = dict(os.environ, SHARDS_TIMING="1", SHARDS_M22_QOS="1" if qos else "0")
    r, w = os.pipe()
    start = time.perf_counter()
    pid = os.posix_spawn(shards, [shards, "run", "--pull", "never", image], env, file_actions=[
        (os.POSIX_SPAWN_DUP2, w, 2),
        (os.POSIX_SPAWN_OPEN, 1, "/dev/null", os.O_WRONLY, 0),
        (os.POSIX_SPAWN_OPEN, 0, "/dev/null", os.O_RDONLY, 0)])
    os.close(w)
    err = b""
    while chunk := os.read(r, 65536):
        err += chunk
    os.close(r)
    _, status = os.waitpid(pid, 0)
    wall = (time.perf_counter() - start) * 1e6
    if os.waitstatus_to_exitcode(status) != 0:
        sys.exit(f"shards run failed: {err.decode(errors='replace')}")
    t = json.loads(next(l for l in err.decode().splitlines() if l.startswith("shards-timing ")).split(" ", 1)[1])
    return {"wall": wall, "restore": t["released_us"], "command": t["exit_us"] - t["released_us"],
            "process": wall - t["exit_us"]}

samples = {False: [], True: []}
for _ in range(templates):
    shutil.rmtree(os.path.join(home, "templates"), ignore_errors=True)
    run(False)  # saves the template
    run(False)  # its first restore
    for i in range(pairs):
        for qos in (False, True) if i % 2 == 0 else (True, False):
            samples[qos].append(run(qos))

def rank(v, p):
    v = sorted(v)
    return v[max(0, -(-len(v) * p // 100) - 1)]

for qos in (False, True):
    rows = samples[qos]
    cells = [f"{k} p50 {rank([r[k] for r in rows], 50) / 1000:.2f} p90 {rank([r[k] for r in rows], 90) / 1000:.2f} "
             f"p99 {rank([r[k] for r in rows], 99) / 1000:.2f} max {max(r[k] for r in rows) / 1000:.2f}"
             for k in ("wall", "restore", "command", "process")]
    print(("user-interactive" if qos else "default") + f" n={len(rows)} ms: " + " | ".join(cells))
PY
