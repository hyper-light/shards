#!/bin/sh
# The merged revision's checks on a Linux runner (main at REV with the branch's stream.rs, as
# linux-build.sh NAME REV built it), run as a bounded unit of its own:
#   systemd-run --user --unit=cife-merged --collect -p MemoryMax=16G -p CPUQuota=400% \
#       sh ~/ci-fluentd-epipe/linux-merged.sh NAME
# every ship test; the flaky test and the new one 200 times each, 32 at a time; then clippy
# of the shards package on the tree it was built from.
set -u
name=$1
cd "$HOME/ci-fluentd-epipe"
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
"./bin/$name" daemon::ship > "$name.ship.txt" 2>&1
sh repeat.sh "./bin/$name" daemon::ship::fluentd::tests::a_part_refused_goes_again_in_its_lines_place 200 32 "$name.flaky.log" > "$name.flaky.summary" 2>&1
sh repeat.sh "./bin/$name" daemon::ship::stream::tests::a_write_that_meets_its_receivers_end_is_followed_as_one_seen 200 32 "$name.new.log" > "$name.new.summary" 2>&1
{ date -u +%FT%TZ; uptime; } >> "$name.new.summary"
(cd repo && sha256sum crates/shards/src/daemon/ship/stream.rs && git log --oneline -1 &&
	CARGO_TARGET_DIR="$HOME/ci-fluentd-epipe/target" SHARDS_HELPERS=skip \
		cargo clippy -p shards --all-targets -j4 -- -D warnings) > "$name.clippy.log" 2>&1
echo "$?" > "$name.clippy.done"
echo done > "$name.checks.done"
