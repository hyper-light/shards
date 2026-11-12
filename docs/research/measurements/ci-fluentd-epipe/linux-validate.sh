#!/bin/sh
# The fix's validation on a Linux runner, run by systemd-run as a bounded unit of its own
# (MemoryMax=16G, CPUQuota=400%) from ~/ci-fluentd-epipe: every ship test of FIX; then the
# flaky test 1000 times each of BASE and FIX at once, 16 runs of each at a time, so that both
# meet the same load (interleaved A/B); then the new test 200 times, 32 at a time.
#
#   systemd-run --user --unit=cife-validate --collect -p MemoryMax=16G -p CPUQuota=400% \
#       sh ~/ci-fluentd-epipe/linux-validate.sh BASE FIX
set -u
base=$1 fix=$2
cd "$HOME/ci-fluentd-epipe"
flaky=daemon::ship::fluentd::tests::a_part_refused_goes_again_in_its_lines_place
new=daemon::ship::stream::tests::a_write_that_meets_its_receivers_end_is_followed_as_one_seen
"./bin/$fix" daemon::ship > "$fix.ship.txt" 2>&1
date -u +%FT%TZ > "$fix.ab.start"
sh repeat.sh "./bin/$base" "$flaky" 1000 16 "$base.ab.flaky.log" > "$base.ab.flaky.summary" 2>&1 &
sh repeat.sh "./bin/$fix" "$flaky" 1000 16 "$fix.ab.flaky.log" > "$fix.ab.flaky.summary" 2>&1 &
wait
{ date -u +%FT%TZ; uptime; } >> "$fix.ab.start"
sh repeat.sh "./bin/$fix" "$new" 200 32 "$fix.new.log" > "$fix.new.summary" 2>&1
{ date -u +%FT%TZ; uptime; } >> "$fix.new.summary"
echo done > "$fix.validate.done"
