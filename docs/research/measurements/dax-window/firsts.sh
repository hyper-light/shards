#!/bin/sh
# What a DAX window's length costs an image's first run with a volume, which boots its
# template and saves it (D189, PM M306): for each LENGTH MiB (0 none), ROUNDS fresh homes
# under WORK, each given SEED's image store (a clone where the file system makes one, so
# that no run pulls), a first run without a volume (which fetches the pinned kernel and
# boots a template of its own), then the timed run with a volume of DIR and the daemon
# giving it a window of LENGTH (SHARDS_SHARE_WINDOW_MIB). Lengths interleaved, round by
# round. With KERNEL and INIT set, each home boots them instead (`shards configure guest`,
# D28), templates and all. One line a run: LENGTH ROUND MS TEMPLATE_KIB.
# Usage: [KERNEL=… INIT=…] firsts.sh SHARDS SEED WORK IMAGE DIR ROUNDS LENGTH_MIB...
#   (SEED holds IMAGE)
set -eu
shards=$1 seed=$2 work=$3 image=$4 dir=$5 rounds=$6
shift 6
cfg=$(mktemp -d)
trap 'rm -rf "$cfg"' EXIT
export DOCKER_CONFIG="$cfg"
timed() {
    python3 -I -c 'import subprocess, sys, time
t = time.monotonic()
r = subprocess.run(sys.argv[1:], stdout=subprocess.DEVNULL)
sys.exit(r.returncode) if r.returncode else print(round((time.monotonic() - t) * 1000))' "$@"
}
for round in $(seq "$rounds"); do
    for len in "$@"; do
        home="$work/home-$len-$round"
        mkdir -p "$home"
        cp -cR "$seed/images" "$home/images" 2>/dev/null || cp -a --reflink=auto "$seed/images" "$home/images"
        export SHARDS_HOME="$home"
        unset SHARDS_SHARE_WINDOW_MIB
        if [ -n "${KERNEL:-}" ]; then "$shards" configure guest --kernel "$KERNEL" --init "$INIT" >/dev/null; fi
        "$shards" run --rm --pull never "$image" true >/dev/null
        "$shards" stop daemon >/dev/null 2>&1 || true
        if [ "$len" -gt 0 ]; then export SHARDS_SHARE_WINDOW_MIB=$len; fi
        ls "$home/templates" | sort >"$cfg/before"
        ms=$(timed "$shards" run --rm --pull never -v "$dir:/v" "$image" true)
        ls "$home/templates" | sort >"$cfg/after"
        made=$(comm -13 "$cfg/before" "$cfg/after" | grep -v '\.origin$' | head -1)
        kib=$([ -n "$made" ] && du -sk "$home/templates/$made" | cut -f1 || echo none)
        echo "$len $round $ms $kib"
        "$shards" stop daemon >/dev/null 2>&1 || true
    done
done
