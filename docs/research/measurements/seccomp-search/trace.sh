#!/bin/sh
# What one install of a filter is made of in the kernel (review 1.22, PM M107): ftrace's
# function_graph over seccomp(2), for each kind of filter the harness installs, as root.
# Usage: trace.sh BIN [DEPTH]
set -eu
bin=$1
depth=${2:-5}
t=/sys/kernel/tracing
[ -e $t/current_tracer ] || mount -t tracefs nodev $t
grep -qw function_graph $t/available_tracers || { echo "this kernel has no function_graph tracer" >&2; exit 1; }
entry=$(grep -m1 -oE '^__(x64|arm64)_sys_seccomp' $t/available_filter_functions)
echo 0 > $t/tracing_on
echo function_graph > $t/current_tracer
echo "$depth" > $t/max_graph_depth
echo "$entry" > $t/set_graph_function
for variant in allow allow-bare linear searched search; do
  for run in 1 2 3; do
    echo > $t/trace
    echo 1 > $t/tracing_on
    "$bin" install "$variant"
    echo 0 > $t/tracing_on
    echo "== $variant, run $run"
    grep -v '^#' $t/trace | grep -E '[0-9.]+ us' | grep -vE '^\s*[0-9]+\)\s+[0-9.]+ us\s+\|\s+[a-z_0-9.]+\(\);$' | head -80
  done
done
echo nop > $t/current_tracer
echo > $t/set_graph_function
