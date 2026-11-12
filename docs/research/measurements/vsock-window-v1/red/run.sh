#!/usr/bin/env bash
# Runs vsock-window-v1's tests, as committed, against the code before them: the base
# (p/vsock-credit-v1, 1e448c7e) and ROOT's uncommitted start (root-start.patch on it), each
# with overlay.py's overlay of the tests. Expected: red, each for the reason its test names.
#
# Usage: run.sh REPO SCRATCH. REPO is a checkout of p/vsock-window-v1 (with the base's
# commit); SCRATCH, which is removed first, holds both trees and their cargo target.
set -uo pipefail
repo=$(cd "$1" && pwd)
scratch=$2
here=$repo/docs/research/measurements/vsock-window-v1
base=1e448c7ec150a9366deb036cca3687d65991d5c7
rm -rf "$scratch"
mkdir -p "$scratch/base" "$scratch/root"
for variant in base root; do
    git -C "$repo" archive "$base" | tar -x -C "$scratch/$variant" || exit 2
done
(cd "$scratch/root" && patch -p1 --quiet < "$here/root-start.patch") || exit 2
for variant in base root; do
    python3 -I "$here/red/overlay.py" "$repo" "$scratch/$variant" "$variant" || exit 2
done
export CARGO_TARGET_DIR=$scratch/target
vsock=devices::virtio::vsock
echo "== base $base + overlay"
(cd "$scratch/base" && cargo test -j4 -p shards-vmm --lib -- \
    "$vsock::muxer::tests::a_lone_connection_is_offered_the_most_window_and_reset_past_it")
echo "== base exit $?"
echo "== root: base + root-start.patch + overlay"
(cd "$scratch/root" && cargo test -j4 -p shards-vmm --lib -- \
    "$vsock::conn::tests::a_window_grows_only_once_its_guest_uses_it" \
    "$vsock::conn::tests::a_window_grows_no_further_than_the_guests_buffer" \
    "$vsock::conn::tests::a_window_never_takes_back_what_it_gave" \
    "$vsock::conn::tests::a_guest_whose_buffer_shrinks_is_owed_its_update" \
    "$vsock::muxer::tests::a_lone_connection_is_offered_the_most_window_and_reset_past_it" \
    "$vsock::muxer::tests::two_connections_share_the_lent_window" \
    "$vsock::muxer::tests::a_busy_device_keeps_its_window_bounds")
echo "== root exit $?"
