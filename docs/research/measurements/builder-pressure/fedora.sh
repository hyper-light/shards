#!/bin/sh
# What a builder gives back under its host's memory pressure, on Fedora: a release build of
# REV's tree with builder-cold's cold.patch and this harness's give.patch, then measure.py's
# rounds, its home in ~/builder-pressure/home, then tables.py's tables. Run as a transient unit
# (systemd-run --user, a MemoryMax of its own), its results in ~/builder-pressure. The build's
# sources are REV's, whatever the mirror's working tree holds beyond it; the harness is the
# mirror's. Usage: fedora.sh MIRROR REV [ROUNDS]
set -eu
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
mirror=$1 rev=$2 rounds=${3:-5}
out=$HOME/builder-pressure
src=$HOME/builder-pressure-src
mkdir -p "$out" "$out/tmp"
# fedora's /tmp is memory: this job's temporary files on disk.
export TMPDIR="$out/tmp"
ulimit -n "$(ulimit -Hn)"
say() { echo "$(date -u +%FT%TZ) $*" | tee -a "$out/progress.txt"; }
say "start; free: $(free -m | awk '/^Mem/ {print $7}') MiB available; load $(cut -d' ' -f1 /proc/loadavg)"
rsync -a --delete --exclude target "$mirror/" "$src/"
git -C "$src" checkout "$rev" -- crates Cargo.toml Cargo.lock
(cd "$src" && git apply docs/research/measurements/builder-cold/cold.patch \
    && git apply docs/research/measurements/builder-pressure/give.patch)
export CARGO_TARGET_DIR="$HOME/builder-pressure-target" CARGO_BUILD_JOBS=4
(cd "$src" && cargo build --release -p shards) > "$out/build.txt" 2>&1
cp "$CARGO_TARGET_DIR/release/shards" "$out/shards.new" && mv "$out/shards.new" "$out/shards"
say "built: $("$out/shards" --version)"
st=0
python3 -I "$src/docs/research/measurements/builder-pressure/measure.py" "$out/shards" "$out/run" --rounds "$rounds" \
    --home "$out/home" > "$out/measure.log" 2>&1 || st=$?
say "measure: exit $st"
python3 -I "$src/docs/research/measurements/builder-pressure/tables.py" "$out/run/out.txt" > "$out/tables.txt" 2>&1 || true
say "done"
