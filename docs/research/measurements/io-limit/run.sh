#!/bin/sh
# M240 (docs/research/platform-measurements.md): a microVM's I/O limits (D178), from its
# guest's writes to a volume. bench.py's ROUNDS rounds against shards' release build
# SHARDS, in a home of their own. With `load`, as many busy loops as two thirds of the
# host's CPUs run beside it.
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
SHARDS_HOME="$home" python3 -I "$here/bench.py" "$shards" "$rounds"
