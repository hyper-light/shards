#!/usr/bin/env bash
# One harness's bring-up on a user's path (G24/P10, D54): the example build context with
# the publisher's verified binaries put in, `shards build image`, `shards run vm -d`, then
# what the kernel recorded of the microVM's processes (`shards read processes`) until the
# harness's own process has ended, and what the run publishes (`shards read ports`).
#
#   bringup.sh SHARDS EXAMPLE_DIR BIN_DIR WORKDIR [ENTRY]
#
# BIN_DIR holds the harness's binaries (fetch.sh's OUTDIR/codex or OUTDIR/claude), copied
# whole as its `bin`. WORKDIR must not exist; it keeps the context, a home of its own
# (SHARDS_HOME), an empty docker config, and each command's output. ENTRY replaces the
# example's harness.json, to try another form of the same harness. The run's own command
# is the base image's `sleep`, so the microVM outlives the harness; the harness's output
# itself is private to the run's owner and read by the real-VM test
# (crates/shards/tests/harness_bringup.rs).
set -euo pipefail

shards=${1:?usage: bringup.sh SHARDS EXAMPLE_DIR BIN_DIR WORKDIR [ENTRY]}
example=${2:?}
bins=${3:?}
work=${4:?}
entry=${5:-}
name=$(basename "$example")
name=${name#harness-}
mkdir "$work"
work=$(cd "$work" && pwd)
mkdir -p "$work/ctx/$name/bin" "$work/home" "$work/docker-config"
cp "$example/Agentfile" "$work/ctx/Agentfile"
if [ -n "$entry" ]; then
    printf '%s\n' "$entry" > "$work/ctx/$name/harness.json"
else
    cp "$example/$name/harness.json" "$work/ctx/$name/harness.json"
fi
cp "$bins"/* "$work/ctx/$name/bin/"
chmod 0755 "$work/ctx/$name/bin"/*
printf '{}\n' > "$work/docker-config/config.json"
export SHARDS_HOME="$work/home" DOCKER_CONFIG="$work/docker-config" SHARDS_LOCAL_STORE=none
unset DOCKER_AUTH_CONFIG SHARDS_DEFAULT_PLATFORM DOCKER_DEFAULT_PLATFORM

step() {
    local log=$1
    shift
    local start end status=0
    start=$(date +%s%N)
    "$@" > "$work/$log" 2>&1 || status=$?
    end=$(date +%s%N)
    printf '%s: exit %s in %s ms\n' "$log" "$status" $(((end - start) / 1000000))
    return $status
}

finish() {
    local f
    for f in build.log run.log "processes-${n:-0}.log" ports.log; do
        [ -f "$work/$f" ] && { printf '==== %s\n' "$f"; cat "$work/$f"; }
    done
    step rm.log "$shards" rm -f "$name" || true
    step stop.log "$shards" stop daemon || true
}
trap finish EXIT

step build.log "$shards" build image --progress=plain -t "harness-$name:1" "$work/ctx"
step run.log "$shards" run vm -d --name "$name" "harness-$name:1" sleep 600
# The kernel's record of the members' processes: the harness's exec, and its exit. The
# script's patience, never runtime policy: both harnesses end within a minute without a
# credential (README).
deadline=$(($(date +%s) + 300))
n=0
while :; do
    step "processes-$n.log" "$shards" read processes "$name" || true
    if grep -q " exit " "$work/processes-$n.log" && grep -q "/harness/$name/bin/$name" "$work/processes-$n.log"; then
        pid=$(awk -v p="/harness/$name/bin/$name" '$0 ~ p && / exec / {for (i = 1; i <= NF; i++) if ($i == "exec") {print $(i - 1); exit}}' "$work/processes-$n.log")
        if [ -n "$pid" ] && awk -v pid="$pid" '{for (i = 1; i < NF; i++) if ($i == pid && $(i + 1) == "exit") f = 1} END {exit !f}' "$work/processes-$n.log"; then
            break
        fi
    fi
    if [ "$(date +%s)" -ge "$deadline" ]; then
        echo "the harness's process did not end within 300 s"
        break
    fi
    n=$((n + 1))
    sleep 2
done
step ports.log "$shards" read ports "$name" || true
