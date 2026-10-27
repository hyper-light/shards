#!/usr/bin/env bash
# PM M345: platform `write_in` (crates/vmm/src/platform/windows_files.rs), which on Windows
# lands each write as a version of its own, written as the rename probe's writers write
# (../whp-run/src/bin/m345_rename.rs, its second part). Run on a Windows host with Microsoft
# Defender's real-time protection on, where the probe's renames over a file were refused
# (jupiter); GitHub's runners have it off. Prints each pass's n, errors, and p50, p90, p99
# and max of a write, in µs.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
powershell -NoProfile -Command "Get-MpComputerStatus | Select-Object RealTimeProtectionEnabled, AMProductVersion | Format-List" || true
cargo test -p shards-vmm --profile release --lib -- --ignored --exact \
    platform::windows_files::tests::write_in_as_the_rename_probes_writers --nocapture
