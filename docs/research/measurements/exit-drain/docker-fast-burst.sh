#!/bin/sh
# PM M182: docker-burst.sh with the test guest's own sender and counter, as
# datagrams_a_command_sends_as_it_ends runs them in microVMs: `udp-send` sends COUNT
# datagrams of 1,400 bytes as fast as it can and exits, `udp-count` counts what comes and
# says the guest's counters; N times, in shards-dind, the binary at TESTGUEST copied in
# first (tar over `docker exec -i`). Containers and the network named exd-probe-*,
# removed after.
#   docker-fast-burst.sh TESTGUEST [COUNT] [N]
set -u
tg=${1:?usage: docker-fast-burst.sh TESTGUEST [COUNT] [N]}
COUNT=${2:-200}
N=${3:-10}
DIND=${DIND:-shards-dind}
IMAGE=${IMAGE:-alpine:3.22}
d() { docker exec "$DIND" docker "$@"; }
tar -C "$(dirname "$tg")" -cf - "$(basename "$tg")" |
	docker exec -i "$DIND" sh -c 'mkdir -p /root/exd && tar -C /root/exd -xf -'
bin=/t/$(basename "$tg")
d network create exd-probe-net >/dev/null
for i in $(seq "$N"); do
	d run -d --name exd-probe-r --network exd-probe-net -v /root/exd:/t:ro "$IMAGE" "$bin" udp-count 0.0.0.0:9999 1000 >/dev/null
	sleep 1
	sent=$(d run --rm --network exd-probe-net -v /root/exd:/t:ro "$IMAGE" "$bin" udp-send exd-probe-r:9999 "$COUNT" 1400 0 | head -1)
	d wait exd-probe-r >/dev/null
	echo "run $i: $sent, $(d logs exd-probe-r | grep '^got')"
	d rm -f exd-probe-r >/dev/null
done
d network rm exd-probe-net >/dev/null
