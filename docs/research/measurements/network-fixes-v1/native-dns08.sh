#!/bin/bash
set -euo pipefail
source_dir="$HOME/g121-tmp/network-arm02-v1/source"
for attempt in 1 2 3 4 5 6; do
  git -C "$source_dir" fetch origin p/fix-network-v1 && break
  if [ "$attempt" = 6 ]; then exit 1; fi
  sleep 30
done
git -C "$source_dir" checkout --detach 4b7e93f10e9e0dd5ae7b6803004a232ccc1995a4
cd "$source_dir"
/usr/local/bin/docker run --rm --name network-dns08-green --platform linux/arm64 -v "$source_dir:/src:ro" -v "$HOME/g121-tmp/network-arm02-v1/target:/target" -v "$HOME/g121-tmp/network-arm02-v1/registry:/usr/local/cargo/registry" -v "$HOME/g121-tmp/network-arm02-v1/git:/usr/local/cargo/git" -v "$HOME/g121-tmp/network-arm02-v1/rustup-downloads:/usr/local/rustup/downloads" -w /src -e CARGO_TARGET_DIR=/target -e CARGO_INCREMENTAL=0 -e CARGO_BUILD_JOBS=4 -e CARGO_PROFILE_DEV_DEBUG=0 -e CARGO_PROFILE_TEST_DEBUG=0 -e SHARDS_HELPERS=skip -e RUSTUP_TOOLCHAIN=1.98.1 rust:1.98.1 bash -c 'set -euo pipefail; export PATH=/usr/local/cargo/bin:$PATH; uname -srm; rustc --version; cargo test -p shards-init dnsrelay::tests; cargo test -p shards-init agentdns::tests; rustup component add clippy --toolchain 1.98.1; cargo clippy -p shards-init --all-targets -- -D warnings'
