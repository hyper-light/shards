#!/bin/sh
# A VM process's own host mappings on Linux, before its DAX windows map anything: the
# baseline step 4's bound on a window's ranges takes from vm.max_map_count (D189). For each
# vCPU count, a run of IMAGE that sleeps, with no volume and with one (a window), and with
# SHARDS_SHARE_WINDOW_MIB=0 (none): its VM process's /proc/PID/maps lines while it sleeps.
# Usage: vm-maps.sh SHARDS HOME [IMAGE]
set -eu
shards=$1 home=$2 image=${3:-mirror.gcr.io/library/alpine}
cfg=$(mktemp -d); vol=$(mktemp -d)
export SHARDS_HOME="$home" DOCKER_CONFIG="$cfg"
echo "host $(hostname); $(uname -r); $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2); max_map_count $(cat /proc/sys/vm/max_map_count)"
for window in default 0; do
    "$shards" stop daemon >/dev/null 2>&1 || true
    if [ "$window" = 0 ]; then export SHARDS_SHARE_WINDOW_MIB=0; else unset SHARDS_SHARE_WINDOW_MIB; fi
    for cpus in 1 2 8 16 32; do
        for v in none volume; do
            name=vm-maps-$$-$window-$cpus-$v
            set -- run --rm --pull never --name "$name" --cpus "$cpus"
            [ "$v" = volume ] && set -- "$@" -v "$vol:/v"
            "$shards" "$@" "$image" sleep 4 >/dev/null 2>&1 &
            run=$!
            sleep 2.5
            # The run's own VM process, as inspect names it (a pool's warm VMs beside it).
            vm=$("$shards" inspect -f '{{.State.Pid}}' "$name" 2>/dev/null || echo 0)
            n=$(wc -l < "/proc/$vm/maps" 2>/dev/null || echo -)
            echo "window $window cpus $cpus share $v: vm $vm maps $n"
            wait "$run" || true
        done
    done
done
"$shards" stop daemon >/dev/null 2>&1 || true
rm -rf "$cfg" "$vol"
