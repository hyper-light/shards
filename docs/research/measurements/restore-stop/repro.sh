#!/bin/zsh
# Does a microVM restored from a save (D176) end with its run once its daemon is killed?
# Each round, in a home of its own: the test guest imported as an image, a run of it
# under --init, an exec beside it as MODE asks, the run saved and restored, its daemon
# killed; then how long the home's processes outlive it, up to 5 s. One that outlives it
# is sampled (macOS `sample`, lsof) and its daemon's log kept, in DIR.
#
#   docs/research/measurements/restore-stop/repro.sh SHARDS TESTGUEST DIR [TIMES] [MODE]
#
# SHARDS: a shards binary (target/e2e/shards-*/shards, as the E2E tests run it); TESTGUEST:
# the static test guest (target/guest/<arch>-unknown-linux-musl/guest/shards-testguest);
# DIR: a short directory, which the homes' sockets live in. MODE: none; exec, an exec that
# sleeps; trickle, one that writes a line a millisecond; flood, one that writes as fast as
# it can (testguest count); stats, `shards stats` asked again and again through the save.
set -u
S=$1
G=$2
D=$3
times=${4:-5}
mode=${5:-flood}
export DOCKER_CONFIG=$D/docker-config
mkdir -p "$DOCKER_CONFIG"
ulimit -n "$(sysctl -n kern.maxfilesperproc 2>/dev/null || ulimit -Hn)"
rm -rf "${D:?}/stage" && mkdir -p "$D/stage/bin" "$D/stage/etc" "$D/stage/tmp" "$D/stage/work"
cp "$G" "$D/stage/bin/testguest"
printf 'root:x:0:0:root:/root:/bin/sh\n' >"$D/stage/etc/passwd"
printf 'root:x:0:\n' >"$D/stage/etc/group"
chmod 1777 "$D/stage/tmp"
tar --uid 0 --gid 0 -C "$D/stage" -cf "$D/image.tar" bin etc tmp work
now() { perl -MTime::HiRes=time -e 'printf "%.3f", time'; }
i=0
while [ "$i" -lt "$times" ]; do
	i=$((i + 1))
	H=$D/h$i
	rm -rf "${H:?}"
	export SHARDS_HOME=$H
	{
		"$S" import "$D/image.tar" repro:v1 >/dev/null || { echo "$i import failed"; continue; }
		"$S" run -d --init --name back repro:v1 /bin/testguest sleep >/dev/null || { echo "$i run failed"; continue; }
		loop=""
		case $mode in
		exec) "$S" exec -d back /bin/testguest sleep || echo "$i exec failed" ;;
		trickle) "$S" exec -d back /bin/testguest counter 1 && sleep 1 ;;
		flood) "$S" exec -d back /bin/testguest count 1000000000 && sleep 1 ;;
		stats)
			(while :; do "$S" stats --no-stream back >/dev/null 2>&1; done) &
			loop=$!
			sleep 0.3
			;;
		esac
		"$S" save vm back snap >/dev/null || echo "$i save failed"
		[ -n "$loop" ] && { kill "$loop"; wait "$loop" 2>/dev/null; }
		"$S" restore vm back >/dev/null || { echo "$i restore failed"; continue; }
	} 2>>"$D/client-$i.err"
	kill -9 "$(cat "$H/daemon.pid")"
	t0=$(now)
	left=""
	while :; do
		left=$(pgrep -f "$H/" | tr '\n' ' ')
		[ -z "$left" ] && break
		[ "$(echo "$(now) - $t0 > 5" | bc)" = 1 ] && break
		sleep 0.02
	done
	if [ -n "$left" ]; then
		echo "$i outlived its daemon by $(echo "$(now) - $t0" | bc) s: $left"
		for p in ${=left}; do
			command -v sample >/dev/null && sample "$p" 2 -file "$D/sample-$i-$p.txt" >/dev/null 2>&1
			lsof -p "$p" >"$D/lsof-$i-$p.txt" 2>/dev/null
		done
		cp "$H/daemon.log" "$D/daemon-$i.log" 2>/dev/null
		for p in ${=left}; do kill -9 "$p"; done
	else
		echo "$i ended $(echo "$(now) - $t0" | bc) s after its daemon"
	fi
	sleep 0.5
	rm -rf "${H:?}"
done
