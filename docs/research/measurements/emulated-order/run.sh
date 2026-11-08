#!/bin/sh
# M250 (docs/research/platform-measurements.md): on an arm64 host, which emulated platform an
# image with no arm64 manifest runs faster in a microVM (D179): 32-bit Arm under qemu-arm, or
# amd64 under qemu-x86_64, the same programs (Alpine 3.22.6's busybox, each CPU's build) in
# one microVM, under Docker's builds of qemu-user for BuildKit (tonistiigi/binfmt, pinned, as
# M241). host.py (M241's) runs ROUNDS rounds, each a native and an emulated run of each
# workload for each CPU in turn, measured from the host, and prints n, p50, p90, p99 and max
# of the emulated runs' cost over the native's, for each CPU. With LOAD, as many busy loops
# run beside it.
#
#   run.sh SHARDS ROUNDS
set -eu
shards=$1
rounds=$2
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
spinners=""
cleanup() {
	[ -n "$spinners" ] && kill $spinners
	"$shards" rm -f m250 >/dev/null 2>&1 || true
	"$shards" stop daemon >/dev/null 2>&1 || true
	rm -r -- "$work"
}
trap cleanup EXIT
case $(uname -m) in
arm64 | aarch64) ;;
*)
	echo "run.sh: an arm64 host's question; this is $(uname -m)" >&2
	exit 1
	;;
esac
binfmt=tonistiigi/binfmt@sha256:400a4873b838d1b89194d982c45e5fb3cda4593fbfd7e08a02e76b03b21166f0
mkdir -p "$work/home" "$work/x" "$work/dl"
export SHARDS_HOME="$work/home"
# Its images are public: no credential helper of the user's is asked for them.
mkdir -p "$work/docker-config"
export DOCKER_CONFIG="$work/docker-config"
"$shards" pull -q "$binfmt" >/dev/null
"$shards" create --name binfmt "$binfmt" >/dev/null
# Each CPU: its qemu, and Alpine's minirootfs of it, pinned by the release's own sha256.
for spec in "x86_64 x86_64 27694aaa55fd7a9e3ef596e0ad4eb66802308bb20172b17030cd5f4d8ae9bac2" \
	"arm armv7 e9249c2c6e01bcd2f7034c1908baf14686b310535b9fd67d3693f85b320e2e90"; do
	set -- $spec
	"$shards" cp "binfmt:/usr/bin/qemu-$1" "$work/x/qemu-$1"
	curl -fsSL --retry 5 --retry-all-errors -o "$work/dl/$2.tar.gz" \
		"https://dl-cdn.alpinelinux.org/alpine/v3.22/releases/$2/alpine-minirootfs-3.22.6-$2.tar.gz"
	echo "$3  $work/dl/$2.tar.gz" | shasum -a 256 -c - >/dev/null
	mkdir -p "$work/x/root-$1"
	tar -xzf "$work/dl/$2.tar.gz" -C "$work/x/root-$1"
done
"$shards" rm binfmt >/dev/null
cp "$here/setup.sh" "$here/serve.sh" "$work/x/"
"$shards" pull -q alpine:3.22 >/dev/null
"$shards" run -d --name m250 --pull never --privileged -u 0 -m 2g -v "$work/x:/x" alpine:3.22 \
	sleep 86400 >/dev/null
"$shards" exec m250 sh /x/setup.sh x86_64 arm
pid=$("$shards" inspect --format '{{.State.Pid}}' m250)
i=0
while [ "$i" -lt "${LOAD:-0}" ]; do
	( while :; do :; done ) &
	spinners="$spinners $!"
	i=$((i + 1))
done
uptime
python3 -I "$here/host.py" "$rounds" \
	x86_64 "$pid" "$shards exec -i m250 sh /x/serve.sh x86_64" \
	arm "$pid" "$shards exec -i m250 sh /x/serve.sh arm"
uptime
