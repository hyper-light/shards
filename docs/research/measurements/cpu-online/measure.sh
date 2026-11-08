#!/bin/sh
# What bringing a microVM's vCPUs online and offline costs its guest (the user's plan, item
# 3: every microVM given the host's CPUs, init bringing the run's --cpus online, `update
# --cpus` onlining more or fewer). A privileged run of CPUS vCPUs takes all but CPU 0
# offline, then online again, ROUNDS times, each pass timed whole on the guest's own clock
# (/proc/timer_list's `now`, nanoseconds; read before and after a pass, so a read's cost is
# a pass's, not a CPU's): one line a pass, `offline|online N_CPUS NANOSECONDS`. Offline
# stops every online CPU for each CPU it takes (kernel/cpu.c, takedown_cpu, stop_machine);
# online starts one.
# Usage: measure.sh SHARDS CPUS ROUNDS   (SHARDS_HOME and DOCKER_CONFIG as the caller sets)
set -eu
shards=$1 cpus=$2 rounds=$3
alpine=mirror.gcr.io/library/alpine@sha256:5291449c3df73caf6ed85e649dec1b9e818b39a5d8c871e97afc13e9cd5e8fa8
"$shards" run --rm --privileged --cpus "$cpus" "$alpine" sh -c '
now() { sed -n "s/^now at \([0-9]*\) nsecs/\1/p" /proc/timer_list | head -n 1; }
c=/sys/devices/system/cpu
n=$(($(nproc) - 1))
r=0
while [ "$r" -lt "$1" ]; do
    t0=$(now); for k in $(seq 1 "$n"); do echo 0 >"$c/cpu$k/online"; done; t1=$(now)
    echo "offline $n $((t1 - t0))"
    t0=$(now); for k in $(seq 1 "$n"); do echo 1 >"$c/cpu$k/online"; done; t1=$(now)
    echo "online $n $((t1 - t0))"
    r=$((r + 1))
done' sh "$rounds"
