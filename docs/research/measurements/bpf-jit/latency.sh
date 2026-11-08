#!/bin/bash
# PM M462 (D215): start latency, request to status, through each home's pools
# (build-ab/ab.py, `run --rm IMAGE true`), over SEGMENTS templates an arm: each segment's
# daemons stopped and templates removed first, so that each restores a template of its own
# (restore costs differ from template to template, PM M29).
#
#     latency.sh SHARDS HOMES OUT [N] [SEGMENTS]
#
# HOMES is fleet.py's: HOMES/A and HOMES/B hold the image and their guests. OUT.S.json
# keeps segment S's samples; the pooled summary goes to stdout.
set -e
shards=$1 homes=$2 out=$3 n=${4:-100} segments=${5:-4}
ab=$(cd "$(dirname "$0")" && pwd)/../build-ab/ab.py
dirs=$(mktemp -d)
mkdir -p "$dirs/cfg"
for arm in A B; do
  mkdir -p "$dirs/$arm"
  ln -s "$shards" "$dirs/$arm/shards"
  ln -s "$(cd "$homes/$arm" && pwd)" "$dirs/$arm/home"
done
image=alpine@sha256:294b683cb724975bec92580e1e685676bd4b50bda910ddb8c51d4cabeaec77e6
stop() {
  for arm in A B; do
    SHARDS_HOME=$dirs/$arm/home DOCKER_CONFIG=$dirs/cfg "$shards" stop daemon >/dev/null 2>&1 || true
  done
}
for s in $(seq 1 "$segments"); do
  stop
  for arm in A B; do rm -rf "$dirs/$arm/home/templates"; done
  echo "segment $s"
  DOCKER_CONFIG=$dirs/cfg AB_FLAGS=--rm AB_DUMP=$out.$s.json python3 "$ab" "$dirs/A" "$dirs/B" "$image" "$n" true
done
stop
rm -rf "$dirs"
# Every segment's samples pooled: n, p50, p90, p99 and max per arm, and the median of the
# paired differences (B less A, turn by turn).
python3 - "$out" "$segments" <<'PY'
import json, math, sys
out, segments = sys.argv[1], int(sys.argv[2])
turns = {"old": [], "new": []}
for s in range(1, segments + 1):
    d = json.load(open(f"{out}.{s}.json"))
    for k in turns:
        turns[k] += [t[0] for t in d[k]]
def q(v, f):
    v = sorted(v)
    return v[min(len(v), max(1, math.ceil(f * len(v)))) - 1]
print(f"pooled over {segments} templates an arm (A = old, B = new)")
for part, of in (("wall", lambda w, c: w), ("command", lambda w, c: c), ("outside", lambda w, c: w - c)):
    for k, arm in (("old", "A"), ("new", "B")):
        v = [of(*t) for t in turns[k]]
        print(f"{part:8} {arm} n {len(v)} p50 {q(v, .5):.0f} p90 {q(v, .9):.0f} p99 {q(v, .99):.0f} max {max(v):.0f} us")
    d = [of(*b) - of(*a) for a, b in zip(turns["old"], turns["new"])]
    print(f"{part:8} B - A, paired: median {q(d, .5):+.0f} us")
PY
