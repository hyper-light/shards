#!/bin/sh
# Builds hvfbench and ad-hoc signs it with the hypervisor entitlement.
set -eu
cd "$(dirname "$0")"
clang -O2 -std=c17 -Wall -Wextra -Werror -o hvfbench hvfbench.c -framework Hypervisor
codesign --entitlements entitlements.plist --force -s - hvfbench
