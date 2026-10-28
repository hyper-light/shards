#!/bin/sh
# PM M370: a group's `cpu.max` held across its microVMs each second (D183, slice 3) where
# the host has no group of processes a user may cap, macOS's and Windows': two microVMs
# under a group, four threads burning in each for SECS seconds, their VM processes' CPU
# time read from the host each second, under a budget of one CPU, of two and of none
# (the ignored test `a_groups_cpu_held_each_second`); with this host, its OS, its load
# and the revision. Run from a user's session on macOS (the agent's QoS clamp starves a
# busy guest: docs/research/measurements/macos-sched/launchd-run.sh).
#
#   docs/research/measurements/cgroup-parent/cpu.sh [SECS]
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../../.." && pwd)
cd "$root"
secs=${1:-30}
echo "host: $(sysctl -n machdep.cpu.brand_string 2>/dev/null || uname -m), $(sw_vers -productName 2>/dev/null || uname -s) $(sw_vers -productVersion 2>/dev/null || uname -r), $(getconf _NPROCESSORS_ONLN) CPUs"
echo "revision: $(git rev-parse --short HEAD)"
echo "load: $(uptime | sed 's/.*load averages*: //')"
M370_SECS=$secs cargo test --profile test-release -p shards --test containers -- --ignored --exact \
	--nocapture a_groups_cpu_held_each_second 2>&1 | grep -E '^M370 '
echo "load after: $(uptime | sed 's/.*load averages*: //')"
