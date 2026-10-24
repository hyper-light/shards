#!/bin/sh
# A volume's file read through FUSE (no DAX window, SHARDS_SHARE_WINDOW_MIB=0) by two builds
# of shards, interleaved round by round, each round's first in turn, so that both see the
# same host: ROUNDS rounds of `dd` of a SIZE MiB file the host's page cache holds (read
# once first), so that each read is the share's own work, its copies and buffers, not the
# disk's; each build with a home of its own given SEED's image store and guest kernel by
# clone. One line a read: ARM ROUND SECONDS (dd's own), then n, p50, p90, p99 and max of
# each arm's.
# Usage: ab.sh A_SHARDS B_SHARDS SEED WORK ROUNDS [SIZE_MIB]   (SEED holds alpine)
set -eu
a=$1 b=$2 seed=$3 work=$4 rounds=$5 size=${6:-1024}
cfg=$(mktemp -d)
trap 'for arm in a b; do SHARDS_HOME="$work/home-$arm" DOCKER_CONFIG="$cfg" "$work/bin-$arm" daemon stop >/dev/null 2>&1 || true; done; rm -rf "$cfg"' EXIT
export DOCKER_CONFIG="$cfg" SHARDS_SHARE_WINDOW_MIB=0
mkdir -p "$work/vol"
[ -f "$work/vol/big" ] || dd if=/dev/urandom of="$work/vol/big" bs=1m count="$size" 2>/dev/null
for arm in a b; do
    if [ "$arm" = a ]; then bin=$a; else bin=$b; fi
    cp "$bin" "$work/bin-$arm.tmp" && mv "$work/bin-$arm.tmp" "$work/bin-$arm"
    home="$work/home-$arm"
    mkdir -p "$home"
    for part in images guest; do
        [ -d "$seed/$part" ] && [ ! -d "$home/$part" ] && cp -cR "$seed/$part" "$home/$part"
    done
    # Its template saved, so that each read is a restored run's.
    SHARDS_HOME="$home" "$work/bin-$arm" run --rm --pull never -v "$work/vol:/vol" alpine true >/dev/null
done
out="$work/reads.txt"
: > "$out"
cat "$work/vol/big" > /dev/null
for round in $(seq "$rounds"); do
    if [ $((round % 2)) -eq 1 ]; then order="a b"; else order="b a"; fi
    for arm in $order; do
        s=$(SHARDS_HOME="$work/home-$arm" "$work/bin-$arm" run --rm --pull never -v "$work/vol:/vol" alpine \
            dd if=/vol/big of=/dev/null bs=1M 2>&1 | sed -n 's/.*copied, \([0-9.]*\) seconds.*/\1/p')
        echo "$arm $round ${s:-failed}" | tee -a "$out"
    done
done
python3 - "$out" <<'EOF'
import sys
rows = [l.split() for l in open(sys.argv[1]) if len(l.split()) == 3 and l.split()[2] != "failed"]
for arm in ("a", "b"):
    xs = sorted(float(r[2]) for r in rows if r[0] == arm)
    if not xs:
        continue
    p = lambda q: xs[min(len(xs) - 1, int(q * len(xs)))]
    print(f"{arm}: n {len(xs)} p50 {p(.5):.3f} p90 {p(.9):.3f} p99 {p(.99):.3f} max {xs[-1]:.3f} s")
EOF
