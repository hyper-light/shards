#!/bin/sh
# dockerd's `--network host`, as a container sees it and the CLI and daemon take it, run in
# shards-dind (Docker in Docker, never the host's Docker): what is printed, what is refused
# with it, what the container's hostname, /etc/hosts and resolv.conf are, whether a port
# it listens on is the host's, and whether it reaches the host's loopback. Containers are
# named nh-probe-*, removed after.
set -u
DIND=${DIND:-shards-dind}
IMAGE=${IMAGE:-alpine:3.22}
d() { docker exec "$DIND" docker "$@"; }
say() { printf '== %s\n' "$1"; }
say "docker"; d version --format '{{.Server.Version}}'
say "plain"; d run --rm --network host "$IMAGE" sh -c 'hostname; cat /etc/hosts; cat /etc/resolv.conf; ls /sys/class/net'; echo "exit $?"
say "the host's own"; docker exec "$DIND" sh -c 'hostname; cat /etc/hosts; ls /sys/class/net'
for opts in "-p 8080:80" "-P --expose 80" "--hostname nh" "--dns 9.9.9.9" "--add-host x:10.0.0.1" "--mac-address 02:42:ac:11:00:99" "--ip 172.17.0.9" "--network-alias a" "--link-local-ip 169.254.1.1" "--domainname example.org" "--dns-search example.org" "--sysctl net.ipv4.ip_forward=1" "--sysctl net.core.somaxconn=1024" "--cap-add NET_ADMIN" "--userns host" "--privileged"; do
	say "with $opts"
	d run --rm --network host $opts "$IMAGE" sh -c 'hostname; cat /etc/resolv.conf | grep -v "^#" | head -3; grep -c . /etc/hosts'; echo "exit $?"
done
say "a listener's port is the host's"
d run -d --name nh-probe-a --network host "$IMAGE" sh -c 'while true; do echo hi | nc -l -p 17771; done' >/dev/null
sleep 1
docker exec "$DIND" sh -c 'nc -w 2 127.0.0.1 17771 </dev/null; echo "from the host: exit $?"'
d rm -f nh-probe-a >/dev/null
say "the host's loopback, from the container"
docker exec -d "$DIND" sh -c 'echo there | nc -l -p 17772'
sleep 1
d run --rm --network host "$IMAGE" sh -c 'nc -w 2 127.0.0.1 17772 </dev/null; echo "exit $?"'
say "a port the host holds already"
docker exec -d "$DIND" sh -c 'nc -l -p 17773 </dev/null'
sleep 1
d run --rm --network host "$IMAGE" sh -c 'nc -l -p 17773 </dev/null & sleep 1; kill $! 2>/dev/null; wait $!; echo "exit $?"' 2>&1
say "inspect"
d create --name nh-probe-b --network host "$IMAGE" true >/dev/null
d inspect -f '{{json .NetworkSettings.Networks}} {{.HostConfig.NetworkMode}} {{.Config.Hostname}}' nh-probe-b
d rm -f nh-probe-b >/dev/null
