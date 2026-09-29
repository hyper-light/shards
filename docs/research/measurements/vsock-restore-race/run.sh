#!/bin/sh
# M20 (docs/research/platform-measurements.md): runs COMMAND N times with every CPU busy,
# and prints each failure's last lines, then the count. Busy CPUs delay the VMM's
# threads, which is what exposed a restored guest's first connection to its restore's
# own reset. COMMAND is a restored run, for example:
#
#   run.sh 400 shards vm restore TEMPLATE -- /bin/testguest exit 0
#   run.sh 400 shards run --pull never IMAGE true
#
# TEMPLATE: `shards vm run --kernel K --init shards-init --rootfs IMAGE --snapshot-dir
# TEMPLATE`. IMAGE for `shards run`: pulled, with the guest recorded (`shards guest use`),
# so each run after the first restores its template.
set -u

n=$1
shift
cpus=$(getconf _NPROCESSORS_ONLN)
load=""
trap 'kill $load 2>/dev/null' EXIT INT TERM
i=0
while [ $i -lt "$cpus" ]; do
	yes >/dev/null &
	load="$load $!"
	i=$((i + 1))
done
failed=0
i=1
while [ $i -le "$n" ]; do
	if ! out=$("$@" 2>&1 </dev/null); then
		failed=$((failed + 1))
		echo "=== run $i failed"
		echo "$out" | tail -5
	fi
	i=$((i + 1))
done
echo "failed: $failed of $n, $cpus CPUs busy"
