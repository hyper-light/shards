#!/bin/sh
# What a DAX window costs a boot by its length, to the longest a default might give (D189,
# PM M306): the test guest as PID 1 (testguest `memory`), 512 MiB and one shared directory
# whose device gives a window of each LENGTH MiB (0: none), with its head, which the
# guest's kernel writes the window's page structures into as it probes the device, a 64th
# of the window. Each boot says PID 1's uptime and the guest's free memory (as M269's
# measure.sh), and /usr/bin/time the VM process's (`run --kernel` becomes it) peak resident
# memory and minor faults. ROUNDS boots of each, interleaved.
# Usage: costs.sh SHARDS KERNEL TESTGUEST ROUNDS LENGTH_MIB...
#   (one line a boot: LENGTH ROUND TOTAL FREE AVAILABLE UPTIME LISTED RSS_BYTES MINOR_FAULTS)
set -eu
shards=$1 kernel=$2 guest=$3 rounds=$4
shift 4
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
case $(uname) in
Darwin) flag=-l ;;
*) flag=-v ;;
esac
for round in $(seq "$rounds"); do
    for len in "$@"; do
        /usr/bin/time "$flag" "$shards" run --kernel "$kernel" --init "$guest" --memory 512 \
            --shares 1 --share-window "$len" \
            --cmdline "console=ttyS0 panic=-1 shards_test=memory" \
            </dev/null >"$work/out" 2>"$work/err" || true
        line=$(tr -d '\r' <"$work/out" | grep -a 'SHARDS-TEST MEMORY' || echo "SHARDS-TEST MEMORY none")
        # macOS: "maximum resident set size" in bytes, "page reclaims"; Linux: kbytes, and
        # "Minor (reclaiming a frame) page faults".
        rss=$(awk '/maximum resident set size/ {print $1} /Maximum resident set size/ {print $NF * 1024}' "$work/err")
        minor=$(awk '/page reclaims/ {print $1} /Minor \(reclaiming a frame\)/ {print $NF}' "$work/err")
        echo "$len $round ${line#SHARDS-TEST MEMORY } ${rss:-none} ${minor:-none}"
    done
done
