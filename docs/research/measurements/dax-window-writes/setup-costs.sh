#!/bin/sh
# What a guest's write into a range of a DAX window not yet mapped pays before it writes
# (D189, PM M302): 500 writes of 1 MiB over a 2 GiB file, its ranges not yet mapped, mapped
# to be read first (guestwrite's premap) or to be written (premapw), on each kernel given,
# the file evicted from the host's page cache first (cold) or read whole (warm), every arm
# in each round, their order turning. Run with a build carrying setup-costs.patch, whose
# VM process logs each FUSE_SETUPMAPPING's parts to the daemon's log: each run's lines are
# kept, prefixed with its round and arm, beside guestwrite's own line (setup-costs.py
# reads them).
# Usage: setup-costs.sh SHARDS INIT DAX_HOME DATA_DIR ROUNDS OUT LABEL=KERNEL... [-- IMAGE]
#   (DAX_HOME holding IMAGE already; DATA_DIR holding `file`, 2 GiB, and `guestwrite`)
set -eu
shards=$1 init=$2 home=$3 dir=$4 rounds=$5 out=$6
shift 6
kernels="" image=mirror.gcr.io/library/alpine
while [ $# -gt 0 ]; do
    case $1 in
    --) image=$2; break ;;
    *) kernels="$kernels $1" ;;
    esac
    shift
done
config=$(mktemp -d)
trap 'rm -rf "$config"' EXIT
log=$home/daemon.log
leave() {
    if [ "$1" = cold ]; then
        python3 -I -c 'import os, sys; fd = os.open(sys.argv[1], os.O_RDONLY); os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)' "$dir/file"
    else
        cat "$dir/file" >/dev/null
    fi
}
SHARDS_HOME=$home "$shards" stop daemon >/dev/null 2>&1 || true
: >"$out"
echo "host: $(uname -n), $(uname -sr), $(uname -m); load $(cut -d' ' -f1 /proc/loadavg) at the start" >>"$out"
r=0
while [ $r -lt "$rounds" ]; do
    set -- $kernels
    i=0
    while [ $i -lt $((r % $#)) ]; do
        first=$1
        shift
        set -- "$@" "$first"
        i=$((i + 1))
    done
    order="$*"
    for cache in cold warm; do
        for mode in none premap premapw; do
            for k in $order; do
                leave "$cache"
                before=$(wc -l <"$log" 2>/dev/null || echo 0)
                args=""
                [ "$mode" != none ] && args=$mode
                # shellcheck disable=SC2086
                said=$(SHARDS_HOME=$home SHARDS_SHARE_WINDOW_MIB=4096 SHARDS_KERNEL=${k#*=} \
                    SHARDS_INIT=$init DOCKER_CONFIG=$config "$shards" run --rm -v "$dir:/data" "$image" \
                    /data/guestwrite /data/file 500 1048576 $args 2>&1 | tr '\n' ' ')
                arm="${k%%=*}-$cache-$mode"
                echo "$r $arm WRITES $said" >>"$out"
                tail -n "+$((before + 1))" "$log" | grep -E "SETUP-COST|MAP-COST" | sed "s/^/$r $arm /" >>"$out" || true
            done
        done
    done
    r=$((r + 1))
done
SHARDS_HOME=$home "$shards" stop daemon >/dev/null 2>&1 || true
echo "load $(cut -d' ' -f1 /proc/loadavg) at the end" >>"$out"
