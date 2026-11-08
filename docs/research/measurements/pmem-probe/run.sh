#!/usr/bin/env bash
# D147: a cold boot's time from PID 1's start to its connecting to the host (markers 1 and
# 3: the root's mount, with init's wait for libnvdimm's pmem node, and the standby's fork),
# for each root given, interleaved, N boots each; then n, p50, p90, p99 and max per root.
#   run.sh N KERNEL INIT ROOTFS...
# SHARDS names the shards to run (shards on PATH by default).
set -euo pipefail
n=$1 kernel=$2 init=$3
shift 3
shards=${SHARDS:-shards}
err=$(mktemp)
trap 'rm -f "$err"' EXIT
for i in $(seq "$n"); do
    for root in "$@"; do
        # A boot that fails is said, with what it said, and ends the measurement.
        if ! SHARDS_TIMING=1 "$shards" run --kernel "$kernel" --init "$init" --rootfs "$root" -- true \
            >/dev/null 2>"$err"; then
            echo "boot $i of $(basename "$root") failed:" >&2
            cat "$err" >&2
            exit 1
        fi
        sed -n "s|^shards-timing |$(basename "$root") |p" "$err"
    done
done | python3 -I -c '
import json, sys
by = {}
for line in sys.stdin:
    root, timing = line.split(" ", 1)
    marks = dict(json.loads(timing)["markers"])
    by.setdefault(root, []).append((marks[3] - marks[1]) / 1000)
for root, v in by.items():
    v.sort()
    q = lambda p: v[min(len(v) - 1, len(v) * p // 100)]
    print(f"{root[:24]} n={len(v)} p50={q(50):.2f} p90={q(90):.2f} p99={q(99):.2f} max={v[-1]:.2f} ms")
'
