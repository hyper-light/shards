#!/bin/sh
# The 2 MiB folio harness on Fedora (the plan's step 3): release builds of REV's tree, of it
# with dax-budget's huge.patch and of it with huge-random.patch, then measure.py's rounds on
# the performance cores and on the efficient cores, the files and homes kept between the two
# in ~/dax-huge/work (the home's Btrfs), its microVMs in a slice held to MemoryMax 12G. Run as
# a transient unit (systemd-run --user), its results in ~/dax-huge. The builds' sources are
# REV's, whatever the mirror's working tree holds beyond it; the harness is the mirror's.
# Usage: fedora.sh MIRROR REV [measure.py's arguments...]
set -eu
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
mirror=$1 rev=$2
shift 2
out=$HOME/dax-huge
src=$HOME/dax-huge-src
mkdir -p "$out" "$out/tmp"
# fedora's /tmp is memory: this job's temporary files on disk.
export TMPDIR="$out/tmp"
ulimit -n "$(ulimit -Hn)"
say() { echo "$(date -u +%FT%TZ) $*" | tee -a "$out/progress.txt"; }
say "start; free: $(free -m | awk '/^Mem/ {print $7}') MiB available; load $(cut -d' ' -f1 /proc/loadavg)"
rsync -a --delete --exclude target "$mirror/" "$src/"
git -C "$src" checkout "$rev" -- crates Cargo.toml Cargo.lock
export CARGO_TARGET_DIR="$HOME/dax-huge-target" CARGO_BUILD_JOBS=4
build() {
    (cd "$src" && cargo build --release -p shards) > "$out/build-$1.txt" 2>&1
    cp "$CARGO_TARGET_DIR/release/shards" "$out/shards-$1.new" && mv "$out/shards-$1.new" "$out/shards-$1"
    say "built $1: $("$out/shards-$1" --version)"
}
build window
for patch in dax-budget/huge dax-huge/huge-random; do
    (cd "$src" && git apply "docs/research/measurements/$patch.patch")
    build "${patch#*/}"
    git -C "$src" checkout "$rev" -- crates
done
systemctl --user set-property --runtime fedorahuge.slice MemoryMax=12G
for pass in p:0-15 e:16-31; do
    name=${pass%%:*} cpus=${pass#*:}
    say "pass $name on CPUs $cpus"
    st=0
    taskset -c "$cpus" python3 "$src/docs/research/measurements/dax-huge/measure.py" "$out/$name" \
        --window "$out/shards-window" --huge "$out/shards-huge" --huge-random "$out/shards-huge-random" \
        --work "$out/work" --parent-slice fedorahuge "$@" > "$out/$name.log" 2>&1 || st=$?
    say "pass $name: exit $st"
    python3 "$src/docs/research/measurements/dax-huge/rule.py" "$out/$name/out.txt" > "$out/$name-rule.txt" 2>&1 || true
done
say "done"
