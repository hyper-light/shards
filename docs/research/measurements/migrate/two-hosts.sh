#!/usr/bin/env bash
# Moves microVMs between two machines (D176 parts 6 and 7, roadmap items 7a and 7b), end to
# end, and measures them. Each subcommand runs on one machine:
#
#   listen.  On the machine moved to: SHARDS=/path/to/shards [ADDR=192.168.1.71:7443]
#            two-hosts.sh listen   (a home of its own in $HOME_THERE, its daemon listening at
#            ADDR, this machine's address on its default route and port 7443 unless given;
#            an invitation for the other written, and printed base64 on its last line; then,
#            where HOLD is given, it holds the daemon up to HOLD seconds, or until `stop`
#            removes its home, and stops it; AGENT=1 on macOS runs the daemon as a launchd
#            user agent instead, which a runner's restart does not end, and `stop` removes)
#   invite.  On the machine moved to, listening: another invitation, printed base64 on the
#            last line. One enrolls one client: each run of `e2e` joins with a home of its own,
#            so each needs one.
#   move.    On the machine moved from: SHARDS=... INVITE=<listen's line> N=20 \
#            MODE="cold live" two-hosts.sh move   (a home of its own in $HOME_HERE, joined
#            as context `lab` once; for each mode, N microVMs of $IMAGE, each counting,
#            moved one at a time, each checked to count on there, each count once; with n,
#            p50, p90, p99 and max, what each move took, and the time each ran nowhere: from
#            its save asked (cold) or its last save asked (live), which stopped it here, to
#            its restore there said done, by this machine's clock; and of each move's stream,
#            the bytes it sent (packed, where the other machine unpacks it), the rate its
#            connection's bytes were acknowledged at over the import, and the longest its
#            peer went unheard (`move: stream ...; link ...` in its log), a failed move's
#            too; a live move's rounds, their bytes and why they ended (`move: rounds`). A
#            move that fails is counted, its log kept, and the series goes on; any failure
#            exits 1 at the end. FIRST resumes a series cut short at that move (its runner
#            gone, say), the moves before it counted with the rest.
#            Results also go to $OUT, $HOME_HERE/results.txt unless given, headed by both
#            machines: this one's, and the other's as THERE says it (listen's `listening at`
#            line), where given. BASELINE=1 tells its microVMs the CPU the two machines
#            share (`shards configure cpu --baseline lab`, D176 part 6). On macOS it runs itself as a launchd job
#            (macos-sched/launchd-run.sh), free of an agent session's QoS clamp, as a user's
#            program runs (PM M317); UNCLAMP=0 runs it in place. On macOS each machine's
#            line says the priority its daemon runs at, which its microVMs' threads start
#            from: 31 a terminal's program's, 20 under the utility clamp, 4 in the
#            background band.
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
# Between two machines on one network: A, moved to, and B, moved from, of one architecture
# (a move to another is refused, D176 part 6), B's CPU having no feature A's lacks (a move
# to one lacking any is refused, naming each: an M5's microVM to an M4, PM M460), each with
# a checkout of one revision. The move goes over B's context of A (D180): QUIC, UDP to A's
# port, TLS 1.3 with each side's Ed25519 key pinned by the other and the invitation's
# secret proved bound to the session, A's daemon serving only the clients its invitations
# enrolled. In order, each in its checkout:
#
#   1. A and B: cargo build --locked -p shards --release; B also
#      cargo test -p shards --profile test-release --test moved --no-run. Before any
#      invitation, as one expires an hour after it is made.
#   2. A, kept running for as long as the rest takes (in the background: a runner ends what
#      a command leaves running): SHARDS=$PWD/target/release/shards HOLD=10800 \
#      two-hosts.sh listen. Its last line is the invitation for `move`; its
#      `listening at` line is THERE.
#   3. A, twice, one for each of B's tests of another machine:
#      SHARDS=$PWD/target/release/shards two-hosts.sh invite   (its last line)
#   4. B: SHARDS=$PWD/target/release/shards INVITE=<2's last line> THERE=<2's listening
#      line> N=30 two-hosts.sh move; then OUT=$HOME/.cache/shards-move-here/results.txt
#      two-hosts.sh summary, its tables.
#   5. B: INVITE=<3's first> two-hosts.sh e2e; then INVITE=<3's second>
#      FILTER=a_volumes_guest_owners_move_with_it two-hosts.sh e2e
#   6. A: SHARDS=$PWD/target/release/shards KEEP_LOG=<a file> two-hosts.sh stop, which ends
#      2's hold.
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
FIRST=${FIRST:-1}
AGENT_LABEL=dev.shards.two-hosts
AGENT_PLIST=$HOME/Library/LaunchAgents/$AGENT_LABEL.plist
MODE=${MODE:-cold live}
OUT=${OUT:-$HOME_HERE/results.txt}
export DOCKER_CONFIG=${DOCKER_CONFIG:-$(mktemp -d)}

there() { SHARDS_HOME=$HOME_THERE "$SHARDS" "$@"; }
now() { python3 -c 'import time; print(time.time_ns())'; }
here() { SHARDS_HOME=$HOME_HERE "$SHARDS" "$@"; }

# This machine: its OS, its CPU, and where it is; on macOS the priority its daemon in home
# `$1` runs at (an agent's session clamps a program it starts, PM M317, and a runner's
# commands may run in the background band).
machine() {
	local cpu pri=""
	cpu=$(sysctl -n machdep.cpu.brand_string 2>/dev/null || sed -n 's/^model name[[:space:]]*: //p' /proc/cpuinfo | head -1)
	if [ "$(uname -s)" = Darwin ] && [ -f "$1/daemon.pid" ]; then
		pri=", daemon at priority $(ps -o pri= -p "$(cat "$1/daemon.pid")" | tr -d ' ')"
	fi
	echo "$(hostname -s 2>/dev/null || hostname), $(uname -sm), $(sw_vers -productVersion 2>/dev/null || uname -r), $cpu$pri"
}

# An address of this machine's that another on its network reaches: its default route's (a
# Mac Studio's is en1, a Linux host's first address may be a bridge's or a tailnet's).
lan() {
	local ifc
	ifc=$(route -n get default 2>/dev/null | sed -n 's/^ *interface: //p') || true
	if [ -n "$ifc" ] && ipconfig getifaddr "$ifc" 2>/dev/null; then
		return
	fi
	ip -4 route get 1.1.1.1 2>/dev/null | sed -n 's/.* src \([0-9.]*\).*/\1/p' | grep . ||
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
	local mode=$1 flag="" work i name start end out status ran last counts was=$failed link
	work='i=0; while :; do i=$((i+1)); echo $i; '
	if [ "$mode" = live ]; then
		flag="--live"
		work="$work"'head -c 1048576 /dev/zero | tr "\0" "x" > /dev/shm/dirty; '
	fi
	work="$work"'sleep 0.05; done'
	# A series resumed (FIRST past 1) goes on with the files of the moves before it.
	if [ "$FIRST" = 1 ]; then
		for f in took nowhere sent rate silence rounds failed; do : > "$HOME_HERE/$f-$mode.txt"; done
	fi
	for i in $(seq "$FIRST" "$((FIRST + N - 1))"); do
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
		link=$(record "$mode" "$out")
		if [ "$status" != 0 ]; then
			echo "$mode move $i failed ($status), $link, log $HOME_HERE/move-$mode-$i.log: $out"
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
		echo "$mode move $i: ran nowhere ${ran:-?} us; $link; $(echo "$out" | grep -E '^move: (round|its last)' | tr '\n' ';')"
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
		echo "$((failed - was))" >> "$HOME_HERE/failed-$mode.txt"
		echo "$mode: $((FIRST + N - 1)) moves, $(awk '{n += $1} END {print n + 0}' "$HOME_HERE/failed-$mode.txt") failed"
		stats "$mode move took (us)" < "$HOME_HERE/took-$mode.txt"
		stats "$mode ran nowhere (us)" < "$HOME_HERE/nowhere-$mode.txt"
		stats "$mode sent (bytes)" < "$HOME_HERE/sent-$mode.txt"
		stats "$mode link (B/s)" < "$HOME_HERE/rate-$mode.txt"
		stats "$mode longest silence (us)" < "$HOME_HERE/silence-$mode.txt"
		if [ "$mode" = live ]; then stats "$mode rounds (n)" < "$HOME_HERE/rounds-$mode.txt"; fi
	} | tee -a "$OUT"
}

# What move `$2`'s log says of its stream and its link, each number appended to its mode's
# (`$1`) file, and said in a line: the bytes it sent (its stream's, where it went as it is:
# the bytes its link acknowledged), the rate they were acknowledged at, the longest its
# peer went unheard, and a live move's rounds and why they ended.
record() {
	local mode=$1 out=$2 line raw sent rate silence rounds
	line=$(echo "$out" | grep -E '^move: (stream|link) ' | tail -1) || true
	raw=$(echo "$line" | sed -n 's/^move: stream \([0-9]*\) bytes, .*/\1/p')
	sent=$(echo "$line" | sed -n 's/^move: stream [0-9]* bytes, sent \([0-9]*\) bytes.*/\1/p')
	if [ -z "$sent" ]; then sent=$(echo "$line" | sed -n 's/.*; link \([0-9]*\) bytes acknowledged.*/\1/p'); fi
	rate=$(echo "$line" | sed -n 's/.* us, \([0-9]*\) B\/s, .*/\1/p')
	silence=$(echo "$line" | sed -n 's/.*longest silence \([0-9]*\) us$/\1/p')
	rounds=$(echo "$out" | sed -n 's/^move: rounds \([0-9]*\), .*/\1/p' | tail -1)
	if [ -n "$sent" ]; then echo "$sent" >> "$HOME_HERE/sent-$mode.txt"; fi
	if [ -n "$rate" ]; then echo "$rate" >> "$HOME_HERE/rate-$mode.txt"; fi
	if [ -n "$silence" ]; then echo "$silence" >> "$HOME_HERE/silence-$mode.txt"; fi
	if [ -n "$rounds" ]; then echo "$rounds" >> "$HOME_HERE/rounds-$mode.txt"; fi
	echo "stream ${raw:-?} B, sent ${sent:-?} B at ${rate:-?} B/s, longest silence ${silence:-?} us$(echo "$out" | sed -n 's/^move: rounds \(.*\)$/, rounds \1/p' | tail -1)"
}

# macOS: the listening daemon as a launchd user agent, in foreground under launchd, its log
# the home's: no child of the runner that asked for it, which ends what its runs started as
# it restarts. `stop` removes it.
agent_start() {
	mkdir -p "$HOME/Library/LaunchAgents"
	rm -f "$HOME_THERE/listening"
	cat > "$AGENT_PLIST" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>$AGENT_LABEL</string>
<key>ProgramArguments</key><array>
<string>$SHARDS</string><string>run</string><string>daemon</string><string>--listen</string><string>$ADDR</string>
</array>
<key>EnvironmentVariables</key><dict>
<key>SHARDS_HOME</key><string>$HOME_THERE</string>
<key>DOCKER_CONFIG</key><string>$DOCKER_CONFIG</string>
</dict>
<key>RunAtLoad</key><true/>
<key>KeepAlive</key><false/>
<key>ProcessType</key><string>Interactive</string>
<key>StandardErrorPath</key><string>$HOME_THERE/daemon.log</string>
<key>StandardOutPath</key><string>$HOME_THERE/daemon.log</string>
</dict></plist>
PLIST
	launchctl bootout "gui/$(id -u)/$AGENT_LABEL" 2>/dev/null || true
	launchctl bootstrap "gui/$(id -u)" "$AGENT_PLIST"
	local deadline=$(($(date +%s) + 60))
	until [ -f "$HOME_THERE/listening" ]; do
		[ "$(date +%s)" -lt "$deadline" ] || { echo "the agent is not listening in a minute: $(tail -5 "$HOME_THERE/daemon.log" 2>&1)" >&2; exit 1; }
		sleep 0.5
	done
}

case "${1:-}" in
listen)
	: "${SHARDS:?the shards binary to run}"
	ADDR=${ADDR:-$(lan):7443}
	mkdir -p "$HOME_THERE"
	if [ "${AGENT:-0}" = 1 ]; then
		agent_start
	else
		there run daemon --detached --listen "$ADDR"
	fi
	there invite client --name mover --output "$HOME_THERE/mover.invite"
	echo "listening at $ADDR: $(machine "$HOME_THERE"), shards $(there version 2>/dev/null | head -1)"
	base64 < "$HOME_THERE/mover.invite" | tr -d '\n'
	echo
	# Held up to HOLD seconds, where given, or until `stop` removes the home: a runner that
	# ends what a command leaves running (kiln's) keeps the daemon while this waits, run in
	# the background.
	if [ -n "${HOLD:-}" ]; then
		held=$(($(date +%s) + HOLD))
		while [ -d "$HOME_THERE" ] && [ "$(date +%s)" -lt "$held" ]; do sleep 5; done
		if [ -d "$HOME_THERE" ]; then there stop daemon || true; fi
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
	if [ -f "$AGENT_PLIST" ]; then
		launchctl bootout "gui/$(id -u)/$AGENT_LABEL" 2>/dev/null || true
		rm -f "$AGENT_PLIST"
	fi
	if [ -n "${KEEP_LOG:-}" ]; then cp "$HOME_THERE/daemon.log" "$KEEP_LOG" 2>/dev/null || true; fi
	rm -rf "$HOME_THERE"
	;;
move)
	: "${SHARDS:?the shards binary to run}"
	: "${INVITE:?the invitation the listening machine printed}"
	if [ "$(uname -s)" = Darwin ] && [ -z "${TWO_HOSTS_UNCLAMPED:-}" ] && [ "${UNCLAMP:-1}" = 1 ]; then
		job=$(mktemp -d)/move
		pass=(TWO_HOSTS_UNCLAMPED=1)
		for v in SHARDS INVITE THERE N FIRST BASELINE MODE IMAGE HOME_HERE HOME_THERE OUT REVISION DOCKER_CONFIG; do
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
	# The CPU this machine and the other share, told its microVMs (D176 part 6), where the
	# two differ (PM M462).
	if [ -n "${BASELINE:-}" ] && [ "$FIRST" = 1 ]; then here configure cpu --baseline lab; fi
	{
		echo "== $(date -u +%FT%TZ): moving from $(machine "$HOME_HERE"), revision ${REVISION:-$(git -C "$root" rev-parse --short HEAD 2>/dev/null || echo unknown)}, image $IMAGE, n $N, from move $FIRST"
		echo "   to: ${THERE:-$(here --context lab info --format '{{.Name}} {{.OperatingSystem}} {{.Architecture}}' 2>/dev/null || echo unknown)}"
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
	echo "| mode | what | n | p50 | p90 | p99 | max |"
	echo "|---|---|---|---|---|---|---|"
	# Each row as the last series (or the last part of a resumed one) said it, over all its
	# moves.
	awk '/^(cold|live) (move took|ran nowhere|sent|link|longest silence|rounds) \([^)]*\): / {
		mode = $1; what = $0
		sub(/^(cold|live) /, "", what); sub(/: .*$/, "", what); sub(/^move /, "", what)
		key = mode " " what
		if (!(key in row)) order[++n] = key
		if ($0 ~ /: none$/) { row[key] = sprintf("| %s | %s | 0 | | | | |", mode, what); next }
		sub(/^[^:]*: /, "")
		row[key] = sprintf("| %s | %s | %s | %s | %s | %s | %s |", mode, what, $2, $4, $6, $8, $10)
	} END { for (i = 1; i <= n; i++) print row[order[i]] }' "$OUT"
	echo
	awk '/^(cold|live): [0-9]+ moves/ { if (!($1 in last)) order[++n] = $1; last[$1] = $0 }
		END { for (i = 1; i <= n; i++) print "- " last[order[i]] }' "$OUT"
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
