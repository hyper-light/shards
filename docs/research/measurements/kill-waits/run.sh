#!/bin/sh
# PM M480 to M482: what a SIGKILL's end costs a microVM by what its command holds, what a
# VM process's end costs once killed, and what a joiner's freeze costs by what it does
# (the ignored tests `kills_cost_by_what_the_command_holds` and
# `freezes_cost_by_what_a_joiner_does`), quiet and beside threads spinning on the host;
# with this host, its OS and the revision. They validate D217's method (ends followed, each
# slice the host's own, kept in its home) and are no figure of it.
#
#   docs/research/measurements/kill-waits/run.sh
#
# KILLS_MIB (64,1024,4096,16384), KILLS_ROUNDS (5), FREEZES_ROUNDS (20) and KILLS_LOAD
# (twice the host's CPUs) as the tests read them.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../../.." && pwd)
cd "$root"
echo "host: $(sysctl -n machdep.cpu.brand_string 2>/dev/null || grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 || uname -m), $(sw_vers -productName 2>/dev/null || uname -s) $(sw_vers -productVersion 2>/dev/null || uname -r), $(getconf _NPROCESSORS_ONLN) CPUs"
echo "revision: $(git rev-parse --short HEAD)"
cargo test --profile test-release -p shards --test containers -- --ignored --exact --nocapture \
	--test-threads 1 kills_cost_by_what_the_command_holds freezes_cost_by_what_a_joiner_does 2>&1 |
	grep -E '^M48[0-2] |panicked|FAILED|test result'
