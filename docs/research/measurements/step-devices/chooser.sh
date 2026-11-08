#!/bin/bash
# PM M497 (D44, D122): both of this directory's measurements, from the repository's root,
# release builds as every measurement's:
#
#     chooser.sh [STEPS_N] [ROUNDS] [CHOOSER_N]
#
# 1. The chooser's round trip, from an agent's side (build.rs `chooser_round_trip_costs`,
#    CHOOSER_N binds and listens an arm), its lines as the agents print them, in ns.
# 2. A build step's device cgroup against a start without one (run.py, the test guest's
#    `stepcgroup` mode, PID 1 of a guest booted ROUNDS times, STEPS_N starts an arm), with
#    the kernel, test guest and copy of shards the first's build made (target/artifacts,
#    target/guest, target/e2e).
#
# Host, OS and revision first. On macOS run it as a user's program, outside an agent
# session's QoS clamp (docs/research/measurements/macos-sched/launchd-run.sh).
set -eo pipefail
steps=${1:-1000} rounds=${2:-5} chooser=${3:-2000}
root=$(git rev-parse --show-toplevel)
cd "$root"
echo "host $(uname -n) $(uname -m) · $(uname -sr) · revision $(git rev-parse --short HEAD)"
arch=$(uname -m)
[ "$arch" = arm64 ] && arch=aarch64
cfg=$(mktemp -d)
export DOCKER_CONFIG=$cfg
SHARDS_CHOOSER_N=$chooser cargo test --release -p shards --test build -- \
  --ignored --exact --nocapture chooser_round_trip_costs 2>&1 | grep -E 'listencost|^test |test result'
kernel=$(ls -t target/artifacts/* | grep -v '\.' | head -1)
guest=target/guest/$arch-unknown-linux-musl/guest/shards-testguest
shards=$(ls -t target/e2e/*/shards | head -1)
python3 docs/research/measurements/step-devices/run.py --shards "$shards" \
  --guest "$guest" --kernel "$kernel" --rounds "$rounds" --n "$steps"
rm -rf "$cfg"
