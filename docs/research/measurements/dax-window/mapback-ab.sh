#!/bin/sh
# mapback.sh's map-back A/B: ROUNDS rounds, each one template's map-back with A's build and
# one with B's, each round's first in turn, so that both see the same host. One line a
# sample: ARM ROUND TOTAL_US TAKEN_US MAPPED_US GIVEN_US FROM_COPY FRESH ZERO_RUNS.
# Usage: mapback-ab.sh A_SHARDS B_SHARDS SEED WORK IMAGE ROUNDS
set -eu
a=$1 b=$2 seed=$3 work=$4 image=$5 rounds=$6
here=$(dirname "$0")
for round in $(seq "$rounds"); do
    if [ $((round % 2)) -eq 1 ]; then order="a b"; else order="b a"; fi
    for arm in $order; do
        if [ "$arm" = a ]; then bin=$a; else bin=$b; fi
        mkdir -p "$work/$arm"
        line=$("$here/mapback.sh" "$bin" "$seed" "$work/$arm" "$image" 1 | tail -1)
        echo "$arm $round ${line#1 }"
    done
done
