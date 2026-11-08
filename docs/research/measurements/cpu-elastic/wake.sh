#!/bin/sh
# What waking another vCPU costs a guest, and what a shell's process costs (D191): wake.c,
# built static for the host's architecture by zig (scripts/zig-cc), run from a read-only
# volume in a microVM of each build, then 200 /bin/true from a shell, timed on the guest's
# clock (/proc/uptime): a build without a limit, and privileged, and each with --cpus 1,
# where the probe stops at its first CPU past 0, which the run has not.
#     wake.sh OLD NEW OLD_HOME NEW_HOME [IMAGE]
# Each home holds IMAGE (alpine), which is not pulled again; DOCKER_CONFIG as the caller sets.
set -eu
old=$1 new=$2 old_home=$3 new_home=$4 image=${5:-alpine}
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../../../.." && pwd)
work=$(mktemp -d)
case $(uname -m) in arm64 | aarch64) arch=aarch64 ;; *) arch=x86_64 ;; esac
"$repo/scripts/zig-cc" -target "$arch-linux-musl" -static -O2 -o "$work/wake" "$here/wake.c" -lpthread
loop='t0=$(cut -d" " -f1 /proc/uptime); i=0; while [ $i -lt 200 ]; do /bin/true; i=$((i+1)); done; t1=$(cut -d" " -f1 /proc/uptime); echo "shell: 200 /bin/true from $t0 to $t1 s"'
for arm in "new" "new --privileged" "old --cpus 1" "new --cpus 1"; do
    set -- $arm
    build=$1
    shift
    if [ "$build" = old ]; then binary=$old home=$old_home; else binary=$new home=$new_home; fi
    echo "== $build $*"
    SHARDS_HOME=$home "$binary" run --rm "$@" -v "$work:/probe:ro" "$image" sh -c "/probe/wake 2000 0 1; $loop"
done
rm -f "$work/wake" && rmdir "$work"
