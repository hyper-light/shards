#!/bin/bash
# Dry-run applies the renderer's patches, as scripts/gpu-libs applies them, to each pinned source.
set -u
src=$1; work=$(mktemp -d); cd "$work" || exit 2
check() { # name url sha flavor
  for i in 1 2 3 4 5; do curl -fsSL --retry 3 -o v.tgz "$2" && break; sleep 15; done
  got=$(sha256sum v.tgz | cut -d' ' -f1); [ "$got" = "$3" ] || { echo "sha mismatch $1 $got"; return 1; }
  rm -rf "$1"; tar xzf v.tgz
  for p in physical-$4 physical-transport-$4; do
    patch -p1 -s -d "$1" < "$src/resources/gpu/$p.patch" && echo "applied $p to $1" || { echo "FAILED $p on $1"; return 1; }
  done
  grep -rn 'DEVICE_FILTER_MAX_COUNT\|UUID_MAX_COUNT' "$1/src" | head -12
}
check virglrenderer-1.3.0 https://gitlab.freedesktop.org/virgl/virglrenderer/-/archive/1.3.0/virglrenderer-1.3.0.tar.gz 065bc56e89e6f631f96101cd62eba0748e48eb888b434edc86e89d05395e76f3 linux
check virglrenderer-0.10.4e-krunkit https://gitlab.freedesktop.org/slp/virglrenderer/-/archive/0.10.4e-krunkit/virglrenderer-0.10.4e-krunkit.tar.gz 09d000623fbdb966cb604eb48c962a0815e8142383e6066d6494809335b76dbb krunkit
cd /; rm -rf "$work"
