#!/bin/sh
# What the virtio-mem driver gives back of memory a guest plugged, used and freed, by how
# plugged memory comes online (D188: a run asks to unplug what it holds past twice what it
# uses; D133: runs' plugged memory comes online as the kernel's own). Each boot is the test
# guest as PID 1 under shards' VMM (testguest `unplug-cost`): it plugs PLUG MiB, fills
# seven eighths of it with its own pages and, between them, FILES files of a page in a
# tmpfs (the kernel's inodes and dentries among them), frees everything, asks for none,
# and says how much is still plugged 0.1 to 30 s after. Each policy ROUNDS times,
# interleaved:
#   kernel       memhp_default_state=online_kernel (runs' today)
#   auto-movable memhp_default_state=online memory_hotplug.online_policy=auto-movable
#   movable      memhp_default_state=online_movable
# Usage: measure.sh SHARDS KERNEL TESTGUEST ROUNDS PLUG_MIB FILES
#   (one line a boot: POLICY ROUND PLUGGED MS:LEFT..., bytes)
set -eu
shards=$1 kernel=$2 guest=$3 rounds=$4 plug=$5 files=$6
memory=256
for round in $(seq "$rounds"); do
    for policy in kernel auto-movable movable; do
        case $policy in
        kernel) online="memhp_default_state=online_kernel" ;;
        auto-movable) online="memhp_default_state=online memory_hotplug.online_policy=auto-movable" ;;
        movable) online="memhp_default_state=online_movable" ;;
        esac
        line=$("$shards" run --kernel "$kernel" --init "$guest" --memory "$memory" \
            --memory-max $((memory + plug)) \
            --cmdline "console=ttyS0 panic=-1 shards_test=unplug-cost shards_plug_mib=$plug shards_files=$files $online" \
            </dev/null 2>/dev/null | tr -d '\r' | grep -a 'SHARDS-TEST UNPLUG' || echo "SHARDS-TEST UNPLUG none")
        echo "$policy $round ${line#SHARDS-TEST UNPLUG }"
    done
done
