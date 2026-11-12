#!/bin/bash
# Repeated plain runs on jupiter, every run's end kept (R09): usage
#   bash repro.sh BIN HOME_NAME N SECS [PARALLEL] [IMAGE] [EXTRA_RUN_ARGS...]
# BIN: a shards.exe (copied to a directory of its own first); HOME_NAME: the SHARDS_HOME
# under ~/g121-whp-stop-runs, reused (pulled once); each batch's records in a new dir.
set -u
bin=${1:?bin}; name=${2:?home}; n=${3:?n}; secs=${4:?secs}; par=${5:-1}; image=${6:-busybox:1.37}
shift 6 2>/dev/null || shift $#
extra=("$@")
base=$HOME/g121-whp-stop-runs
home=$base/home-$name
batch=$base/batch-$name-$(date +%Y%m%d-%H%M%S)
mkdir -p "$home" "$batch" "$base/docker-config"
export SHARDS_HOME=$(cygpath -w "$home")
export DOCKER_CONFIG=$(cygpath -w "$base/docker-config")
echo "bin $bin home $SHARDS_HOME batch $batch n $n secs $secs par $par image $image extra ${extra[*]:-}" | tee "$batch/summary.txt"
"$bin" version > "$batch/version.txt" 2>&1
if ! "$bin" image inspect "$image" > /dev/null 2>&1; then
  for i in 1 2 3; do "$bin" pull "$image" > "$batch/pull.txt" 2>&1 && break; sleep 30; done
fi
one() {
  local i=$1
  local log=$home/daemon.log
  local before=$(stat -c %s "$log" 2>/dev/null || echo 0)
  local t0=$(date +%s%N)
  "$bin" run --rm "${extra[@]}" "$image" sleep "$secs" > "$batch/run-$i.out" 2> "$batch/run-$i.err"
  local rc=$?
  local t1=$(date +%s%N)
  local ms=$(( (t1 - t0) / 1000000 ))
  echo "run $i rc=$rc ms=$ms start_ns=$t0 end_ns=$t1 log_from=$before" >> "$batch/runs.txt"
  if [ $rc -ne 0 ]; then
    echo "FAIL run $i rc=$rc ms=$ms: $(tr '\r\n' '  ' < "$batch/run-$i.err" | cut -c1-300)" | tee -a "$batch/summary.txt"
  fi
}
began=$(date +%s)
i=0
while [ $i -lt "$n" ]; do
  for j in $(seq 1 "$par"); do
    i=$((i + 1))
    [ $i -gt "$n" ] && break
    one $i &
  done
  wait
done
fails=$(grep -c ' rc=[1-9]' "$batch/runs.txt" 2>/dev/null)
echo "done: $n runs, $fails failed, in $(( $(date +%s) - began )) s" | tee -a "$batch/summary.txt"
cp "$home/daemon.log" "$batch/daemon.log" 2>/dev/null
echo "batch $batch"
