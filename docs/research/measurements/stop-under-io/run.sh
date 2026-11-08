#!/bin/sh
# PM M419: stops beside a gigabyte build (the ignored test `stops_beside_a_gigabyte_build`):
# four threads each starting a run of `sleep` and stopping it, ROUNDS times each with
# nothing beside them, then for as long as an_image_of_gigabytes_boots's build goes on
# beside them; with this host, its OS and the revision. On CI's Windows runners, where its
# stops gave up: `gh workflow run budget-e2e.yml -f test=windows:containers
# -f name=ignored:stops_beside_a_gigabyte_build`, its lines `M419 ` in each job's log.
#
#   docs/research/measurements/stop-under-io/run.sh [ROUNDS]
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../../.." && pwd)
cd "$root"
echo "host: $(sysctl -n machdep.cpu.brand_string 2>/dev/null || uname -m), $(sw_vers -productName 2>/dev/null || uname -s) $(sw_vers -productVersion 2>/dev/null || uname -r), $(getconf _NPROCESSORS_ONLN) CPUs"
echo "revision: $(git rev-parse --short HEAD)"
M419_ROUNDS=${1:-10} cargo test --profile test-release -p shards --test containers -- --ignored --exact \
	--nocapture stops_beside_a_gigabyte_build 2>&1 | grep -E '^M419 '
