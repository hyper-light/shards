#!/bin/sh
# What shards' kernel patch 0009 changes of a guest's writes through a DAX window (D189, PM
# M301): M299's passes (measure.py) whose cold arms meet pages the host does not hold, with
# one build of shards, on kernel A (without the patch: its guest reads no held record) and
# kernel B (with it), every arm on each kernel within each round, so that both meet the
# same load.
# Usage: held-ab.sh SHARDS INIT KERNEL_A KERNEL_B PLAIN_HOME DAX_HOME DATA_DIR [IMAGE]
#   (each home holding IMAGE already; INIT the shards-init the build carries)
set -eu
shards=$1 init=$2 a=$3 b=$4 plain=$5 dax=$6 dir=$7 image=${8:-mirror.gcr.io/library/alpine}
here=$(dirname "$0")
for pass in "--size 2048 --writes 5000" \
    "--size 2048 --writes 2000 --write-size 65536 --premap" \
    "--size 2048 --writes 500 --write-size 1048576"; do
    echo "== $pass"
    # shellcheck disable=SC2086
    SHARDS_INIT=$init python3 -I "$here/measure.py" "$shards" --plain "$plain" --dax "$dax" \
        --dir "$dir" --window 4096 --rounds 5 --image "$image" --kernel "A=$a" --kernel "B=$b" $pass
done
