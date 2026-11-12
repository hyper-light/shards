#!/bin/sh
# PM M529: builds the harness and runs it: the host's limits, a thread's costs, five bursts of
# 2,000 threads held parked, and a copy of itself in a job holding its process's commit to
# 64 MiB, which starts threads until Windows refuses one. On Windows (Git Bash), from this
# directory; needs cargo. Name a shards.exe to read its stack reserve and commit too.
set -eu
cargo build --release --locked
bin=${CARGO_TARGET_DIR:-target}/release/win-thread-limits.exe
"$bin" --burst 2000 --rounds 5 --job-limit 64 ${1:+--image "$1"}
