#!/bin/sh
# PM M242: two builds' context test binaries, BEFORE and AFTER (each
# `cargo test --release -p shards-build --test context --no-run`, the measurement tests
# a_context_load_costs and a_take_costs in both), five rounds, interleaved, which goes
# first alternating.
#
#   ab.sh BEFORE AFTER
before=$1
after=$2
for r in 1 2 3 4 5; do
	if [ $((r % 2)) -eq 1 ]; then order="$before $after"; else order="$after $before"; fi
	for side in $order; do
		echo "== round $r $side $(uptime | sed 's/.*load averages: //')"
		SNAPSHOT_TAKES=40 CONTEXT_FILES=1000 "$side" --ignored --nocapture --exact a_context_load_costs 2>&1 | grep -a ' of 1000 files'
		SNAPSHOT_TAKES=5000 "$side" --ignored --nocapture --exact a_take_costs 2>&1 | grep -a 'a take of'
	done
done
