#!/usr/bin/env bash
# Plan step 6, slice 1 (R09, R10): a member's signal dispositions, red, green and with its
# guard removed, on one real VM of this host's hypervisor.
#
#   docs/research/measurements/member-exec-tty-v1/signals.sh BASE
#
# Run from the root of a checkout of the branch, BASE the main revision before the fix (its
# crates/init/src/domains.rs, which reset only the signal mask). Each phase rebuilds
# shards-init and runs tests/member_exec.rs's one test; its log goes to a directory of its
# own, named on the last line. The tree is restored as found.
set -uo pipefail
base=${1:?the main revision before the fix}
T=a_member_starts_with_every_signal_at_its_default
S=$(mktemp -d "${TMPDIR:-/tmp}/s6-signals.XXXX")
cp crates/init/src/domains.rs "$S/domains.fixed"
cp crates/init/src/defaults.rs "$S/defaults.fixed"
echo "rev $(git rev-parse --short HEAD), changed: $(git status --short | tr '\n' ' ')"
uname -a | cut -c1-100
phase() {
	echo "=== $1 start $(date -u +%H:%M:%S)"
	nice -n 10 cargo test -p shards --profile test-release --test member_exec -- --exact $T --nocapture >"$S/$1.log" 2>&1
	echo "=== $1 exit $? at $(date -u +%H:%M:%S)"
	grep -a -E '^test |test result|confined (signals|pipe)|panicked|SKIP' "$S/$1.log" | head -20
}
touch crates/init/src/*.rs crates/testguest/src/*.rs
# Red: the member's start as main had it.
git show "$base:crates/init/src/domains.rs" >crates/init/src/domains.rs
touch crates/init/src/domains.rs
echo "red: defaults::signals calls in domains.rs: $(grep -c 'defaults::signals' crates/init/src/domains.rs)"
phase red
cp "$S/domains.fixed" crates/init/src/domains.rs
touch crates/init/src/domains.rs
echo "green: defaults::signals calls in domains.rs: $(grep -c 'defaults::signals' crates/init/src/domains.rs)"
phase green
# The guard removed: defaults::signals without its loop of dispositions, the mask alone.
perl -0pi -e 's/    for sig in 1\.\.=shards_abi::run::NSIG as libc::c_int \{\n        \/\/ SAFETY: signal\(2\) setting a disposition[^\n]*\n[^\n]*\n    \}\n//' crates/init/src/defaults.rs
touch crates/init/src/defaults.rs
echo "mutant: disposition loop lines left: $(grep -c 'SAFETY: signal(2) setting a disposition' crates/init/src/defaults.rs)"
phase mutant
cp "$S/defaults.fixed" crates/init/src/defaults.rs
touch crates/init/src/defaults.rs
echo "restored: $(git status --short | tr '\n' ' ')"
echo "logs in $S"
