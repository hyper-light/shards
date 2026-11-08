#!/bin/sh
# PM M247: how long a member of a shards network waits for a name its network holds, from
# inside its microVM: testguest's `dnsrtt` asks 127.0.0.11 (init's relay to the network
# process) for another member's name N times, one at a time and a millisecond apart, and
# says p50, p90, p99 and the slowest, in microseconds. What a resolver that caches pays
# each time, now that a network's answers are kept by no cache (D46). The host quiet, then
# with as many busy loops as it has CPUs but two; this host, its OS, its load and the
# revision measured.
#
#   docs/research/measurements/dns-lookup/run.sh [N]
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../../.." && pwd)
cd "$root"
n=${1:-2000}
echo "host: $(sysctl -n machdep.cpu.brand_string 2>/dev/null || uname -m), $(sw_vers -productName 2>/dev/null || uname -s) $(sw_vers -productVersion 2>/dev/null || uname -r)"
echo "revision: $(git rev-parse --short HEAD)"
cargo test --release -p shards --test containers --no-run 2>/dev/null
measure() {
	echo "$1, load: $(uptime | sed 's/.*load averages*: //')"
	M247_N=$n cargo test --release -p shards --test containers -- --ignored --nocapture \
		a_names_lookup_round_trip 2>&1 | grep -E '^dnsrtt '
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
