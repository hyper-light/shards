#!/bin/sh
# ab-pull.sh OLD_BIN NEW_BIN HOMES ROUNDS REF...: ROUNDS rounds of cold pulls of each
# REF by two builds of shards, which goes first alternating, each build with its own
# home under HOMES (and so its own daemon). Prints per build and reference how many
# pulls failed, and p50, p90 and max seconds of those that did not; the first lines of
# each failure go to HOMES/failures.txt.
set -u
old=$1 new=$2 homes=$3 rounds=$4
shift 4
mkdir -p "$homes"
: >"$homes/runs.txt"
: >"$homes/failures.txt"
i=0
while [ "$i" -lt "$rounds" ]; do
    case $((i % 2)) in 0) order="old new" ;; *) order="new old" ;; esac
    for ref in "$@"; do
        for side in $order; do
            case $side in old) bin=$old ;; new) bin=$new ;; esac
            export SHARDS_HOME="$homes/$side"
            "$bin" rmi -f "$ref" >/dev/null 2>&1
            start=$(python3 -c 'import time; print(time.time())')
            "$bin" pull -q "$ref" >"$homes/last" 2>&1
            code=$?
            secs=$(python3 -c "import time; print(f'{time.time() - $start:.2f}')")
            [ "$code" -eq 0 ] || { echo "$side $ref round $i:"; tail -2 "$homes/last"; } >>"$homes/failures.txt"
            echo "$side $ref $code $secs" >>"$homes/runs.txt"
        done
    done
    i=$((i + 1))
done
for side in old new; do SHARDS_HOME="$homes/$side" "$( [ $side = old ] && echo "$old" || echo "$new")" daemon stop >/dev/null 2>&1; done
python3 - "$homes/runs.txt" <<'PY'
import sys, collections
runs = collections.defaultdict(list)
for l in open(sys.argv[1]):
    side, ref, code, secs = l.split()
    runs[(ref, side)].append((int(code), float(secs)))
def q(s, p): return s[min(len(s) - 1, int(p * len(s)))]
print("| reference | build | n | failed | p50 s | p90 s | max s |")
print("|---|---|---|---|---|---|---|")
for (ref, side), v in sorted(runs.items()):
    ok = sorted(s for c, s in v if c == 0)
    fails = sum(c != 0 for c, _ in v)
    stats = f"{q(ok,.5):.2f} | {q(ok,.9):.2f} | {ok[-1]:.2f}" if ok else "- | - | -"
    print(f"| {ref} | {side} | {len(v)} | {fails} | {stats} |")
PY
