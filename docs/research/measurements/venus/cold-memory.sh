#!/usr/bin/env bash
# E6, the renderer's memory per VM (D144): cold boots of one kernel, init and root, with
# the GPU (`--gpu`, the GPU's VM process) and without (the VM process), interleaved; for
# each command, the VM process's RSS and footprint as the run ends (SHARDS_TIMING), as n,
# p50, p90, p99 and max in MiB. A run that fails is said, with what it said, and ends it.
#   cold-memory.sh N KERNEL INIT ROOTFS
# SHARDS names the shards to run (shards on PATH by default): one that carries the GPU's
# VM process.
set -euo pipefail
n=$1 kernel=$2 init=$3 root=$4
shards=${SHARDS:-shards}
err=$(mktemp)
trap 'rm -f "$err"' EXIT
for i in $(seq "$n"); do
    for gpu in with without; do
        flags=()
        [ "$gpu" = with ] && flags=(--gpu)
        for cmd in true "vulkaninfo --summary"; do
            # shellcheck disable=SC2086 # the command's words
            if ! SHARDS_TIMING=1 "$shards" run --kernel "$kernel" --init "$init" ${flags[@]+"${flags[@]}"} \
                --rootfs "$root" -- $cmd >/dev/null 2>"$err"; then
                echo "boot $i, $gpu the GPU, of $cmd failed:" >&2
                cat "$err" >&2
                exit 1
            fi
            sed -n "s/^shards-timing /$gpu ${cmd%% *} /p" "$err"
        done
    done
done | python3 -I -c '
import json, sys
by = {}
for line in sys.stdin:
    gpu, cmd, timing = line.split(" ", 2)
    t = json.loads(timing)
    by.setdefault((gpu, cmd), []).append((t["rss_kib"], t["footprint_kib"]))
for (gpu, cmd), v in sorted(by.items()):
    for i, name in ((0, "rss"), (1, "footprint")):
        s = sorted(x[i] / 1024 for x in v)
        q = lambda p: s[min(len(s) - 1, len(s) * p // 100)]
        print(f"{gpu:7} {cmd:10} {name:9} n={len(s)} p50={q(50):.1f} p90={q(90):.1f} p99={q(99):.1f} max={s[-1]:.1f} MiB")
'
