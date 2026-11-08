#!/bin/sh
# PM M488: push.py's sweeps from one host. URL is a registry (a local registry:2, or
# https://ttl.sh); WAN=1 sizes the cases for a home uplink.
#   sweep.sh URL [WAN]
# Uploads at once (1 to 16) for large blobs and for small ones, then large blobs at four
# at once sent whole and in chunks of 1, 4 and 8 MiB.
set -eu
here=$(dirname "$0")
url=$1
repo=agclose-push-$(od -An -N4 -tx4 /dev/urandom | tr -d ' ')
uname -a; date -u; uptime
if [ "${2:-}" = 1 ]; then
  n=8; size=8388608; levels=1,2,4,8
else
  n=16; size=16777216; levels=1,2,4,8,16
fi
python3 "$here/push.py" "$url" "$repo" --blobs $n --size $size --concurrency $levels --rounds 3
python3 "$here/push.py" "$url" "$repo" --blobs 32 --size 262144 --concurrency 1,2,4,8,16 --rounds 3
for chunk in 0 1048576 4194304 8388608; do
  python3 "$here/push.py" "$url" "$repo" --blobs $n --size $size --concurrency 4 --rounds 3 --chunk $chunk
done
uptime
