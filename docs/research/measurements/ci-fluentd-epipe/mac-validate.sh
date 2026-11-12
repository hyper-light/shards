#!/bin/sh
# The fix's validation on this Mac, as linux-validate.sh's on Linux: the flaky test 1000 times
# each of BASE and FIX at once, P runs of each at a time, so that both meet the same load
# (interleaved A/B), then the new test 200 times, P at a time; logs to OUT.
#
#   mac-validate.sh BASE FIX P OUT
set -u
base=$1 fix=$2 p=$3 out=$4
here=$(cd "$(dirname "$0")" && pwd)
flaky=daemon::ship::fluentd::tests::a_part_refused_goes_again_in_its_lines_place
new=daemon::ship::stream::tests::a_write_that_meets_its_receivers_end_is_followed_as_one_seen
{ date -u +%FT%TZ; uptime; } > "$out/ab.start"
sh "$here/repeat.sh" "$base" "$flaky" 1000 "$p" "$out/base.ab.flaky.log" > "$out/base.ab.flaky.summary" 2>&1 &
sh "$here/repeat.sh" "$fix" "$flaky" 1000 "$p" "$out/fix.ab.flaky.log" > "$out/fix.ab.flaky.summary" 2>&1 &
wait
{ date -u +%FT%TZ; uptime; } >> "$out/ab.start"
sh "$here/repeat.sh" "$fix" "$new" 200 "$p" "$out/fix.new.log" > "$out/fix.new.summary" 2>&1
{ date -u +%FT%TZ; uptime; } >> "$out/fix.new.summary"
echo done > "$out/validate.done"
