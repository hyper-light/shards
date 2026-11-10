#!/usr/bin/env bash
# Run from the repository root on a Linux host; the fixture uses native sockets.
set -uo pipefail
proxy_phase=${1:?name this run, for example baseline or changed}
proxy_evidence=${PROXY_EVIDENCE_DIR:-docs/research/measurements/proxy-capacity/results}
mkdir -p "$proxy_evidence"
{
    printf 'phase=%s\n' "$proxy_phase"
    git rev-parse HEAD
    sha256sum crates/shards/src/build/proxy/server.rs crates/shards/src/build/proxy/tests.rs
    uname -a
    rustc -Vv
    getconf CLK_TCK
    printf 'samples=%s sample_ms=%s profile=release\n' "${SHARDS_PROXY_SAMPLES:-20}" "${SHARDS_PROXY_SAMPLE_MS:-100}"
} > "$proxy_evidence/$proxy_phase.environment"
SHARDS_HELPERS=skip cargo test -j 4 --release -p shards --bin shards \
    build::proxy::server::tests::proxy_capacity_wait_costs \
    -- --exact --ignored --nocapture --test-threads=1 \
    2>&1 | tee "$proxy_evidence/$proxy_phase.log"
proxy_status=${PIPESTATUS[0]}
printf '%s\n' "$proxy_status" > "$proxy_evidence/$proxy_phase.exit"
exit "$proxy_status"
