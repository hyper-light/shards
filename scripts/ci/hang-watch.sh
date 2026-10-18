#!/bin/bash
# CI on Linux, beside the tests: a test's `shards` command that has run 50 s (the tests'
# helper ends one at 60 s) is taken as hung, and the state of every process of its home is
# kept in OUT: ps, each thread's kernel stack and gdb's backtraces of the home's daemon and
# what runs under it (its spawner, and the VM, share and network processes it made). Once a
# command. The hang this is for: a microVM engine's command waiting on a named volume's
# first mount, twice on x86_64 CI (jobs 117501329133, 117577952136), never on a run of its
# own (240 rounds). Usage: scripts/ci/hang-watch.sh OUT
set -u
out=$1
mkdir -p "$out"

dump() {
    local cli=$1 at home daemon="" d p next tree
    at="$out/hang-$(date +%H%M%S)-$cli"
    mkdir -p "$at"
    tr '\0' ' ' < "/proc/$cli/cmdline" > "$at/command.txt" 2>/dev/null
    home=$(tr '\0' '\n' < "/proc/$cli/environ" 2>/dev/null | sed -n 's/^SHARDS_HOME=//p' | head -1)
    echo "home: $home" >> "$at/command.txt"
    ps -eLo pid,tid,ppid,pgid,etimes,stat,wchan:32,comm,args > "$at/ps.txt"
    [ -n "$home" ] && cp "$home/daemon.log" "$at/daemon.log" 2>/dev/null
    # The home's daemon, whose working directory is its home, and every process under it.
    for d in $(pgrep -f "shards daemon"); do
        [ "$(readlink "/proc/$d/cwd" 2>/dev/null)" = "$home" ] && daemon=$d
    done
    tree=$daemon
    next=$daemon
    while [ -n "$next" ]; do
        next=$(for p in $next; do ps -o pid= --ppid "$p"; done | tr -d ' ' | tr '\n' ' ')
        tree="$tree $next"
    done
    echo "tree: $tree" >> "$at/command.txt"
    for p in $tree; do
        {
            echo "== $p: $(tr '\0' ' ' < "/proc/$p/cmdline" 2>/dev/null)"
            for t in /proc/"$p"/task/*; do
                echo "-- thread ${t##*/} $(cat "$t/comm" 2>/dev/null) $(awk '{print $3}' "$t/stat" 2>/dev/null)"
                sudo cat "$t/stack" 2>/dev/null
            done
        } > "$at/kernel-$p.txt"
    done
    for p in $tree; do
        sudo timeout 15 gdb -p "$p" -batch -ex "set pagination off" -ex "thread apply all bt" > "$at/gdb-$p.txt" 2>&1 &
    done
    wait
}

seen=" "
while true; do
    for cli in $(pgrep -f "/target/[^ ]*/e2e/shards-[0-9a-f]+/shards |/target/e2e/shards-[0-9a-f]+/shards "); do
        case "$(tr '\0' ' ' < "/proc/$cli/cmdline" 2>/dev/null)" in
            *"shards daemon"* | *"shards spawner"* | *"shards share"*) continue ;;
        esac
        age=$(ps -o etimes= -p "$cli" 2>/dev/null | tr -d ' ')
        [ -n "$age" ] && [ "$age" -ge 50 ] || continue
        case "$seen" in *" $cli "*) continue ;; esac
        seen="$seen$cli "
        dump "$cli"
    done
    sleep 2
done
