#!/bin/sh
# PM M430's harness (D214): a disk plugged into a running microVM, read, written and
# unplugged ROUNDS times, and boots with and without a PCI bus of slots alternated BOOTS
# times each, in tests/hotplug.rs; its PLUG-STATS lines (n, p50, p90, p99, max in µs) and
# the host it ran on, to OUT. Run from the repository, on a host that runs VMs: on macOS
# through docs/research/measurements/macos-sched/launchd-run.sh for a user's scheduling.
set -eu
ROUNDS=${ROUNDS:-100}
BOOTS=${BOOTS:-30}
OUT=${OUT:-docs/research/measurements/pci-hotplug/$(uname -s)-$(uname -m).txt}
{
    echo "host: $(uname -a)"
    echo "revision: $(git rev-parse HEAD)"
    echo "rounds: $ROUNDS, boots: $BOOTS each"
    SHARDS_REQUIRE_VMS=1 SHARDS_HELPERS=skip SHARDS_PLUG_ROUNDS=$ROUNDS SHARDS_PLUG_BOOTS=$BOOTS \
        cargo test -p shards --profile test-release --test hotplug -- --nocapture 2>&1 |
        grep -E "PLUG-STATS|test result|panicked"
} | tee "$OUT"
