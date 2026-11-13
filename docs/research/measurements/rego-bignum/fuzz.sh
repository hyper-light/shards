#!/bin/bash
# CI's fuzz job for PM M536, from a checkout's root on Linux: rego-builtins and rego built
# as CI builds them (nightly-2026-09-05, cargo-fuzz 0.13.1, AddressSanitizer), each
# rego-builtins artifact replayed alone with CI's flags, then rego-builtins fuzzed SECS
# seconds seeded with them all and the fuzzer's number through each builtin that writes an
# integer's every digit, then rego for CI's 60 s. Targets, corpus and artifacts under DIR
# (default /tmp/rego-bignum-fuzz), each run in a scope of 16 GiB.
#
#   bash docs/research/measurements/rego-bignum/fuzz.sh [SECS] [DIR]
set -u
secs=${1:-300}
T=${2:-/tmp/rego-bignum-fuzz}
rm -rf "$T/corpus" "$T/artifacts"
mkdir -p "$T/corpus" "$T/artifacts" "$T/rego-corpus"
echo "host $(hostname), $(uname -srm), $(nproc) cores, $(git rev-parse --short HEAD 2>/dev/null), $(date -u +%FT%TZ); $(cargo +nightly-2026-09-05 fuzz --version 2>&1); $(rustc +nightly-2026-09-05 --version)"
bound() { systemd-run --user --scope -q -p MemoryMax=16G "$@"; }
start=$(date +%s)
bound nice -n 10 cargo +nightly-2026-09-05 fuzz build --target-dir "$T/target" >"$T/build.log" 2>&1 || {
	tail -30 "$T/build.log"
	exit 1
}
echo "fuzz build: $(($(date +%s) - start)) s"
bin=$T/target/x86_64-unknown-linux-gnu/release
cp docs/research/measurements/ci-rego-compare/artifacts/* "$T/corpus/"
for b in floor ceil round; do printf '%s\n[\r80000000000e00029174680]\t' $b >"$T/corpus/seed-$b"; done
printf 'format_int\n[\r80000000000e00029174680, 10]\t' >"$T/corpus/seed-format_int-10"
printf 'format_int\n[\r80000000000e00029174680, 2]\t' >"$T/corpus/seed-format_int-2"
printf 'abs\n[\r1e646456992]\t' >"$T/corpus/seed-abs-max"
printf 'floor\n[\r1e646456992]\t' >"$T/corpus/seed-floor-max"
ptime=docs/research/measurements/rego-bignum/ptime.py
for a in "$T"/corpus/*; do
	rc=0
	bound python3 "$ptime" "$bin/rego-builtins" -rss_limit_mb=2048 -max_len=65536 "$a" >"$T/replay.log" 2>&1 || rc=$?
	what=$(grep -a -m1 -o -E 'ERROR: libFuzzer: [a-z-]+|Executed [^ ]+ in [0-9]+ ms' "$T/replay.log" | sed "s|$T/corpus/||")
	echo "replay $(basename "$a"): exit $rc, $what, $(grep -a -o 'maxrss=[0-9.]*MiB' "$T/replay.log")"
done
rc=0
bound "$bin/rego-builtins" "$T/corpus" -max_total_time="$secs" -rss_limit_mb=2048 -max_len=65536 \
	-artifact_prefix="$T/artifacts/" >"$T/fuzz-rego-builtins.log" 2>&1 || rc=$?
echo "rego-builtins, $secs s: exit $rc, $(grep -a -E '^Done [0-9]+ runs' "$T/fuzz-rego-builtins.log" | tail -1); $(grep -a -o 'rss: [0-9]*Mb' "$T/fuzz-rego-builtins.log" | tail -1)"
grep -a -E 'ERROR: libFuzzer|SUMMARY|Test unit written|panicked at|Slowest unit' "$T/fuzz-rego-builtins.log" | head -12
for f in "$T"/artifacts/*; do [ -e "$f" ] && echo "$(basename "$f"): $(base64 -w0 "$f" | head -c 400)"; done
rc=0
bound "$bin/rego" "$T/rego-corpus" -max_total_time=60 -rss_limit_mb=2048 -max_len=65536 \
	-artifact_prefix="$T/artifacts/rego-" >"$T/fuzz-rego.log" 2>&1 || rc=$?
echo "rego, 60 s: exit $rc, $(grep -a -E '^Done [0-9]+ runs' "$T/fuzz-rego.log" | tail -1)"
grep -a -E 'ERROR: libFuzzer|SUMMARY|Test unit written|panicked at' "$T/fuzz-rego.log" | head -6
echo "done $(date -u +%FT%TZ)"
