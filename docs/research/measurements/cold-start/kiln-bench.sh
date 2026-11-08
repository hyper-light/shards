#!/bin/bash
# kiln's sandbox_launch bench, its cold phase alone, as kiln ran it on this Mac (its run 6:
# --max-load 2.0 --settle-s 20), each Shards call through `wrap` (PM M315): its arguments and
# stderr in WORK/calls, each run's Claude Code a debug file in its home, the daemon at
# SHARDS_LOG=debug in WORK/home, a home given SEED's image store and guest by clone. Holds
# the Mac's bench marker.
# Usage: kiln-bench.sh SHARDS SANDBOX_LAUNCH IMAGE SEED WORK [RUNS]
set -u
shards=$1 bench=$2 image=$3 seed=$4 work=$5 runs=${6:-60}
here=$(cd "$(dirname "$0")" && pwd)
M=${TMPDIR:-/tmp}/shards-work/BENCH_RUNNING
if [ -e "$M" ]; then echo "another bench holds the Mac: $(cat "$M")"; exit 75; fi
mkdir -p "$(dirname "$M")" "$work/home" "$work/calls" "$work/config"
echo "kiln-bench.sh: sandbox_launch cold phase x$runs, from $(date -u +%H:%M:%SZ)" > "$M"
trap 'rm -f "$M"' EXIT
for part in images guest; do
  [ -d "$seed/$part" ] && [ ! -d "$work/home/$part" ] && cp -cR "$seed/$part" "$work/home/$part"
done
uptime > "$work/conditions.txt"
env KILN_SHARDS="$here/wrap" REAL_SHARDS="$shards" CALLS="$work/calls" KILN_SHARDS_HOME="$work/home" \
  DOCKER_CONFIG="$work/config" SHARDS_TIMING=1 SHARDS_LOG=debug \
  "$bench" --image "$image" --phases cold --runs "$runs" --max-load 2.0 --settle-s 20 \
  --out "$work/sandbox_launch.json" > "$work/bench.log" 2>&1
echo "exit $?" >> "$work/bench.log"
uptime >> "$work/conditions.txt"
