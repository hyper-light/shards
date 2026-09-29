#!/bin/sh
# M23 (docs/research/platform-measurements.md): a warm-pool daemon's handoff, and the
# client process that asks for it. Builds the harness, then runs `bench N` with its sockets
# in a fresh directory, next to PROGRAM ARG... (for example a signed `shards version`).
#
#   docs/research/measurements/daemon-ipc/run.sh [N] [PROGRAM ARG...]
set -eu

here=$(cd "$(dirname "$0")" && pwd)
n=${1:-500}
[ $# -gt 0 ] && shift
cd "$here"
cargo build --release --quiet
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
target/release/daemon-ipc bench "$n" "$dir" "$@"
