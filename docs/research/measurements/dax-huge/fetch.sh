#!/bin/sh
# The DAX huge workflow's outputs into results/: for each run (and attempt) given, each
# runner's measure.py output (NAME.out.txt) and rule.py's tables and verdicts of it
# (NAME.txt), NAME the run, its attempt, the runner's copy and its CPU (as its header says).
# Runners that measured nothing (another vendor's, VENDOR) left no artifact.
# Usage: fetch.sh RUN[:ATTEMPT]...   (run from the repository's root)
set -eu
here=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$here/results"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/dax-huge-fetch.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
for spec in "$@"; do
    run=${spec%%:*}
    attempt=1
    [ "$spec" = "$run" ] || attempt=${spec#*:}
    mkdir -p "$tmp/$run"
    gh run download "$run" --repo hyper-light/shards -D "$tmp/$run"
    for dir in "$tmp/$run"/dax-huge-*-"$attempt"; do
        [ -f "$dir/out.txt" ] || continue
        copy=$(basename "$dir" | sed 's/^dax-huge-\(.*\)-[0-9]*$/\1/')
        cpu=$(sed -n '1s/^host [^;]*; [^;]*; [^;]*; [^;]*; \([^;]*\);.*/\1/p' "$dir/out.txt" |
              sed 's/(R)//g; s/(TM)//g; s/ CPU//; s/ @.*//; s/ [0-9]*-Core Processor//; s/Intel Xeon Platinum/xeon/; s/AMD EPYC/epyc/' |
              tr 'A-Z ' 'a-z-')
        name="ci-$run-$attempt-$copy-$cpu"
        cp "$dir/out.txt" "$here/results/$name.out.txt"
        python3 -I "$here/rule.py" "$here/results/$name.out.txt" > "$here/results/$name.txt"
        echo "$name: $(grep -c '^cell ' "$here/results/$name.out.txt") cells, $(grep -c '^FAILED' "$here/results/$name.out.txt" || true) failed"
    done
done
