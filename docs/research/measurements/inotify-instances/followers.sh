#!/bin/sh
# What a daemon's watches cost a user's inotify instances (PM M257): a daemon in a home of
# its own, one run, N followers of its log (`logs -f`); after 10 s, the instances the
# daemon holds, against what the user holds and may hold, and how many followers still
# follow. Usage: followers.sh SHARDS [N]. Linux only; pulls alpine:3.22 the first time.
set -eu
shards=$1
n=${2:-100}
dir=$(mktemp -d /tmp/inotify-followers.XXXXXX)
mkdir "$dir/config"
export SHARDS_HOME="$dir/home" DOCKER_CONFIG="$dir/config"
held() { # the inotify descriptors of process $1
    c=0
    for fd in /proc/"$1"/fd/*; do
        [ "$(readlink "$fd" 2>/dev/null)" = "anon_inode:inotify" ] && c=$((c + 1))
    done
    echo "$c"
}
user_held() {
    c=0
    for p in /proc/[0-9]*; do
        [ -O "$p" ] && c=$((c + $(held "${p#/proc/}")))
    done
    echo "$c"
}
before=$(user_held)
"$shards" run -d --name followed --pull missing alpine:3.22 sh -c 'while :; do sleep 1; done' >/dev/null
daemon=$(cat "$SHARDS_HOME/daemon.pid")
i=0
pids=
while [ "$i" -lt "$n" ]; do
    "$shards" logs -f followed >/dev/null 2>"$dir/follower.$i" &
    pids="$pids $!"
    i=$((i + 1))
done
sleep 10
following=0
for p in $pids; do
    kill -0 "$p" 2>/dev/null && following=$((following + 1))
done
printf 'followers %s\nstill following %s\ndaemon holds %s\nuser held %s before, %s now, of %s\n' \
    "$n" "$following" "$(held "$daemon")" "$before" "$(user_held)" \
    "$(cat /proc/sys/fs/inotify/max_user_instances)"
printf 'their errors:\n'
cat "$dir"/follower.* | sort | uniq -c
"$shards" rm -f followed >/dev/null
"$shards" daemon stop >/dev/null 2>&1 || true
wait
case $dir in /tmp/inotify-followers.*) rm -rf "$dir" ;; esac
