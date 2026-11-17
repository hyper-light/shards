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
#   run.sh OUT ab ROUNDS
#     The comparison of shards as main builds it (A) and with $B_RUSTFLAGS for its musl
#     helpers (B: CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS, which the nested helper
#     build inherits, crates/shards/build.rs), each built in a target directory of its own,
#     ROUNDS rounds, their order alternating (A B, then B A), each round's envelope.
#   run.sh OUT order ROUNDS
#     `ab` with B the VM process linked in the order of the functions A's boot touched: A's
#     comparison with fault-around at one page, its boot's pages of shards-vm (pages.tsv)
#     made an ordering file (order.py), and B linked by it (LLD --symbol-ordering-file).
#
# OUT keeps each run's samples (pages.tsv among them), stdout, stderr and envelope, each
# arm's sections, and the summary of the binaries' mappings (maps.py).
set -euo pipefail
out=${1:?an output directory}
mode=${2:?windows, ab or order}
here=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$out"
out=$(cd "$out" && pwd)
musl=CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS
knob=/sys/kernel/debug/fault_around_bytes
host=""

# One run of the comparison as LABEL, VAR=VALUE settings before it; its status kept.
compare() {
  local label=$1
  shift
  local status=0
  env "$@" SHARDS_BENCH_SAMPLES="$out/$label" \
    cargo bench --locked -p shards --bench firecracker -- --runs 30 \
    >"$out/$label.stdout" 2>"$out/$label.stderr" || status=$?
  echo "$status" >"$out/$label.status"
  scripts/bench-envelope "$out/$label" >"$out/$label.envelope" 2>&1 || true
}

# The host's fault-around: noted before the first change, and put back as this ends.
fault_around() {
  if [ -z "$host" ]; then host=$(sudo -n cat "$knob"); fi
  sudo -n sh -c "echo $1 > $knob"
}
restore() { if [ -n "$host" ]; then sudo -n sh -c "echo $host > $knob"; fi; }
trap restore EXIT

flags() { if [ "$1" = b ]; then echo "$B_RUSTFLAGS"; fi; }
vm() { echo "$out/target-$1/helpers/x86_64-unknown-linux-musl/release/shards-vm"; }

# Arm ARM built in its own target directory, its VM process's sections kept.
build() {
  env CARGO_TARGET_DIR="$out/target-$1" "$musl=$(flags "$1")" \
    cargo bench --locked -p shards --bench firecracker --no-run >"$out/build-$1.log" 2>&1
  readelf -lSdW "$(vm "$1")" >"$out/readelf-$1.txt"
  size -A "$(vm "$1")" >"$out/size-$1.txt"
}

# ROUNDS rounds of both arms, their order alternating, and their mappings summed.
rounds() {
  for round in $(seq "$1"); do
    if [ $((round % 2)) = 1 ]; then order="a b"; else order="b a"; fi
    for arm in $order; do
      compare "$arm-$round" CARGO_TARGET_DIR="$out/target-$arm" "$musl=$(flags "$arm")"
    done
  done
  python3 -I "$here/maps.py" "$out"/[ab]-*.stderr | tee "$out/maps.txt"
}

case "$mode" in
windows)
  page=$(getconf PAGESIZE)
  for bytes in "$(sudo -n cat "$knob")" "$page"; do
    fault_around "$bytes"
    compare "windows-$bytes"
  done
  restore
  python3 -I "$here/maps.py" "$out"/windows-*.stderr | tee "$out/maps.txt"
  ;;
ab)
  # Checked here: a failure inside the substitutions below would not stop the script.
  : "${B_RUSTFLAGS:?B_RUSTFLAGS names the flags of arm B}"
  build a
  build b
  rounds "${3:?rounds}"
  ;;
order)
  rounds=${3:?rounds}
  build a
  fault_around "$(getconf PAGESIZE)"
  compare touched CARGO_TARGET_DIR="$out/target-a" "$musl="
  restore
  python3 -I "$here/order.py" "$(vm a)" "$out/touched/pages.tsv" >"$out/order.txt"
  B_RUSTFLAGS="-C link-arg=--symbol-ordering-file=$out/order.txt -C link-arg=--no-warn-symbol-ordering"
  build b
  rounds "$rounds"
  ;;
*)
  echo "run.sh OUT windows | run.sh OUT ab ROUNDS | run.sh OUT order ROUNDS" >&2
  exit 2
  ;;
esac
