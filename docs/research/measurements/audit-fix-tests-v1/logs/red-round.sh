#!/bin/sh
# Red round: the new tests against the unfixed code (findings 6, 8, 10, 13). Linux runner, from
# the worktree root. Expected: each named test FAILS, showing its defect.
. docs/research/measurements/audit-fix-tests-v1/logs/linux-build.env
git rev-parse HEAD
git status --short
echo "== testguest (findings 6, 10, 13)"
echo '$ cargo test -p shards-testguest -j2 -- late_write_waits_out an_aborted_accept_without inputs_have_explicit_local_roles'
cargo test -p shards-testguest -j2 -- late_write_waits_out an_aborted_accept_without inputs_have_explicit_local_roles 2>&1
echo "[exit $?]"
echo "== managed_output (finding 8)"
echo '$ cargo test -p shards -j2 --test managed_output -- a_lost_observer_stream_says_how_its_driver_ended'
cargo test -p shards -j2 --test managed_output -- a_lost_observer_stream_says_how_its_driver_ended 2>&1
echo "[exit $?]"
