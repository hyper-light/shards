#!/bin/bash
# Run from a source checkout on a permitted Mac with Docker Desktop.
set -euo pipefail
source_dir=$(pwd)
task_root="${NETWORK_CHECK_ROOT:-$HOME/g121-tmp/network-dns03-repro}"
container_name="network-dns03-repro-$$"
stage="${1:-green}"
mkdir -p "$task_root"
trap 'docker rm -f "$container_name" >/dev/null 2>&1 || true' EXIT
docker run --rm --name "$container_name" --platform linux/arm64 \
    -v "$source_dir:/src:ro" -v "$task_root/target:/target" \
    -v "$task_root/registry:/usr/local/cargo/registry" \
    -v "$task_root/git:/usr/local/cargo/git" \
    -v "$task_root/rustup-downloads:/usr/local/rustup/downloads" -w /src \
    -e CARGO_TARGET_DIR=/target -e CARGO_INCREMENTAL=0 -e CARGO_BUILD_JOBS=4 \
    -e CARGO_PROFILE_DEV_DEBUG=0 -e CARGO_PROFILE_TEST_DEBUG=0 -e SHARDS_HELPERS=skip \
    -e RUSTUP_TOOLCHAIN=1.98.1 rust:1.98.1 bash -c '
        set -euo pipefail
        export PATH=/usr/local/cargo/bin:$PATH
        uname -srm
        rustc --version
        git config --global --add safe.directory /src
        git rev-parse HEAD
        case "$1" in
            red) cargo test -p shards-net dns_answers_replace_the_names_previous_authority ;;
            green)
                cargo test -p shards-net dns_
                cargo test -p shards-net ports_reach_their_ports_alone
                cargo test -p shards-dns a_responses
                cargo test -p shards-dns tcp_
                rustup component add clippy --toolchain 1.98.1
                cargo clippy -p shards-net -p shards-dns --all-targets -- -D warnings
                ;;
            check) cargo check -p shards-net -p shards-dns --tests ;;
            *) exit 2 ;;
        esac
    ' bash "$stage"
