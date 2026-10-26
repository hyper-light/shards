#!/bin/sh
# DAX_SMALLEST on Fedora (the plan's step 1 test and step 3): a release build of the synced
# tree with smallest.patch, window_costs, two homes seeded with alpine, then measure.py's
# matrix and sets on the performance cores and on the efficient cores. Run as a transient
# unit (systemd-run --user), its results in ~/dax-smallest. The build's sources are REV's
# (the code the Mac's numbers were taken of, with smallest.patch), whatever the mirror's
# working tree holds beyond it; the harness is the mirror's. Usage: step3.sh MIRROR REV
set -eu
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
mirror=$1 rev=$2
out=$HOME/dax-smallest
src=$HOME/dax-smallest-src
mkdir -p "$out" "$out/tmp"
# fedora's /tmp is memory: this job's temporary files on disk.
export TMPDIR="$out/tmp"
ulimit -n "$(ulimit -Hn)"
say() { echo "$(date -u +%FT%TZ) $*" | tee -a "$out/progress.txt"; }
say "start; free: $(free -m | awk '/^Mem/ {print $7}') MiB available; load $(cut -d' ' -f1 /proc/loadavg)"
rsync -a --delete --exclude target "$mirror/" "$src/"
git -C "$src" checkout "$rev" -- crates Cargo.toml Cargo.lock
(cd "$src" && git apply docs/research/measurements/dax-smallest/smallest.patch)
say "patched $(git -C "$src" rev-parse --short HEAD) + $(git -C "$src" diff --stat | tail -1)"
export CARGO_TARGET_DIR="$HOME/dax-smallest-target" CARGO_BUILD_JOBS=4
(cd "$src" && cargo build --release -p shards) > "$out/build.txt" 2>&1
cp "$CARGO_TARGET_DIR/release/shards" "$out/shards.new" && mv "$out/shards.new" "$out/shards"
say "built: $("$out/shards" --version)"
st=0
(cd "$src" && cargo test --release -p shards-vmm window_costs -- --ignored --nocapture) > "$out/window_costs.txt" 2>&1 || st=$?
say "window_costs: exit $st"
mkdir -p "$out/docker-config"
for h in fuse window; do
    [ -d "$out/home-$h/images" ] ||
        SHARDS_HOME="$out/home-$h" DOCKER_CONFIG="$out/docker-config" "$out/shards" pull -q mirror.gcr.io/library/alpine
done
say "homes seeded"
for pass in p:0-15 e:16-31; do
    name=${pass%%:*} cpus=${pass#*:}
    say "pass $name on CPUs $cpus"
    st=0
    taskset -c "$cpus" python3 "$src/docs/research/measurements/dax-smallest/measure.py" "$out/shards" \
        --fuse "$out/home-fuse" --window "$out/home-window" --dir "$out/data" \
        --sizes 4k,16k,32k,64k,256k,1m,2044k,2m --files 1000 --rounds 5 --image mirror.gcr.io/library/alpine \
        --sets --mix "$src/docs/research/measurements/dax-smallest/mix.txt" --small 70000 \
        --set-smallest 0,262144,1048576 --set-rounds 3 \
        --out "$out/$name-cores.txt" > "$out/$name-cores.log" 2>&1 || st=$?
    say "pass $name: exit $st"
    python3 "$src/docs/research/measurements/dax-smallest/rule.py" "$out/$name-cores.txt" > "$out/$name-rule.txt" 2>&1 || true
    s=$(sed -n 's/.*the rule.s S by the matrix: //p' "$out/$name-rule.txt")
    st=0
    say "pass $name: the rule's S by the matrix $s"
    # sustained-io's npm and cargo cells at 2 MiB and at this pass's S.
    arms=2097152
    case $s in *k) b=$(( ${s%k} * 1024 )) ;; *m) b=$(( ${s%m} * 1048576 )) ;; *) b= ;; esac
    [ -z "$b" ] || [ "$b" = 2097152 ] || arms="$arms,$b"
    taskset -c "$cpus" python3 "$src/docs/research/measurements/dax-smallest/cells.py" "$out/shards" \
        --home "$HOME/sio/home" --sio "$HOME/sio" --arms "$arms" --cells npm,cargo --rounds 5 \
        --rev "$(git -C "$src" rev-parse --short HEAD) + smallest.patch" --out "$out/$name-cells.txt" \
        > "$out/$name-cells.log" 2>&1 || st=$?
    say "pass $name cells ($arms): exit $st"
done
say "done"
touch "$out/.done"
