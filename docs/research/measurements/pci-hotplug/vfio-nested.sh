#!/bin/sh
# D214's host functions plugged in, on arm64 KVM nested in a guest with EL2 (an M3 or later
# Mac): boot.rs's L1 harness gives its guest QEMU's edu test function at 01.0, and runs in
# it tests/hotplug.rs's `a_hosts_function_is_plugged_into_a_running_microvm` (ROUNDS
# rounds, its PLUG-STATS lines: n, p50, p90, p99, max in µs, of each step and of each of
# the host plug's own; its PLUG-STEPS lines, the host plug's own steps each round, in µs)
# and tests/plug.rs's `a_hosts_function_is_plugged_into_a_run_for_its_workload`, one at a
# time. The L1 guest's
# console, and what the host and revision were, to OUT. Run from the repository; a Mac
# without EL2 skips. An agent's session runs clamped at utility QoS, which starves the
# nested guests: time it as a user's program runs (macos-sched/launchd-run.sh).
set -eu
ROUNDS=${ROUNDS:-30}
OUT=${OUT:-docs/research/measurements/pci-hotplug/$(uname -s)-$(uname -m)-vfio-nested.txt}
console=$(mktemp)
{
    echo "host: $(uname -a)"
    echo "revision: $(git rev-parse HEAD)"
    echo "rounds: $ROUNDS"
    SHARDS_REQUIRE_VMS=1 SHARDS_PLUG_ROUNDS=$ROUNDS SHARDS_L1_PCI_TESTS=01.0 \
        SHARDS_L1_TESTS="hotplug:a_hosts_function --nocapture,plug:a_hosts_function --nocapture" \
        SHARDS_L1_CONSOLE="$console" \
        cargo test -p shards --profile test-release --test boot \
        snapshot_tests_pass_on_a_kvm_host_of_this_architecture -- --nocapture 2>&1 |
        grep -E "SKIP|test result|panicked" || true
    grep -aE "PLUG-STATS|PLUG-STEPS|E2E STATUS|test result|SKIP|panicked" "$console" || true
} | tee "$OUT"
rm -f "$console"
