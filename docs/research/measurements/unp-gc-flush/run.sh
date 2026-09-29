#!/bin/sh
# M24 (docs/research/platform-measurements.md): whether the kernel's collector of
# in-flight descriptors flushes a Unix socket in flight whose sender has already closed
# its own descriptor for it. Builds probe.c with the host's C compiler and runs TRIALS
# trials (default 1000) of each case.
#
#   run.sh [TRIALS]
#
# For Linux from a macOS host, cross-build it and run it in a Linux VM:
#   zig cc -target aarch64-linux-musl -O2 -o probe-linux probe.c
set -eu
dir=$(cd "$(dirname "$0")" && pwd)
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT
cc -O2 -Wall -Wextra -o "$out/probe" "$dir/probe.c"
"$out/probe" "${1:-1000}"
