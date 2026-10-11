#!/bin/sh
# PM M180: what a guest that asks for its gateway's MAC loses. A UDP server on the host's
# network (D124) answers one datagram from this host and exits, at the loopback's address
# (its answer relayed by init's supervisor) and at every address (its own), N runs each;
# each run interleaved with one whose guest first deleted its neighbors (CAP_NET_ADMIN), as
# its gateway was unknown before init gave the gateway's MAC, so that its answer waits on
# ARP's answer. Answers lost (none in 5 s) of each, with the host quiet and then with as
# many busy loops as it has CPUs but two, with this host, its OS, its load and the revision
# measured.
#
#   docs/research/measurements/gateway-neighbor/run.sh [N]
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../../.." && pwd)
cd "$root"
n=${1:-500}
echo "host: $(sysctl -n machdep.cpu.brand_string 2>/dev/null || uname -m), $(sw_vers -productName 2>/dev/null || uname -s) $(sw_vers -productVersion 2>/dev/null || uname -r)"
echo "revision: $(git rev-parse --short HEAD)"
cargo test --release -p shards --test containers --no-run 2>/dev/null
measure() {
	echo "$1, load: $(uptime | sed 's/.*load averages*: //')"
	M180_N=$n cargo test --release -p shards --test containers -- --ignored --nocapture \
		answers_lost_without_the_gateways_mac 2>&1 | grep -E '^(relay|direct) '
	echo "load after: $(uptime | sed 's/.*load averages*: //')"
}
measure quiet
busy=""
trap 'for p in $busy; do kill "$p" 2>/dev/null || :; done' EXIT
cpus=$(getconf _NPROCESSORS_ONLN)
for _ in $(seq $((cpus > 2 ? cpus - 2 : 1))); do
	yes >/dev/null &
	busy="$busy $!"
done
measure loaded
