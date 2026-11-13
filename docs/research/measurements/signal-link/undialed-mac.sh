#!/bin/bash
# Item 3 (SIGNALS_UNDIALED, D16) on macOS arm64 under Hypervisor.framework: the Mac Studio
# (kiln's adas-mac-studio.local-2), in a clone of p/to-guest-v1 (~/to-guest/src) that each
# step sets to its own source, one target directory for them all (~/to-guest/target), each
# step's log in ~/to-guest/logs:
#   green          REV: the VM process's unit suite, the new E2E test, and the signal
#                  link's other E2E tests, whose helpers it shares;
#   mutation-host  REV with the host's relay passing SIGNALS_UNDIALED by: the new unit test
#                  and the new E2E test;
#   mutation-init  REV with init's notice made to say nothing: the new E2E test;
#   red            BASE (origin/main) with the test hook and the tests alone
#                  (undialed-red.patch): the new E2E test.
#   undialed-mac.sh REV BASE
set -u
rev=$1 main=$2 base=$HOME/to-guest
export CARGO_TARGET_DIR=$base/target TMPDIR=$base/tmp
mkdir -p "$base/logs" "$base/tmp" "$base/tools"
cd "$base/src" || exit 1
git checkout -q --detach "$rev" || exit 1
[ "$(zig version 2> /dev/null)" = 0.16.0 ] || PATH="$(TMPDIR=$base/tools scripts/install-zig):$PATH"
rec=docs/research/measurements/signal-link
cp "$rec/undialed-red.patch" "$base/undialed-red.patch"

said() {
	echo "# $1: $(git rev-parse --short=12 HEAD), changed: $(git status --short | awk '{print $2}' | tr '\n' ' '); $(date -u +%FT%TZ) $(uname -srm) macOS $(sw_vers -productVersion); $(rustc --version); load $(sysctl -n vm.loadavg)"
}

# e2e FILE TEST LOG: one E2E test, alone.
e2e() {
	local log=$base/logs/$3.log out=$base/logs/$3.out t0
	t0=$(date +%s)
	said "tests/$1.rs $2" > "$log"
	nice -n 10 cargo test --profile test-release -p shards --test "$1" -- "$2" --exact --nocapture --test-threads 1 > "$out" 2>&1
	local rc=$?
	grep -aE "^test |SKIP|panicked|left:|right:|the exec:|test result" "$out" | cut -c1-600 | head -40 >> "$log"
	echo "## exit $rc after $(( $(date +%s) - t0 )) s" >> "$log"
}

# unit FILTER LOG: the VM process's unit tests FILTER names.
unit() {
	local log=$base/logs/$2.log out=$base/logs/$2.out
	said "shards-vm unit tests ${1:-(all)}" > "$log"
	nice -n 10 cargo test -p shards-vm-process --bin shards-vm --profile test-release -- $1 > "$out" 2>&1
	local rc=$?
	grep -aE "^test .*(FAILED|signal|dial|frame|exec_whose|guest_that|only_execs|readiness)|panicked|did not fail|test result" "$out" | cut -c1-600 | head -60 >> "$log"
	echo "## exit $rc" >> "$log"
}

# mutate FILE OLD NEW: OLD, found once in FILE, replaced by NEW.
mutate() {
	python3 - "$1" "$2" "$3" <<'EOF'
import sys
path, old, new = sys.argv[1:]
text = open(path).read()
assert text.count(old) == 1, f"{path}: {text.count(old)} of {old!r}"
open(path, "w").write(text.replace(old, new))
EOF
}

unit "" green-unit
e2e containers a_signal_connection_never_made_fails_execs_and_kills_at_once green-undialed
e2e containers a_lost_signal_connection_fails_execs_and_kills_at_once green-lost
e2e containers a_runs_end_says_no_lost_signal_connection green-end
e2e containers a_paused_microvms_unpause_waits_for_no_exec_on_its_way green-pause
e2e saved what_a_restoring_microvm_is_sent_goes_on_its_guests_dial green-restore

host=crates/shards/src/workload.rs
mutate $host '            kind::SIGNALS_UNDIALED => lose(
                to,
                undialed(payload).ok_or("the guest sent a malformed failure of its signal dial")?,
            ),' '            kind::SIGNALS_UNDIALED => {
                undialed(payload).ok_or("the guest sent a malformed failure of its signal dial")?;
            }' || exit 1
unit workload::tests::a_guest_that_cannot_dial_its_signal_connection_loses_it_at_once mutation-host-unit
e2e containers a_signal_connection_never_made_fails_execs_and_kills_at_once mutation-host
git checkout -- $host

init=crates/init/src/run.rs
mutate $init '    held.extend(&[
        &run::header(kind::SIGNALS_UNDIALED, 4),
        &e.raw_os_error().unwrap_or(0).to_be_bytes(),
    ]);' '    let _ = (held, e);' || exit 1
e2e containers a_signal_connection_never_made_fails_execs_and_kills_at_once mutation-init
git checkout -- $init

git checkout -q --detach "$main" && git apply "$base/undialed-red.patch" || exit 1
e2e containers a_signal_connection_never_made_fails_execs_and_kills_at_once red
git checkout -q -- . && git checkout -q --detach "$rev"
echo "# done $(date -u +%FT%TZ)" >> "$base/logs/red.log"
