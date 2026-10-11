#!/bin/sh
# dockerd's endpoint sysctls (`--network NAME,driver-opt=com.docker.network.endpoint.sysctls=…`)
# as a container sees them, run in shards-dind (Docker in Docker, never the host's Docker):
# one alone; one of the same key as a `--sysctl`, each way round; two in one option, one
# with `ifname` in lower case; one the kernel has no file for; one with a value the kernel
# refuses. Each container is removed as it ends (`--rm`), and none is named.
set -u
DIND=${DIND:-shards-dind}
IMAGE=${IMAGE:-alpine:3.22}
EP=com.docker.network.endpoint.sysctls
d() { docker exec "$DIND" docker "$@"; }
say() { printf '== %s\n' "$1"; }
read_it='cat /proc/sys/net/ipv4/conf/eth0/log_martians'
say "docker"; d version --format '{{.Server.Version}}'
say "alone"
d run --rm --network "name=bridge,driver-opt=$EP=net.ipv4.conf.IFNAME.log_martians=1" "$IMAGE" sh -c "$read_it"; echo "exit $?"
say "--sysctl 0, endpoint 1"
d run --rm --sysctl net.ipv4.conf.eth0.log_martians=0 --network "name=bridge,driver-opt=$EP=net.ipv4.conf.IFNAME.log_martians=1" "$IMAGE" sh -c "$read_it"; echo "exit $?"
say "--sysctl 1, endpoint 0"
d run --rm --sysctl net.ipv4.conf.eth0.log_martians=1 --network "name=bridge,driver-opt=$EP=net.ipv4.conf.IFNAME.log_martians=0" "$IMAGE" sh -c "$read_it"; echo "exit $?"
say "two in one, lower-case ifname"
d run --rm --network "name=bridge,\"driver-opt=$EP=net.ipv4.conf.ifname.log_martians=1,net.ipv4.conf.IFNAME.accept_redirects=0\"" "$IMAGE" \
    sh -c "$read_it; cat /proc/sys/net/ipv4/conf/eth0/accept_redirects"; echo "exit $?"
say "a key with no file"
d run --rm --network "name=bridge,driver-opt=$EP=net.ipv4.conf.IFNAME.nonexistent=1" "$IMAGE" true; echo "exit $?"
say "a value refused"
d run --rm --network "name=bridge,driver-opt=$EP=net.ipv4.conf.IFNAME.log_martians=abc" "$IMAGE" true; echo "exit $?"
say "ipv6"
d run --rm --network "name=bridge,driver-opt=$EP=net.ipv6.conf.IFNAME.disable_ipv6=1" "$IMAGE" cat /proc/sys/net/ipv6/conf/eth0/disable_ipv6; echo "exit $?"
# Which comes last, a container's `--sysctl` or its endpoint's: `all.forwarding` sets every
# interface's, so eth0's after both says which was written after the other.
say "--sysctl all.forwarding=1, endpoint eth0 forwarding=0"
d run --rm --sysctl net.ipv4.conf.all.forwarding=1 --network "name=bridge,driver-opt=$EP=net.ipv4.conf.IFNAME.forwarding=0" "$IMAGE" \
    sh -c 'cat /proc/sys/net/ipv4/conf/all/forwarding /proc/sys/net/ipv4/conf/eth0/forwarding'; echo "exit $?"
say "--sysctl all.forwarding=0, endpoint eth0 forwarding=1"
d run --rm --sysctl net.ipv4.conf.all.forwarding=0 --network "name=bridge,driver-opt=$EP=net.ipv4.conf.IFNAME.forwarding=1" "$IMAGE" \
    sh -c 'cat /proc/sys/net/ipv4/conf/all/forwarding /proc/sys/net/ipv4/conf/eth0/forwarding'; echo "exit $?"
say "create, then start: when the --sysctl refusal comes"
d create --sysctl net.ipv4.conf.eth0.log_martians=1 "$IMAGE" true; echo "exit $?"
say "--network none, an interface's --sysctl"
d run --rm --network none --sysctl net.ipv4.conf.eth0.log_martians=1 "$IMAGE" true; echo "exit $?"
say "an interface's --sysctl other than eth0's"
d run --rm --sysctl net.ipv4.conf.eth1.log_martians=1 "$IMAGE" true; echo "exit $?"
say "an endpoint sysctl on none, which has no interface"
d run --rm --network "name=none,driver-opt=$EP=net.ipv4.conf.IFNAME.log_martians=1" "$IMAGE" sh -c 'ls /proc/sys/net/ipv4/conf'; echo "exit $?"
