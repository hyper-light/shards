#!/bin/sh
# What a request costs through a slot, and what a slot costs its daemon (PM M348).
# Usage: run.sh [DIR [N]]: DIR for the slots' home, made afresh; N counted rounds of each.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
cargo build --release --manifest-path "$here/Cargo.toml" >&2
exec "$here/target/release/request-slots" "${1:-${TMPDIR:-/tmp}/request-slots}" "${2:-2000}"
