#!/bin/sh
# Builds a release `shards` of the tree at MIRROR and runs measure.py with it: N turns,
# results to OUT (a directory). Linux and macOS; on macOS run it as a user's program runs
# (docs/research/measurements/macos-sched/launchd-run.sh), the session's QoS clamp aside.
#
#     run.sh MIRROR OUT N
set -eu
mirror=$1 out=$2 n=$3
mkdir -p "$out"
: "${CARGO_TARGET_DIR:=$HOME/builder-ceiling-target}"
export CARGO_TARGET_DIR
cd "$mirror"
git log --oneline -1 > "$out/revision"
cargo build --release -p shards -j 6 > "$out/build.log" 2>&1
bin=$out/bin
mkdir -p "$bin"
cp "$CARGO_TARGET_DIR/release/shards" "$bin/shards.tmp" && mv "$bin/shards.tmp" "$bin/shards"
home=${SHARDS_HOME_DIR:-$HOME/builder-ceiling-home}
mkdir -p "$home" "$out/docker-config"
export DOCKER_CONFIG=$out/docker-config
SHARDS_HOME=$home "$bin/shards" pull alpine:3.22 > "$out/pull.log" 2>&1
python3 docs/research/measurements/builder-ceiling/measure.py "$bin" "$home" "$n" "$out/samples.json" \
  > "$out/summary.txt" 2> "$out/progress.log"
SHARDS_HOME=$home "$bin/shards" stop daemon > /dev/null 2>&1 || true
touch "$out/done"
