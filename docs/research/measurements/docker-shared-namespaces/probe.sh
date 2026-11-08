# What dockerd does with another container's PID and network namespaces
# (docs/research/platform-measurements.md M169): the modes it keeps, what each container
# sees and lists, `--init` with a shared PID namespace, what a joiner leaves as it ends,
# and its refusals' words. Run inside a Docker-in-Docker container, its containers named
# sh-p119-* and removed at the end:
#
#     docker exec -i shards-dind sh -s < probe.sh
set -u
x() { echo "\$ docker $*"; docker "$@" 2>&1; echo "[exit $?]"; }
x run -d --name sh-p119-prov alpine:3.20 sleep 300
PROV=$(docker inspect -f '{{.Id}}' sh-p119-prov)
echo "prov id: $PROV"
x run -d --name sh-p119-join --network container:sh-p119-prov --pid container:sh-p119-prov alpine:3.20 sleep 301
x inspect -f '{{.HostConfig.NetworkMode}} | {{.HostConfig.PidMode}}' sh-p119-join
x exec sh-p119-join ps -o pid,comm,args
x top sh-p119-join
x top sh-p119-prov
x run --rm --pid container:sh-p119-prov alpine:3.20 ps -o pid,args
x rename sh-p119-prov sh-p119-prov2
x restart -t 1 sh-p119-join
x inspect -f '{{.State.Status}} {{.HostConfig.NetworkMode}}' sh-p119-join
x run -d --name sh-p119-init --network container:sh-p119-prov2 --pid container:sh-p119-prov2 --init alpine:3.20 sleep 302
x exec sh-p119-init ps -o pid,args
x inspect -f '{{.HostConfig.Init}} {{.Path}} {{.Args}}' sh-p119-init
x run -d --name sh-p119-kids --network container:sh-p119-prov2 --pid container:sh-p119-prov2 alpine:3.20 sh -c 'sleep 1000 & sleep 2'
sleep 5
x inspect -f '{{.State.Status}} {{.State.ExitCode}}' sh-p119-kids
x exec sh-p119-prov2 ps -o pid,args
x run --rm --pid container:sh-p119-nope alpine:3.20 true
x create --name sh-p119-off alpine:3.20 true
x run --rm --pid container:sh-p119-off alpine:3.20 true
x run --rm --pid container: alpine:3.20 true
x run --rm --pid bogus alpine:3.20 true
x run --rm --pid host alpine:3.20 sh -c 'ps -o pid,comm | head -3'
x rm -f sh-p119-join sh-p119-init sh-p119-kids sh-p119-off sh-p119-prov2
x ps -a --filter name=sh-p119 --format '{{.Names}}'
