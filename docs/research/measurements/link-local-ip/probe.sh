#!/bin/sh
# dockerd's link-local addresses (`--link-local-ip`, an endpoint's `link-local-ip`), as a
# container sees them and inspect shows them, run in shards-dind (Docker in Docker, never
# the host's Docker): on the default bridge and a user network, IPv4 and IPv6, one that
# is not link-local, one on `none`; whether two containers reach each other at them, and
# one without any reaches one with; inspect while created, running and exited. Containers
# and the network are named mll-probe-*, removed after.
set -u
DIND=${DIND:-shards-dind}
IMAGE=${IMAGE:-alpine:3.22}
d() { docker exec "$DIND" docker "$@"; }
say() { printf '== %s\n' "$1"; }
addrs='ip -o addr show dev eth0 | sed "s/^[0-9]*: //"; ip -o route; ip -o -6 route 2>/dev/null'
say "docker"; d version --format '{{.Server.Version}}'
say "--link-local-ip, IPv4, default bridge"; d run --rm --link-local-ip 169.254.1.1 "$IMAGE" sh -c "$addrs"; echo "exit $?"
say "endpoint link-local-ip, two, default bridge"; d run --rm --network name=bridge,link-local-ip=169.254.1.2,link-local-ip=169.254.7.7 "$IMAGE" sh -c "$addrs"; echo "exit $?"
say "IPv6, default bridge (no IPv6)"; d run --rm --link-local-ip fe80::1234 "$IMAGE" sh -c "$addrs"; echo "exit $?"
say "not link-local"; d run --rm --link-local-ip 10.9.8.7 "$IMAGE" sh -c "$addrs"; echo "exit $?"
say "not link-local, IPv6"; d run --rm --link-local-ip fd00::7 "$IMAGE" sh -c "$addrs"; echo "exit $?"
say "the network's own subnet"; d run --rm --link-local-ip 172.17.0.200 "$IMAGE" sh -c "$addrs"; echo "exit $?"
say "unspecified"; d run --rm --link-local-ip 0.0.0.0 "$IMAGE" true; echo "exit $?"
say "malformed"; d run --rm --link-local-ip nope "$IMAGE" true; echo "exit $?"
say "none"; d run --rm --network none --link-local-ip 169.254.1.3 "$IMAGE" sh -c 'ip -o addr'; echo "exit $?"
say "a user network, IPv6 on"
d network create --ipv6 --subnet fd7b::/64 mll-probe-net >/dev/null
d run --rm --network name=mll-probe-net,link-local-ip=169.254.2.1,link-local-ip=fe80::2:1 "$IMAGE" sh -c "$addrs"; echo "exit $?"
say "reach: a server at its link-local addresses, a client with its own, one with none"
d run -d --name mll-probe-srv --network name=mll-probe-net,link-local-ip=169.254.2.10,link-local-ip=fe80::2:10 "$IMAGE" sh -c 'while true; do echo hi | nc -l -p 7000; done' >/dev/null
for to in 169.254.2.10 fe80::2:10%eth0; do
	printf 'with its own, to %s: ' "$to"
	d run --rm --network name=mll-probe-net,link-local-ip=169.254.2.11,link-local-ip=fe80::2:11 "$IMAGE" sh -c "nc -w 3 $to 7000 </dev/null; echo exit \$?" 2>&1 | tr '\n' ' '; echo
	printf 'with none, to %s: ' "$to"
	d run --rm --network mll-probe-net "$IMAGE" sh -c "nc -w 3 $to 7000 </dev/null; echo exit \$?" 2>&1 | tr '\n' ' '; echo
done
printf 'off the network, with its own, to 169.254.2.10: '
d run --rm --link-local-ip 169.254.2.12 "$IMAGE" sh -c "nc -w 3 169.254.2.10 7000 </dev/null; echo exit \$?" 2>&1 | tr '\n' ' '; echo
say "inspect: the server's endpoint, and network inspect"
d inspect -f '{{json .NetworkSettings.Networks}}' mll-probe-srv
d network inspect -f '{{json .Containers}}' mll-probe-net
d rm -f mll-probe-srv >/dev/null
say "inspect: created, running, exited"
ll='{{range $k, $v := .NetworkSettings.Networks}}{{$k}} {{json $v.IPAMConfig}}{{end}}'
d create --name mll-probe-c --link-local-ip 169.254.3.1 --link-local-ip fe80::3:1 "$IMAGE" sleep 2 >/dev/null
printf 'created: '; d inspect -f "$ll" mll-probe-c
d start mll-probe-c >/dev/null
printf 'running: '; d inspect -f "$ll" mll-probe-c
d wait mll-probe-c >/dev/null
printf 'exited: '; d inspect -f "$ll" mll-probe-c
printf 'HostConfig: '; d inspect -f '{{json .HostConfig.NetworkMode}}' mll-probe-c
d rm -f mll-probe-c >/dev/null
d network rm mll-probe-net >/dev/null
say "/etc/hosts with one"; d run --rm --link-local-ip 169.254.4.1 "$IMAGE" cat /etc/hosts; echo "exit $?"
say "inspect, IPv4 alone: created, running, exited, restarted"
d create --name mll-probe-d --link-local-ip 169.254.5.1 "$IMAGE" sleep 2 >/dev/null
printf 'created: '; d inspect -f "$ll" mll-probe-d
d start mll-probe-d >/dev/null
printf 'running: '; d inspect -f "$ll" mll-probe-d
d exec mll-probe-d ip -o -4 addr show dev eth0
d wait mll-probe-d >/dev/null
printf 'exited: '; d inspect -f "$ll" mll-probe-d
d start mll-probe-d >/dev/null
printf 'restarted: '; d inspect -f "$ll" mll-probe-d
d rm -f mll-probe-d >/dev/null
say "another's network"
d run -d --name mll-probe-e "$IMAGE" sleep 30 >/dev/null
d run --rm --network container:mll-probe-e --link-local-ip 169.254.6.1 "$IMAGE" sh -c 'ip -o -4 addr show dev eth0'; echo "exit $?"
d rm -f mll-probe-e >/dev/null
say "the same twice"; d run --rm --link-local-ip 169.254.8.1 --link-local-ip 169.254.8.1 "$IMAGE" sh -c 'ip -o -4 addr show dev eth0'; echo "exit $?"
say "both ways at once"; d run --rm --link-local-ip 169.254.9.1 --network name=bridge,link-local-ip=169.254.9.2 "$IMAGE" sh -c 'ip -o -4 addr show dev eth0'; echo "exit $?"
say "an IPv4-mapped IPv6 one"; d run --rm --link-local-ip ::ffff:169.254.10.1 "$IMAGE" sh -c 'ip -o addr show dev eth0'; echo "exit $?"
say "a scoped IPv6 one"; d run --rm --link-local-ip fe80::1%eth0 "$IMAGE" true; echo "exit $?"
say "not link-local, on none"; d run --rm --network none --link-local-ip 10.9.8.7 "$IMAGE" true; echo "exit $?"
say "not link-local, on another's network"
d run -d --name mll-probe-f "$IMAGE" sleep 30 >/dev/null
d run --rm --network container:mll-probe-f --link-local-ip 10.9.8.7 "$IMAGE" true; echo "exit $?"
d rm -f mll-probe-f >/dev/null
say "an IPv4-mapped one, as inspect shows it"
d create --name mll-probe-g --link-local-ip ::ffff:169.254.11.1 "$IMAGE" true >/dev/null
d inspect -f "$ll" mll-probe-g
d rm -f mll-probe-g >/dev/null
say "a run's first error of two: not link-local, then the network not found"
d run --rm --network nope --link-local-ip 10.9.8.7 "$IMAGE" true; echo "exit $?"
say "one bad of two"; d run --rm --link-local-ip 169.254.12.1 --link-local-ip 10.9.8.6 "$IMAGE" true; echo "exit $?"
say "a zoned IPv6 one, and one of each, as inspect shows them (created, never started)"
d create --name mll-probe-h --link-local-ip fe80::1%eth0 --link-local-ip 169.254.13.1 "$IMAGE" true >/dev/null
d inspect -f "$ll" mll-probe-h
d rm -f mll-probe-h >/dev/null
d create --name mll-probe-i --network none --link-local-ip 169.254.14.1 "$IMAGE" true >/dev/null
d inspect -f "$ll" mll-probe-i
d rm -f mll-probe-i >/dev/null
