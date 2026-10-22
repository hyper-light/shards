#!/bin/sh
# What a builder microVM holds of its memory as a build goes (D188's open item: builders
# at the host's pressure): Dockerfile's steps print the builder guest's /proc/meminfo before
# and after an npm install, each build --no-cache, ROUNDS times.
#   measure.sh SHARDS ROUNDS   (SHARDS_HOME and DOCKER_CONFIG as the caller sets)
set -eu
shards=$1 rounds=$2
ctx=$(mktemp -d)
cp "$(dirname "$0")/Dockerfile" "$ctx/Dockerfile"
for n in $(seq "$rounds"); do
    "$shards" build --progress=plain --no-cache -t builder-mem:1 "$ctx" 2>&1 |
        grep -E ' (before|after)$|kB$|node_modules' | sed "s/^/$n /"
done
rm -rf "$ctx"
