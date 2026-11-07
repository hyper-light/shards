#!/bin/sh
# Builds probe.c, signs it with the hypervisor entitlement (resources/hvf.entitlements), and
# runs it, with the host's model and macOS version (PM M422). Usage: run.sh [OUT_DIR]
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../../.." && pwd)
out=${1:-${TMPDIR:-/tmp}}/hvf-el2-vhe
mkdir -p "$out"
clang -O2 -Wall -Werror -o "$out/probe" "$here/probe.c" -framework Hypervisor
codesign --force --sign - --entitlements "$root/resources/hvf.entitlements" "$out/probe" 2>/dev/null
sysctl -n machdep.cpu.brand_string
sw_vers -productVersion
"$out/probe"
