#!/usr/bin/env bash
# PM M216: MoltenVK's host-visible memory as a GPU's blobs map it (D144).
#   docs/research/measurements/moltenvk-memory/run.sh MOLTENVK_DIR
# MOLTENVK_DIR is a MoltenVK release's MoltenVK/ directory (its include/ and static/), as
# scripts/gpu-libs unpacks it. layout.c: where each host-visible allocation's mapping is,
# and what of its pages are its own; reuse.c: whether a freed allocation's pages, still
# mapped elsewhere, are given to the next.
set -euo pipefail
mvk=${1:?usage: run.sh MOLTENVK_DIR}
here=$(cd "$(dirname "$0")" && pwd)
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT
for probe in layout reuse; do
    clang -O1 -I"$mvk/include" "$here/$probe.c" \
        "$mvk/static/MoltenVK.xcframework/macos-arm64_x86_64/libMoltenVK.a" \
        -framework Metal -framework IOSurface -framework IOKit -framework QuartzCore \
        -framework Foundation -framework CoreFoundation -framework CoreGraphics -framework AppKit \
        -lc++ -o "$out/$probe"
    MVK_CONFIG_LOG_LEVEL=0 "$out/$probe" | grep -v '^\[mvk'
done
