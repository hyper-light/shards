#!/bin/sh
# What a DAX window's page structures take of its guest's memory with its head and without
# (D189, shards' kernel patch 0008, PM M300): M269's measure.sh, booting the kernel without
# the patch and the one with it in turn, round by round, a boot of each LENGTH MiB window.
# Usage: heads-ab.sh SHARDS OLD_KERNEL NEW_KERNEL TESTGUEST ROUNDS LENGTH_MIB...
#   (one line a boot: old|new ROUND LENGTH 1 TOTAL FREE AVAILABLE UPTIME; heads-ab.py reads them)
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
