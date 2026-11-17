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
#
# OUT keeps each run's samples, stdout, stderr and envelope, each arm's sections, and the
# summary of the binaries' mappings (maps.py).
set -euo pipefail
out=${1:?an output directory}
mode=${2:?windows or ab}
here=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$out"
out=$(cd "$out" && pwd)

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

case "$mode" in
windows)
  knob=/sys/kernel/debug/fault_around_bytes
  host=$(sudo -n cat "$knob")
  page=$(getconf PAGESIZE)
  restore() { sudo -n sh -c "echo $host > $knob"; }
  trap restore EXIT
  for bytes in "$host" "$page"; do
    sudo -n sh -c "echo $bytes > $knob"
    compare "windows-$bytes"
  done
  python3 -I "$here/maps.py" "$out"/windows-*.stderr | tee "$out/maps.txt"
  ;;
ab)
  rounds=${3:?rounds}
  # Checked here: a failure inside the substitutions below would not stop the script.
  : "${B_RUSTFLAGS:?B_RUSTFLAGS names the flags of arm B}"
  musl=CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS
  flags() { if [ "$1" = b ]; then echo "$B_RUSTFLAGS"; fi; }
  for arm in a b; do
    env CARGO_TARGET_DIR="$out/target-$arm" "$musl=$(flags "$arm")" \
      cargo bench --locked -p shards --bench firecracker --no-run >"$out/build-$arm.log" 2>&1
    vm="$out/target-$arm/helpers/x86_64-unknown-linux-musl/release/shards-vm"
    readelf -lSdW "$vm" >"$out/readelf-$arm.txt"
    size -A "$vm" >"$out/size-$arm.txt"
  done
  for round in $(seq "$rounds"); do
    if [ $((round % 2)) = 1 ]; then order="a b"; else order="b a"; fi
    for arm in $order; do
      compare "$arm-$round" CARGO_TARGET_DIR="$out/target-$arm" "$musl=$(flags "$arm")"
    done
  done
  python3 -I "$here/maps.py" "$out"/[ab]-*.stderr | tee "$out/maps.txt"
  ;;
*)
  echo "run.sh OUT windows | run.sh OUT ab ROUNDS" >&2
  exit 2
  ;;
esac
