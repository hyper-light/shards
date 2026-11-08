#!/bin/sh
# PM M488, D226: the learned pull and push, cold (a path never seen) and warm (the same
# home's second transfer over it), timed from the client's start to its exit.
#   learned.sh SHARDS SOURCE TARGET [ROUNDS [LAYERS [SIZE]]]
# SOURCE and TARGET are registries (host:port, plain HTTP, anonymous), TARGET another than
# SOURCE: a push to the registry it pulled from mounts its blobs instead. Each round makes
# two images of fresh random layers in SOURCE (mkimage.py), and in a fresh home pulls the
# first (cold) then the second (warm), then pushes the first to TARGET (cold) and the
# second (warm). Prints each transfer's seconds and the records the home learned.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
shards=$1 source=$2 target=$3
rounds=${4:-3} layers=${5:-16} size=${6:-16777216}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/config/docker" "$work/docker"
printf '{"insecure-registries":["%s","%s"]}\n' "$source" "$target" > "$work/config/docker/daemon.json"
export XDG_CONFIG_HOME="$work/config" DOCKER_CONFIG="$work/docker"
now() { python3 -c 'import time; print(time.monotonic())'; }
timed() {
  t0=$(now)
  "$@" >/dev/null
  t1=$(now)
  python3 -c "print(f'{$t1 - $t0:.2f}')"
}
uname -a; date -u; uptime
echo "# $rounds rounds, $layers layers of $size bytes, $source -> $target"
for r in $(seq 1 "$rounds"); do
  id=$(od -An -N4 -tx4 /dev/urandom | tr -d ' ')
  a=$(python3 -I "$here/mkimage.py" "http://$source" "m488/learned-$id" a --layers "$layers" --size "$size")
  b=$(python3 -I "$here/mkimage.py" "http://$source" "m488/learned-$id" b --layers "$layers" --size "$size")
  export SHARDS_HOME="$work/home-$r"
  pull_cold=$(timed "$shards" pull -q "$a")
  pull_warm=$(timed "$shards" pull -q "$b")
  "$shards" tag "$a" "$target/m488/learned-$id:a"
  "$shards" tag "$b" "$target/m488/learned-$id:b"
  push_cold=$(timed "$shards" push "$target/m488/learned-$id:a")
  push_warm=$(timed "$shards" push "$target/m488/learned-$id:b")
  echo "round $r: pull cold $pull_cold s, warm $pull_warm s; push cold $push_cold s, warm $push_warm s"
  for f in "$SHARDS_HOME"/images/links/*/*; do
    echo "  $(basename "$(dirname "$f")") $(basename "$f"): $(cat "$f")"
  done
  "$shards" stop daemon >/dev/null 2>&1 || true
done
uptime
