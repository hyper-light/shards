#!/bin/sh
# 24 busy loops for SECONDS, unclamped (a launchd job), then ended.
s=${1:-90}
i=0
while [ $i -lt 24 ]; do yes > /dev/null & i=$((i+1)); done
sleep "$s"
kill $(jobs -p) 2>/dev/null
wait
