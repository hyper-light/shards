#!/bin/sh
# sustained-io's npm and cargo trees for a Mac, for cells.py (sustained-io/fetch.sh's, those two
# alone, its pins and steps; fetch.sh is Linux's): npm/cli v12.2.1 with the npm cache its offline install reads, and
# rust-analyzer 2026-10-19 with its dependencies vendored. Usage: fetch-mac.sh
set -eu
SIO=${SIO:-$HOME/sio}
mkdir -p "$SIO/src"
checkout() {
    [ -d "$4/.git" ] || git -c advice.detachedHead=false clone -q --depth 1 --branch "$2" "$1" "$4"
    at=$(git -C "$4" rev-parse HEAD)
    [ "$at" = "$3" ] || { echo "$1 $2 is $at, not $3" >&2; exit 1; }
}
checkout https://github.com/npm/cli.git v12.2.1 ae98c62586b6fea44114f72e249e246a2981de70 "$SIO/src/cli"
if [ ! -d "$SIO/npm-cache/_cacache" ]; then
    (cd "$SIO/src/cli" && npm ci -q --cache "$SIO/npm-cache" --ignore-scripts --no-audit --no-fund)
    find "$SIO/src/cli" -name node_modules -type d -prune -exec rm -rf {} +
fi
checkout https://github.com/rust-lang/rust-analyzer.git 2026-10-19 5ec20d044dbf04895877a55921ddc0ae6e73e127 \
    "$SIO/src/rust-analyzer"
if [ ! -d "$SIO/src/rust-analyzer/vendor" ]; then
    mkdir -p "$SIO/src/rust-analyzer/.cargo"
    (cd "$SIO/src/rust-analyzer" && cargo vendor --locked vendor >.cargo/config.toml)
    grep -q vendored-sources "$SIO/src/rust-analyzer/.cargo/config.toml"
fi
du -sh "$SIO/src/cli" "$SIO/npm-cache" "$SIO/src/rust-analyzer"
echo "fetched"
