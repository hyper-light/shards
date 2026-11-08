#!/bin/sh
# PM M178: what a bind costs on the host's network (D124): a TCP listener's bind(2) and
# listen(2) at every IPv4 address, at a port of the kernel's choosing (std's
# TcpListener::bind), and a UDP socket's bind(2) alone, each closed before the next, timed
# in the guest by the test guest's `binds`, on the host's network and on the default
# bridge, runs interleaved:
#   binds.sh SHARDS TESTGUEST [N] [ROUNDS]
# TESTGUEST is the test guest built for the guest's architecture
# (target/guest/<arch>-unknown-linux-musl/guest/shards-testguest), run in alpine:3.22.
# Two lines a run, in microseconds:
#   NETWORK ROUND binds tcp|udp n=N p50 P p90 P p99 P max M
set -eu
usage='usage: binds.sh SHARDS TESTGUEST [N] [ROUNDS]'
shards=${1:?$usage}
guest=${2:?$usage}
n=${3:-200}
rounds=${4:-10}
home=$(mktemp -d)
dir=$(mktemp -d)
cp "$guest" "$dir/tg"
trap 'SHARDS_HOME=$home "$shards" stop daemon >/dev/null 2>&1 || :; rm -r "$home" "$dir"' EXIT
one() {
	SHARDS_HOME=$home "$shards" run --rm --network "$1" -v "$dir:/t:ro" alpine:3.22 /t/tg binds "$n" 2>/dev/null |
		sed "s/^/$1 $2 /"
}
# The image pulled, and its template made, before any is measured.
one bridge 0 >/dev/null
for r in $(seq "$rounds"); do
	if [ $((r % 2)) = 1 ]; then
		one host "$r"
		one bridge "$r"
	else
		one bridge "$r"
		one host "$r"
	fi
done
