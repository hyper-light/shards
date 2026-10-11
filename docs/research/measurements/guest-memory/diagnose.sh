#!/bin/sh
# Where a run's guest keeps its microVM's memory, for two builds of shards on one host:
# CI's x86_64 runs found `-m` short of its limit from 504cd087 on
# (run_limits_resources_as_docker_run_does), where M155, at 41839c7, measured a margin.
#   diagnose.sh SHARDS_A SHARDS_B [LIMIT...]
# For each build and each limit (700m and 2500m, the test's, by default), a run's guest
# says its /proc/meminfo, what its kernel said of memory as it booted, its zones' pages
# and reserves, its physical memory map and its command line. The host first.
set -u
a=${1:?usage: diagnose.sh SHARDS_A SHARDS_B [LIMIT...]}
b=${2:?usage: diagnose.sh SHARDS_A SHARDS_B [LIMIT...]}
shift 2
[ $# -gt 0 ] || set -- 700m 2500m
echo "== host"
uname -sr
nproc
free -m
lscpu | grep -E 'Model name|Hypervisor vendor|Flags' | cut -c1-400
guest='cat /proc/meminfo; echo "-- boot"; dmesg | grep -iE "Memory:|reserv|initrd|e820|BIOS-|Freeing|swiotlb|crashkernel|percpu|pmem|DMA"; echo "-- zones"; grep -E "^Node|managed|present|spanned|protection|^ +(min|low|high) " /proc/zoneinfo; echo "-- iomem"; cat /proc/iomem; echo "-- cmdline"; cat /proc/cmdline'
homes=$(mktemp -d)
trap 'for h in a b; do SHARDS_HOME=$homes/$h "$a" stop daemon >/dev/null 2>&1; SHARDS_HOME=$homes/$h "$b" stop daemon >/dev/null 2>&1; done; rm -r "$homes"' EXIT
for build in a b; do
	shards=$a
	[ "$build" = b ] && shards=$b
	mkdir "$homes/$build"
	for limit in "$@"; do
		echo "== $build ($shards) -m $limit"
		# Privileged, for its kernel's log; where a build has no --privileged, without.
		SHARDS_HOME=$homes/$build "$shards" run --rm --privileged -m "$limit" alpine:3.22 sh -c "$guest" ||
			SHARDS_HOME=$homes/$build "$shards" run --rm -m "$limit" alpine:3.22 sh -c "$guest"
		echo "exit $?"
	done
done
