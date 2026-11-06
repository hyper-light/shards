#!/bin/sh
# The waits shards-api's bounds hold, measured (PM M463): this file's tests' own (the
# simulator and stand-ins for shards, tests/sim.rs `the_waits_of_these_tests`), and real
# guests' (tests/real.rs `the_waits_of_real_guests`: SHARDS_API_REAL=1 with
# SHARDS_API_KERNEL, SHARDS_API_INIT and SHARDS_BIN as tests/real.rs says). N of each
# (SHARDS_API_WAITS_N, default 40); each printed as n, p50, p90, p99 and max. On macOS, run it
# as a launchd one-shot, as kiln's headroom guard pauses what it runs.
set -eu
cd "$(dirname "$0")/../../../.."
echo "$(git rev-parse HEAD) $(uname -srm) $(date -u +%FT%TZ) load: $(uptime | sed 's/.*load average[s]*: //')"
cargo test --release -p shards-api --test sim -- --ignored --nocapture the_waits_of_these_tests
cargo test --release -p shards-api --test real -- --ignored --nocapture the_waits_of_real_guests
echo "done $(date -u +%FT%TZ) load: $(uptime | sed 's/.*load average[s]*: //')"
