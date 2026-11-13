#!/bin/bash
# Item 3's kill half (SIGNALS_UNDIALED, D16) on Linux x86_64 under KVM: adacachy, in its clone
# of p/to-guest-v1 (~/g121-tmp/to-guest3/src), its logs in ~/g121-tmp/to-guest3/logs. A run
# whose guest cannot make its signal connection, killed with nothing asked of it before
# (undialed-kill-experiment.patch's `undialed_kill_alone`: how long `kill` took and what the
# daemon's log says of it), once each:
#   kill-green          REV with the experiment;
#   kill-mutation-host  REV with the host's relay passing SIGNALS_UNDIALED by, and the experiment;
#   kill-red            BASE (origin/main) with undialed-red.patch and the experiment;
# then never-made-green: REV's own E2E test, whose assertion holds the kill to its VM
# process's own stop, never the daemon's fallback.
#   undialed-kill.sh REV BASE
set -u
rev=$1 main=$2 base=$HOME/g121-tmp/to-guest3
export CARGO_TARGET_DIR=$base/target TMPDIR=$base/tmp
mkdir -p "$base/logs" "$base/tmp" "$base/tools"
cd "$base/src" || exit 1
git checkout -q --detach "$rev" || exit 1
[ "$(zig version 2> /dev/null)" = 0.16.0 ] || PATH="$(TMPDIR=$base/tools scripts/install-zig):$PATH"
rec=docs/research/measurements/signal-link
cp "$rec/undialed-red.patch" "$rec/undialed-kill-experiment.patch" "$base/"

said() {
	echo "# $1: $(git rev-parse --short=12 HEAD), changed: $(git status --short | awk '{print $2}' | tr '\n' ' '); $(date -u +%FT%TZ) $(uname -srm); $(rustc --version); load $(cut -d' ' -f1-3 /proc/loadavg)"
}

# e2e TEST LOG: one test of tests/containers.rs, alone.
e2e() {
	local log=$base/logs/$2.log out=$base/logs/$2.out t0
	t0=$(date +%s)
	said "tests/containers.rs $1" > "$log"
	nice -n 10 cargo test --profile test-release -p shards --test containers -j 8 -- "$1" --exact --nocapture --test-threads 1 > "$out" 2>&1
	local rc=$?
	grep -aE "^KILL|^test |SKIP|panicked|did not exit|left:|right:|the exec:|test result" "$out" | cut -c1-600 | head -40 >> "$log"
	echo "## exit $rc after $(( $(date +%s) - t0 )) s" >> "$log"
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

git apply "$base/undialed-kill-experiment.patch" || exit 1
e2e undialed_kill_alone kill-green
git checkout -q -- .

host=crates/shards/src/workload.rs
mutate $host '            kind::SIGNALS_UNDIALED => lose(
                to,
                undialed(payload).ok_or("the guest sent a malformed failure of its signal dial")?,
            ),' '            kind::SIGNALS_UNDIALED => {
                undialed(payload).ok_or("the guest sent a malformed failure of its signal dial")?;
            }' || exit 1
git apply "$base/undialed-kill-experiment.patch" || exit 1
e2e undialed_kill_alone kill-mutation-host
git checkout -q -- .

git checkout -q --detach "$main" && git apply "$base/undialed-red.patch" && git apply "$base/undialed-kill-experiment.patch" || exit 1
e2e undialed_kill_alone kill-red
git checkout -q -- . && git checkout -q --detach "$rev"

e2e a_signal_connection_never_made_fails_execs_and_kills_at_once never-made-green
echo "# done $(date -u +%FT%TZ)" >> "$base/logs/never-made-green.log"
