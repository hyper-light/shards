#!/usr/bin/env bash
# bringup.sh's wait on the kernel's record, held to v1's record of Codex's run with no VM,
# binaries or network: a stand-in `shards` and a stand-in `date` first on PATH.
#
#   bringup-test.sh [SCRIPT]    SCRIPT: the bringup.sh to hold to it, by default this one's
#
# The stand-in's `watch processes` gives the record v1 kept of Codex's run
# (records/codex/processes.log), whose children exit before Codex does, then keeps its
# follow open, as a follow does until its run ends; its `read processes` gives that record
# without Codex's exit, as a record read before the harness has ended is. bringup.sh is run
# twice: with a stand-in `date` that moves 301 s at each call, past any deadline of 300 s,
# and with this host's own `date`. It passes where each run ends its follow at Codex's own
# exit, not at a child's, and ends the follow it started.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
script=${1:-$here/bringup.sh}
tmp=$(mktemp -d)
# A script that leaves its follow running fails, and the follow is ended here.
trap 'kill "$(cat "$tmp/watcher" 2> /dev/null)" 2> /dev/null || true; rm -rf "$tmp"' EXIT
record=$here/records/codex/processes.log
pid=$(awk 'index($0, "  exec   /harness/codex/bin/codex ") {for (i = 1; i <= NF; i++) if ($i == "exec") {print $(i - 1); exit}}' "$record")
mkdir "$tmp/bin" "$tmp/clock" "$tmp/bins"
: > "$tmp/bins/codex"
cat > "$tmp/bin/shards" <<EOF
#!/usr/bin/env bash
case "\$1 \$2" in
"build image" | "rm -f" | "stop daemon") ;;
"run vm") echo stand-in ;;
"watch processes") echo \$\$ > "$tmp/watcher"; cat "$record"; exec tail -f /dev/null ;;
"read processes") grep -v -- " $pid  exit   " "$record" ;;
"read ports") echo "PORT" ;;
*) echo "stand-in shards: \$*" >&2; exit 2 ;;
esac
EOF
cat > "$tmp/clock/date" <<EOF
#!/usr/bin/env bash
n=\$((\$(cat "$tmp/calls" 2> /dev/null || echo 0) + 1))
echo \$n > "$tmp/calls"
case "\$1" in
+%s%N) echo "\$((1000000000 + n * 301))000000000" ;;
+%s) echo \$((1000000000 + n * 301)) ;;
*) exec /bin/date "\$@" ;;
esac
EOF
chmod +x "$tmp/bin/shards" "$tmp/clock/date"
# One run of the script, PATH led by $2: passes or says why.
held() {
    local run=$1 path=$2 status=0 last
    rm -f "$tmp/watcher"
    PATH="$path:$PATH" bash "$script" "$tmp/bin/shards" "$here/../../../examples/harness-codex" \
        "$tmp/bins" "$tmp/work-$run" > "$tmp/out-$run.log" 2>&1 || status=$?
    if [ -s "$tmp/watcher" ] && kill -0 "$(cat "$tmp/watcher")" 2> /dev/null; then
        kill "$(cat "$tmp/watcher")" 2> /dev/null || true
        echo "FAIL ($run): the follow it started still ran"
    elif [ "$status" -ne 0 ]; then
        echo "FAIL ($run): bringup.sh exit $status"
    elif [ ! -s "$tmp/work-$run/watched.log" ]; then
        echo "FAIL ($run): no follow of the record"
    else
        last=$(tail -n 1 "$tmp/work-$run/watched.log")
        case $last in
        *" $pid  exit   code 1")
            echo "PASS ($run): the follow ended at Codex's own exit ($(grep -c "" "$tmp/work-$run/watched.log") rows), its watch ended"
            return 0
            ;;
        *) echo "FAIL ($run): the follow ended at: $last" ;;
        esac
    fi
    sed 's/^/  | /' "$tmp/out-$run.log" | head -12
    return 1
}
failed=0
held stand-in-clock "$tmp/bin:$tmp/clock" || failed=1
held host-date "$tmp/bin" || failed=1
exit $failed
