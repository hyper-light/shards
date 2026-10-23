#!/bin/sh
# What a DAX window's length costs a run's start, its volume's mount among it (D189, PM
# M306): one home an arm, NAME:LENGTH:HOME[:KERNEL], its daemon giving each volume a window
# of LENGTH MiB (SHARDS_SHARE_WINDOW_MIB, 0 none, `default` the daemon's own, as long as the
# host's memory) and serving its runs from its template,
# and booting KERNEL with INIT where it names one (`shards configure guest`, D28); ROUNDS
# rounds, a run of each arm in turn, a second apart, each with a volume of DIR and
# SHARDS_TIMING's line, whose guest markers time the run's setup in its microVM (its request
# to its command's start, its volume's mount among it) and the whole run there (to its
# answer). Each HOME holds IMAGE already.
# One line a run: NAME ROUND SETUP_US RUN_US.
# Usage: [INIT=…] mounts.sh SHARDS IMAGE DIR ROUNDS NAME:LENGTH_MIB:HOME[:KERNEL]...
set -eu
shards=$1 image=$2 dir=$3 rounds=$4
shift 4
cfg=$(mktemp -d)
trap 'rm -rf "$cfg"' EXIT
export DOCKER_CONFIG="$cfg"
for arm in "$@"; do
    IFS=: read -r name len home kernel <<ARM
$arm
ARM
    SHARDS_HOME=$home "$shards" stop daemon >/dev/null 2>&1 || true
    if [ -n "$kernel" ]; then SHARDS_HOME=$home "$shards" configure guest --kernel "$kernel" --init "$INIT" >/dev/null; fi
    # Its daemon started at its length, and its template made, by a first run.
    if [ "$len" = default ]; then
        SHARDS_HOME=$home "$shards" run --rm --pull never -v "$dir:/v" "$image" true >/dev/null
    else
        SHARDS_HOME=$home SHARDS_SHARE_WINDOW_MIB=$len "$shards" run --rm --pull never -v "$dir:/v" "$image" true >/dev/null
    fi
done
for round in $(seq "$rounds"); do
    for arm in "$@"; do
        IFS=: read -r name len home kernel <<ARM
$arm
ARM
        sleep 1
        SHARDS_HOME=$home SHARDS_TIMING=1 "$shards" run --rm --pull never -v "$dir:/v" "$image" true 2>"$cfg/timing" >/dev/null
        python3 -I -c 'import json, sys
for line in open(sys.argv[1]):
    if line.startswith("shards-timing "):
        t = json.loads(line.split(" ", 1)[1])
        m = dict(t["markers"])
        print(sys.argv[2], sys.argv[3], m[4] - t["request_us"], t["answered_us"] - t["request_us"])
        break' "$cfg/timing" "$name" "$round"
    done
done
for arm in "$@"; do
    IFS=: read -r name len home kernel <<ARM
$arm
ARM
    SHARDS_HOME=$home "$shards" stop daemon >/dev/null 2>&1 || true
done
