# The checkpoint test's A/B, engine.rs whole held to MASK, ROUNDS rounds, pre and post in turn
# (the order alternating): each run's result, and the checkpoint test's rm -f.
rounds=$1; mask=$2
for r in $(seq "$rounds"); do
  if [ $((r % 2)) = 1 ]; then order="pre post"; else order="post pre"; fi
  for t in $order; do
    log=~/ab-$t-$r.keep; rm -f $log
    out=$(SHARDS_TEST_KEEP_LOG=$(cygpath -w $log) bash /tmp/wcg-run2.sh ~/wcg-$t ~/wcg-target-$t "$mask" engine 2>&1)
    res=$(printf '%s\n' "$out" | grep -a -E "^test result" | tail -1)
    cp=$(printf '%s\n' "$out" | grep -a -E "^test the_docker_cli_checkpoints_and_restores_microvms \.\.\." | tail -1)
    rm_line=$(grep -a "^=== rm -f" $log 2>/dev/null | head -1)
    lost=$(grep -a -c "signal connection is lost" $log 2>/dev/null)
    notyet=$(grep -a -c "not yet ended" $log 2>/dev/null)
    echo "round $r $t | $cp | $rm_line | lost-lines=${lost:-0} not-yet-ended=${notyet:-0} | $res"
    printf '%s\n' "$out" | grep -a -E "FAILED|panicked" | head -6
  done
done
