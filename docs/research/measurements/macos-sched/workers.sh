#!/bin/zsh
# The block stress test's verified reads in 3 s (tests/block.rs concurrent_readers, the
# count it prints), for each ARM in turn, ROUNDS rounds, the order turned each round, with the
# host as it is (`quiet`) and with one busy loop a host CPU at the default priority
# (`burn`), as a user's build would have it. An arm is NAME or NAME:VAR=VALUE,..., the
# environment its tests run with: SHARDS_VM_BINARY a VM process of another build, and with
# a scratch build's, SHARDS_WORKER_POLICY its device workers' policy (default,
# interactive: QoS user-interactive, or realtime: the vCPUs'). One line a test: CONDITION
# ARM ROUND OPS LOAD. Run it from the repository through launchd-run.sh, as a user's
# program runs.
# Usage: workers.sh OUT ROUNDS ARM...
out=$1 rounds=$2; shift 2
typeset -A envs
arms=()
for spec in "$@"; do
  arms+=(${spec%%:*})
  [[ $spec == *:* ]] && envs[${spec%%:*}]=${spec#*:} || envs[${spec%%:*}]=""
done
M=/tmp/claude-501/shards-work/BENCH_RUNNING
if [[ -e $M ]]; then print "another bench holds the Mac: $(cat $M)"; exit 75; fi
print "shards g121 agent: device worker policies, from $(date -u +%H:%M:%SZ)" > $M
burners=()
trap 'for p in $burners; do kill $p 2>/dev/null; done; rm -f $M' EXIT
: > $out
n=${#arms}
for cond in quiet burn; do
  if [[ $cond == burn ]]; then
    for i in $(seq $(sysctl -n hw.ncpu)); do sh -c 'while :; do :; done' & burners+=($!); done
    sleep 2
  fi
  for r in $(seq $rounds); do
    k=$(( (r - 1) % n ))
    order=(${arms[$(( k + 1 )),-1]} ${arms[1,$k]})
    for arm in $order; do
      ops=$(env ${(s:,:)envs[$arm]} cargo test -q -p shards --profile test-release --test block -- concurrent_readers --nocapture 2>&1 | sed -n 's/^verified reads in 3 s: //p')
      print "$cond $arm $r ${ops:-none} $(sysctl -n vm.loadavg | awk '{print $2}')" >> $out
    done
  done
  for p in $burners; do kill $p 2>/dev/null; done
  burners=()
done
