#!/bin/sh
# Where a DAX window's 64 KiB writes into ranges a read mapped first spend their time (D189,
# PM M301): guestwrite's writes by class, each 2 MiB range's first write and the rest (its
# stderr line), on each kernel given, every kernel in each round, their order turning, the
# file evicted from the host's page cache first (cold) or read whole (warm).
# Usage: classes.sh SHARDS INIT DAX_HOME DATA_DIR ROUNDS LABEL=KERNEL... [-- IMAGE]
#   (DAX_HOME holding IMAGE already; DATA_DIR holding `file`, 2 GiB, and `guestwrite`,
#   measure.py's, built for the guest)
set -eu
shards=$1 init=$2 home=$3 dir=$4 rounds=$5
shift 5
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
leave() {
    if [ "$1" = cold ]; then
        python3 -I -c 'import os, sys; fd = os.open(sys.argv[1], os.O_RDONLY); os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)' "$dir/file"
    else
        cat "$dir/file" >/dev/null
    fi
}
SHARDS_HOME=$home "$shards" stop daemon >/dev/null 2>&1 || true
echo "host: $(uname -n), $(uname -sr), $(uname -m); load $(cut -d' ' -f1 /proc/loadavg) at the start"
r=0
while [ $r -lt "$rounds" ]; do
    # This round's order: the kernels turned by the round.
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
        for k in $order; do
            leave "$cache"
            said=$(SHARDS_HOME=$home SHARDS_SHARE_WINDOW_MIB=4096 SHARDS_KERNEL=${k#*=} \
                SHARDS_INIT=$init DOCKER_CONFIG=$config "$shards" run --rm -v "$dir:/data" "$image" \
                /data/guestwrite /data/file 2000 65536 premap 2>&1 | tr '\n' ' ')
            echo "$r ${k%%=*}-$cache $said"
        done
    done
    r=$((r + 1))
done
SHARDS_HOME=$home "$shards" stop daemon >/dev/null 2>&1 || true
echo "load $(cut -d' ' -f1 /proc/loadavg) at the end"
