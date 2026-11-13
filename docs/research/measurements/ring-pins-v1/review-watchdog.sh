#!/usr/bin/env bash
# Runs one test of a test binary under a watchdog (review.md, F1): a test that never ends is
# killed, and said so, rather than left to run. The red and mutation runs of
# `an_offer_longer_than_its_slot_is_refused` never end: the device's cutting of an offer past
# its slot pushes empty runs forever, its memory growing as fast as it pushes. So the test is
# killed once its resident memory passes CAP_KB (default 256 MiB: what the test needs is a
# slot's 33 runs and a 64 MiB guest it never touches, so passing it is the loop's alone, and
# it stops the loop long before a shared runner feels it), or after MOST_S seconds where it
# grows no memory. Exit: the test's own where it ended; 2 where it was killed.
# Usage: review-watchdog.sh BINARY TEST [CAP_KB] [MOST_S]
set -u
bin=$1 test=$2 cap=${3:-262144} most=${4:-5}
out=$(mktemp)
"$bin" "$test" --exact --test-threads=1 >"$out" 2>&1 &
pid=$!
start=$(date +%s)
while kill -0 "$pid" 2>/dev/null; do
  rss=$(ps -o rss= -p "$pid" | tr -d ' ')
  if [ "${rss:-0}" -gt "$cap" ] || [ $(($(date +%s) - start)) -ge "$most" ]; then
    kill -9 "$pid"
    wait "$pid" 2>/dev/null
    cat "$out"
    rm -f "$out"
    echo "WATCHDOG: $test still running after $(($(date +%s) - start)) s, ${rss:-0} kB resident: killed"
    exit 2
  fi
  sleep 0.01
done
wait "$pid"
code=$?
cat "$out"
rm -f "$out"
echo "WATCHDOG: $test ended, exit $code"
exit "$code"
