#!/bin/sh
# PM M182: what a command's datagrams to a member of its network come to as it ends (the
# ignored test `datagrams_a_command_sends_as_it_ends`): one, to a member its guest never
# sent to, the command ending at once or after 500 ms; a burst of BURST 1,400-byte
# datagrams, ending at once or after a second; N of each, interleaved, quiet and with as
# many busy loops as the host has CPUs but two; with this host, its OS, its load and the
# revision. docker-burst.sh is dockerd's burst, for comparison.
#
#   docs/research/measurements/exit-drain/run.sh [N] [BURST]
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../../.." && pwd)
cd "$root"
n=${1:-50}
burst=${2:-200}
echo "host: $(sysctl -n machdep.cpu.brand_string 2>/dev/null || uname -m), $(sw_vers -productName 2>/dev/null || uname -s) $(sw_vers -productVersion 2>/dev/null || uname -r)"
echo "revision: $(git rev-parse --short HEAD)"
cargo test --release -p shards --test containers --no-run 2>/dev/null
measure() {
	echo "$1, load: $(uptime | sed 's/.*load averages*: //')"
	M182_N=$n M182_BURST=$burst cargo test --release -p shards --test containers -- --ignored --nocapture \
		datagrams_a_command_sends_as_it_ends 2>&1 | grep -E '^(one|a burst)'
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
