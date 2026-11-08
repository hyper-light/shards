#!/bin/bash
# segments.sh SHARDS OUT FILE...: prints each FILE a line at a time, one write a line, in
# a microVM (alpine's busybox awk), into shards' log store (--log-opt max-size=20m), and
# copies the first segment shards wrote of it, its records as the guest's output came, to
# OUT/NAME.seg. A home of its own, in OUT, whose daemon it stops at the end.
set -euo pipefail
shards=$1; out=$2; shift 2
export SHARDS_HOME="$out/home"
mkdir -p "$SHARDS_HOME"
for f in "$@"; do
  dir=$(cd "$(dirname "$f")" && pwd); name=$(basename "$f")
  id=$("$shards" run -d -q --log-opt max-size=20m --log-opt max-file=100 -v "$dir:/c:ro" \
    alpine:3.22 awk '{ print; fflush() }' "/c/$name")
  "$shards" wait "$id" > /dev/null
  cp "$SHARDS_HOME/containers/$id/log" "$out/${name%.log}.seg"
  "$shards" rm "$id" > /dev/null
done
"$shards" stop daemon
