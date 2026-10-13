#!/usr/bin/env bash
# E6, a VM's memory as a user's runs have it (D144): `shards run` of the E6 image with the
# GPU (`--gpus all`, a cold boot in the GPU's VM process) and without (its template
# restored), interleaved, N times; for each command, the VM process's RSS and footprint
# as the run ends (SHARDS_TIMING), as n, p50, p90, p99 and max in MiB. The image is
# run.sh's (shards-e6:1). A run that fails is said, with what it said, and ends it.
#   run-memory.sh N
set -euo pipefail
n=$1
shards=${SHARDS:-shards}
err=$(mktemp)
trap 'rm -f "$err"' EXIT
for i in $(seq "$n"); do
    for gpu in with without; do
        flags=()
        [ "$gpu" = with ] && flags=(--gpus all)
        for cmd in true "vulkaninfo --summary"; do
            # shellcheck disable=SC2086 # the command's words
            if ! SHARDS_TIMING=1 "$shards" run --rm ${flags[@]+"${flags[@]}"} shards-e6:1 $cmd >/dev/null 2>"$err"; then
                echo "run $i, $gpu the GPU, of $cmd failed:" >&2
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
    restored = 2 in dict(t["markers"])
    by.setdefault((gpu, cmd, "restored" if restored else "booted"), []).append((t["rss_kib"], t["footprint_kib"]))
for (gpu, cmd, how), v in sorted(by.items()):
    for i, name in ((0, "rss"), (1, "footprint")):
        s = sorted(x[i] / 1024 for x in v)
        q = lambda p: s[min(len(s) - 1, len(s) * p // 100)]
        print(f"{gpu:7} {cmd:10} {how:8} {name:9} n={len(s)} p50={q(50):.1f} p90={q(90):.1f} p99={q(99):.1f} max={s[-1]:.1f} MiB")
'
