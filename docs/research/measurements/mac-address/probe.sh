#!/bin/sh
# dockerd's MAC addresses, as a container sees and inspect shows them, run in shards-dind
# (Docker in Docker, never the host's Docker): one given each way (`--mac-address`, the
# endpoint's `mac-address`), one malformed each way, a multicast one, one on `none`, on
# `host` and on another's network, the same one twice on a network, and none given (what
# dockerd makes). Each container is `--rm`, but those inspected, removed by name after.
set -u
DIND=${DIND:-shards-dind}
IMAGE=${IMAGE:-alpine:3.22}
d() { docker exec "$DIND" docker "$@"; }
say() { printf '== %s\n' "$1"; }
mac='cat /sys/class/net/eth0/address'
say "docker"; d version --format '{{.Server.Version}}'
say "--mac-address"; d run --rm --mac-address 02:42:ac:11:00:99 "$IMAGE" sh -c "$mac"; echo "exit $?"
say "endpoint mac-address"; d run --rm --network name=bridge,mac-address=02:42:ac:11:00:98 "$IMAGE" sh -c "$mac"; echo "exit $?"
say "upper case"; d run --rm --mac-address 02:42:AC:11:00:97 "$IMAGE" sh -c "$mac"; echo "exit $?"
say "--mac-address malformed"; d run --rm --mac-address 02:42:zz:11:00:99 "$IMAGE" true; echo "exit $?"
say "endpoint mac-address malformed"; d run --rm --network name=bridge,mac-address=bad "$IMAGE" true; echo "exit $?"
say "multicast"; d run --rm --mac-address 01:00:5e:00:00:01 "$IMAGE" sh -c "$mac"; echo "exit $?"
say "none"; d run --rm --network none --mac-address 02:42:ac:11:00:96 "$IMAGE" sh -c "ls /sys/class/net"; echo "exit $?"
say "host"; d run --rm --network host --mac-address 02:42:ac:11:00:95 "$IMAGE" true; echo "exit $?"
say "another's network"
d run -d --name mac-probe-a "$IMAGE" sleep 60 >/dev/null
d run --rm --network container:mac-probe-a --mac-address 02:42:ac:11:00:94 "$IMAGE" true; echo "exit $?"
say "the same twice on one network"
d run -d --name mac-probe-b --mac-address 02:42:ac:11:00:93 "$IMAGE" sleep 60 >/dev/null; echo "exit $?"
d run --rm --mac-address 02:42:ac:11:00:93 "$IMAGE" sh -c "$mac"; echo "exit $?"
say "inspect, given and made: each network's MacAddress, then the whole NetworkSettings"
d inspect -f '{{range $k, $v := .NetworkSettings.Networks}}{{$k}} {{$v.MacAddress}}{{end}}' mac-probe-b
d inspect -f '{{range $k, $v := .NetworkSettings.Networks}}{{$k}} {{$v.MacAddress}}{{end}}' mac-probe-a
d inspect -f '{{json .NetworkSettings}}' mac-probe-b
say "a created container's, before it starts"
d create --name mac-probe-c --mac-address 02:42:ac:11:00:92 "$IMAGE" true >/dev/null
d inspect -f '{{range $k, $v := .NetworkSettings.Networks}}{{$k}} {{$v.MacAddress}}{{end}}' mac-probe-c
d create --name mac-probe-d "$IMAGE" true >/dev/null
d inspect -f '{{range $k, $v := .NetworkSettings.Networks}}{{$k}} [{{$v.MacAddress}}]{{end}}' mac-probe-d
d rm -f mac-probe-c mac-probe-d >/dev/null
say "made, two containers"; d run --rm "$IMAGE" sh -c "$mac"; d run --rm "$IMAGE" sh -c "$mac"
d rm -f mac-probe-a mac-probe-b >/dev/null
say "dash form"; d run --rm --mac-address 02-42-ac-11-00-91 "$IMAGE" sh -c "$mac"; echo "exit $?"
say "dot form"; d run --rm --mac-address 0242.ac11.0090 "$IMAGE" sh -c "$mac"; echo "exit $?"
say "eight octets (EUI-64)"; d run --rm --mac-address 02:42:ac:11:00:99:00:01 "$IMAGE" sh -c "$mac"; echo "exit $?"
say "all zeros"; d run --rm --mac-address 00:00:00:00:00:00 "$IMAGE" sh -c "$mac"; echo "exit $?"
say "spaces around"; d run --rm --mac-address " 02:42:ac:11:00:8f " "$IMAGE" sh -c "$mac"; echo "exit $?"
say "as inspect shows each form given: created, running, exited (and Config's MacAddress)"
nets='{{range $k, $v := .NetworkSettings.Networks}}{{$k}} [{{$v.MacAddress}}]{{end}}'
for form in 02:42:AC:11:00:8e 02-42-ac-11-00-8d 0242.ac11.008c 02:42:ac:11:00:8b:00:01 " 02:42:ac:11:00:8a " ""; do
	d create --name mac-probe-f --mac-address "$form" "$IMAGE" sleep 3 >/dev/null || { echo "create [$form]: exit $?"; continue; }
	printf 'given [%s] created: ' "$form"; d inspect -f "$nets" mac-probe-f
	d start mac-probe-f >/dev/null
	printf 'given [%s] running: ' "$form"; d inspect -f "$nets" mac-probe-f
	d wait mac-probe-f >/dev/null
	printf 'given [%s] exited: ' "$form"; d inspect -f "$nets" mac-probe-f
	printf 'given [%s] Config: ' "$form"; d inspect -f '{{json .Config}}' mac-probe-f | grep -o '"MacAddress":"[^"]*"' || echo none
	d start mac-probe-f >/dev/null
	printf 'given [%s] restarted: ' "$form"; d inspect -f "$nets" mac-probe-f
	d rm -f mac-probe-f >/dev/null
done
say "one network's endpoint given one, on a user network"
d network create mac-probe-net >/dev/null
d run -d --name mac-probe-g --network name=mac-probe-net,mac-address=02:42:ac:11:00:89 "$IMAGE" sleep 30 >/dev/null
d inspect -f "$nets" mac-probe-g
d network inspect -f '{{range .Containers}}{{.Name}} {{.MacAddress}}{{end}}' mac-probe-net
d rm -f mac-probe-g >/dev/null; d network rm mac-probe-net >/dev/null
say "eight octets given on a user network, and the default bridge's members"
d network create mac-probe-net >/dev/null
d run -d --name mac-probe-h --network name=mac-probe-net,mac-address=02:42:ac:11:00:88:00:01 "$IMAGE" sleep 30 >/dev/null
d inspect -f "$nets" mac-probe-h
d network inspect -f '{{range .Containers}}{{.Name}} {{.MacAddress}}{{end}}' mac-probe-net
d exec mac-probe-h cat /sys/class/net/eth0/address
d rm -f mac-probe-h >/dev/null; d network rm mac-probe-net >/dev/null
d run -d --name mac-probe-i --mac-address 02:42:ac:11:00:87:00:01 "$IMAGE" sleep 30 >/dev/null
d network inspect -f '{{range .Containers}}{{.Name}} {{.MacAddress}}{{end}}' bridge
d rm -f mac-probe-i >/dev/null
say "given on none and host: created, running"
for net in none host; do
	d create --name mac-probe-j --network "$net" --mac-address 02:42:ac:11:00:86 "$IMAGE" sleep 3 >/dev/null
	printf '%s created: ' "$net"; d inspect -f "$nets" mac-probe-j
	d start mac-probe-j >/dev/null
	printf '%s running: ' "$net"; d inspect -f "$nets" mac-probe-j
	d rm -f mac-probe-j >/dev/null
done
say "two given one MAC on a network: does each reach the other? (and two of different MACs, the control)"
d network create mac-probe-net >/dev/null
for pair in "02:42:ac:11:00:85 02:42:ac:11:00:85" "02:42:ac:11:00:84 02:42:ac:11:00:83"; do
	set -- $pair
	d run -d --name mac-probe-k --network name=mac-probe-net,mac-address="$1" "$IMAGE" sh -c 'while true; do echo hi | nc -l -p 7000; done' >/dev/null
	for i in 1 2 3; do
		printf '%s to %s, try %s: ' "$2" "$1" "$i"
		d run --rm --network name=mac-probe-net,mac-address="$2" "$IMAGE" sh -c 'nc -w 3 mac-probe-k 7000 </dev/null; echo "exit $?"' 2>&1 | tr '\n' ' '; echo
	done
	d rm -f mac-probe-k >/dev/null
done
d network rm mac-probe-net >/dev/null
