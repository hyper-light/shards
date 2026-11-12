#!/bin/bash
# CI's fuzz job (.github/workflows/ci.yml, `fuzz`) on a Linux host, for
# docs/research/measurements/ci-rego-compare: CI's nightly and cargo-fuzz 0.13.1 (built with
# the pinned 1.98.1), the ten artifacts replayed with CI's flags, rego-builtins fuzzed from
# them for SECS seconds, then every other target for OTHER seconds each, as CI runs them
# (-rss_limit_mb=2048 -max_len=65536). Reads `git archive REV` on stdin; bounded with
# systemd-run (16 GiB, four cores) and -j4; removes its copy, targets and tools at the end.
#
#   git archive REV | bash fuzz-linux.sh LABEL [SECS] [OTHER]
set -euo pipefail
label=$1
secs=${2:-600}
other=${3:-60}
work=$HOME/ci-rego-$label
tools=$HOME/ci-rego-tools-$label
rm -rf "$work" "$tools"
mkdir -p "$work"
tar -x -C "$work"
cd "$work"
trap 'cd / && rm -rf "$work" "$tools"' EXIT
echo "host $(hostname), $(uname -srm), $(nproc) cores, at $(date -u +%FT%TZ)"
scripts/retry 5 30 rustup toolchain install nightly-2026-09-05 --profile minimal >/dev/null 2>&1
scripts/retry 5 30 rustup toolchain install 1.98.1 --profile minimal >/dev/null 2>&1
cargo +1.98.1 install --locked cargo-fuzz --version 0.13.1 --root "$tools" >/dev/null 2>&1
export PATH="$tools/bin:$PATH" CARGO_BUILD_JOBS=4
echo "$(rustc +nightly-2026-09-05 --version), $(cargo fuzz --version)"
bound() { systemd-run --user --scope -q -p MemoryMax=16G -p CPUQuota=400% "$@"; }
start=$(date +%s)
bound cargo +nightly-2026-09-05 fuzz build >build.log 2>&1 || { tail -30 build.log; exit 1; }
echo "fuzz build: $(($(date +%s) - start)) s"
bin=fuzz/target/x86_64-unknown-linux-gnu/release
art=docs/research/measurements/ci-rego-compare/artifacts
for a in "$art"/*; do
	start=$(date +%s)
	rc=0
	bound timeout 900 "$bin/rego-builtins" -rss_limit_mb=2048 -timeout=300 "$a" >replay.log 2>&1 || rc=$?
	what=$(grep -a -m1 -o -E 'ERROR: libFuzzer: [a-z-]+|Executed [^ ]+ in [0-9]+ ms' replay.log || true)
	echo "replay $(basename "$a"): exit $rc in $(($(date +%s) - start)) s, $what"
done
mkdir -p fuzz/corpus/rego-builtins
cp "$art"/* fuzz/corpus/rego-builtins/
fuzz() {
	local t=$1 s=$2 rc=0
	bound cargo +nightly-2026-09-05 fuzz run "$t" -- -max_total_time="$s" -rss_limit_mb=2048 -max_len=65536 >"fuzz-$t.log" 2>&1 || rc=$?
	echo "$t, $s s: exit $rc, $(grep -a -E '^Done [0-9]+ runs' "fuzz-$t.log" | tail -1)"
	grep -a -E 'ERROR: libFuzzer|SUMMARY|Test unit written|panicked at' "fuzz-$t.log" | head -6 || true
	if [ "$rc" -ne 0 ]; then
		for f in fuzz/artifacts/"$t"/*; do echo "$f: $(base64 -w0 "$f" | head -c 600)"; done
	fi
}
fuzz rego-builtins "$secs"
for t in $(cargo +nightly-2026-09-05 fuzz list); do
	[ "$t" = rego-builtins ] || fuzz "$t" "$other"
done
echo "done at $(date -u +%FT%TZ)"
