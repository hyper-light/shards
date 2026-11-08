#!/bin/sh
# PM M181: whether a port a run on the host's network (D124) served is the host's to bind
# again once the run's end is told, its server killed with it (`shards kill`, its client
# waited for as a shell waits), N runs alone and N containers joining one such run (D119),
# the ignored test `ports_held_past_their_containers_end`: quiet, then with as many busy
# loops as the host has CPUs but two; with this host, its OS, its load and the revision.
#
#   docs/research/measurements/network-host/release.sh [N]
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../../.." && pwd)
cd "$root"
n=${1:-100}
echo "host: $(sysctl -n machdep.cpu.brand_string 2>/dev/null || uname -m), $(sw_vers -productName 2>/dev/null || uname -s) $(sw_vers -productVersion 2>/dev/null || uname -r)"
echo "revision: $(git rev-parse --short HEAD)"
cargo test --release -p shards --test containers --no-run 2>/dev/null
measure() {
	echo "$1, load: $(uptime | sed 's/.*load averages*: //')"
	M181_N=$n cargo test --release -p shards --test containers -- --ignored --nocapture \
		ports_held_past_their_containers_end 2>&1 | grep -E '^(runs|joiners): '
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
