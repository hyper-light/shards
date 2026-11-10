#!/usr/bin/env bash
set -eu
: "${CARGO_TARGET_DIR:?private target required}"
export CARGO_BUILD_JOBS=4
record=${1:?fresh evidence directory}
mkdir -p "$record"
git rev-parse HEAD > "$record/source.txt"
sha256sum crates/init/src/{run.rs,managed.rs,join.rs} crates/testguest/src/{main.rs,workload.rs,cleanup.rs} crates/shards/tests/managed_cleanup.rs > "$record/source.sha256"
uname -a > "$record/environment.txt"
rustc --version >> "$record/environment.txt"
touch crates/testguest/src/{main.rs,workload.rs,cleanup.rs} crates/shards/tests/managed_cleanup.rs
set +e
cargo test --locked -j4 -p shards --test managed_cleanup main_cleanup_reaps_owned_children_and_preserves_independent_joiner -- --exact --ignored --nocapture --test-threads=1 > "$record/run.log" 2>&1
code=$?
set -e
printf '%s\n' "$code" > "$record/run.exit"
cat "$record/run.log"
exit "$code"
