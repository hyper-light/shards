#!/bin/sh
# sustained-io's image, shards-sio:1, on a Mac for cells.py's npm and cargo cells: the
# context sustained-io/images.sh stages (the trees fetch.sh fetched into SIO, default
# ~/sio, less their .git, and the guest's scripts), cloned on APFS (cp -c) where images.sh
# hard-links, built from sustained-io's Dockerfile less its fio, which no cell here runs.
# The shards home is $SIO/home, its client an empty DOCKER_CONFIG.
# Usage: images-mac.sh SHARDS
set -eu
shards=$1
SIO=${SIO:-$HOME/sio}
sio_dir=$(cd "$(dirname "$0")/../sustained-io" && pwd)
export SHARDS_HOME="$SIO/home" DOCKER_CONFIG="$SIO/docker-config"
mkdir -p "$DOCKER_CONFIG"
ctx=$SIO/ctx
[ ! -e "$ctx" ] || rm -r "$ctx"
mkdir -p "$ctx/sio"
cp -c -R "$SIO/src/cli" "$ctx/cli"
cp -c -R "$SIO/src/rust-analyzer" "$ctx/rust-analyzer"
cp -c -R "$SIO/npm-cache" "$ctx/npm-cache"
rm -rf "$ctx/cli/.git" "$ctx/rust-analyzer/.git"
cp "$sio_dir/work.sh" "$ctx/sio/work"
cp "$sio_dir/init.sh" "$ctx/sio/init"
cp "$sio_dir/fio.ini" "$ctx/sio/fio.ini"
chmod 755 "$ctx/sio/work" "$ctx/sio/init"
grep -v '^COPY fio ' "$sio_dir/Dockerfile" > "$SIO/Dockerfile.mac"
"$shards" build -q -t shards-sio:1 -f "$SIO/Dockerfile.mac" "$ctx"
"$shards" images shards-sio:1
