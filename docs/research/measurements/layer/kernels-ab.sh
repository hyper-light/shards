#!/bin/sh
# What a run's writable layer takes of its guest's memory on each of two kernels (D190,
# shards' kernel patch 0010, PM M277): measure.sh booting each kernel in turn, round by
# round, so that both meet the same load. kernels-ab.py reads its lines.
# Usage: kernels-ab.sh SHARDS OLD_KERNEL NEW_KERNEL TESTGUEST ROUNDS HOLDS_GIB...
#   (one line a boot: old|new ROUND HOLDS 1 key=value...)
set -eu
shards=$1 old=$2 new=$3 guest=$4 rounds=$5
shift 5
here=$(dirname "$0")
for round in $(seq "$rounds"); do
    for k in old new; do
        if [ "$k" = old ]; then kernel=$old; else kernel=$new; fi
        sh "$here/measure.sh" "$shards" "$kernel" "$guest" 1 "$@" | sed "s/^/$k $round /"
    done
done
