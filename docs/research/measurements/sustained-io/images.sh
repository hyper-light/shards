#!/bin/sh
# The sustained-I/O benchmark's image, shards-sio:1, built by shards from Dockerfile over
# what fetch.sh fetched into SIO (default ~/sio), and the same bytes for every system:
# loaded into Docker, and exported as the ext4 root ($SIO/rootfs.ext4) that Firecracker,
# Cloud Hypervisor and shards' VMM boot, its environment written to /sio/env for init.sh.
# The shards home is $SIO/home. Usage: images.sh SHARDS
set -eu
shards=$1
SIO=${SIO:-$HOME/sio}
here=$(cd "$(dirname "$0")" && pwd)
export SHARDS_HOME="$SIO/home" DOCKER_CONFIG="$SIO/docker-config"
mkdir -p "$DOCKER_CONFIG"

# The context: the fetched trees as hard links, less their .git, and the guest's scripts.
ctx=$SIO/ctx
[ ! -e "$ctx" ] || rm -r "$ctx"
mkdir -p "$ctx/sio"
for tree in src/cli src/rust-analyzer npm-cache; do cp -al "$SIO/$tree" "$ctx/"; done
rm -r "$ctx/cli/.git" "$ctx/rust-analyzer/.git"
cp "$SIO/bin/fio" "$ctx/fio"
cp "$here/work.sh" "$ctx/sio/work"
cp "$here/init.sh" "$ctx/sio/init"
cp "$here/fio.ini" "$ctx/sio/fio.ini"
chmod 755 "$ctx/sio/work" "$ctx/sio/init"
"$shards" build -q -t shards-sio:1 -f "$here/Dockerfile" "$ctx"

# Docker's copy.
"$shards" save -o "$SIO/image.tar" shards-sio:1
docker load -q -i "$SIO/image.tar"

# The ext4 root: the image's filesystem as a container's export has it, and its
# environment, as Docker gives its processes, for init.sh to read. Unpacked and made in a
# user namespace of this user's (its root this user, its other users this user's
# subordinate IDs, /etc/subuid), where each file keeps its owner as mke2fs reads it:
# Fedora's mke2fs reads no tarball (built --without-libarchive).
"$shards" create --name sio-export shards-sio:1 true >/dev/null
"$shards" export -o "$SIO/rootfs.tar" sio-export
"$shards" rm sio-export >/dev/null
mkdir -p "$SIO/env"
"$shards" image inspect --format '{{range .Config.Env}}{{println .}}{{end}}' shards-sio:1 |
    sed -e '/^$/d' -e "s/'/'\\\\''/g" -e "s/^/export '/" -e "s/\$/'/" >"$SIO/env/env"
rm -f "$SIO/rootfs.ext4"
truncate -s 24G "$SIO/rootfs.ext4"
unshare --map-root-user --map-auto sh -c '
    set -eu
    [ ! -e "$1/rootfs" ] || rm -r "$1/rootfs"
    mkdir "$1/rootfs"
    tar -xpf "$1/rootfs.tar" --numeric-owner -C "$1/rootfs"
    cp "$1/env/env" "$1/rootfs/sio/env"
    mke2fs -q -t ext4 -L sio -d "$1/rootfs" "$1/rootfs.ext4"
    rm -r "$1/rootfs"' sh "$SIO"

"$shards" image inspect --format '{{.Id}}' shards-sio:1
"$shards" run --rm shards-sio:1 sh -c 'node --version; npm --version; rustc --version; fio --version'
