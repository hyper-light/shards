# The checkpoint test's A/B with an exec into the restored microVM, engine.rs whole held to
# MASK, ROUNDS rounds, pre and post in turn (alternating order).
rounds=$1; mask=$2
for r in $(seq "$rounds"); do
  if [ $((r % 2)) = 1 ]; then order="pre post"; else order="post pre"; fi
  for t in $order; do
    log=~/ab2-$t-$r.keep; rm -f $log
    out=$(SHARDS_TEST_KEEP_LOG=$(cygpath -w $log) bash /tmp/wcg-run2.sh ~/wcg-$t ~/wcg-target-$t "$mask" engine 2>&1)
    cp=$(printf '%s\n' "$out" | grep -a -E "^test the_docker_cli_checkpoints_and_restores_microvms \.\.\." | tail -1 | sed 's/.*\.\.\. //')
    exec_line=$(grep -a "^=== exec" $log 2>/dev/null | head -1 | cut -c1-160)
    rm_line=$(grep -a "^=== rm -f" $log 2>/dev/null | head -1)
    lost=$(grep -a -c "signal connection is lost" $log 2>/dev/null)
    echo "round $r $t | cp test $cp | $exec_line | $rm_line | lost-lines=${lost:-0}"
  done
done
