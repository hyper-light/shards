#!/bin/sh
# PM M182: docker-fast-burst.sh through shards: the test guest's `udp-send` and
# `udp-count` (TESTGUEST, the binary built for the guest's architecture, given by a
# volume), on a user network of a home of its own; the receiver given CPUS vCPUs
# (`--cpus`).
#   shards-fast-burst.sh SHARDS TESTGUEST [COUNT] [N] [CPUS]
set -u
usage='usage: shards-fast-burst.sh SHARDS TESTGUEST [COUNT] [N] [CPUS]'
shards=${1:?$usage}
tg=${2:?$usage}
COUNT=${3:-200}
N=${4:-10}
CPUS=${5:-1}
IMAGE=${IMAGE:-alpine:3.22}
home=$(mktemp -d)
dir=$(mktemp -d)
cp "$tg" "$dir/tg"
trap 'SHARDS_HOME=$home "$shards" stop daemon >/dev/null 2>&1 || :; rm -r "$home" "$dir"' EXIT
s() { SHARDS_HOME=$home "$shards" "$@"; }
s pull -q "$IMAGE" >/dev/null
s network create --subnet 10.78.4.0/24 exd >/dev/null
for i in $(seq "$N"); do
	s run -d --name exd-r --network exd --ip 10.78.4.50 --cpus "$CPUS" -v "$dir:/t:ro" "$IMAGE" /t/tg udp-count 0.0.0.0:9999 1000 >/dev/null 2>&1
	sleep 1
	sent=$(s run --rm --network exd -v "$dir:/t:ro" "$IMAGE" /t/tg udp-send 10.78.4.50:9999 "$COUNT" 1400 0 | head -1)
	s wait exd-r >/dev/null
	echo "run $i: $sent, $(s logs exd-r | grep '^got')"
	s rm -f exd-r >/dev/null
done
