#!/bin/sh
# Phase A: R09 demonstrations that need no build of this repository (Linux runner, from the
# worktree root). Each command is echoed before its output and followed by its exit status.
set -u
d=docs/research/measurements/audit-fix-tests-v1/demos
out=$(mktemp -d "${TMPDIR:-/tmp}/afv1-phase-a.XXXXXX")
run() {
	echo "\$ $*"
	"$@"
	echo "[exit $?]"
}
echo "== host"
uname -a
git rev-parse HEAD
python3 --version
rustc --version
echo "== f01 process-wide descriptor count"
run python3 -I "$d/f01_fd_count.py"
echo "== f04 line before rename"
run python3 -I "$d/f04_rename_order.py" 2000
echo "== f11 PID reuse after reap"
run unshare -Urpf --mount-proc python3 -I "$d/f11_pid_reuse.py"
echo "== f12 gate exposure and route probe"
run python3 -I "$d/f12_gate.py" route
run unshare -rn python3 -I "$d/f12_gate.py" no-route
echo "== f02 early return in an ignored campaign"
run rustc --edition 2024 --test -o "$out/f02" "$d/f02_early_return.rs"
run "$out/f02" --ignored --test-threads=1
echo "== f05 records directory reuse"
run rustc --edition 2024 -o "$out/f05" "$d/f05_records_dir.rs"
run "$out/f05" "$out/records"
echo "== f09 vacuous negative check"
run rustc --edition 2024 -o "$out/f09" "$d/f09_vacuous.rs"
run "$out/f09"
rm -rf "$out"
