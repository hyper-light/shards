#!/bin/bash
# Experiment (not committed): where a restart over the kept file spends its time.
export PATH=$HOME/.local/bin:$HOME/.cargo/bin:$PATH TMPDIR=$HOME/p-d190-tmp
cd ~/p-d190-src && (git worktree add -f ~/p-d190-exp HEAD >/dev/null 2>&1 || true)
cd ~/p-d190-exp && git checkout -q --detach origin/p/d190 && git checkout -q -- . && git apply ~/p-d190-exp.patch && git log --oneline -1
CARGO_TARGET_DIR=$HOME/p-d190-target cargo build -j 8 --release -p shards 2>&1 | tail -1
S=$HOME/p-d190-target/release/shards
H=$(mktemp -d $HOME/p-d190-tmp/exp-XXXX); mkdir -p $H/dc
export SHARDS_HOME=$H DOCKER_CONFIG=$H/dc
$S pull -q alpine >/dev/null
$S run --name warm alpine true
$S run --name x alpine true
for i in $(seq 1 12); do
  t0=$(date +%s%N); $S start -a x; t1=$(date +%s%N)
  sleep 0.3
  id=$($S inspect -f '{{.Id}}' x)
  echo "round $i start $(( (t1-t0)/1000 )) us: $(debugfs -R 'cat /over-timing' $H/containers/$id/layer.ext4 2>/dev/null | tr '\n' ' ')"
done
grep -E 'EXP' $H/daemon.log | tail -60
$S daemon stop >/dev/null 2>&1
rm -rf $H
