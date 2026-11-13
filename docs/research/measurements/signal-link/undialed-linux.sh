#!/bin/bash
# Item 3 (SIGNALS_UNDIALED, D16) on Linux x86_64 under KVM: adacachy, in a clone of
# p/to-guest-v1 at REV (~/g121-tmp/to-guest3/src), its logs in ~/g121-tmp/to-guest3/logs:
# shards-init's unit suite (its relay builds for Linux alone), the VM process's, then the new
# E2E test, on a loaded host as it is.
#   undialed-linux.sh REV
set -u
rev=$1 base=$HOME/g121-tmp/to-guest3
export CARGO_TARGET_DIR=$base/target TMPDIR=$base/tmp
mkdir -p "$base/logs" "$base/tmp" "$base/tools"
cd "$base/src" || exit 1
git checkout -q --detach "$rev" || exit 1
[ "$(zig version 2> /dev/null)" = 0.16.0 ] || PATH="$(TMPDIR=$base/tools scripts/install-zig):$PATH"
said() {
	echo "# $1: $(git rev-parse --short=12 HEAD); $(date -u +%FT%TZ) $(uname -srm); $(rustc --version); load $(cut -d' ' -f1-3 /proc/loadavg)"
}
log=$base/logs/linux-unit.log
said "unit suites" > "$log"
nice -n 10 cargo test -p shards-init --profile test-release -j 8 > "$base/logs/linux-init.out" 2>&1
echo "## shards-init exit $?" >> "$log"
grep -aE "^test .*(FAILED|signal)|panicked|test result" "$base/logs/linux-init.out" >> "$log"
nice -n 10 cargo test -p shards-vm-process --bin shards-vm --profile test-release -j 8 > "$base/logs/linux-vm.out" 2>&1
echo "## shards-vm exit $?" >> "$log"
grep -aE "^test .*(FAILED|signal|dial|guest_that|readiness)|panicked|test result" "$base/logs/linux-vm.out" >> "$log"
log=$base/logs/linux-undialed.log
said "tests/containers.rs a_signal_connection_never_made_fails_execs_and_kills_at_once" > "$log"
t0=$(date +%s)
nice -n 10 cargo test --profile test-release -p shards --test containers -j 8 -- \
	a_signal_connection_never_made_fails_execs_and_kills_at_once --exact --nocapture --test-threads 1 \
	> "$base/logs/linux-undialed.out" 2>&1
rc=$?
grep -aE "^test |SKIP|panicked|left:|right:|the exec:|test result" "$base/logs/linux-undialed.out" | cut -c1-600 | head -40 >> "$log"
echo "## exit $rc after $(( $(date +%s) - t0 )) s; done $(date -u +%FT%TZ)" >> "$log"
