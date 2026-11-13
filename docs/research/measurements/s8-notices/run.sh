#!/bin/bash
# How long a host's edit takes to wake an inotify watch in a shards guest (D235): tests/plug.rs
# `notices_latency`, built as releases and measurements are (`--profile release`), on this
# host's hypervisor. Says the host, its OS and the revision, then the test's own lines: n,
# p50, p90, p99 and max in milliseconds, and each sample. On macOS run it as a user's
# program runs (docs/research/measurements/macos-sched/launchd-run.sh), not under an agent
# session's QoS clamp. Usage: run.sh [SAMPLES], from the repository's root.
set -u
samples=${1:-200}
echo "== $(uname -srm) $(hostname) $(git rev-parse --short=9 HEAD) $(date -u +%FT%TZ)"
SHARDS_NOTICE_SAMPLES=$samples cargo test --profile release -p shards --test plug -- \
  notices_latency --exact --ignored --nocapture 2>&1 | grep -E "^notices latency|^samples ms|^test result|panicked"
