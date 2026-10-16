#!/bin/sh
# M241 (docs/research/platform-measurements.md): another CPU's programs in a microVM, under
# qemu-user through the guest kernel's binfmt_misc (D179), each run measured from the host.
# On an arm64 host it runs x86_64 programs, on an x86_64 host aarch64 ones: Docker's builds of
# qemu-user for BuildKit (tonistiigi/binfmt, pinned) registered as Docker's binfmt tool
# registers them (flags CFP), against Alpine 3.22.6's minirootfs of that CPU (pinned), held in
# the guest's memory. host.py runs ROUNDS pairs of each workload, natively then emulated, and
# prints n, p50, p90, p99 and max of their ratios.
#
# With DIND, the name of a Docker-in-Docker container of Docker Desktop for Mac, the same
# workloads run in a container of Docker Desktop's VM too, under the emulators Docker Desktop
# registered, each round's runs interleaved with shards'. Its VM is the one Virtualization
# framework process (DOCKER_VM_PID names it where there are more). With LOAD, as many busy
# loops run beside it.
#
#   run.sh SHARDS ROUNDS [DIND]
set -eu
shards=$1
rounds=$2
dind=${3:-}
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
tag=m241-$$
spinners=""
cleanup() {
	[ -n "$spinners" ] && kill $spinners
	"$shards" rm -f m241 >/dev/null 2>&1 || true
	"$shards" stop daemon >/dev/null 2>&1 || true
	if [ -n "$dind" ]; then
		docker exec "$dind" docker rm -f "$tag" >/dev/null 2>&1 || true
		docker exec "$dind" rm -r "/$tag" 2>/dev/null || true
	fi
	rm -r -- "$work"
}
trap cleanup EXIT
case $(uname -m) in
arm64 | aarch64)
	other=x86_64
	rootfs_sha=27694aaa55fd7a9e3ef596e0ad4eb66802308bb20172b17030cd5f4d8ae9bac2
	;;
x86_64 | amd64)
	other=aarch64
	rootfs_sha=821565fa8f3953eefd12497b166b4b50add2f7c57fb312e75862f5867e06fefe
	;;
*)
	echo "run.sh: a host of $(uname -m)" >&2
	exit 1
	;;
esac
binfmt=tonistiigi/binfmt@sha256:400a4873b838d1b89194d982c45e5fb3cda4593fbfd7e08a02e76b03b21166f0
mkdir -p "$work/home" "$work/x/root" "$work/dl"
export SHARDS_HOME="$work/home"
"$shards" pull -q "$binfmt" >/dev/null
"$shards" create --name binfmt "$binfmt" >/dev/null
"$shards" cp "binfmt:/usr/bin/qemu-$other" "$work/x/qemu"
"$shards" rm binfmt >/dev/null
curl -fsSL --retry 5 --retry-all-errors -o "$work/dl/rootfs.tar.gz" \
	"https://dl-cdn.alpinelinux.org/alpine/v3.22/releases/$other/alpine-minirootfs-3.22.6-$other.tar.gz"
echo "$rootfs_sha  $work/dl/rootfs.tar.gz" | shasum -a 256 -c - >/dev/null
tar -xzf "$work/dl/rootfs.tar.gz" -C "$work/x/root"
cp "$here/setup.sh" "$here/serve.sh" "$work/x/"
"$shards" pull -q alpine:3.22 >/dev/null
"$shards" run -d --name m241 --pull never --privileged -u 0 -m 2g -v "$work/x:/x" alpine:3.22 \
	sleep 86400 >/dev/null
"$shards" exec m241 sh /x/setup.sh "$other" register
set -- shards "$("$shards" inspect --format '{{.State.Pid}}' m241)" "$shards exec -i m241 sh /x/serve.sh"
if [ -n "$dind" ]; then
	vm=${DOCKER_VM_PID:-$(pgrep -f 'Virtualization.framework/.*/com.apple.Virtualization.VirtualMachine$')}
	case $vm in
	*[!0-9]* | "")
		echo "run.sh: Docker Desktop's VM is not one process ($vm): DOCKER_VM_PID names it" >&2
		exit 1
		;;
	esac
	docker exec "$dind" mkdir "/$tag"
	docker cp "$work/x" "$dind:/$tag/x" >/dev/null
	docker exec "$dind" docker run -d --name "$tag" --privileged -u 0 -m 2g \
		--tmpfs /tmp:rw,exec,size=1g -v "/$tag/x:/x" alpine:3.22 sleep 86400 >/dev/null
	docker exec "$dind" docker exec "$tag" sh /x/setup.sh "$other"
	set -- "$@" docker "$vm" "docker exec -i $dind docker exec -i $tag sh /x/serve.sh"
fi
i=0
while [ "$i" -lt "${LOAD:-0}" ]; do
	( while :; do :; done ) &
	spinners="$spinners $!"
	i=$((i + 1))
done
uptime
python3 -I "$here/host.py" "$rounds" "$@"
uptime
