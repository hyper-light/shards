#!/bin/sh
# run.sh ROUNDS OUT LAYOUT...: builds rootfs-layout from the working tree, then ROUNDS
# rounds, each building every image once in every variant (today, memory, blobs), the
# variants' order rotating from round to round. Each build's line (`VARIANT IMAGE MS
# CPU_MS RSS_MB`) is appended to OUT as it ends; then n, p50, p90, p99 and max of wall
# time, CPU time and RSS per image and variant are printed. BUSY=N and IOBUSY=N load
# the host as rootfs-build's ab.sh does.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
rounds=$1 out=$2
shift 2
work=$(mktemp -d)
spinners=
trap 'set +e; for p in $spinners; do pkill -P "$p" 2>/dev/null; kill "$p" 2>/dev/null; done; rm -rf "$work"' EXIT
cargo build -q --release --manifest-path "$here/Cargo.toml" --target-dir "$work/target"
bin=$work/target/release/rootfs-layout
for v in today memory blobs; do "$bin" "$v" "$work/store" "$@" >/dev/null; done
# The load: BUSY spinners and IOBUSY writers, each kept by its slot. Before every round
# a slot whose process has gone (another process may kill it) gets a new one, and the loss
# is logged with its time beside the results: a run says whether its load held.
spinner() { yes >/dev/null & }
writer() { sh -c 'while :; do dd if=/dev/zero of="$1" bs=1m count=1024 2>/dev/null; rm -f "$1"; done' sh "$work/io-$1" & }
ensure_load() {
    n=0
    while [ "$n" -lt "${BUSY:-0}" ]; do
        eval "pid=\${busy_$n:-}"
        if [ -z "$pid" ] || ! kill -0 "$pid" 2>/dev/null; then
            [ -n "$pid" ] && echo "$(date +%s) spinner $n was gone; restarted" >>"$work/load.log"
            spinner
            eval "busy_$n=$!"
            spinners="$spinners $!"
        fi
        n=$((n + 1))
    done
    n=0
    while [ "$n" -lt "${IOBUSY:-0}" ]; do
        eval "pid=\${io_$n:-}"
        if [ -z "$pid" ] || ! kill -0 "$pid" 2>/dev/null; then
            [ -n "$pid" ] && echo "$(date +%s) writer $n was gone; restarted" >>"$work/load.log"
            writer "$n"
            eval "io_$n=$!"
            spinners="$spinners $!"
        fi
        n=$((n + 1))
    done
}
: >"$work/load.log"
i=0
while [ "$i" -lt "$rounds" ]; do
    ensure_load
    case $((i % 3)) in
        0) order="today memory blobs" ;;
        1) order="memory blobs today" ;;
        *) order="blobs today memory" ;;
    esac
    for v in $order; do "$bin" "$v" "$work/store" "$@" >>"$out"; done
    i=$((i + 1))
done
python3 - "$out" <<'PY'
import sys, collections
runs = collections.defaultdict(list)
for line in open(sys.argv[1]):
    v, image, ms, cpu, rss = line.split()
    runs[(image, v)].append((float(ms), float(cpu), float(rss)))
def q(s, p): return s[min(len(s) - 1, int(p * len(s)))]
print("| image | variant | n | wall p50 | p90 | p99 | max | CPU p50 | p99 | RSS MB p50 | max |")
print("|---|---|---|---|---|---|---|---|---|---|---|")
for (image, v), xs in sorted(runs.items()):
    w = sorted(x[0] for x in xs); c = sorted(x[1] for x in xs); r = sorted(x[2] for x in xs)
    print(f"| {image} | {v} | {len(w)} | {q(w,.5):.0f} | {q(w,.9):.0f} | {q(w,.99):.0f} | {w[-1]:.0f} | {q(c,.5):.0f} | {q(c,.99):.0f} | {q(r,.5):.0f} | {r[-1]:.0f} |")
PY
if [ -s "$work/load.log" ]; then echo "The load was lost and restored:"; cat "$work/load.log"; else echo "The load held throughout."; fi
