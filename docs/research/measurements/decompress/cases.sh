#!/bin/sh
# Writes the cases decode.rs checks into DIR, with the xz and bzip2 tools.
set -eu
d=$1; mkdir -p "$d"; cd "$d"
head -c 3000000 /dev/urandom > rnd
i=0; while [ $i -lt 20000 ]; do echo "line $i of text, repeated words words words"; i=$((i+1)); done > txt
cat rnd txt > in; : > empty
for c in none crc32 crc64 sha256; do xz -c -C $c in > $c.xz; done
xz -c --x86 --lzma2 in > x86.xz; xz -c --arm64 --lzma2 in > arm64.xz
xz -c --delta=dist=4 --lzma2 in > delta.xz; xz -c -T4 --block-size=500000 in > blocks.xz
xz -c empty > empty.xz; (xz -c rnd; xz -c txt) > multi.xz
(xz -c rnd; head -c 8 /dev/zero; xz -c txt) > padded.xz
bzip2 -c in > one.bz2; (bzip2 -c rnd; bzip2 -c txt) > multi.bz2; bzip2 -c empty > empty.bz2
python3 - <<'PY'
for src, dst in [("crc64.xz", "corrupt.xz"), ("one.bz2", "corrupt.bz2")]:
    b = bytearray(open(src, "rb").read()); b[len(b) // 2] ^= 0x40; open(dst, "wb").write(b)
for src, dst in [("crc64.xz", "truncated.xz"), ("one.bz2", "truncated.bz2")]:
    c = open(src, "rb").read(); open(dst, "wb").write(c[: len(c) - 100])
PY
