#!/bin/sh
# The inotify instances this user's processes hold, against the user's limits (inotify(7):
# fs.inotify.max_user_instances and max_user_watches): each process's count, PID and name,
# most first, then the total (PM M257). Counted by descriptor, of the processes whose
# descriptors /proc shows the user: an instance a fork inherited counts once for each
# holder, and a process that hides its descriptors (not dumpable, prctl(2)) counts none,
# so this is not the kernel's own count. Linux only.
set -eu
printf 'max_user_instances %s\nmax_user_watches %s\n' \
    "$(cat /proc/sys/fs/inotify/max_user_instances)" "$(cat /proc/sys/fs/inotify/max_user_watches)"
held=$(for p in /proc/[0-9]*; do
    [ -O "$p" ] || continue
    n=0
    for fd in "$p"/fd/*; do
        [ "$(readlink "$fd" 2>/dev/null)" = "anon_inode:inotify" ] && n=$((n + 1))
    done
    [ "$n" -eq 0 ] || printf '%s %s %s\n' "$n" "${p#/proc/}" "$(tr -d '\n' < "$p/comm" 2>/dev/null)"
done)
printf '%s\n' "$held" | sort -rn
printf '%s\n' "$held" | awk '{ total += $1 } END { print "total", total + 0 }'
