#!/bin/bash
# A daemon stepping aside, its slots' bridges cut off by its exit (D185, PM M261): the three
# tests x86_64 CI failed in run 39191414199, run N times in each tree given, pinned to two
# CPUs with a busy loop on each, about a CI runner's pressure (Linux: taskset).
#
#   ab.sh N TREE...    e.g. ab.sh 20 /tmp/old /path/to/shards
#
# Each tree is a checkout of shards whose tests build (zig on PATH: scripts/install-zig).
# Prints each failure's message, and per tree how many of its 2N runs failed.
set -u
N=$1
shift
out=$(mktemp -d) || exit 1
loops=()
trap 'kill "${loops[@]}" 2>/dev/null' EXIT
for tree in "$@"; do
    cd "$tree" || exit 1
    if ! cargo test -q --profile test-release -p shards --test containers --test daemon --no-run > "$out/build.txt" 2>&1; then
        echo "$tree: build failed"
        tail -5 "$out/build.txt"
        continue
    fi
    loops=()
    for _ in 1 2; do
        taskset -c 2,3 sh -c 'while :; do :; done' &
        loops+=($!)
    done
    fails=0
    for _ in $(seq "$N"); do
        if ! taskset -c 2,3 cargo test -q --profile test-release -p shards --test containers -- \
            rm_cancels_a_run_whose_vm_is_not_ready a_container_outlives_a_daemon_that_dies > "$out/c.txt" 2>&1; then
            fails=$((fails + 1))
            grep -E 'stopped before|panicked' "$out/c.txt" | head -2
        fi
        if ! taskset -c 2,3 cargo test -q --profile test-release -p shards --test daemon -- \
            a_rebuilt_binary_replaces_the_daemon > "$out/d.txt" 2>&1; then
            fails=$((fails + 1))
            grep -E 'stopped before|panicked' "$out/d.txt" | head -2
        fi
    done
    kill "${loops[@]}"
    echo "$tree: $fails failed runs of $((2 * N))"
done
rm -r "$out"
