#!/bin/sh
# What a DAX window costs a run restored from its template, by its length (D189, PM M306):
# with the daemon giving each volume a window of LENGTH MiB (SHARDS_SHARE_WINDOW_MIB, 0
# none, `default` the daemon's own, as long as the host's memory), the image's first run
# with a volume, which boots its template and saves it,
# timed; the template's size on disk; ROUNDS runs after, each restored from it, timed; and
# a detached run's VM process's resident memory, beside its pool's warm VMs'. HOME holds
# IMAGE already. With KERNEL and INIT set, HOME boots them (`shards configure guest`, D28).
# One line a measure: LENGTH first|paced|burst|rss|template VALUE (ms, KiB).
# Usage: [KERNEL=… INIT=…] restores.sh SHARDS HOME IMAGE DIR ROUNDS LENGTH_MIB...
set -eu
shards=$1 home=$2 image=$3 dir=$4 rounds=$5
shift 5
cfg=$(mktemp -d)
trap 'rm -rf "$cfg"' EXIT
export SHARDS_HOME="$home" DOCKER_CONFIG="$cfg"
if [ -n "${KERNEL:-}" ]; then "$shards" configure guest --kernel "$KERNEL" --init "$INIT" >/dev/null; fi
# A run, timed in the process that starts it: a monotonic clock's readings are that
# process's own (Python 3.9's on macOS counts from its start).
timed() {
    python3 -I -c 'import subprocess, sys, time
t = time.monotonic()
r = subprocess.run(sys.argv[1:], stdout=subprocess.DEVNULL)
sys.exit(r.returncode) if r.returncode else print(round((time.monotonic() - t) * 1000))' "$@"
}
for len in "$@"; do
    "$shards" stop daemon >/dev/null 2>&1 || true
    if [ "$len" = default ]; then unset SHARDS_SHARE_WINDOW_MIB; else export SHARDS_SHARE_WINDOW_MIB=$len; fi
    ls "$home/templates" 2>/dev/null | sort >"$cfg/before"
    echo "$len first $(timed "$shards" run --rm --pull never -v "$dir:/v" "$image" true)"
    ls "$home/templates" 2>/dev/null | sort >"$cfg/after"
    made=$(comm -13 "$cfg/before" "$cfg/after" | grep -v '\.origin$' | head -1)
    [ -n "$made" ] && echo "$len template $(du -sk "$home/templates/$made" | cut -f1)"
    # Paced, a second apart, as its pool keeps up; then back to back, as a burst outruns
    # its refills.
    for round in $(seq "$rounds"); do
        sleep 1
        echo "$len paced $(timed "$shards" run --rm --pull never -v "$dir:/v" "$image" true)"
    done
    for round in $(seq "$rounds"); do
        echo "$len burst $(timed "$shards" run --rm --pull never -v "$dir:/v" "$image" true)"
    done
    "$shards" rm -f rss >/dev/null 2>&1 || true
    "$shards" run -d --name rss --pull never -v "$dir:/v" "$image" sleep 60 >/dev/null
    sleep 2
    if [ -n "$made" ]; then
        for pid in $(pgrep -f "$home/templates/$made" || true); do
            echo "$len rss $(ps -o rss= -p "$pid" | tr -d ' ')"
        done
    fi
    "$shards" rm -f rss >/dev/null 2>&1 || true
done
"$shards" stop daemon >/dev/null 2>&1 || true
