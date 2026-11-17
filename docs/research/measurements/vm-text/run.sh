#!/bin/bash
# What a VM process's boot keeps of its own binary, against Firecracker's (D36, Envelope row
# 11). From the repository's root, on a Linux KVM host whose root it may use (`sudo -n`), as
# CI's comparison runner is (.github/workflows/vm-text.yml):
#
#   run.sh OUT windows
#     The Firecracker comparison as CI runs it (benches/firecracker.rs, --runs 30) with the
#     host's fault-around at its own size, then at one page (Linux mm/memory.c
#     `fault_around_bytes`, in debugfs; put back after): each VMM's file-backed resident
#     bytes at boot and restore, split into the pages it touches and the cached pages a read
#     fault maps around them.
#   run.sh OUT ab ROUNDS REF
#     The comparison of shards as this checkout builds it (A) and as git revision REF builds
#     it (B, fetched from origin), ROUNDS rounds, their order alternating (A B, then B A),
#     each round's envelope.
#   run.sh OUT order ROUNDS
#     `ab` with B this checkout with the order of its boot kept for the VM process
#     (crates/vm-process/order/<arch>.txt, which vm-process/build.rs links by, LLD's
#     --symbol-ordering-file): A's comparison with fault-around at one page, its boot's pages
#     of shards-vm (the bench's pages.tsv) made that order by order.py.
#
# Each arm is a tree of its own (B a git worktree), built in a target directory of its own.
# An arm is never flags: the nested build of the VM process takes none from its parent's
# environment, as Cargo sets CARGO_ENCODED_RUSTFLAGS for a build script, which outranks every
# other source of flags (Cargo's configuration reference, `build.rustflags`; vm-text's run
# 44076339609 built its flags' arm as the other).
#
# OUT keeps each run's samples (pages.tsv among them), stdout, stderr and envelope, each
# arm's sections, and the summary of the binaries' mappings (maps.py).
set -euo pipefail
out=${1:?an output directory}
mode=${2:?windows, ab or order}
here=$(cd "$(dirname "$0")" && pwd)
root=$(pwd)
mkdir -p "$out"
out=$(cd "$out" && pwd)
knob=/sys/kernel/debug/fault_around_bytes
host=""

tree() { if [ "$1" = b ]; then echo "$out/tree-b"; else echo "$root"; fi; }
vm() { echo "$out/target-$1/helpers/$(uname -m)-unknown-linux-musl/release/shards-vm"; }

# One run of the comparison as LABEL, by arm ARM's tree and target directory; its status kept.
compare() {
  local label=$1 arm=$2 status=0
  (cd "$(tree "$arm")" && env CARGO_TARGET_DIR="$out/target-$arm" SHARDS_BENCH_SAMPLES="$out/$label" \
    cargo bench --locked -p shards --bench firecracker -- --runs 30) \
    >"$out/$label.stdout" 2>"$out/$label.stderr" || status=$?
  echo "$status" >"$out/$label.status"
  "$root/scripts/bench-envelope" "$out/$label" >"$out/$label.envelope" 2>&1 || true
}

# The host's fault-around: noted before the first change, and put back as this ends, with
# arm B's tree removed.
fault_around() {
  if [ -z "$host" ]; then host=$(sudo -n cat "$knob"); fi
  sudo -n sh -c "echo $1 > $knob"
}
restore() { if [ -n "$host" ]; then sudo -n sh -c "echo $host > $knob"; fi; }
finish() {
  restore
  if [ -d "$out/tree-b" ]; then git -C "$root" worktree remove --force "$out/tree-b" || true; fi
}
trap finish EXIT

# Arm B's tree, at revision REV.
tree_b() { git -C "$root" worktree add --detach "$out/tree-b" "$1" >"$out/tree-b.log" 2>&1; }

# Arm ARM built, its VM process's sections kept.
build() {
  (cd "$(tree "$1")" && env CARGO_TARGET_DIR="$out/target-$1" \
    cargo bench --locked -p shards --bench firecracker --no-run) >"$out/build-$1.log" 2>&1
  readelf -lSdW "$(vm "$1")" >"$out/readelf-$1.txt"
  size -A "$(vm "$1")" >"$out/size-$1.txt"
}

# ROUNDS rounds of both arms, their order alternating, and their mappings summed.
rounds() {
  for round in $(seq "$1"); do
    if [ $((round % 2)) = 1 ]; then order="a b"; else order="b a"; fi
    for arm in $order; do
      compare "$arm-$round" "$arm"
    done
  done
  python3 -I "$here/maps.py" "$out"/[ab]-*.stderr | tee "$out/maps.txt"
}

case "$mode" in
windows)
  page=$(getconf PAGESIZE)
  for bytes in "$(sudo -n cat "$knob")" "$page"; do
    fault_around "$bytes"
    compare "windows-$bytes" a
  done
  restore
  python3 -I "$here/maps.py" "$out"/windows-*.stderr | tee "$out/maps.txt"
  ;;
ab)
  rounds=${3:?rounds}
  git -C "$root" fetch --quiet --depth 1 origin "${4:?a git revision for arm B}"
  tree_b FETCH_HEAD
  build a
  build b
  rounds "$rounds"
  ;;
order)
  rounds=${3:?rounds}
  build a
  fault_around "$(getconf PAGESIZE)"
  compare touched a
  restore
  python3 -I "$here/order.py" "$(vm a)" "$out/touched/pages.tsv" >"$out/order.txt"
  tree_b HEAD
  mkdir -p "$out/tree-b/crates/vm-process/order"
  cp "$out/order.txt" "$out/tree-b/crates/vm-process/order/$(uname -m).txt"
  build b
  rounds "$rounds"
  ;;
*)
  echo "run.sh OUT windows | run.sh OUT ab ROUNDS REF | run.sh OUT order ROUNDS" >&2
  exit 2
  ;;
esac
