#!/bin/bash
# PM M532: what the guest kernel has at boot for each vCPU count and memory size, for the
# storm's memory (crates/shards/tests/snapshot.rs `storm_mib`). The test guest's `limits`
# mode, booted without `quiet`, ROUNDS times for each CPUS:MIB given; one line a boot: the
# kernel's `Memory:` line (available and reserved at boot), its thread limit
# (kernel.threads-max), the tasks there as init starts (the kernel's own threads and init),
# MemTotal, and how the boot ended.
#
#   probe.sh ROUNDS CPUS:MIB...
#
# KERNEL, GUEST and SHARDS name the pinned guest kernel, the test guest and the `shards`
# binary (on Windows, Windows paths; the E2E tests put the kernel in target/artifacts and
# build the guest into target/guest).
set -u
rounds=$1
shift
for pair in "$@"; do
	cpus=${pair%%:*}
	mib=${pair##*:}
	for r in $(seq "$rounds"); do
		out=$(timeout 300 "$SHARDS" run --kernel "$KERNEL" --init "$GUEST" --cpus "$cpus" --memory "$mib" \
			--cmdline "console=ttyS0 panic=-1 shards_test=limits" 2>&1)
		field() { printf '%s\n' "$out" | grep -a "SHARDS-TEST LIMIT $1 " | awk '{print $4}'; }
		avail=$(printf '%s\n' "$out" | grep -a -o "Memory: [0-9]*K/[0-9]*K available" | head -1)
		reserved=$(printf '%s\n' "$out" | grep -a -o "[0-9]*K reserved" | head -1)
		ended=$(printf '%s\n' "$out" | grep -a -o -E "SHARDS-TEST PASS|Kernel panic - not syncing: [A-Za-z_ ]*" | sort -u | tr '\n' ';')
		echo "cpus=$cpus mib=$mib round=$r | $avail | $reserved | threads-max=$(field kernel.threads-max) | tasks=$(field tasks) | MemTotal=$(field MemTotal) | $ended"
	done
done
