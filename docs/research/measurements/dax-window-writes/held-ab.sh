#!/bin/sh
# What shards' kernel patch 0009 changes of a guest's writes through a DAX window (D189, PM
# M301): M299's passes (measure.py) whose cold arms meet pages the host does not hold, with
# one build of shards, on each kernel given (one without the patch, whose guest reads no
# held record, and those with it), every arm on each kernel within each round, so that all
# meet the same load.
# Usage: held-ab.sh SHARDS INIT PLAIN_HOME DAX_HOME DATA_DIR LABEL=KERNEL... [-- IMAGE]
#   (each home holding IMAGE already; INIT the shards-init the build carries)
set -eu
shards=$1 init=$2 plain=$3 dax=$4 dir=$5
shift 5
kernels="" image=mirror.gcr.io/library/alpine
while [ $# -gt 0 ]; do
    case $1 in
    --) image=$2; break ;;
    *) kernels="$kernels --kernel $1" ;;
    esac
    shift
done
here=$(dirname "$0")
for pass in "--size 2048 --writes 5000" \
    "--size 2048 --writes 2000 --write-size 65536 --premap" \
    "--size 2048 --writes 500 --write-size 1048576"; do
    echo "== $pass"
    # shellcheck disable=SC2086
    SHARDS_INIT=$init python3 -I "$here/measure.py" "$shards" --plain "$plain" --dax "$dax" \
        --dir "$dir" --window 4096 --rounds 5 --image "$image" $kernels $pass
done
