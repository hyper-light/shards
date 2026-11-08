#!/bin/sh
# What an image's entries cost a build in memory, and what real images hold (PM M47).
#   run.sh [N...]: peak RSS for layers of N empty files (default 100000 400000 1000000).
set -eu
here=$(cd "$(dirname "$0")" && pwd)
cargo build --release --manifest-path "$here/Cargo.toml" >&2
for n in ${@:-100000 400000 1000000}; do "$here/target/release/image-budgets" "$n"; done
