#!/bin/sh
# What a seccomp filter's layout costs the syscalls it checks (review 1.7, PM M107), on
# Linux. Usage: run.sh [ROUNDS [REV]].
set -eu
here=$(cd "$(dirname "$0")" && pwd)
exec cargo run -q --release --manifest-path "$here/Cargo.toml" -- "${1:-20}" "${2:-unknown}"
