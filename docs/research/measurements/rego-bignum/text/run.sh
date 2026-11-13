#!/bin/sh
# PM M525's cases, each its own process: the fuzzer's number through each builtin that
# answers all of its integer's digits, then floor of 1eD for D up to Go's last finite
# power of ten (646456992), each time and peak RSS. Run from the repository's root:
#   sh docs/research/measurements/rego-bignum/text/run.sh [cargo +toolchain]
set -eu
cargo=${1:-cargo}
dir=docs/research/measurements/rego-bignum/text
$cargo build --release --manifest-path "$dir/Cargo.toml" >&2
bin=${CARGO_TARGET_DIR:-$dir/target}/release/rego-bignum-text
host="$(uname -sm), $(sysctl -n machdep.cpu.brand_string 2>/dev/null || grep -m1 'model name' /proc/cpuinfo | cut -d: -f2), $(git rev-parse --short HEAD)"
echo "host: $host"
fz=80000000000e00029174680
for c in "abs [$fz]" "floor [$fz]" "ceil [-$fz]" "round [$fz]" "format_int [$fz,10]" "format_int [$fz,16]" "format_int [$fz,2]"; do
	set -- $c
	"$bin" "$1" "$2"
done
for d in 100000 1000000 3000000 10000000 29174690 100000000 200000000 400000000 646456992; do
	"$bin" floor "[1e$d]"
done
# At the bound and a byte under it.
"$bin" floor "[$fz]" bound
b=$("$bin" floor "[$fz]" bound | sed -n 's/.*bound \([0-9]*\) B.*/\1/p')
"$bin" floor "[$fz]" "$((b - 1))"
