#!/bin/sh
# M236 (docs/research/platform-measurements.md): a read waiting on a stream socket as its
# peer half-closes, and shutdown(SHUT_RDWR) after a peer's half-close (race.c); and what
# each way of waiting for a message costs (roundtrip.c). Builds both with the host's C
# compiler and runs every case.
#
#   run.sh [ROUNDS]
#
# For Linux, cross-build them and run them in a microVM:
#   zig cc -target aarch64-linux-musl -O2 -o race-linux race.c
set -eu
dir=$(cd "$(dirname "$0")" && pwd)
rounds=${1:-50000}
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT
cc -O2 -o "$out/race" "$dir/race.c" -lpthread
cc -O2 -o "$out/roundtrip" "$dir/roundtrip.c"
"$out/roundtrip" 100 1000 0
"$out/roundtrip" 100 1000 50
"$out/race" rdwr
"$out/race" unix-recv "$rounds" 400
"$out/race" unix-poll "$rounds" 400
"$out/race" tcp-recv "$rounds" 400 0
"$out/race" tcp-recv "$rounds" 400 1
