#!/bin/sh
# What a template's map-back of its DAX windows' heads costs (D189; PM M307: 5.4 ms on the
# Mac, 0.19 on Fedora, for nine windows): ROUNDS fresh homes under WORK, each given SEED's
# image store and guest kernel (a clone where the file system makes one, so that no run
# pulls; the first round's kernel kept for the rest where SEED has none), each saving
# a template of IMAGE with eight volumes, nine windows with its join share's, each as long
# as the daemon's default; from each home's daemon log, at info, the map-back in all and
# its parts: its heads taken from the guest at stage 2, mapped back from the snapshot's
# copy, and given back, with the copy's blocks mapped, the blocks made fresh and the runs
# of zero blocks given back. One line a round:
#   ROUND TOTAL_US TAKEN_US MAPPED_US GIVEN_US FROM_COPY FRESH ZERO_RUNS [INDEX_US COPY_US]
# Usage: mapback.sh SHARDS SEED WORK IMAGE ROUNDS   (SEED holds IMAGE)
set -eu
shards=$1 seed=$2 work=$3 image=$4 rounds=$5
cfg=$(mktemp -d)
trap 'rm -rf "$cfg"' EXIT
export DOCKER_CONFIG="$cfg" SHARDS_LOG=info
unset SHARDS_SHARE_WINDOW_MIB
vols=""
for v in a b c d e f g h; do
    mkdir -p "$work/vol-$v"
    vols="$vols -v $work/vol-$v:/$v"
done
for round in $(seq "$rounds"); do
    home="$work/home-$round"
    mkdir -p "$home"
    for part in images guest; do
        [ -d "$seed/$part" ] || continue
        cp -cR "$seed/$part" "$home/$part" 2>/dev/null || cp -a --reflink=auto "$seed/$part" "$home/$part"
    done
    export SHARDS_HOME="$home"
    # shellcheck disable=SC2086 # $vols is the eight -v options.
    "$shards" run --rm --pull never $vols "$image" true >/dev/null
    "$shards" stop daemon >/dev/null 2>&1 || true
    total=$(sed -n 's/.*heads mapped back from its snapshot in \([0-9]*\) us.*/\1/p' "$home/daemon.log" | tail -1)
    parts=$(sed -n 's/.*taken from its guest in \([0-9]*\) us, mapped back in \([0-9]*\) us (\([0-9]*\) blocks from the copy, \([0-9]*\) fresh, \([0-9]*\) runs of zeros given back[^)]*), given back in \([0-9]*\) us.*/\1 \2 \6 \3 \4 \5/p' "$home/daemon.log" | tail -1)
    split=$(sed -n 's/.*the copy.s index read in \([0-9]*\) us, its blocks mapped in \([0-9]*\) us.*/\1 \2/p' "$home/daemon.log" | tail -1)
    echo "$round ${total:-none} ${parts:-none} ${split:-}"
    # The guest kernel the first round downloaded, for the rounds after it.
    [ -d "$seed/guest" ] || cp -R "$home/guest" "$seed/guest"
    rm -rf "$home"
done
