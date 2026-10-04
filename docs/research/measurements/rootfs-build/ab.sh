#!/bin/sh
# ab.sh OLD_REV ROUNDS LAYOUT...: builds rootfs-build against OLD_REV's crates/image
# (in a git worktree) and against the working tree's, then runs them in turn, one root
# filesystem of each image a round, after one warm-up round each; prints n, p50, p90,
# p99 and max per image and build, and the CPU time a build took. LAYOUTs are `docker save` output, unpacked. With
# BUSY=N in the environment, N processes spin on the CPU throughout the rounds, and with
# IOBUSY=N, N more write 1 GiB files to the store's file system over and over: a busy
# host, which is the usual one.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
repo=$(git -C "$here" rev-parse --show-toplevel)
old=$1 rounds=$2
shift 2
work=$(mktemp -d)
spinners=
trap 'set +e; for p in $spinners; do pkill -P "$p" 2>/dev/null; kill "$p" 2>/dev/null; done; git -C "$repo" worktree remove --force "$work/old" >/dev/null 2>&1 || true; rm -rf "$work"' EXIT
git -C "$repo" worktree add --detach "$work/old" "$old" >/dev/null
mkdir -p "$work/old/docs/research/measurements/rootfs-build"
cp -R "$here/Cargo.toml" "$here/src" "$work/old/docs/research/measurements/rootfs-build/"
for side in old new; do
    case $side in old) dir=$work/old/docs/research/measurements/rootfs-build ;; new) dir=$here ;; esac
    cargo build -q --release --manifest-path "$dir/Cargo.toml" --target-dir "$work/target-$side"
done
for side in old new; do "$work/target-$side/release/rootfs-build" 1 "$work/store-$side" "$@" >/dev/null; done
n=0
while [ "$n" -lt "${BUSY:-0}" ]; do
    yes >/dev/null &
    spinners="$spinners $!"
    n=$((n + 1))
done
n=0
while [ "$n" -lt "${IOBUSY:-0}" ]; do
    sh -c 'while :; do dd if=/dev/zero of="$1" bs=1m count=1024 2>/dev/null; rm -f "$1"; done' sh "$work/io-$n" &
    spinners="$spinners $!"
    n=$((n + 1))
done
i=0
while [ "$i" -lt "$rounds" ]; do
    for side in old new; do
        "$work/target-$side/release/rootfs-build" 1 "$work/store-$side" "$@" | sed "s/^/$side /"
    done
    i=$((i + 1))
done | python3 -c '
import sys, collections
runs = collections.defaultdict(list)
for line in sys.stdin:
    side, image, ms, cpu = line.split()
    runs[(image, side)].append((float(ms), float(cpu)))
def q(s, p): return s[min(len(s) - 1, int(p * len(s)))]
print("| image | build | n | p50 ms | p90 | p99 | max | CPU ms p50 | p90 |\n|---|---|---|---|---|---|---|---|---|")
for (image, side), v in sorted(runs.items()):
    s = sorted(m for m, _ in v)
    c = sorted(c for _, c in v)
    print(f"| {image} | {side} | {len(s)} | {q(s,.5):.0f} | {q(s,.9):.0f} | {q(s,.99):.0f} | {s[-1]:.0f} | {q(c,.5):.0f} | {q(c,.9):.0f} |")
'
