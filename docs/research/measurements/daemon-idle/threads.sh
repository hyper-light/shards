#!/bin/sh
# An idle daemon's wakeups, thread by thread (Linux): a daemon in a home of its own, no
# run, and over SECONDS each of its threads' context switches (/proc/PID/task/TID/status,
# voluntary and not) and the daemon's CPU time (/proc/PID/stat's utime and stime, in clock
# ticks). Usage: threads.sh SHARDS [SECONDS]
set -eu
shards=$1
secs=${2:-30}
dir=$(mktemp -d /tmp/daemon-idle.XXXXXX)
mkdir "$dir/config"
export SHARDS_HOME="$dir/home" DOCKER_CONFIG="$dir/config"
"$shards" ps > /dev/null
daemon=$(cat "$SHARDS_HOME/daemon.pid")
sleep 5
snap() {
    for t in /proc/"$daemon"/task/*; do
        printf '%s %s %s\n' "${t##*/}" "$(tr ' ' _ < "$t/comm")" \
            "$(awk '/^voluntary_ctxt_switches/ { v = $2 } /^nonvoluntary_ctxt_switches/ { n = $2 } END { print v + n }' "$t/status")"
    done
}
ticks() { awk '{ print $14 + $15 }' /proc/"$daemon"/stat; }
snap > "$dir/before"
t0=$(ticks)
sleep "$secs"
snap > "$dir/after"
t1=$(ticks)
echo "$secs s, $(wc -l < "$dir/after") threads, CPU $((t1 - t0)) ticks of $(getconf CLK_TCK) a second"
awk 'NR == FNR { was[$1] = $3; next } { d = $3 - was[$1]; total += d; if (d > 0) print "  " $2 " (" $1 "): " d } END { print "switches", total + 0 }' \
    "$dir/before" "$dir/after"
"$shards" daemon stop > /dev/null 2>&1 || true
case $dir in /tmp/daemon-idle.*) rm -rf "$dir" ;; esac
