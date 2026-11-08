#!/bin/sh
# D228 (docs/research/platform-measurements.md, PM M495): what a client's bind through its
# context costs its guest against the machine's own bind of the same files. Two homes on
# this host, the client's context over loopback QUIC; each round, in turns, the owner's run
# and the client's, each timing 2,000 opens of a 4 KiB read and a 64 MiB file read and
# written (the test guest's `share-cost`). Run from the repository:
#
#     docs/research/measurements/client-binds/run.sh [ROUNDS]
set -eu
SHARDS_COST_ROUNDS="${1:-5}" exec cargo test --profile test-release -p shards --test context \
    -- --ignored --exact what_a_clients_bind_costs --nocapture
