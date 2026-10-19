#!/bin/sh
# What a virtio-fs DAX window costs a guest as it boots, by its length (D189): the page
# structures Linux makes of it as it probes the device (virtio_fs_setup_dax,
# devm_memremap_pages of MEMORY_DEVICE_FS_DAX), a 64th of it. Each boot is the test guest
# as PID 1 under shards' VMM (testguest `memory`), with 512 MiB and one shared directory
# whose device offers a window of each LENGTH MiB (0: none); it says MemTotal, MemFree,
# MemAvailable and its uptime as it begins. ROUNDS boots of each, interleaved.
# Usage: measure.sh SHARDS KERNEL TESTGUEST ROUNDS LENGTH_MIB...
#   (one line a boot: LENGTH ROUND TOTAL FREE AVAILABLE UPTIME, bytes and seconds)
set -eu
shards=$1 kernel=$2 guest=$3 rounds=$4
shift 4
for round in $(seq "$rounds"); do
    for len in "$@"; do
        line=$("$shards" run --kernel "$kernel" --init "$guest" --memory 512 --shares 1 \
            --share-window "$len" --cmdline "console=ttyS0 panic=-1 shards_test=memory" \
            </dev/null 2>/dev/null | tr -d '\r' | grep -a 'SHARDS-TEST MEMORY' || echo "SHARDS-TEST MEMORY none")
        echo "$len $round ${line#SHARDS-TEST MEMORY }"
    done
done
