#!/bin/sh
# M117: what the guest kernel keeps of a microVM's memory for itself. For each size, runs
# a workload with `-m SIZE` (which, with resources.rs's table emptied, boots a VM of that
# size) and prints the VM's KiB, the guest's MemTotal and MemAvailable, and the overhead:
# the VM's KiB less MemAvailable. Runs each size three times.
#
#   SHARDS_HOME=$(mktemp -d) docs/research/measurements/guest-memory/measure.sh target/release/shards
set -eu
shards=${1:-shards}
image=alpine@sha256:294b683cb724975bec92580e1e685676bd4b50bda910ddb8c51d4cabeaec77e6
echo "mib vm_kib memtotal_kib memavailable_kib overhead_kib"
for mib in 256 384 512 768 1024 1536 2048 3072 3584 4096 4608 6144 8192 12288 16384; do
  for _ in 1 2 3; do
    "$shards" run --rm -m "${mib}m" "$image" \
      awk -v mib="$mib" '/^MemTotal/ {t=$2} /^MemAvailable/ {a=$2}
        END {printf "%d %d %d %d %d\n", mib, mib*1024, t, a, mib*1024-a}' /proc/meminfo
  done
done
