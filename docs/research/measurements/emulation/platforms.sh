#!/bin/sh
# PM M243 (D179): how Docker keeps and runs the platforms of one image (the containerd
# image store): a run for another platform where the host's is here, which platform the
# image then runs, a run of a platform not here where it may not pull, by name and by
# ID, and what each pull says. Run it in a Docker daemon, as
#   docker exec -i shards-dind sh < platforms.sh
# It pulls hello-world by its index's digest, a name nothing else there uses, and removes
# that name alone.
set -u
R=hello-world@sha256:11a3eb1597e864c51b3f56733cb2e8a90bf0c284bfc05f9444d81c558bc848aa
HOST=$(docker version --format '{{.Server.Arch}}')
case "$HOST" in
arm64) OTHER=amd64 ;;
*) OTHER=arm64 ;;
esac
OUT=$(mktemp)
ERR=$(mktemp)
step() { echo "=== $1"; }
run() {
	"$@" >"$OUT" 2>"$ERR"
	echo "exit $?"
	echo "-- stderr"
	cat "$ERR"
	echo "-- stdout"
	head -3 "$OUT"
}
docker version --format 'engine {{.Server.Version}} {{.Server.Os}}/{{.Server.Arch}}'
docker info --format '{{.DriverStatus}}'
step "1 pull --platform linux/$HOST"
run docker pull --platform "linux/$HOST" "$R"
step "2 inspect"
docker image inspect "$R" --format '{{.Architecture}} {{.Id}}'
step "3 run --platform linux/$OTHER"
run docker run --rm --platform "linux/$OTHER" "$R"
step "4 inspect after linux/$OTHER"
docker image inspect "$R" --format '{{.Architecture}} {{.Id}}'
step "5 run, no platform"
run docker run --rm "$R"
step "6 run --pull never --platform linux/s390x"
run docker run --rm --pull never --platform linux/s390x "$R"
step "7 pull --platform linux/$OTHER again"
run docker pull --platform "linux/$OTHER" "$R"
step "8 run --pull always"
run docker run --rm --pull always "$R"
step "9 image ls --tree"
docker image ls --tree 2>&1 | grep -A8 hello-world
ID=$(docker image inspect "$R" --format '{{.Id}}' | cut -c8-19)
step "10 run --pull never --platform linux/s390x $ID"
run docker run --rm --pull never --platform linux/s390x "$ID"
step "11 run --platform linux/s390x $ID"
run docker run --rm --platform linux/s390x "$ID"
step "12 remove"
docker rmi "$R" >/dev/null && echo removed
rm -f "$OUT" "$ERR"
