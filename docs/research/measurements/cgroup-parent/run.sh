#!/bin/sh
# M248 (docs/research/platform-measurements.md): placing a process in a slice of the user's
# service manager, as the daemon does for `--cgroup-parent` (D183), and where the process's
# memory is charged. Runs the harness from the caller's own cgroup (from an ssh login, a
# session scope the user's manager may not move a process out of itself) and from a scope
# of the user's manager (`systemd-run --user --scope`), quiet and with every CPU busy:
# each placement whole (`place`), by its steps (`phases`), a delegated scope placed once
# with each child moved into a cgroup of its own beneath it (`delegated`), and memory
# touched before placing (`moved`); and, for reference, systemd's own `systemd-run --user
# --scope true`, timed whole. Then stops the slice it made. Each run prints its summaries
# and its three slowest.
#
#   docs/research/measurements/cgroup-parent/run.sh [N]
#
# PROBE names a built harness (for a host with no Rust toolchain: build it elsewhere with
# `cargo build --release --target x86_64-unknown-linux-musl` from this directory, copy it
# over); otherwise it is built here.
set -eu

n=${1:-300}
slice=shardsprobe.slice
if [ -z "${PROBE:-}" ]; then
    here=$(cd "$(dirname "$0")" && pwd)
    (cd "$here" && cargo build --release --quiet)
    PROBE=$here/target/release/cgroup-parent
fi
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT
cpus=$(nproc)
run() {
    echo "== $1"
    shift
    "$@" >"$out/run"
    grep -v '^[0-9]' "$out/run"
    echo "slowest:"
    grep '^[0-9]' "$out/run" | awk '{ print $NF, $0 }' | sort -n | tail -3 | cut -d' ' -f2-
}
user_scope="systemd-run --user --scope --quiet"
echo "host: $(uname -srm), $cpus CPUs; $(systemctl --version | head -1)"
for load in 0 "$cpus"; do
    run "place, from this login, $load busy" "$PROBE" place "$n" "$slice" "$load"
    run "place, from a scope of the user's manager, $load busy" $user_scope "$PROBE" place "$n" "$slice" "$load"
    run "phases, from this login, $load busy" "$PROBE" phases "$n" "$slice" "$load"
    run "delegated, from this login, $load busy" "$PROBE" delegated "$n" "$slice" "$load"
done
echo "== touched first, placed after"
"$PROBE" moved 5 "$slice"
echo "== for reference: systemd-run --user --scope true, whole, $n times"
for i in $(seq "$n"); do
    s=$(date +%s%N)
    $user_scope true
    e=$(date +%s%N)
    echo $(((e - s) / 1000))
done | sort -n | awk '{ v[NR] = $1 } END {
    printf "n=%d p50=%d p90=%d p99=%d max=%d us\n", NR, v[int(NR * 0.50 + 0.999)], v[int(NR * 0.90 + 0.999)], v[int(NR * 0.99 + 0.999)], v[NR]
}'
systemctl --user stop "$slice"
