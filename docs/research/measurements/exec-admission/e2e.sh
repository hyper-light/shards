#!/bin/sh
# The execs-at-once E2E on this host's hypervisor (crates/shards/tests/containers.rs,
# `execs_at_once_each_end_with_their_commands_status`), ROUNDS times (default 10), each
# round's whole output kept in LOGS (default e2e-logs): whether it passed, what it says of
# each burst, and, first, the host, its OS, the revision (REVISION, else git's) and the
# toolchain. A host that cannot run VMs fails rather than skips (SHARDS_REQUIRE_VMS).
#
#   docs/research/measurements/exec-admission/e2e.sh [ROUNDS]
set -u
rounds=${1:-10}
logs=${LOGS:-e2e-logs}
test=execs_at_once_each_end_with_their_commands_status
mkdir -p "$logs"
{
	echo "host $(hostname) $(uname -srm), $(getconf _NPROCESSORS_ONLN) CPUs, load $(uptime | sed 's/.*average[s]*: //')"
	echo "revision ${REVISION:-$(git rev-parse HEAD 2>/dev/null)}"
	rustc --version
	zig version 2>/dev/null | sed 's/^/zig /'
} | tee "$logs/header.txt"
if ! cargo test --profile test-release -p shards --test containers --no-run >"$logs/build.log" 2>&1; then
	tail -40 "$logs/build.log"
	exit 1
fi
passed=0
for i in $(seq 1 "$rounds"); do
	if SHARDS_REQUIRE_VMS=1 cargo test --profile test-release -p shards --test containers "$test" -- --exact --nocapture \
		>"$logs/round-$i.log" 2>&1; then
		passed=$((passed + 1))
		said=passed
	else
		said=FAILED
	fi
	echo "round $i $said"
	grep -E 'execs at once into|never ended|did not end as|refused|SKIP' "$logs/round-$i.log" | cut -c1-400
done
echo "$passed of $rounds rounds passed"
