#!/bin/zsh
# Builds of shards after the host's vnodes churn (PM M315): each its own home (given SEED's
# image store and guest by clone) and daemon, a pool of none, its template made first; then
# each trial, a build at a time, each trial's first in turn: a run, VNODES files looked at
# (churn.py, in WORK/churn), and a run whose prefetch says its page-ins (SHARDS_LOG=debug).
# Each run is the call ARGV recorded (`wrap`), with no name and --rm.
# Usage: vnode.sh ARGV SEED WORK TRIALS VNODES NAME=SHARDS...
here=${0:A:h}
zmodload zsh/datetime
argv_file=$1 seed=$2 work=$3 trials=$4 vnodes=$5
shift 5
typeset -A bin
for spec in "$@"; do bin[${spec%%=*}]=${spec#*=}; done
# `wrap` wrote the call as one line, each space between its words escaped; its words hold
# no spaces.
raw=$(cat $argv_file)
args=(${(s: :)${raw//\\ / }})
i=${args[(i)--debug-file]}
(( i <= $#args )) && args[i,i+1]=()
i=${args[(i)--name]}
(( i <= $#args )) && args[i,i+1]=()
args[3,2]=(--rm)
M=${TMPDIR:-/tmp}/shards-work/BENCH_RUNNING
if [[ -e $M ]]; then print "another bench holds the Mac: $(cat $M)"; exit 75; fi
mkdir -p ${M:h} $work/config
print "vnode.sh: $trials trials of ${(k)bin}, from $(date -u +%H:%M:%SZ)" > $M
stop_all() { for arm in ${(k)bin}; do SHARDS_HOME=$work/home-$arm DOCKER_CONFIG=$work/config $bin[$arm] stop daemon >/dev/null 2>&1; done }
trap 'stop_all; rm -f $M' EXIT
for arm in ${(k)bin}; do
  h=$work/home-$arm
  mkdir -p $h
  for p in images guest; do [[ -d $h/$p ]] || cp -cR $seed/$p $h/$p; done
  SHARDS_HOME=$h DOCKER_CONFIG=$work/config $bin[$arm] stop daemon >/dev/null 2>&1
  SHARDS_HOME=$h DOCKER_CONFIG=$work/config SHARDS_LOG=debug SHARDS_POOL=0 $bin[$arm] run daemon --detached >/dev/null 2>&1
  SHARDS_HOME=$h DOCKER_CONFIG=$work/config $bin[$arm] "${args[@]}" < /dev/null > /dev/null 2>&1
done
run_once() {
  local h=$work/home-$1
  local before=$(wc -l < $h/daemon.log)
  local t0=$EPOCHREALTIME
  SHARDS_HOME=$h DOCKER_CONFIG=$work/config $bin[$1] "${args[@]}" < /dev/null > /dev/null 2>&1
  local t1=$EPOCHREALTIME
  print -n "run $(( (t1 - t0) * 1000 )) ms; "
  tail -n +$((before + 1)) $h/daemon.log | grep -o "ran over .* us of it on its CPU; .*\]))" | sed 's/the process.s faults, page-ins, copy-on-write faults and context switches meanwhile/faults, page-ins, cow, csw/' | head -1
}
churn() {
  local a=$(sysctl -n kern.num_recycledvnodes)
  print -n "$(python3 -I $here/churn.py $work/churn $vnodes): "
  local b=$(sysctl -n kern.num_recycledvnodes)
  print -n $((b - a))
}
arms=(${(ko)bin})
for t in $(seq $trials); do
  if (( t % 2 )); then order=($arms); else order=(${(Oa)arms}); fi
  for arm in $order; do
    print -n "$t $arm warm: "; run_once $arm
    print -n "$t $arm churned $(churn) vnodes; then: "; run_once $arm
  done
done
