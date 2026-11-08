#!/bin/sh
# PM M488, D226: a registry:2 behind an induced lossy link, for learned.sh. The loss and
# delay are tc-netem's (man 8 tc-netem) on the registry container's own interface, set
# from a sidecar in its network namespace: nothing of the host's own links changes.
#   netem.sh up PORT DELAY LOSS     e.g. netem.sh up 5091 40ms 1%
#   netem.sh down PORT
set -eu
name=m488-netem-$2
case $1 in
up)
  docker run -d --name "$name" -p "$2:5000" registry:2 >/dev/null
  docker run --rm --net "container:$name" --cap-add NET_ADMIN alpine \
    sh -c "apk add -q iproute2-tc && tc qdisc add dev eth0 root netem delay $3 loss $4 && tc qdisc show dev eth0"
  ;;
down)
  docker rm -f -v "$name" >/dev/null
  ;;
esac
