#!/bin/bash
# The first run of a fresh home, as engine_ring's tests make theirs (R09): usage
#   bash fresh.sh BIN N PAR TAG [RUN_ARGS...]
# Each trial: a new SHARDS_HOME given the guest's files and the image (load), one
# `run -d --name tI [RUN_ARGS] busybox:1.37 sleep 5`, then rm -f, the daemon stopped. A failed
# trial's home is kept whole; a passing one's removed.
set -u
bin=${1:?bin}; n=${2:?n}; par=${3:?par}; tag=${4:?tag}
shift 4
args=("$@")
base=$HOME/g121-whp-stop-runs
src=$base/home-a
tar=$base/busybox.tar
batch=$base/fresh-$tag-$(date +%Y%m%d-%H%M%S)
mkdir -p "$batch" "$base/docker-config"
export DOCKER_CONFIG=$(cygpath -w "$base/docker-config")
if [ ! -f "$tar" ]; then
  SHARDS_HOME=$(cygpath -w "$src") "$bin" save -o "$(cygpath -w "$tar")" busybox:1.37 || exit 1
fi
echo "bin $bin n $n par $par tag $tag args ${args[*]:-} batch $batch" | tee "$batch/summary.txt"
trial() {
  local i=$1
  local h=$batch/h$i
  mkdir -p "$h/guest"
  cp "$src"/guest/sha256-* "$h/guest/"
  local H
  H=$(cygpath -w "$h")
  SHARDS_HOME=$H "$bin" load -q -i "$(cygpath -w "$tar")" > "$batch/load-$i.txt" 2>&1
  local t0=$(date +%s%N)
  SHARDS_HOME=$H "$bin" run -d --name "t$i" "${args[@]}" busybox:1.37 sleep 5 > "$batch/run-$i.out" 2> "$batch/run-$i.err"
  local rc=$?
  local t1=$(date +%s%N)
  echo "trial $i rc=$rc ms=$(( (t1 - t0) / 1000000 )) start_ns=$t0" >> "$batch/runs.txt"
  if [ $rc -ne 0 ]; then
    echo "FAIL trial $i rc=$rc ms=$(( (t1 - t0) / 1000000 )): $(tr '\r\n' '  ' < "$batch/run-$i.err" | cut -c1-300)" >> "$batch/summary.txt"
    SHARDS_HOME=$H "$bin" ps -a > "$batch/ps-$i.txt" 2>&1
    SHARDS_HOME=$H "$bin" logs "t$i" > "$batch/logs-$i.txt" 2>&1
  fi
  SHARDS_HOME=$H "$bin" rm -f "t$i" > /dev/null 2>&1
  SHARDS_HOME=$H "$bin" stop daemon > "$batch/stop-$i.txt" 2>&1
  if [ $rc -eq 0 ]; then
    rm -rf "$h" 2>/dev/null || { sleep 2; rm -rf "$h"; }
  else
    cp "$h/daemon.log" "$batch/daemon-$i.log" 2>/dev/null
  fi
}
began=$(date +%s)
i=0
while [ $i -lt "$n" ]; do
  for j in $(seq 1 "$par"); do
    i=$((i + 1))
    [ $i -gt "$n" ] && break
    trial $i &
  done
  wait
done
fails=$(grep -c ' rc=[1-9]' "$batch/runs.txt" 2>/dev/null)
echo "done: $n trials, $fails failed, in $(( $(date +%s) - began )) s" | tee -a "$batch/summary.txt"
grep FAIL "$batch/summary.txt" | head -20
echo "batch $batch"
