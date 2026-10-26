#!/bin/sh
# What a builder holds cold, on Fedora: a release build of REV's tree with cold.patch, then
# measure.py's rounds of the real builds, its home kept in ~/builder-cold/home (the home's
# Btrfs) between runs. Run as a transient unit (systemd-run --user, a MemoryMax of its own),
# its results in ~/builder-cold. The build's sources are REV's, whatever the mirror's working
# tree holds beyond it; the harness is the mirror's. Usage: fedora.sh MIRROR REV [ROUNDS]
set -eu
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
mirror=$1 rev=$2 rounds=${3:-5}
out=$HOME/builder-cold
src=$HOME/builder-cold-src
mkdir -p "$out" "$out/tmp"
# fedora's /tmp is memory: this job's temporary files on disk.
export TMPDIR="$out/tmp"
ulimit -n "$(ulimit -Hn)"
say() { echo "$(date -u +%FT%TZ) $*" | tee -a "$out/progress.txt"; }
say "start; free: $(free -m | awk '/^Mem/ {print $7}') MiB available; load $(cut -d' ' -f1 /proc/loadavg)"
rsync -a --delete --exclude target "$mirror/" "$src/"
git -C "$src" checkout "$rev" -- crates Cargo.toml Cargo.lock
(cd "$src" && git apply docs/research/measurements/builder-cold/cold.patch)
export CARGO_TARGET_DIR="$HOME/builder-cold-target" CARGO_BUILD_JOBS=4
(cd "$src" && cargo build --release -p shards) > "$out/build.txt" 2>&1
cp "$CARGO_TARGET_DIR/release/shards" "$out/shards.new" && mv "$out/shards.new" "$out/shards"
say "built: $("$out/shards" --version)"
st=0
python3 -I "$src/docs/research/measurements/builder-cold/measure.py" "$out/shards" "$out/run" --rounds "$rounds" \
    --home "$out/home" > "$out/measure.log" 2>&1 || st=$?
say "measure: exit $st"
python3 -I "$src/docs/research/measurements/builder-cold/tables.py" "$out/run/out.txt" > "$out/tables.txt" 2>&1 || true
say "done"
