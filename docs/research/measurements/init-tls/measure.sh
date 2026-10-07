#!/bin/sh
# What shards-init's TLS (the in-VM server, D60) costs a microVM: for each of two inits,
# interleaved run by run, MemTotal and MemAvailable inside the guest as a run starts,
# restored from that init's template (the first run of each builds it and is dropped).
#
#   SHARDS_HOME=$(mktemp -d) docs/research/measurements/init-tls/measure.sh SHARDS KERNEL OLD_INIT NEW_INIT [RUNS]
set -eu
shards=$1 kernel=$2 old=$3 new=$4 runs=${5:-10}
image=alpine@sha256:294b683cb724975bec92580e1e685676bd4b50bda910ddb8c51d4cabeaec77e6
echo "init run memtotal_kib memavailable_kib"
for i in $(seq 0 "$runs"); do
  for which in old new; do
    eval init=\$$which
    SHARDS_KERNEL=$kernel SHARDS_INIT=$init "$shards" run --rm "$image" \
      awk -v w="$which" -v i="$i" '/^MemTotal/ {t=$2} /^MemAvailable/ {a=$2} END {print w, i, t, a}' /proc/meminfo
  done
done | awk '$2 > 0'
