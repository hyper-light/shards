#!/bin/sh
# What publishing a container's record costs at each level of durability (PM M46).
# Usage: run.sh [DIR [N]]: DIR on the filesystem to measure, N publishes per level.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
cargo build --release --manifest-path "$here/Cargo.toml" >&2
exec "$here/target/release/record-sync" "${1:-${TMPDIR:-/tmp}/record-sync}" "${2:-1000}"
