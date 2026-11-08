#!/bin/sh
# PM M176: how much of a shell's loop of short-lived processes a run's record of processes
# holds (D123), and what an exec takes, on two guest kernels, runs interleaved:
#   measure.sh SHARDS INIT KERNEL_A KERNEL_B [ROUNDS]
# Each round runs, in alpine:3.22 on each kernel (SHARDS_KERNEL, with shards-init INIT,
# a home of its own),
#   sh -c 'for i in $(seq 2000); do /bin/true; done'
# timed in the guest by /proc/uptime around the loop (busybox's date has no %N; 10 ms in
# 2000 execs), then reads the run's record (`shards read processes`): the forks of the
# shell's processes, the execs of /bin/true said whole, those said without a command line,
# the exits, and the records of events lost. One line a run:
#   KERNEL ROUND NS_PER_EXEC FORKS TRUE_EXECS EMPTY_EXECS EXITS LOST
# The order of the two kernels turns each round.
set -eu
usage='usage: measure.sh SHARDS INIT KERNEL_A KERNEL_B [ROUNDS]'
shards=${1:?$usage}
init=${2:?$usage}
kernel_a=${3:?$usage}
kernel_b=${4:?$usage}
rounds=${5:-20}
homes=$(mktemp -d)
mkdir "$homes/a" "$homes/b"
trap 'for h in a b; do SHARDS_HOME=$homes/$h "$shards" stop daemon >/dev/null 2>&1 || :; done; rm -r "$homes"' EXIT
loop='s=$(awk "{printf \"%d\", \$1 * 100}" /proc/uptime); for i in $(seq 2000); do /bin/true; done; e=$(awk "{printf \"%d\", \$1 * 100}" /proc/uptime); echo $(( (e - s) * 10000000 / 2000 ))'
one() {
	k=$1 r=$2
	kernel=$kernel_a
	[ "$k" = b ] && kernel=$kernel_b
	ns=$(SHARDS_HOME=$homes/$k SHARDS_KERNEL=$kernel SHARDS_INIT=$init "$shards" run --name "r$r" alpine:3.22 sh -c "$loop")
	record=$(SHARDS_HOME=$homes/$k "$shards" read processes "r$r")
	forks=$(printf '%s\n' "$record" | grep -c ' fork ' || :)
	whole=$(printf '%s\n' "$record" | grep -cE ' exec +/bin/true$' || :)
	empty=$(printf '%s\n' "$record" | grep -cE ' exec +(\[.*\]|\(ended .*\))?$' || :)
	exits=$(printf '%s\n' "$record" | grep -c ' exit ' || :)
	lost=$(printf '%s\n' "$record" | grep -c ' lost ' || :)
	echo "$k $r $ns $forks $whole $empty $exits $lost"
}
# The image pulled, and each kernel's template made, before any is measured.
for k in a b; do one "$k" 0 >/dev/null; done
for r in $(seq "$rounds"); do
	if [ $((r % 2)) = 1 ]; then
		one a "$r"
		one b "$r"
	else
		one b "$r"
		one a "$r"
	fi
done
