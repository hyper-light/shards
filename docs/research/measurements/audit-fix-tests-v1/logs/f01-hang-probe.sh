#!/bin/sh
# Why does observer_sockets_are_checked_by_identity_beside_other_tests_descriptors stall when
# it runs alone? Runs it, then after 10 s reads each thread's state and stack, and kills it.
. docs/research/measurements/audit-fix-tests-v1/logs/linux-build.env
cargo test -p shards -j2 --test managed_output --no-run >/dev/null 2>&1
bin=$(ls -t target/debug/deps/managed_output-* | grep -v '\.d$' | head -1)
echo "binary $bin"
"$bin" --exact observer_sockets_are_checked_by_identity_beside_other_tests_descriptors --nocapture &
pid=$!
sleep 10
echo "== threads of $pid"
for t in /proc/$pid/task/*; do
	echo "$(basename $t) $(cat $t/comm) wchan=$(cat $t/wchan) state=$(awk '{print $3}' $t/stat)"
done
echo "== sockets"
ls -l /proc/$pid/fd | grep socket
echo "== gdb"
command -v gdb && gdb -batch -p "$pid" -ex "thread apply all bt 25" 2>&1 | grep -v '^\[New LWP\|^warning:' | head -200
kill -9 "$pid"
wait "$pid" 2>/dev/null
echo "[killed]"
