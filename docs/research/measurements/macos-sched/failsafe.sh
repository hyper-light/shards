#!/bin/zsh
# CPU-bound microVMs under XNU's real-time fail-safe (D4): for each LOAD, runs of busy
# loops for SPIN seconds, their VM processes' threads looked at every 20 ms (threads.py)
# and a host timeshare thread's late wakeups recorded (probe.py) from 2 s before to 4 s
# after. LOAD is N, a run of N busy loops, or NxR, R such runs at once.
# Writes OUT/threads-LOAD.txt, OUT/probe-LOAD.txt and OUT/load.txt.
# Run it through launchd-run.sh, as a user's program runs.
# Usage: failsafe.sh SHARDS HOME OUT SPIN LOAD...
here=${0:A:h}
shards=$1 home=$2 out=$3 spin=$4; shift 4
M=/tmp/claude-501/shards-work/BENCH_RUNNING
if [[ -e $M ]]; then print "another bench holds the Mac: $(cat $M)"; exit 75; fi
print "shards g121 agent: vCPU fail-safe, from $(date -u +%H:%M:%SZ)" > $M
trap 'rm -f $M' EXIT
mkdir -p $out $home/../cfg
export SHARDS_HOME=$home DOCKER_CONFIG=$home/../cfg
$shards run --rm --pull never alpine true || exit 1
for load in "$@"; do
  n=${load%%x*} runs=1
  [[ $load == *x* ]] && runs=${load#*x}
  print "$load before $(uptime)" >> $out/load.txt
  python3 $here/threads.py shards-vm $(( spin + 6 )) > $out/threads-$load.txt &
  local t=$!
  python3 $here/probe.py $(( spin + 6 )) > $out/probe-$load.txt &
  local p=$!
  sleep 2
  local started=()
  for r in $(seq $runs); do
    $shards run --rm --pull never alpine sh -c "i=0; while [ \$i -lt $n ]; do timeout $spin sh -c 'while :; do :; done' & i=\$((i+1)); done; wait" &
    started+=($!)
  done
  wait $started $t $p
  print "$load after $(uptime)" >> $out/load.txt
done
$shards stop daemon >/dev/null 2>&1
