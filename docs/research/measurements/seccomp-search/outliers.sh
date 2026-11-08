#!/bin/sh
# What a seccomp(2) that takes milliseconds waits on (review 1.23, PM M107): ftrace's
# function_graph over seccomp(2) while the harness runs, keeping only the calls, and the
# calls under them, that took longer than THRESH µs (tracing_thresh). As root.
# Usage: outliers.sh BIN [ROUNDS [THRESH]]
set -eu
bin=$1
rounds=${2:-200}
thresh=${3:-5000}
t=/sys/kernel/tracing
[ -e $t/current_tracer ] || mount -t tracefs nodev $t
grep -qw function_graph $t/available_tracers || { echo "this kernel has no function_graph tracer" >&2; exit 1; }
entry=$(grep -m1 -oE '^__(x64|arm64)_sys_seccomp' $t/available_filter_functions)
echo 0 > $t/tracing_on
echo function_graph > $t/current_tracer
echo 0 > $t/max_graph_depth
echo "$entry" > $t/set_graph_function
echo "$thresh" > $t/tracing_thresh
echo 16384 > $t/buffer_size_kb
echo > $t/trace
echo 1 > $t/tracing_on
"$bin" "$rounds" outliers | grep -E '^(install|slowest)'
echo 0 > $t/tracing_on
echo "== seccomp(2) and what under it took more than $thresh µs"
grep -v '^#' $t/trace | head -400
echo 0 > $t/tracing_thresh
echo nop > $t/current_tracer
echo > $t/set_graph_function
