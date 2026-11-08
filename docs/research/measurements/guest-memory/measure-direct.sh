#!/bin/sh
# M117 on any host, from an unchanged shards: what the guest kernel keeps of a microVM's
# memory by its size. A first run stores the pinned guest and the image's root filesystem;
# each size is then booted at exactly that size (`run --kernel … --rootfs … --memory`),
# three times, and the guest's MemTotal and MemAvailable read. Prints, per run, the VM's
# MiB and KiB, MemTotal, MemAvailable and the overhead (the VM's KiB less MemAvailable).
#
#   SHARDS_HOME=$(mktemp -d) docs/research/measurements/guest-memory/measure-direct.sh target/release/shards
set -eu
shards=$1
image=alpine@sha256:294b683cb724975bec92580e1e685676bd4b50bda910ddb8c51d4cabeaec77e6
"$shards" run --rm "$image" true
digest() { "$shards" guest | awk -v k="$1" '$1 == k { sub(":", "-", $2); print $2 }'; }
kernel="$SHARDS_HOME/guest/$(digest kernel)"
init="$SHARDS_HOME/guest/$(digest init)"
rootfs=$(find "$SHARDS_HOME/images/rootfs" -name '*.erofs' | head -1)
echo "host $(uname -sm) kernel $(digest kernel)"
echo "mib vm_kib memtotal_kib memavailable_kib overhead_kib"
for mib in 256 384 512 768 1024 1536 2048 3072 3584 4096 4608 6144 8192 12288 16384; do
  for _ in 1 2 3; do
    "$shards" run --kernel "$kernel" --init "$init" --rootfs "$rootfs" --memory "$mib" -- \
      awk -v mib="$mib" '/^MemTotal/ {t=$2} /^MemAvailable/ {a=$2}
        END {printf "%d %d %d %d %d\n", mib, mib*1024, t, a, mib*1024-a}' /proc/meminfo
  done
done
