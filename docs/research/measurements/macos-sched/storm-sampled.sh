#!/bin/sh
# One 64-vCPU storm run (tests/snapshot.rs storm::restores_of_a_busy_machine_..., its
# binary built), its VM's threads sampled every 20 ms (threads.py): OUT its status and
# seconds, OUT.threads the samples, OUT.test its output. Through launchd-run.sh it runs as a
# user's program runs; else in its starter's session, an agent's clamped one (PM M335).
# Usage: DOCKER_CONFIG=EMPTY_DIR storm-sampled.sh OUT (an absolute path)
cd "$(dirname "$0")/../../../.." || exit 1
out=$1
python3 -I docs/research/measurements/macos-sched/threads.py shards-vm 60 0.02 > "$out.threads" 2> "$out.err" &
sleep 1
B=$(ls -t target/test-release/deps/snapshot-* | grep -v '\.d$' | head -1)
s=$(date +%s)
SHARDS_STORM_CPUS=64 DOCKER_CONFIG="${DOCKER_CONFIG:?an empty docker config dir}" scripts/hvf-run "$B" storm::restores_of_a_busy_machine_lose_no_interrupt_and_repeat_no_write --exact > "$out.test" 2>&1
echo "storm exit $? $(( $(date +%s) - s ))s load $(sysctl -n vm.loadavg)" > "$out"
wait
