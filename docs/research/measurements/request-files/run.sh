#!/bin/sh
# M254 and M255 (docs/research/platform-measurements.md): what a client's request to its
# daemon costs through files in the home, against a Unix socket (D185); each step of a
# made-ahead slot's; flock(2) on a FIFO; and what making and removing a slot costs. On this host, and with SHARDS KERNEL INIT given, on Linux in a
# microVM of shards' too, of the host's architecture.
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
for p in requests steps fifolock slotcost; do
	cc -O1 -w -o "$work/$p" "$here/$p.c"
done
mkdir -p "$work/rq" "$work/st" "$work/fl" "$work/sc"
"$work/requests" "$work/rq" 2000
"$work/steps" "$work/st" 2000
"$work/fifolock" "$work/fl"
# M255: a slot's making and removal.
"$work/slotcost" "$work/sc" 2000
uname -sr
uptime
if [ -n "$shards" ]; then
	mkdir -p "$work/config" "$work/linux"
	case $(uname -m) in
	arm64 | aarch64) arch=aarch64 platform=linux/arm64 ;;
	*) arch=x86_64 platform=linux/amd64 ;;
	esac
	for p in requests steps fifolock; do
		ZIG_TARGET=$arch-linux-musl "$repo/scripts/zig-cc" -target "$arch-linux-musl" -O1 -w -static \
			-o "$work/linux/$p" "$here/$p.c"
	done
	SHARDS_HOME=$work/home DOCKER_CONFIG=$work/config SHARDS_KERNEL=$2 SHARDS_INIT=$3 \
		"$shards" run --rm --platform "$platform" -v "$work/linux:/t:ro" alpine:3.22 \
		sh -c 'mkdir -p /var/rq /var/st /var/fl && /t/requests /var/rq 2000 && /t/steps /var/st 2000 && /t/fifolock /var/fl && uname -sr'
fi
