#!/bin/sh
# PM M182: what dockerd's containers on a user network make of a burst of UDP datagrams,
# run in shards-dind (Docker in Docker, never the host's Docker): a receiver counts what
# comes (python3, its socket's buffer the kernel's default), a sender sends COUNT
# datagrams of 1,400 bytes at once and exits; N times. Containers and the network are
# named exd-probe-*, removed after.
set -u
DIND=${DIND:-shards-dind}
IMAGE=${IMAGE:-python:3.13-alpine}
COUNT=${1:-200}
N=${2:-10}
d() { docker exec "$DIND" docker "$@"; }
d network create exd-probe-net >/dev/null
d version --format 'docker {{.Server.Version}}'
d run --rm "$IMAGE" sh -c 'echo "rmem_default $(cat /proc/sys/net/core/rmem_default)"'
for i in $(seq "$N"); do
	d run -d --name exd-probe-r --network exd-probe-net "$IMAGE" python3 -c '
import socket
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.bind(("0.0.0.0", 9999))
s.settimeout(10)
n = 0
try:
    s.recv(65536)
    n = 1
    s.settimeout(1)
    while True:
        s.recv(65536)
        n += 1
except socket.timeout:
    pass
print("got", n)
' >/dev/null
	sleep 1
	d run --rm --network exd-probe-net "$IMAGE" python3 -c "
import socket
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
p = b'x' * 1400
for _ in range($COUNT):
    s.sendto(p, ('exd-probe-r', 9999))
"
	d wait exd-probe-r >/dev/null
	echo "run $i: $(d logs exd-probe-r)"
	d rm -f exd-probe-r >/dev/null
done
d network rm exd-probe-net >/dev/null
