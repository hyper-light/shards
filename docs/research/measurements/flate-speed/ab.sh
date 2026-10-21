#!/bin/bash
# PM M256: two builds of shards-flate's `speed` example, OLD and NEW, run on the same
# files in ROUNDS rounds, the order alternating each round, each run gzipping each file
# once (`speed -1 1 FILE...`), so that both see the host as it is at the time. Every line
# is one sample; `summary.py` takes them to n, p50, p90, p99 and max. The outputs'
# SHA-256 must be one per file: both builds make Go's bytes.
# Usage: ab.sh OLD NEW ROUNDS FILE...
set -eu
old=$1; new=$2; rounds=$3; shift 3
for r in $(seq "$rounds"); do
    if [ $((r % 2)) -eq 1 ]; then order="old new"; else order="new old"; fi
    for which in $order; do
        bin=$old; [ "$which" = new ] && bin=$new
        "$bin" -1 1 "$@" | sed "s/^/$which /"
    done
done
