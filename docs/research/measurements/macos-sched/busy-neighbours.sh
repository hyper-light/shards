#!/bin/zsh
# The block stress test's verified reads in 3 s (tests/block.rs concurrent_readers) beside
# two other microVMs each keeping 18 vCPUs busy (runs of 18 busy loops), as a gate's other
# tests' microVMs keep theirs: each ARM's build serving both the neighbours and the stress,
# ROUNDS rounds, the arms in turn. An arm is NAME:SHARDS:VAR=VALUE,...: SHARDS the build
# whose daemon runs the neighbours, the environment the stress's VM process (with
# SHARDS_VM_BINARY, another build's). One line a test: ARM ROUND OPS LOAD.
# Run it from the repository through launchd-run.sh, as a user's program runs.
# Usage: busy-neighbours.sh OUT ROUNDS ARM...
out=$1 rounds=$2; shift 2
typeset -A bins envs
arms=()
for spec in "$@"; do
  name=${spec%%:*} rest=${spec#*:}
  arms+=($name)
  bins[$name]=${rest%%:*}
  [[ $rest == *:* ]] && envs[$name]=${rest#*:} || envs[$name]=""
done
M=/tmp/claude-501/shards-work/BENCH_RUNNING
if [[ -e $M ]]; then print "another bench holds the Mac: $(cat $M)"; exit 75; fi
print "shards g121 agent: block stress beside busy microVMs, from $(date -u +%H:%M:%SZ)" > $M
trap 'rm -f $M' EXIT
: > $out
for r in $(seq $rounds); do
  for arm in $arms; do
    home=/tmp/claude-501/shards-work/fsafe/neighbours-$arm
    mkdir -p $home
    for p in images guest; do [[ -d $home/$p ]] || cp -cR /tmp/claude-501/shards-work/coldk/home-gpu2/$p $home/$p; done
    neighbours=()
    for i in 1 2; do
      SHARDS_HOME=$home DOCKER_CONFIG=$home/../cfg $bins[$arm] run --rm --pull never alpine sh -c \
        'i=0; while [ $i -lt 18 ]; do timeout 20 sh -c "while :; do :; done" & i=$((i+1)); done; wait' >/dev/null 2>&1 &
      neighbours+=($!)
    done
    sleep 4
    ops=$(env ${(s:,:)envs[$arm]} cargo test -q -p shards --profile test-release --test block -- concurrent_readers --nocapture 2>&1 | sed -n 's/^verified reads in 3 s: //p')
    print "$arm $r ${ops:-none} $(sysctl -n vm.loadavg | awk '{print $2}')" >> $out
    wait $neighbours
  done
done
for arm in $arms; do SHARDS_HOME=/tmp/claude-501/shards-work/fsafe/neighbours-$arm $bins[$arm] stop daemon >/dev/null 2>&1; done
