#!/bin/sh
# M239 (docs/research/platform-measurements.md): a running microVM's save and restore,
# from the caller's view, and the save's phases. bench.py's ROUNDS rounds against shards'
# release build SHARDS, in a home of their own, with the daemon's info log on, whose lines
# give each save's phases: its guest going quiet, and the microVM saved (vCPUs parked,
# memory and state written, vCPUs released). With `load`, as many busy loops as two
# thirds of the host's CPUs run beside it.
#
#   run.sh SHARDS ROUNDS [load]
set -eu
shards=$1
rounds=$2
here=$(cd "$(dirname "$0")" && pwd)
home=$(mktemp -d)
spinners=""
trap '[ -n "$spinners" ] && kill $spinners; SHARDS_HOME="$home" "$shards" stop daemon >/dev/null 2>&1 || true; rm -r -- "$home"' EXIT
if [ "${3:-}" = load ]; then
	n=$(( $(getconf _NPROCESSORS_ONLN) * 2 / 3 ))
	i=0
	while [ "$i" -lt "$n" ]; do
		( while :; do :; done ) &
		spinners="$spinners $!"
		i=$((i + 1))
	done
fi
SHARDS_LOG=info SHARDS_HOME="$home" python3 -I "$here/bench.py" "$shards" "$rounds" | grep -v '^snapshot sizes'
for phase in "quiet in" "saved in"; do
	grep -o "$phase [0-9]* us" "$home/daemon.log" | awk '{print $3 / 1000}' | sort -n |
		awk -v phase="$phase" '{a[NR] = $1} END {print phase ", ms: n=" NR " p50=" a[int(NR * 0.5) + 1] " p90=" a[int(NR * 0.9) + 1] " p99=" a[int(NR * 0.99) + 1] " max=" a[NR]}'
done
