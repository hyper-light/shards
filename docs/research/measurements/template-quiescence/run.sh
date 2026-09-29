#!/bin/sh
# M21 (docs/research/platform-measurements.md): templates saved by two shards-init builds,
# alternating which goes first. Each round saves one template per init and restores it
# twice with SHARDS_TIMING. Prints each restore's timing line, then per init: the guest's
# time from release to the VM stopping, and how many restores had a phase over 5 ms.
#
#   run.sh ROUNDS SHARDS KERNEL IMAGE A=INIT_A B=INIT_B
#
# SHARDS_HOME must hold IMAGE (`shards pull`); this replaces its recorded guest and its
# templates. IMAGE's own command must exit on its own, as the E2E test image's does.
set -eu

rounds=$1 shards=$2 kernel=$3 image=$4
a_name=${5%%=*} a_init=${5#*=}
b_name=${6%%=*} b_init=${6#*=}
out=$(mktemp)
trap 'rm -f "$out"' EXIT
r=1
while [ $r -le "$rounds" ]; do
	if [ $((r % 2)) -eq 1 ]; then order="$a_name=$a_init $b_name=$b_init"; else order="$b_name=$b_init $a_name=$a_init"; fi
	for pick in $order; do
		name=${pick%%=*} init=${pick#*=}
		"$shards" guest use --kernel "$kernel" --init "$init" >/dev/null
		rm -rf "$SHARDS_HOME/templates"
		"$shards" run --pull never "$image" </dev/null >/dev/null 2>&1
		for _ in 1 2; do
			line=$(SHARDS_TIMING=1 "$shards" run --pull never "$image" </dev/null 2>&1 >/dev/null |
				grep '^shards-timing ')
			echo "$name $line" | tee -a "$out"
		done
	done
	r=$((r + 1))
done
# Per init: exit_us - released_us, and the longest step between release, the markers and
# the stop. Then nearest-rank p50, p90 and the max of the first.
for name in "$a_name" "$b_name"; do
	grep "^$name " "$out" | awk '
		{
			line = $0
			match(line, /"released_us":[0-9]+/); rel = substr(line, RSTART + 14, RLENGTH - 14)
			match(line, /"exit_us":[0-9]+/); ex = substr(line, RSTART + 10, RLENGTH - 10)
			prev = rel; worst = 0; rest = line
			while (match(rest, /\[[0-9]+,[0-9]+\]/)) {
				pair = substr(rest, RSTART + 1, RLENGTH - 2); rest = substr(rest, RSTART + RLENGTH)
				split(pair, p, ","); if (p[2] - prev > worst) worst = p[2] - prev; prev = p[2]
			}
			if (ex - prev > worst) worst = ex - prev
			print ex - rel, worst
		}' | sort -n | awk -v name="$name" '
		{ v[NR] = $1; if ($2 > 5000) stalled++ }
		END {
			printf "%s: n=%d release->stopped p50 %d p90 %d max %d us; restores with a step > 5 ms: %d\n",
				name, NR, v[int((NR * 50 + 99) / 100)], v[int((NR * 90 + 99) / 100)], v[NR], stalled + 0
		}'
done
