#!/bin/sh
# PM M181: a container joining a host-network container's network (`--network
# container:NAME`), as dockerd gives it, run in shards-dind (Docker in Docker, never the
# host's Docker): what it is told, its names and files, whether a port its listener takes
# is the host's while its provider runs and after the provider stops, whether a container
# joins a stopped one, and inspect. Containers are named nhj-probe-*, removed after.
set -u
DIND=${DIND:-shards-dind}
IMAGE=${IMAGE:-alpine:3.22}
d() { docker exec "$DIND" docker "$@"; }
say() { printf '== %s\n' "$1"; }
say "docker"; d version --format '{{.Server.Version}}'
d run -d --name nhj-probe-p --network host "$IMAGE" sleep 600 >/dev/null
say "a joiner, its stderr too"
d run --rm --network container:nhj-probe-p "$IMAGE" sh -c 'hostname; grep -c . /etc/hosts; grep -v "^#" /etc/resolv.conf | head -3; ls /sys/class/net | wc -l' 2>&1; echo "exit $?"
say "the host's own"; docker exec "$DIND" sh -c 'hostname; grep -c . /etc/hosts; ls /sys/class/net | wc -l'
say "a joiner's listener's port is the host's"
d run -d --name nhj-probe-a --network container:nhj-probe-p "$IMAGE" sh -c 'while true; do echo hi | nc -l -p 17781; done' >/dev/null
sleep 1
docker exec "$DIND" sh -c 'nc -w 2 127.0.0.1 17781 </dev/null; echo "from the host: exit $?"'
say "a port the host holds already, in a joiner"
docker exec -d "$DIND" sh -c 'nc -l -p 17783 </dev/null'
sleep 1
d run --rm --network container:nhj-probe-p "$IMAGE" sh -c 'nc -l -p 17783 </dev/null & sleep 1; kill $! 2>/dev/null; wait $!; echo "exit $?"' 2>&1
say "inspect of a joiner"
d inspect -f '{{json .NetworkSettings.Networks}} {{.HostConfig.NetworkMode}} {{.Config.Hostname}}' nhj-probe-a | sed 's/container:[0-9a-f]*/container:ID/'
say "its provider stopped"
d stop -t 0 nhj-probe-p >/dev/null
d inspect -f '{{.State.Status}}' nhj-probe-a
docker exec "$DIND" sh -c 'nc -w 2 127.0.0.1 17781 </dev/null; echo "the first listener, from the host: exit $?"'
d exec -d nhj-probe-a sh -c 'echo two | nc -l -p 17782'
sleep 1
docker exec "$DIND" sh -c 'nc -w 2 127.0.0.1 17782 </dev/null; echo "a listener made since, from the host: exit $?"'
say "joining a stopped provider"
d run --rm --network container:nhj-probe-p "$IMAGE" true 2>&1; echo "exit $?"
d rm -f nhj-probe-a nhj-probe-p >/dev/null
