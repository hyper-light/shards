#!/usr/bin/env bash
# Moves microVMs between two machines (D176 parts 6 and 7), end to end, and measures them.
#
#   listen.  On the machine moved to: SHARDS=/path/to/shards ADDR=192.168.1.71:7443 \
#            two-hosts.sh listen   (a home of its own in $HOME_THERE, its daemon listening at
#            ADDR, an invitation for the other written and printed base64 on its last line)
#   move.    On the machine moved from: SHARDS=... INVITE=<that line> N=20 MODE=cold|live \
#            two-hosts.sh move     (a home of its own in $HOME_HERE, joined as context `lab`;
#            N microVMs of $IMAGE, each counting, moved one at a time; each checked to count
#            on there, each count once; what each move took, and for live moves the time each
#            ran nowhere, with n, p50, p90, p99 and max)
#   stop.    On the machine moved to: its daemon stopped, its home removed.
#
# The counter writes a line every 50 ms, and for live moves rewrites 1 MiB of its memory
# each time it counts (20 MiB/s), so that the rounds have its writes to chase.
set -euo pipefail
: "${SHARDS:?the shards binary to run}"
HOME_THERE=${HOME_THERE:-$HOME/.cache/shards-move-there}
HOME_HERE=${HOME_HERE:-$HOME/.cache/shards-move-here}
IMAGE=${IMAGE:-alpine:3.20}
N=${N:-20}
MODE=${MODE:-cold}
export DOCKER_CONFIG=${DOCKER_CONFIG:-$(mktemp -d)}

there() { SHARDS_HOME=$HOME_THERE "$SHARDS" "$@"; }
now() { python3 -c 'import time; print(time.time_ns())'; }
here() { SHARDS_HOME=$HOME_HERE "$SHARDS" "$@"; }

# Quantiles of the numbers on stdin, one a line.
stats() {
	sort -n | awk -v what="$1" '{v[NR]=$1} END {
		if (NR == 0) { print what ": none"; exit }
		p50 = v[int((NR-1)*0.50+0.5)+1]; p90 = v[int((NR-1)*0.90+0.5)+1]
		p99 = v[int((NR-1)*0.99+0.5)+1]
		printf "%s: n %d p50 %d p90 %d p99 %d max %d\n", what, NR, p50, p90, p99, v[NR] }'
}

case "${1:-}" in
listen)
	: "${ADDR:?the address to listen at, one the other machine reaches}"
	mkdir -p "$HOME_THERE"
	there run daemon --detached --listen "$ADDR"
	there invite client --name mover --output "$HOME_THERE/mover.invite"
	echo "listening at $ADDR: $(uname -sm), $(sw_vers -productVersion 2>/dev/null || uname -r), shards $(there version 2>/dev/null | head -1)"
	base64 < "$HOME_THERE/mover.invite" | tr -d '\n'
	echo
	;;
stop)
	there stop daemon || true
	rm -rf "$HOME_THERE"
	;;
move)
	: "${INVITE:?the invitation the listening machine printed}"
	mkdir -p "$HOME_HERE"
	echo "$INVITE" | base64 -d > "$HOME_HERE/lab.invite"
	# Joined once: an invitation enrolls one client, which stays enrolled.
	here inspect context lab >/dev/null 2>&1 || here create context lab --invite-file "$HOME_HERE/lab.invite"
	here pull -q "$IMAGE" >/dev/null
	echo "moving from: $(uname -sm), $(sw_vers -productVersion 2>/dev/null || uname -r), revision ${REVISION:-unknown}, mode $MODE, image $IMAGE"
	work='i=0; while :; do i=$((i+1)); echo $i; '
	[ "$MODE" = live ] && work="$work"'head -c 1048576 /dev/zero | tr "\0" "x" > /dev/shm/dirty; '
	work="$work"'sleep 0.05; done'
	flag=""
	[ "$MODE" = live ] && flag="--live"
	: > "$HOME_HERE/took.txt"
	: > "$HOME_HERE/downtime.txt"
	for i in $(seq 1 "$N"); do
		name="mv$i"
		here run -d --pull never --name "$name" "$IMAGE" sh -c "$work" >/dev/null
		until [ "$(here logs "$name" 2>/dev/null | wc -l)" -ge 10 ]; do sleep 0.1; done
		start=$(now)
		out=$(SHARDS_MOVE_TIMES=1 here move vm $flag "$name" lab 2>&1) || { echo "move $i failed: $out"; exit 1; }
		end=$(now)
		echo $(( (end - start) / 1000 )) >> "$HOME_HERE/took.txt"
		echo "$out" | sed -n 's/^move: downtime \([0-9]*\) us$/\1/p' >> "$HOME_HERE/downtime.txt"
		echo "move $i: $(echo "$out" | grep -E '^move: (downtime|round)' | tr '\n' ';')"
		# Counting on there, each count once.
		until [ "$(here --context lab logs "$name" 2>/dev/null | wc -l)" -ge 40 ]; do sleep 0.2; done
		counts=$(here --context lab logs "$name" | grep -E '^[0-9]+$')
		last=$(echo "$counts" | tail -1)
		if [ "$(echo "$counts" | wc -l | tr -d ' ')" != "$last" ] || [ "$(echo "$counts" | sort -n | uniq | wc -l | tr -d ' ')" != "$last" ]; then
			echo "move $i: counts lost or doubled there (last $last)"; exit 1
		fi
		here --context lab rm -f "$name" >/dev/null
	done
	stats "move took (us)" < "$HOME_HERE/took.txt"
	[ "$MODE" = live ] && stats "downtime (us)" < "$HOME_HERE/downtime.txt"
	;;
*)
	echo "usage: two-hosts.sh listen|move|stop" >&2
	exit 2
	;;
esac
