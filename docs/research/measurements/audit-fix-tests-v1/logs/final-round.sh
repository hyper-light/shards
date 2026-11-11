#!/bin/sh
# Final round (Linux runner, worktree root): format; the new and changed unit tests (no VM test
# is selected); each guard-removal mutation, which must fail its own test; the tree restored;
# and every affected test target compiled.
. docs/research/measurements/audit-fix-tests-v1/logs/runner-prelude.sh
out=$(mktemp)
git rev-parse HEAD
git status --short
step() {
	echo "== $1"
	shift
	echo "\$ $*"
	"$@" >"$out" 2>&1
	status=$?
	grep -v '^warning: aws-lc-sys\|^   Compiling ' "$out"
	echo "[exit $status]"
}
mutate() {
	name=$1
	shift
	patch -p1 -s <"$m/$name.patch" || {
		echo "== $name: patch did not apply"
		return
	}
	step "mutation $name (must fail)" timeout 240 "$@"
	patch -p1 -R -s <"$m/$name.patch"
	touch $(targets "$m/$name.patch")
}
before=$(git diff | sha256sum)
step fmt cargo fmt --all --check
step "finding 1 alone" cargo test -p shards -j2 --test managed_output -- --exact observer_sockets_are_checked_by_identity_beside_other_tests_descriptors --nocapture
step managed_output-unit cargo test -p shards -j2 --test managed_output -- a_lost_observer_stream observer_sockets_are_checked ended_member_absence the_gate_releases metadata_requires
step managed_network-unit cargo test -p shards -j2 --test managed_network
step managed_cleanup-unit cargo test -p shards -j2 --test managed_cleanup -- a_child_reaped_through_its_client_is_returned
step testguest-intercept-unit cargo test -p shards-testguest -j2 -- managed_network::intercept
mutate m01-identity-check-blind cargo test -p shards -j2 --test managed_output -- observer_sockets_are_checked
mutate m03-status-only-verdict cargo test -p shards -j2 --test managed_network -- a_guest_verdict_is_its_pass_line
mutate m06-late-write-no-retry cargo test -p shards-testguest -j2 -- late_write_waits_out
mutate m08-next-hides-driver cargo test -p shards -j2 --test managed_output -- a_lost_observer_stream
mutate m09-no-positive-control cargo test -p shards -j2 --test managed_output -- ended_member_absence
mutate m10-udp-accepts-local-origins cargo test -p shards-testguest -j2 -- inputs_have_explicit_local_roles
mutate m12-gate-admits-any-peer cargo test -p shards -j2 --test managed_output -- the_gate_releases
mutate m13-abort-is-a-stop cargo test -p shards-testguest -j2 -- an_aborted_accept_without
after=$(git diff | sha256sum)
echo "== tree restored after mutations: $([ "$before" = "$after" ] && echo yes || echo NO)"
step no-run cargo test -p shards --no-run -j2 --test managed_output --test managed_network --test managed_limits --test managed_cleanup --test build
rm -f "$out"
