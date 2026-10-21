#!/bin/sh
# M253 (docs/research/platform-measurements.md): how each end of a FIFO learns the other
# has closed (D185). On this host (macOS or Linux), and with SHARDS KERNEL INIT given, on
# Linux in a microVM of shards' too, of the host's architecture.
#
#   run.sh [SHARDS KERNEL INIT]
set -eu
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../../../.." && pwd)
shards=${1:-}
work=$(mktemp -d)
cleanup() {
	if [ -n "$shards" ] && [ -d "$work/home" ]; then
		SHARDS_HOME=$work/home DOCKER_CONFIG=$work/config "$shards" stop daemon >/dev/null 2>&1 || true
	fi
	rm -r -- "$work"
}
trap cleanup EXIT
cc -O1 -Wall -o "$work/ends" "$here/ends.c"
"$work/ends" "$work" 3 5000
# Part 3: 8 races at once, of each wait.
cc -O1 -w -o "$work/race" "$here/race.c"
for how in read select; do
	for k in 1 2 3 4 5 6 7 8; do
		"$work/race" "$work/race-$k" "$how" 20000 &
	done
	wait
done
uname -sr
uptime
if [ -n "$shards" ]; then
	mkdir -p "$work/config" "$work/linux"
	case $(uname -m) in
	arm64 | aarch64) arch=aarch64 platform=linux/arm64 ;;
	*) arch=x86_64 platform=linux/amd64 ;;
	esac
	ZIG_TARGET=$arch-linux-musl "$repo/scripts/zig-cc" -target "$arch-linux-musl" -O1 -static \
		-o "$work/linux/ends" "$here/ends.c"
	SHARDS_HOME=$work/home DOCKER_CONFIG=$work/config SHARDS_KERNEL=$2 SHARDS_INIT=$3 \
		"$shards" run --rm --platform "$platform" -v "$work/linux:/t:ro" alpine:3.22 \
		sh -c 'mkdir -p /tmp/ends && /t/ends /tmp/ends 3 5000 && uname -sr'
fi
