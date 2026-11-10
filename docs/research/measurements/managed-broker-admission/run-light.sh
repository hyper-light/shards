#!/bin/sh
set -eu
: "${CARGO_HOME:?use the admitted private Cargo home}"
: "${CARGO_TARGET_DIR:?use the admitted private target}"
cd "$(dirname "$0")/../../../.."
cargo +1.98.1 test -p shards-server --locked --profile test-release -j1 broker_ -- --nocapture
cargo +1.98.1 test -p shards-server --locked --profile test-release -j1 only_a_regular_file_of_the_scratch_is_handed -- --nocapture
cargo +1.98.1 test -p shards-server --locked --profile test-release -j1 a_programs_call_is_its_callers -- --nocapture
cargo +1.98.1 clippy -p shards-server --locked --all-targets --profile test-release -j1 -- -D warnings
