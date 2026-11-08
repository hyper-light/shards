#!/bin/sh
# M252 (docs/research/platform-measurements.md): what reading `shards configure disk`'s
# settings costs at each use (D186). Run from the repository, whose toolchain builds it.
#
#   run.sh
set -eu
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -r -- "$work"' EXIT
rustc -O -o "$work/settings-read" "$here/main.rs"
"$work/settings-read" "$work"
uptime
