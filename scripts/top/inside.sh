#!/bin/sh
# Inside scripts/top/generate's container: starts the processes, then records
# shards-init's dump and each case's `ps CASE -q PIDS`, as dockerd runs it (moby
# daemon/top_unix.go), at one instant of uptime.
set -eu
apt-get update -qq >/dev/null
apt-get install -qq -y "procps=$1" >/dev/null 2>&1
useradd -m averyveryverylongusername
cd /top

sleep 1000 &
nice -n 5 sleep 1001 &
setpriv --reuid=averyveryverylongusername --regid=averyveryverylongusername --init-groups sleep 1002 &
setpriv --reuid=4321 --regid=4321 --clear-groups sleep 1003 &
# A zombie: sleep 0, never reaped by the sleep its shell became.
sh -c 'sleep 0 & exec sleep 1004' &
script -qfc 'sleep 1005' /dev/null </dev/null >/dev/null &
sh -c 'i=0; while [ $i -lt 3000000 ]; do i=$((i+1)); done; exec sleep 1006' &
# Its arguments shown on the shell, kept by the command after its sleep.
sh -c 'sleep 1007; :' "an arg with  spaces" "tab	here" &
# Until the busy one has become its sleep.
until ps -eo args | grep -q '^sleep 1006'; do
	sleep 0.2
done
sleep 1

# Every process but this driver.
pids=$(ls /proc | grep -E '^[0-9]+$' | grep -vx "$$" | sort -n | tr '\n' ',' | sed 's/,$//')
echo "$pids" >pids
# Uptime held still, as LXCFS holds a container's: ps and the dump count every
# time-dependent column from the same instant (procps reads /proc/uptime as it starts).
cat /proc/uptime >uptime
mount --bind uptime /proc/uptime
./shards-init processes >dump
n=0
while IFS= read -r args; do
	n=$((n + 1))
	# shellcheck disable=SC2086 # split as dockerd splits them, on spaces
	ps $args -q "$pids" >"$(printf '%02d' $n).out" 2>&1 || true
done <cases
