#!/bin/sh
# The code-mode helper stopped and continued while it waits, as a job-control stop, a
# debugger or a cgroup freeze does: runs sandbox::tests::a_worker_not_reaped_ends_no_other
# from test binary $1, $2 times; each time, once its helper (comm shards-sandbox) is up,
# sends it SIGSTOP then SIGCONT, and with WORKERS=1 its workers (comm shards-program, each
# waiting in read(2) for its program) too. Counts each outcome, with the helper's end the
# test says. Signals only processes of the test it started.
#
#   [WORKERS=1] stop-helper.sh TEST_BINARY N
set -u
bin=$1
n=$2
pass=0
fail=0
i=0
while [ "$i" -lt "$n" ]; do
  i=$((i + 1))
  "$bin" --exact sandbox::tests::a_worker_not_reaped_ends_no_other > /tmp/stop-helper.out 2>&1 &
  t=$!
  h=""
  k=0
  while [ -z "$h" ] && [ "$k" -lt 500 ]; do
    h=$(pgrep -P "$t" -x shards-sandbox | head -1)
    k=$((k + 1))
  done
  if [ -n "$h" ]; then
    # In its poll, between the test's messages.
    sleep 0.2
    w=""
    if [ "${WORKERS:-0}" = 1 ]; then w=$(pgrep -P "$h" -x shards-program); fi
    kill -STOP "$h" $w 2>/dev/null
    kill -CONT "$h" $w 2>/dev/null
  fi
  if wait "$t"; then
    pass=$((pass + 1))
  else
    fail=$((fail + 1))
    echo "run $i: $(grep -o 'helper_signal=[0-9]*' /tmp/stop-helper.out | head -1) $(grep -o 'worker_two_ran_on=[a-z]*' /tmp/stop-helper.out | head -1) $(grep -o 'first=[^ ]*' /tmp/stop-helper.out | head -1)"
  fi
done
echo "RESULT stopped-and-continued workers=${WORKERS:-0} n=$n pass=$pass fail=$fail ($(uname -m))"
