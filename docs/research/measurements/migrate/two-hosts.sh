#!/usr/bin/env bash
# Moves microVMs between two machines (D176 parts 6 and 7, roadmap items 7a and 7b), end to
# end, and measures them. Each subcommand runs on one machine:
#
#   listen.  On the machine moved to: SHARDS=/path/to/shards [ADDR=192.168.1.71:7443]
#            two-hosts.sh listen   (a home of its own in $HOME_THERE, its daemon listening at
#            ADDR, this machine's LAN address and port 7443 unless given; an invitation for
#            the other written, and printed base64 on its last line before HOLD, seconds
#            it then waits, where given, keeping the daemon, and stops it)
#   invite.  On the machine moved to, listening: another invitation, printed base64 on the
#            last line. One enrolls one client: each run of `e2e` joins with a home of its own,
#            so each needs one.
#   move.    On the machine moved from: SHARDS=... INVITE=<listen's line> N=20 \
#            MODE="cold live" two-hosts.sh move   (a home of its own in $HOME_HERE, joined
#            as context `lab` once; for each mode, N microVMs of $IMAGE, each counting,
#            moved one at a time, each checked to count on there, each count once; with n,
#            p50, p90, p99 and max, what each move took, and the time each ran nowhere: from
#            its save asked (cold) or its last save asked (live), which stopped it here, to
#            its restore there said done, by this machine's clock. A move that fails is
#            counted, its log kept, and the series goes on; any failure exits 1 at the end.
#            Results also go to $OUT, $HOME_HERE/results.txt unless given. On macOS it runs
#            itself as a launchd job (macos-sched/launchd-run.sh), free of an agent
#            session's QoS clamp, as a user's program runs (PM M317); UNCLAMP=0 runs it in
#            place.
#   e2e.     On the machine moved from, in a checkout: INVITE=<invite's line> two-hosts.sh e2e
#            (tests/moved.rs, its two-host test moving to the listening machine saved and
#            live, SHARDS_MOVE_INVITE its invitation; FILTER picks other tests of moved.rs,
#            "" every one of them, the two-home ones too)
#   stop.    On the machine moved to: its daemon stopped, its home removed, its daemon's
#            log kept at $KEEP_LOG first where that is given.
#   local.   One machine, two homes: listen on loopback, move every mode; then the E2E tests,
#            all of moved.rs between two homes of its own (FILTER, "" unless given: all), and
#            each of its tests of another machine (`a_microvm_moves_to_another_host_saved_and_live`,
#            `a_volumes_guest_owners_move_with_it`) moving to the listening home, each with
#            an invitation of its own; stop.
#   summary. $OUT's series as Markdown: n, p50, p90, p99 and max of what each move took and
#            of the time each ran nowhere, by mode, with the machines and what failed; and
#            the E2E tests' results in $LOG, where given (.github/workflows/migrate-e2e.yml
#            writes it to its jobs' summaries).
#
# The counter writes a line every 50 ms, and for live moves rewrites 1 MiB of its memory
# each time it counts (20 MiB/s), so that the rounds have its writes to chase.
set -euo pipefail
self="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"
root="$(cd "$(dirname "$self")/../../../.." && pwd)"
HOME_THERE=${HOME_THERE:-$HOME/.cache/shards-move-there}
HOME_HERE=${HOME_HERE:-$HOME/.cache/shards-move-here}
IMAGE=${IMAGE:-alpine:3.20}
N=${N:-20}
MODE=${MODE:-cold live}
OUT=${OUT:-$HOME_HERE/results.txt}
export DOCKER_CONFIG=${DOCKER_CONFIG:-$(mktemp -d)}

there() { SHARDS_HOME=$HOME_THERE "$SHARDS" "$@"; }
now() { python3 -c 'import time; print(time.time_ns())'; }
here() { SHARDS_HOME=$HOME_HERE "$SHARDS" "$@"; }

# This machine: its OS, its CPU, and where it is.
machine() {
	local cpu
	cpu=$(sysctl -n machdep.cpu.brand_string 2>/dev/null || sed -n 's/^model name[[:space:]]*: //p' /proc/cpuinfo | head -1)
	echo "$(hostname -s 2>/dev/null || hostname), $(uname -sm), $(sw_vers -productVersion 2>/dev/null || uname -r), $cpu"
}

# An address of this machine's that another on its network reaches.
lan() {
	ipconfig getifaddr en0 2>/dev/null || ipconfig getifaddr en1 2>/dev/null ||
		hostname -I 2>/dev/null | awk '{print $1}'
}

# Quantiles of the numbers on stdin, one a line.
stats() {
	sort -n | awk -v what="$1" '{v[NR]=$1} END {
		if (NR == 0) { print what ": none"; exit }
		p50 = v[int((NR-1)*0.50+0.5)+1]; p90 = v[int((NR-1)*0.90+0.5)+1]
		p99 = v[int((NR-1)*0.99+0.5)+1]
		printf "%s: n %d p50 %d p90 %d p99 %d max %d\n", what, NR, p50, p90, p99, v[NR] }'
}

# Whether `$1`'s log (`$2` the home's flags) has `$3` lines within a minute.
lines_within() {
	local name=$1 where=$2 want=$3 deadline=$(($(date +%s) + 60))
	# shellcheck disable=SC2086 # $where is flags, split
	until [ "$(here $where logs "$name" 2>/dev/null | wc -l)" -ge "$want" ]; do
		[ "$(date +%s)" -lt "$deadline" ] || return 1
		sleep 0.2
	done
}

# A series of N moves of `$1` (cold or live), its results said and appended to $OUT, the
# moves that failed added to `failed`.
failed=0
series() {
	local mode=$1 flag="" work i name start end out status ran last counts was=$failed
	work='i=0; while :; do i=$((i+1)); echo $i; '
	if [ "$mode" = live ]; then
		flag="--live"
		work="$work"'head -c 1048576 /dev/zero | tr "\0" "x" > /dev/shm/dirty; '
	fi
	work="$work"'sleep 0.05; done'
	: > "$HOME_HERE/took-$mode.txt"
	: > "$HOME_HERE/nowhere-$mode.txt"
	for i in $(seq 1 "$N"); do
		name="mv-$mode-$i"
		# One a failed move left here, or an earlier series' there.
		here rm -f "$name" >/dev/null 2>&1 || true
		here --context lab rm -f "$name" >/dev/null 2>&1 || true
		here run -d --pull never --name "$name" "$IMAGE" sh -c "$work" >/dev/null
		if ! lines_within "$name" "" 10; then
			echo "$mode move $i: $name counted not 10 here in a minute: $(here logs "$name" 2>&1 | tail -5)"
			failed=$((failed + 1))
			here rm -f "$name" >/dev/null 2>&1 || true
			continue
		fi
		start=$(now)
		status=0
		out=$(SHARDS_MOVE_TIMES=1 here move vm $flag "$name" lab 2>&1) || status=$?
		end=$(now)
		echo "$out" > "$HOME_HERE/move-$mode-$i.log"
		if [ "$status" != 0 ]; then
			echo "$mode move $i failed ($status), log $HOME_HERE/move-$mode-$i.log: $out"
			failed=$((failed + 1))
			here rm -f "$name" >/dev/null 2>&1 || true
			here --context lab rm -f "$name" >/dev/null 2>&1 || true
			continue
		fi
		echo $(((end - start) / 1000)) >> "$HOME_HERE/took-$mode.txt"
		if [ "$mode" = live ]; then
			ran=$(echo "$out" | sed -n 's/^move: downtime \([0-9]*\) us$/\1/p')
		else
			ran=$(echo "$out" | sed -n 's/^move: resumed there at \([0-9]*\) us$/\1/p')
		fi
		if [ -n "$ran" ]; then echo "$ran" >> "$HOME_HERE/nowhere-$mode.txt"; fi
		echo "$mode move $i: ran nowhere ${ran:-?} us; $(echo "$out" | grep -E '^move: (round|its last)' | tr '\n' ';')"
		# Counting on there, each count once.
		if ! lines_within "$name" "--context lab" 40; then
			echo "$mode move $i: $name counted not to 40 there in a minute: $(here --context lab logs "$name" 2>&1 | tail -5)"
			failed=$((failed + 1))
			here --context lab rm -f "$name" >/dev/null 2>&1 || true
			continue
		fi
		counts=$(here --context lab logs "$name" | grep -E '^[0-9]+$')
		last=$(echo "$counts" | tail -1)
		if [ "$(echo "$counts" | wc -l | tr -d ' ')" != "$last" ] || [ "$(echo "$counts" | sort -n | uniq | wc -l | tr -d ' ')" != "$last" ]; then
			echo "$mode move $i: counts lost or doubled there (last $last)"
			echo "$counts" > "$HOME_HERE/counts-$mode-$i.txt"
			failed=$((failed + 1))
		fi
		here --context lab rm -f "$name" >/dev/null
	done
	{
		echo "$mode: $N moves, $((failed - was)) failed"
		stats "$mode move took (us)" < "$HOME_HERE/took-$mode.txt"
		stats "$mode ran nowhere (us)" < "$HOME_HERE/nowhere-$mode.txt"
	} | tee -a "$OUT"
}

case "${1:-}" in
listen)
	: "${SHARDS:?the shards binary to run}"
	ADDR=${ADDR:-$(lan):7443}
	mkdir -p "$HOME_THERE"
	there run daemon --detached --listen "$ADDR"
	there invite client --name mover --output "$HOME_THERE/mover.invite"
	echo "listening at $ADDR: $(machine), shards $(there version 2>/dev/null | head -1)"
	base64 < "$HOME_THERE/mover.invite" | tr -d '\n'
	echo
	# Held up HOLD seconds, where given: a runner that ends what a command leaves running
	# (kiln's) keeps the daemon while this waits, run in the background.
	if [ -n "${HOLD:-}" ]; then
		sleep "$HOLD"
		there stop daemon || true
	fi
	;;
invite)
	: "${SHARDS:?the shards binary to run}"
	# Unique, as two asked within a second are.
	name="e2e$(date +%s)$RANDOM"
	there invite client --name "$name" --output "$HOME_THERE/$name.invite"
	base64 < "$HOME_THERE/$name.invite" | tr -d '\n'
	echo
	;;
stop)
	: "${SHARDS:?the shards binary to run}"
	there stop daemon || true
	if [ -n "${KEEP_LOG:-}" ]; then cp "$HOME_THERE/daemon.log" "$KEEP_LOG" 2>/dev/null || true; fi
	rm -rf "$HOME_THERE"
	;;
move)
	: "${SHARDS:?the shards binary to run}"
	: "${INVITE:?the invitation the listening machine printed}"
	if [ "$(uname -s)" = Darwin ] && [ -z "${TWO_HOSTS_UNCLAMPED:-}" ] && [ "${UNCLAMP:-1}" = 1 ]; then
		job=$(mktemp -d)/move
		pass=(TWO_HOSTS_UNCLAMPED=1)
		for v in SHARDS INVITE N MODE IMAGE HOME_HERE HOME_THERE OUT REVISION DOCKER_CONFIG; do
			if [ -n "${!v+x}" ]; then pass+=("$v=${!v}"); fi
		done
		status=0
		"$(dirname "$self")/../macos-sched/launchd-run.sh" "$job" env "${pass[@]}" "$self" move || status=$?
		cat "$job.log"
		exit "$status"
	fi
	mkdir -p "$HOME_HERE"
	echo "$INVITE" | base64 -d > "$HOME_HERE/lab.invite"
	# Joined once: an invitation enrolls one client, which stays enrolled.
	here inspect context lab >/dev/null 2>&1 || here create context lab --invite-file "$HOME_HERE/lab.invite"
	here pull -q "$IMAGE" >/dev/null
	{
		echo "== $(date -u +%FT%TZ): moving from $(machine), revision ${REVISION:-$(git -C "$root" rev-parse --short HEAD 2>/dev/null || echo unknown)}, image $IMAGE, n $N"
		echo "   to: $(here --context lab info --format '{{.Name}} {{.OperatingSystem}} {{.Architecture}}' 2>/dev/null || echo unknown)"
	} | tee -a "$OUT"
	for mode in $MODE; do
		series "$mode"
	done
	[ "$failed" = 0 ] || { echo "$failed moves failed" >&2; exit 1; }
	;;
e2e)
	: "${INVITE:?an invitation the listening machine printed (two-hosts.sh invite)}"
	invite=$(mktemp)
	echo "$INVITE" | base64 -d > "$invite"
	cd "$root"
	SHARDS_MOVE_INVITE=$invite cargo test -p shards --profile test-release --test moved -- --nocapture \
		${FILTER-a_microvm_moves_to_another_host_saved_and_live}
	;;
local)
	: "${SHARDS:?the shards binary to run}"
	export ADDR=${ADDR:-127.0.0.1:7443}
	trap '"$self" stop' EXIT
	INVITE=$(HOLD= "$self" listen | tail -1)
	export INVITE
	status=0
	"$self" move || status=$?
	# Between two homes of its own, no other machine named: an invitation enrolls one client,
	# and each test of another machine joins with a home of its own.
	(cd "$root" && cargo test -p shards --profile test-release --test moved -- --nocapture ${FILTER-}) || status=$?
	for t in a_microvm_moves_to_another_host_saved_and_live a_volumes_guest_owners_move_with_it; do
		INVITE=$("$self" invite | tail -1) FILTER=$t "$self" e2e || status=$?
	done
	exit "$status"
	;;
summary)
	if [ ! -f "$OUT" ]; then
		echo "No results: $OUT was not written."
		exit 0
	fi
	grep -E '^(== |   to: )' "$OUT" | sed 's/^ *//; s/^== /- from /; s/^to: /- to: /' || true
	echo
	echo "| mode | µs | n | p50 | p90 | p99 | max |"
	echo "|---|---|---|---|---|---|---|"
	awk '/^(cold|live) (move took|ran nowhere) \(us\): / {
		mode = $1; what = ($2 == "move") ? "took" : "ran nowhere"
		if ($0 ~ /: none$/) { printf "| %s | %s | 0 | | | | |\n", mode, what; next }
		sub(/^.*\(us\): /, "")
		printf "| %s | %s | %s | %s | %s | %s | %s |\n", mode, what, $2, $4, $6, $8, $10
	}' "$OUT"
	echo
	grep -E '^(cold|live): [0-9]+ moves' "$OUT" | sed 's/^/- /' || true
	if [ -n "${LOG:-}" ] && [ -f "$LOG" ]; then
		echo
		echo "tests/moved.rs:"
		echo
		grep -aE '^test [A-Za-z0-9_:]+ \.\.\. ' "$LOG" | sed 's/^/- /' || true
		grep -aE '^test result: ' "$LOG" | sed 's/^/- /' || true
		grep -aE '^(cold|live) move [0-9]+[: ]' "$LOG" | grep -avE 'ran nowhere [0-9]+ us;' | sed 's/^/- /' || true
	fi
	;;
*)
	echo "usage: two-hosts.sh listen|invite|move|e2e|stop|local|summary" >&2
	exit 2
	;;
esac
