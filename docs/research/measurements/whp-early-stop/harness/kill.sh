#!/bin/bash
# The early stop's live reproduction (R09): a run's daemon ended from outside mid-run, as
# kiln's between-turns sweep ended the E2E's (TerminateProcess). Usage:
#   bash kill.sh BIN TAG N MODE DELAY_MS
# MODE: cold  - a fresh home's first run, `run -d --name tI busybox:1.37 sleep 30`
#       warm  - an attached `run --rm busybox:1.37 sleep 30` in a home whose template is made
#       vm    - as warm, but the run's VM process is ended instead of the daemon (the case the
#               message was written for)
# Each trial's client status, stdout, stderr and the home's daemon.log are kept in the batch.
set -u
bin=${1:?bin}; tag=${2:?tag}; n=${3:?n}; mode=${4:?mode}; delay=${5:?delay_ms}
base=$HOME/g121-whp-stop-runs
src=$base/home-a
tar=$base/busybox.tar
batch=$base/kill-$tag-$mode-$(date +%Y%m%d-%H%M%S)
mkdir -p "$batch" "$base/docker-config"
export DOCKER_CONFIG=$(cygpath -w "$base/docker-config")
echo "bin $bin ($(sha256sum "$bin" | cut -c1-16)) mode $mode n $n delay ${delay}ms batch $batch" | tee "$batch/summary.txt"
pid_of() { tr -d '\r' < "$1/daemon.pid" 2>/dev/null | awk '{print $1}'; }
end_pid() { taskkill //F //PID "$1" > /dev/null 2>&1 || powershell.exe -NoProfile -Command "Stop-Process -Force -Id $1" > /dev/null 2>&1; }
for i in $(seq 1 "$n"); do
  h=$batch/h$i
  mkdir -p "$h/guest"
  cp "$src"/guest/sha256-* "$h/guest/"
  H=$(cygpath -w "$h")
  SHARDS_HOME=$H "$bin" load -q -i "$(cygpath -w "$tar")" > "$batch/load-$i.txt" 2>&1
  if [ "$mode" != cold ]; then
    # Its template made, and a warm VM waiting: the next run is a pool's.
    SHARDS_HOME=$H "$bin" run --rm busybox:1.37 true > "$batch/prime-$i.txt" 2>&1
  fi
  t0=$(date +%s%N)
  if [ "$mode" = cold ]; then
    SHARDS_HOME=$H "$bin" run -d --name "t$i" busybox:1.37 sleep 30 > "$batch/run-$i.out" 2> "$batch/run-$i.err" &
  else
    SHARDS_HOME=$H "$bin" run --rm busybox:1.37 sleep 30 > "$batch/run-$i.out" 2> "$batch/run-$i.err" &
  fi
  client=$!
  sleep "$(awk "BEGIN{print $delay/1000}")"
  daemon=$(pid_of "$h")
  if [ "$mode" = vm ]; then
    # The run's VM: the oldest shards-vm child of the daemon (the pool's refill is newer).
    target=$(powershell.exe -NoProfile -Command "Get-CimInstance Win32_Process -Filter \"ParentProcessId=$daemon\" | Where-Object { \$_.Name -like 'shards-vm*' } | Sort-Object CreationDate | Select-Object -First 1 -ExpandProperty ProcessId" | tr -d '\r')
  else
    target=$daemon
  fi
  t_kill=$(date +%s%N)
  end_pid "$target"
  wait "$client"
  rc=$?
  t1=$(date +%s%N)
  echo "trial $i mode=$mode rc=$rc client_ms=$(( (t1 - t0) / 1000000 )) ended=${target:-none} (daemon ${daemon:-none}) at_ms=$(( (t_kill - t0) / 1000000 )) after_ms=$(( (t1 - t_kill) / 1000000 )): $(tr '\r\n' '  ' < "$batch/run-$i.err" | cut -c1-400)" | tee -a "$batch/summary.txt"
  cp "$h/daemon.log" "$batch/daemon-$i.log" 2>/dev/null
  # Whatever is left of the home's processes goes; the home with them.
  SHARDS_HOME=$H "$bin" stop daemon > /dev/null 2>&1
  for p in $(powershell.exe -NoProfile -Command "Get-CimInstance Win32_Process | Where-Object { \$_.CommandLine -like '*$(basename "$batch")*h$i*' } | Select-Object -ExpandProperty ProcessId" | tr -d '\r'); do end_pid "$p"; done
  sleep 1
  rm -rf "$h" 2>/dev/null || { sleep 3; rm -rf "$h"; }
done
echo "batch $batch"
