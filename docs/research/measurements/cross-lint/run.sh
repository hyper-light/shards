#!/bin/sh
# M1 (docs/research/registry-pull.md §8): what linting every CI target costs from one
# host once aws-lc-sys's C is in the build. For each target, N cold runs (the target's
# build directory removed, so AWS-LC compiles again) and N warm runs (one workspace
# source touched), each timed around scripts/lint. Prints one line per run:
#   target kind seconds
#
#   docs/research/measurements/cross-lint/run.sh [N] [TARGET...]
set -eu

root=$(cd "$(dirname "$0")/../../../.." && pwd)
n=${1:-5}
[ $# -gt 0 ] && shift
all="aarch64-apple-darwin x86_64-apple-darwin x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu
x86_64-pc-windows-msvc aarch64-pc-windows-msvc x86_64-unknown-linux-musl aarch64-unknown-linux-musl"

# Wall-clock seconds of scripts/lint TARGET, from POSIX `time -p`.
seconds() {
	/usr/bin/time -p sh -c 'scripts/lint "$1" >/dev/null 2>&1' _ "$1" 2>&1 | awk '/^real/ { print $2 }'
}

cd "$root"
for target in ${*:-$all}; do
	# One untimed run, so every dependency is downloaded and the host's build scripts exist.
	scripts/lint "$target" >/dev/null 2>&1
	i=0
	while [ $i -lt "$n" ]; do
		rm -rf "target/$target"
		echo "$target cold $(seconds "$target")"
		i=$((i + 1))
	done
	i=0
	while [ $i -lt "$n" ]; do
		touch crates/registry/src/lib.rs
		echo "$target warm $(seconds "$target")"
		i=$((i + 1))
	done
done
