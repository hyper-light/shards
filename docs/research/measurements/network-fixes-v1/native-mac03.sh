#!/bin/bash
# Run from the source root on a permitted arm64 Mac runner, never the shared author Mac.
set -euo pipefail
export CARGO_TARGET_DIR="${NETWORK_MAC_TARGET:-$HOME/g121-tmp/network-dns03-macos-v1/target}"
export CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=4 SHARDS_HELPERS=skip
uname -srm
rustc --version
git rev-parse HEAD
case "${1:-green}" in
    red) cargo test -p shards-net --profile test-release --lib dns_answers_replace_the_names_previous_authority ;;
    green)
        cargo test -p shards-net --profile test-release --lib dns_
        cargo test -p shards-net --profile test-release --lib ports_reach_their_ports_alone
        cargo clippy -p shards-net --all-targets -- -D warnings
        ;;
    check) cargo check -p shards-net --tests ;;
    *) exit 2 ;;
esac
