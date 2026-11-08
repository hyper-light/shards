#!/bin/sh
# M251 (docs/research/platform-measurements.md): which x86-64 levels shards' emulator runs
# (D179's pinned qemu-x86_64), and how an emulated program that dies by a signal ends its
# run. v3.c and v4.c each run their level's own instructions and exit 0; each is built for
# linux/amd64 and run in an amd64 Alpine microVM (emulated on an arm64 host): as the run's
# command (PID 1 of its PID namespace), under `--init`, and as a shell's child.
#
#   run.sh SHARDS KERNEL INIT
set -eu
shards=$1
kernel=$2
init=$3
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../../../.." && pwd)
work=$(mktemp -d)
cleanup() {
	SHARDS_HOME=$work/home DOCKER_CONFIG=$work/config "$shards" stop daemon >/dev/null 2>&1 || true
	rm -r -- "$work"
}
trap cleanup EXIT
mkdir -p "$work/config" "$work/probes"
for v in v3 v4; do
	ZIG_TARGET=x86_64-linux-musl "$repo/scripts/zig-cc" -target x86_64-linux-musl -O1 -static \
		-o "$work/probes/$v" "$here/$v.c"
done
# Docker Hub's alpine:3.22, amd64's manifest (2026-10-15).
alpine=alpine@sha256:3e9b4b680bfc9fb5269227cffbd6d42be39fbf7c0b908123913864aa4447e764
run() {
	SHARDS_HOME=$work/home DOCKER_CONFIG=$work/config SHARDS_KERNEL=$kernel SHARDS_INIT=$init \
		"$shards" run --rm --platform linux/amd64 -v "$work/probes:/t:ro" "$@"
}
for v in v3 v4; do
	# As the run's command, bounded: one that never ends is said so.
	set +e
	run "$alpine" "/t/$v" >/dev/null 2>&1 &
	pid=$!
	n=0
	while kill -0 "$pid" 2>/dev/null && [ "$n" -lt 120 ]; do
		sleep 1
		n=$((n + 1))
	done
	if kill -0 "$pid" 2>/dev/null; then
		echo "$v as PID 1: no end in ${n} s"
		kill "$pid"
		SHARDS_HOME=$work/home DOCKER_CONFIG=$work/config "$shards" rm -f $(SHARDS_HOME=$work/home DOCKER_CONFIG=$work/config "$shards" ps -q) >/dev/null 2>&1
	else
		wait "$pid"
		echo "$v as PID 1: exit $?"
	fi
	run --init "$alpine" "/t/$v" >/dev/null 2>&1
	echo "$v under --init: exit $?"
	run "$alpine" /bin/sh -c "/t/$v; echo \$?" 2>/dev/null | tail -1 | sed "s/^/$v as a child: exit /"
	set -e
done
