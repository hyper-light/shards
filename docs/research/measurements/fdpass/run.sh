#!/bin/sh
# M25 (docs/research/platform-measurements.md): what the kernel does when descriptors
# pass over Unix sockets (SCM_RIGHTS): limits, truncation, close-on-exec, which kinds
# pass, MSG_PEEK, in-flight limits, peer credentials. Builds probe.c with the host's C
# compiler and runs it. Run it as a user other than root, so permission checks apply.
#
#   run.sh
#
# For Linux from a macOS host, cross-build it and run it in a Linux VM:
#   zig cc -target aarch64-linux-musl -O2 -o probe-linux probe.c
set -eu
dir=$(cd "$(dirname "$0")" && pwd)
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT
cc -O2 -o "$out/probe" "$dir/probe.c"
"$out/probe"
