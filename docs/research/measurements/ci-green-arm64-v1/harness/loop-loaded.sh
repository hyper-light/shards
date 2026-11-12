#!/bin/sh
# Runs the built test binary $BIN_PATH $N times with test filter $FILTER, its tests in
# parallel (the harness's default threads), beside $BURN busy loops; counts each outcome
# and keeps the first lines of each failure. The busy loops end with this script.
set -u
i=0
burners=""
while [ "$i" -lt "$BURN" ]; do
  i=$((i + 1))
  ( while :; do :; done ) &
  burners="$burners $!"
done
trap 'kill $burners 2>/dev/null' EXIT INT TERM
pass=0
fail=0
i=0
while [ "$i" -lt "$N" ]; do
  i=$((i + 1))
  if out=$("$BIN_PATH" "$FILTER" 2>&1); then
    pass=$((pass + 1))
  else
    fail=$((fail + 1))
    echo "run $i FAILED:"
    echo "$out" | grep -E "FAILED|panicked|os error|Custom" | head -6
  fi
done
echo "RESULT filter=$FILTER n=$N burn=$BURN pass=$pass fail=$fail"
