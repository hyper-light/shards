#!/bin/sh
# Where a DAX window's boot memory goes on this host (D189, PM M307): the test guest idling
# (testguest `idle`), 512 MiB, one share with a window of LEN MiB (0 none), and its VM
# process's smaps, and smaps_rollup's totals, once the guest is up.
# Usage: smaps.sh SHARDS KERNEL TESTGUEST LEN OUT
set -u
shards=$1 kernel=$2 guest=$3 len=$4 out=$5
log=$(mktemp)
"$shards" run --kernel "$kernel" --init "$guest" --memory 512 --shares 1 --share-window "$len" \
    --cmdline "console=ttyS0 panic=-1 shards_test=idle" </dev/null >"$log" 2>&1 &
run=$!
for _ in $(seq 200); do
    grep -aq "SHARDS-TEST READY" "$log" && break
    sleep 0.05
done
sleep 0.5
pid=$(pgrep -f "shards-vm .*$kernel" | head -1)
[ -n "$pid" ] || pid=$run
cat "/proc/$pid/smaps" >"$out"
grep -E "^(Rss|Pss|AnonHugePages)" "/proc/$pid/smaps_rollup" >>"$out.rollup"
kill "$run" 2>/dev/null; kill "$pid" 2>/dev/null
wait "$run" 2>/dev/null
rm -f "$log"
