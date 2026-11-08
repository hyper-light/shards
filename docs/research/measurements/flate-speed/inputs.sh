#!/bin/bash
# The inputs PM M256 gzips, into DIR: zeros.bin, 256 MiB written as the build test's
# `testguest big` writes a file (each 4 KiB page a 1 then zeros); repo.tar, this repository
# at HEAD (git archive: text, mostly); bins.tar, the release examples' binaries.
# Usage: inputs.sh DIR (from inside the repository)
set -eu
dir=$1
mkdir -p "$dir"
python3 -c '
import sys
chunk = bytearray(1 << 20)
for i in range(0, len(chunk), 4096):
    chunk[i] = 1
out = sys.stdout.buffer
for _ in range(256):
    out.write(chunk)
' > "$dir/zeros.bin"
git archive --format=tar HEAD > "$dir/repo.tar"
tar cf "$dir/bins.tar" -C target/release/examples .
