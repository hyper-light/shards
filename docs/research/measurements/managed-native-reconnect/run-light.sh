#!/bin/sh
# Explicit source/output/toolchain; no Cargo target or shared cache.
set -eu
source_dir=$1
output_dir=$2
toolchain=$3
test ! -e "$output_dir"
mkdir "$output_dir"
rustc "+$toolchain" -Vv > "$output_dir/rustc.txt"
uname -a > "$output_dir/uname.txt"
rustc "+$toolchain" --edition 2024 "$source_dir/abi/build.rs" -o "$output_dir/abi-build"
CARGO_MANIFEST_DIR="$source_dir/abi" OUT_DIR="$output_dir" "$output_dir/abi-build" > "$output_dir/abi-build.log"
OUT_DIR="$output_dir" rustc "+$toolchain" --edition 2024 --crate-name shards_abi --crate-type rlib "$source_dir/abi/src/lib.rs" -o "$output_dir/libshards_abi.rlib"
rustc "+$toolchain" --edition 2024 --test "$source_dir/pure.rs" --extern "shards_abi=$output_dir/libshards_abi.rlib" -o "$output_dir/pure-tests"
"$output_dir/pure-tests" --nocapture --test-threads=1 > "$output_dir/pure-tests.log" 2>&1
