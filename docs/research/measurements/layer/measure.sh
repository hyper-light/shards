#!/bin/sh
# What a run's writable layer costs its microVM, by its size (D190, PM M277): an ext4 file
# system shards-ext4 makes, given as `shards run vm --layer`, a pmem device whose page
# structures its guest keeps in the device's head, not its memory (where M264's, probed
# with them in memory, took a 64th of the device). Each boot runs the test guest's `layer`
# verb (crates/testguest, `shards_test=layer`): MemFree as it starts and once the NVDIMM
# buses' probes are done, the DAX mount's and the overlay's times, first and three times
# again, 64 MiB written and synced, 1,000 small files and 8 MiB through a mapping, and what
# they took of its memory; then what keeping it costs as a run ends (PM M278): its journal
# written in place as init's KEEP writes it (sync, syncfs, EXT4_IOC_CHECKPOINT), against a
# freeze (FIFREEZE) and a read-only remount, three rounds after 100 more files each, the
# first of each the first in its microVM. HOLDS 0 is a boot without a layer, to set beside
# the others.
#
# Usage: measure.sh SHARDS KERNEL TESTGUEST ROUNDS HOLDS_GIB...
#   SHARDS a release build's `shards`; KERNEL shards' pinned kernel for this host;
#   TESTGUEST the test guest, built for this host's musl target with the `guest` profile.
# One line per boot: HOLDS ROUND key=value...
set -eu
shards=$1 kernel=$2 guest=$3 rounds=$4
shift 4
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../../.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
for holds in "$@"; do
    image=""
    if [ "$holds" -gt 0 ]; then
        bytes=$((holds << 30))
        # The rustup proxy, without the DYLD_* that keep rust-lld from loading libLLVM.
        env -u DYLD_LIBRARY_PATH -u DYLD_FALLBACK_LIBRARY_PATH \
            cargo run --quiet --manifest-path "$root/Cargo.toml" -p shards-ext4 --example images -- "$work" "$bytes"
        image=$work/$bytes.img
    fi
    for round in $(seq "$rounds"); do
        set -- --kernel "$kernel" --init "$guest" --cpus 2 --memory 1024 \
            --cmdline "console=ttyS0 panic=-1 shards_test=layer"
        if [ -n "$image" ]; then
            # A copy each boot, sharing the image's blocks where the file system can.
            copy=$work/boot.img
            rm -f "$copy"
            cp -c "$image" "$copy" 2>/dev/null || cp --reflink=auto "$image" "$copy"
            set -- "$@" --layer "$copy"
        fi
        said=$("$shards" run vm "$@" 2>&1 | tr -d '\r' | grep -a -e '^LAYER' -e 'SHARDS-TEST FAIL' | sed 's/^LAYER //' | tr '\n' ' ')
        echo "$holds $round $said"
    done
done
