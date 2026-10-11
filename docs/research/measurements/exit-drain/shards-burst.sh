#!/bin/sh
# PM M182: docker-burst.sh's burst, through shards: a receiver counts what comes (python3,
# its socket's buffer the kernel's default), a sender sends COUNT datagrams of 1,400 bytes
# at once and exits; N times, microVMs on a user network of their own home.
#   shards-burst.sh SHARDS [COUNT] [N]
set -u
shards=${1:?usage: shards-burst.sh SHARDS [COUNT] [N]}
COUNT=${2:-200}
N=${3:-10}
IMAGE=${IMAGE:-python:3.13-alpine}
home=$(mktemp -d)
trap 'SHARDS_HOME=$home "$shards" stop daemon >/dev/null 2>&1 || :; rm -r "$home"' EXIT
s() { SHARDS_HOME=$home "$shards" "$@"; }
s pull -q "$IMAGE" >/dev/null
s network create exd >/dev/null
s run --rm "$IMAGE" sh -c 'echo "rmem_default $(cat /proc/sys/net/core/rmem_default)"'
for i in $(seq "$N"); do
	s run -d --name exd-r --network exd "$IMAGE" python3 -c '
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
' >/dev/null 2>&1
	sleep 1
	s run --rm --network exd "$IMAGE" python3 -c "
import socket
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
p = b'x' * 1400
for _ in range($COUNT):
    s.sendto(p, ('exd-r', 9999))
"
	s wait exd-r >/dev/null
	echo "run $i: $(s logs exd-r)"
	s rm -f exd-r >/dev/null
done
