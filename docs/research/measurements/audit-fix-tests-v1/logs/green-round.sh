#!/bin/sh
# Green round (Linux runner, worktree root): format, the changed and new unit tests (no VM
# test is selected), and compiling every affected test target.
. docs/research/measurements/audit-fix-tests-v1/logs/linux-build.env
git rev-parse HEAD
git status --short
zig version
step() {
	echo "== $1"
	shift
	echo "\$ $*"
	"$@" 2>&1
	echo "[exit $?]"
}
step fmt cargo fmt --all --check
step testguest-intercept-unit cargo test -p shards-testguest -j2 -- managed_network::intercept
step managed_output-unit cargo test -p shards -j2 --test managed_output -- a_lost_observer_stream observer_sockets_are_checked ended_member_absence the_gate_releases metadata_requires
step managed_network-unit cargo test -p shards -j2 --test managed_network
step managed_cleanup-unit cargo test -p shards -j2 --test managed_cleanup -- a_child_reaped_through_its_client_is_returned
step no-run cargo test -p shards --no-run -j2 --test managed_output --test managed_network --test managed_limits --test managed_cleanup --test build
