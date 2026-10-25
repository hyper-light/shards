#!/bin/sh
# N (default 24) busy loops for SECONDS, then ended: unclamped through launchd-run.sh.
# Usage: busy-loops.sh SECONDS [N]
s=${1:-90}
i=0
while [ $i -lt ${2:-24} ]; do yes > /dev/null & i=$((i+1)); done
sleep "$s"
kill $(jobs -p) 2>/dev/null
wait
