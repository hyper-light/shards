#!/bin/sh
# Why some rounds of a window's cold 1 MiB writes into ranges not yet mapped cost some
# 500 µs more than others (PM M302): N runs back to back, each a FUSE run then a window
# run on kernel K, the file evicted before each, with /proc/vmstat's and pressure's deltas
# (pressure: each kind's total stalled µs) over each run, and its writes' line. Pin it with
# taskset to one kind of a hybrid CPU's cores: its daemon and VMs inherit the set.
# Usage: rounds.sh SHARDS INIT PLAIN_HOME DAX_HOME DATA_DIR KERNEL N OUT
set -u
S=$1 I=$2 P=$3 D=$4 DIR=$5 K=$6 N=$7 OUT=$8
cfg=$(mktemp -d)
evict() { python3 -I -c 'import os,sys; fd=os.open(sys.argv[1], os.O_RDONLY); os.posix_fadvise(fd,0,0,os.POSIX_FADV_DONTNEED)' "$DIR/file"; }
vm() { awk '/^(pgfault|pgmajfault|pgpgin|pgpgout|nr_dirty|nr_writeback|nr_dirtied|nr_written|pgsteal_kswapd|pgsteal_direct|pgscan_kswapd|pgscan_direct|thp_fault_alloc|compact_stall|workingset_refault_file|nr_free_pages|nr_file_pages) /{print $1"="$2}' /proc/vmstat | tr '\n' ' '; }
pr() { for f in cpu io memory; do printf "%s=%s " $f "$(awk '/^some/{print $5}' /proc/pressure/$f | cut -d= -f2)"; done; }
for h in $P $D; do SHARDS_HOME=$h $S stop daemon >/dev/null 2>&1; done
: > "$OUT"
i=0
while [ $i -lt "$N" ]; do
  for arm in plain dax; do
    evict
    if [ $arm = plain ]; then H=$P; WIN=""; else H=$D; WIN=4096; fi
    b=$(vm); bp=$(pr); t0=$(date +%s%N)
    said=$(env SHARDS_HOME=$H ${WIN:+SHARDS_SHARE_WINDOW_MIB=$WIN} SHARDS_KERNEL=$K SHARDS_INIT=$I DOCKER_CONFIG=$cfg $S run --rm -v "$DIR:/data" mirror.gcr.io/library/alpine /data/guestwrite /data/file 500 1048576 2>&1 | tr '\n' ' ')
    t1=$(date +%s%N); a=$(vm); ap=$(pr)
    echo "$i $arm ms=$(( (t1-t0)/1000000 )) WRITES $said" >> "$OUT"
    echo "$i $arm BEFORE $b PRESSURE $bp" >> "$OUT"
    echo "$i $arm AFTER $a PRESSURE $ap" >> "$OUT"
  done
  i=$((i+1))
done
for h in $P $D; do SHARDS_HOME=$h $S stop daemon >/dev/null 2>&1; done
rm -rf "$cfg"
