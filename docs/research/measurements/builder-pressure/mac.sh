#!/bin/sh
# What a give-back costs and returns on a Mac, where no unprivileged process may raise the
# system's pressure: a release build of REV's tree with builder-cold's cold.patch and this
# harness's give.patch, then measure.py's rounds (held, its builds told every 2 s that their
# host is short of memory), its home in ~/builder-pressure/home, then tables.py's tables, its
# results in ~/builder-pressure. Run as a launchd job (macos-sched/launchd-run.sh), as a
# user's program runs. The build's sources are REV's, whatever the mirror's working tree holds
# beyond it; the harness is the mirror's. Usage: mac.sh MIRROR REV [ROUNDS]
set -eu
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:/usr/bin:/bin:/usr/sbin:/sbin:$PATH"
mirror=$1 rev=$2 rounds=${3:-5}
out=$HOME/builder-pressure
src=$HOME/builder-pressure-src
mkdir -p "$out"
say() { echo "$(date -u +%FT%TZ) $*" | tee -a "$out/progress.txt"; }
say "start; free: $(df -h "$HOME" | awk 'NR==2 {print $4}'); load $(sysctl -n vm.loadavg)"
rsync -a --delete --exclude target "$mirror/" "$src/"
git -C "$src" checkout "$rev" -- crates Cargo.toml Cargo.lock
(cd "$src" && git apply docs/research/measurements/builder-cold/cold.patch \
    && git apply docs/research/measurements/builder-pressure/give.patch)
export CARGO_TARGET_DIR="$HOME/builder-pressure-target"
(cd "$src" && cargo build --release -p shards) > "$out/build.txt" 2>&1
cp "$CARGO_TARGET_DIR/release/shards" "$out/shards.new" && mv "$out/shards.new" "$out/shards"
say "built: $("$out/shards" --version)"
st=0
python3 -I "$src/docs/research/measurements/builder-pressure/measure.py" "$out/shards" "$out/run" --rounds "$rounds" \
    --home "$out/home" > "$out/measure.log" 2>&1 || st=$?
say "measure: exit $st"
python3 -I "$src/docs/research/measurements/builder-pressure/tables.py" "$out/run/out.txt" > "$out/tables.txt" 2>&1 || true
say "done"
