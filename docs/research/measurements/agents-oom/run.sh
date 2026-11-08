#!/bin/sh
# M237 (docs/research/platform-measurements.md): what each agent of a run holds, sampled
# every 250 ms by init, as tests/build.rs's agents_out_of_memory_end_whole_and_spare_the_run
# runs: sampler.patch gives init a timer that says each domain's memory.stat and each of
# its processes (name, state, CPU ticks, syscall), and has the test watch 8 s past the
# filler's end rather than 3, and print what init said whether it passes or not. Applied
# for the run and taken off again; run it from a clean tree.
#
#   run.sh
set -eu
root=$(cd "$(dirname "$0")/../../../.." && pwd)
cd "$root"
git apply docs/research/measurements/agents-oom/sampler.patch
trap 'git apply -R docs/research/measurements/agents-oom/sampler.patch' EXIT
cargo test -q -p shards --profile test-release --test build -- \
	agents_out_of_memory_end_whole_and_spare_the_run --nocapture 2>&1 | grep '^oom-'
