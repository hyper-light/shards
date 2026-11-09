#!/bin/sh
# How long past its patience an unserved connect's refusal comes (shards-api's
# conformance::OVERSHOOT): the client against a shards that never binds its socket, and the
# simulator against a guest that never listens there, N of each (default 300), patience
# conformance::UNSERVED_PATIENCE. Prints n, p50, p90, p99 and max of each.
set -eu
cd "$(dirname "$0")/../../../.."
echo "$(git rev-parse HEAD) $(uname -sr) $(date -u +%FT%TZ) load: $(uptime | sed 's/.*load average: //')"
SHARDS_API_OVERSHOOT_N="${N:-300}" cargo test --release -p shards-api --test sim -- --ignored --nocapture overshoot
