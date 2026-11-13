#!/usr/bin/env bash
# One harness's bring-up on a user's path (G24/P10, D54): the example build context with
# the publisher's verified binaries put in, `shards build image`, `shards run vm -d`, then
# the kernel's record of the microVM's processes, followed as it grows (`shards watch
# processes`) until the harness's own process has ended and then read whole (`shards read
# processes`), and what the run publishes (`shards read ports`).
#
#   bringup.sh SHARDS EXAMPLE_DIR BIN_DIR WORKDIR [ENTRY]
#
# BIN_DIR holds the harness's binaries (fetch.sh's OUTDIR/codex or OUTDIR/claude), copied
# whole as its `bin`. WORKDIR must not exist; it keeps the context, a home of its own
# (SHARDS_HOME), an empty docker config, and each command's output. ENTRY replaces the
# example's harness.json, to try another form of the same harness. The run's own command
# is the base image's `sleep infinity`, so the microVM outlives the harness however long it
# runs, until the script removes it; the harness's output itself is private to the run's
# owner and read by the real-VM test (crates/shards/tests/harness_bringup.rs). Nothing here
# keeps a clock: each wait ends at a command's exit or a row of the record.
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

# Each step's time, in milliseconds where date gives nanoseconds (GNU's %N), else in whole
# seconds (BSD's date, macOS's, prints %N's N as it is).
step() {
    local log=$1
    shift
    local start end status=0 unit=ms
    start=$(date +%s%N)
    case $start in *N) unit=s start=$(date +%s) ;; esac
    "$@" > "$work/$log" 2>&1 || status=$?
    if [ "$unit" = ms ]; then
        end=$(date +%s%N)
        printf '%s: exit %s in %s ms\n' "$log" "$status" $(((end - start) / 1000000))
    else
        end=$(date +%s)
        printf '%s: exit %s in %s s\n' "$log" "$status" $((end - start))
    fi
    return $status
}

finish() {
    local f
    for f in build.log run.log processes.log ports.log; do
        [ -f "$work/$f" ] && { printf '==== %s\n' "$f"; cat "$work/$f"; }
    done
    step rm.log "$shards" rm -f "$name" || true
    step stop.log "$shards" stop daemon || true
}
trap finish EXIT

step build.log "$shards" build image --progress=plain -t "harness-$name:1" "$work/ctx"
# busybox's sleep, as GNU's, takes "inf" for no end (busybox edca770d11, 1.31.0 on).
step run.log "$shards" run vm -d --name "$name" "harness-$name:1" sleep infinity
# The kernel's record of the members' processes (D123), followed as it grows until it holds
# the harness's exec and that process's exit, by the exec's PID, not a child's; or a `lost`
# row (init's, PID 0), past which it waits for no process's end; or until it ends with the
# run. Then it is read whole.
mkfifo "$work/watch.fifo"
"$shards" watch processes "$name" > "$work/watch.fifo" 2> "$work/watch.err" &
watcher=$!
pid=
while IFS= read -r line; do
    printf '%s\n' "$line" >> "$work/watched.log"
    case $line in
    *"  lost   "*)
        if [ "$(printf '%s\n' "${line%%  lost   *}" | awk '{print $NF}')" = 0 ]; then break; fi
        ;;
    esac
    if [ -z "$pid" ]; then
        pid=$(printf '%s\n' "$line" | awk -v p="  exec   /harness/$name/bin/$name " 'index($0, p) {for (i = 1; i <= NF; i++) if ($i == "exec") {print $(i - 1); exit}}')
    elif printf '%s\n' "$line" | awk -v pid="$pid" '{for (i = 1; i < NF; i++) if ($i == pid && $(i + 1) == "exit") f = 1} END {exit !f}'; then
        break
    fi
done < "$work/watch.fifo"
kill "$watcher" 2> /dev/null || true
wait "$watcher" 2> /dev/null || true
step processes.log "$shards" read processes "$name" || true
step ports.log "$shards" read ports "$name" || true
