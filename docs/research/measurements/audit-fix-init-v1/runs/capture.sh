#!/usr/bin/env bash
# Runs "$@" with stdout and stderr together, prints that raw output, then one last line
# "capture-sha256 <hex> exit <status>" over exactly those bytes, so that a copy saved
# elsewhere can be checked against what the runner produced. Output goes through a file
# under target/ (build output, never synced back).
set -u
mkdir -p target/audit-fix-init-v1
out=target/audit-fix-init-v1/capture.out
"$@" > "$out" 2>&1
status=$?
cat "$out"
echo "capture-sha256 $(sha256sum < "$out" | cut -d' ' -f1) exit $status"
exit 0
