#!/usr/bin/env bash
# Windows (Git Bash): runs a build's test binary (runs.sh build) N times with libtest's
# ARGS, as runs.sh loop does, and when a run is still going after LIMIT seconds has the
# debugger print every thread's stack (cdb, WinDbg's console debugger, attached
# noninvasively: `~*kn`) into stacks-RUN.txt before it ends the run. A line a run in
# summary.txt: its number, exit code, seconds and the tests that failed or hung.
#   hang.sh LABEL N LIMIT ARGS...
set -uo pipefail
label=$1
n=$2
limit=$3
shift 3
dir=$HOME/api-flakes/$label
src=$(cat "$dir/SRC")
: > "$dir/summary.txt"
i=1
while [ "$i" -le "$n" ]; do
  log=$dir/run-$i.log
  began=$(date +%s)
  (cd "$src/crates/api" && exec "$dir/api.exe" "$@") > "$log" 2>&1 &
  job=$!
  while kill -0 "$job" 2> /dev/null && [ $(($(date +%s) - began)) -lt "$limit" ]; do sleep 1; done
  if kill -0 "$job" 2> /dev/null; then
    winpid=$(cat "/proc/$job/winpid")
    cdbX64.exe -pv -p "$winpid" -c '.symfix+ C:\symcache; .reload; ~*kn; q' > "$dir/stacks-$i.txt" 2>&1
    taskkill //F //T //PID "$winpid" > /dev/null 2>&1
    wait "$job"
    code=hung
  else
    wait "$job"
    code=$?
  fi
  took=$(($(date +%s) - began))
  failed=$(sed -n -e 's/^test \(.*\) \.\.\. FAILED$/\1/p' \
    -e 's/^test \(.*\) has been running for over 60 seconds$/HUNG:\1/p' "$log" | tr '\n' ' ')
  printf '%s %s %ss %s\n' "$i" "$code" "$took" "$failed" >> "$dir/summary.txt"
  if [ "$code" = 0 ]; then rm -f "$log"; fi
  i=$((i + 1))
done
printf '%s: %s of %s runs failed or hung\n' "$label" "$(awk '$2 != 0' "$dir/summary.txt" | wc -l)" "$n"
