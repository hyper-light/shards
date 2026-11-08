#!/bin/sh
# Where macOS's own log takes a program's lines, and how long one it keeps whole (PM M374):
# a syslog datagram of each of three levels sent to /var/run/syslog, the socket a syslog
# driver's local default sends to; the same through syslog(3) (logger); and os_log lines of
# 512 bytes to 64 KiB, then 1,008 to 1,024, read back with log show. No root needed.
set -eu
dir=$(mktemp -d)
cc -O1 -o "$dir/sizes" "$(dirname "$0")/sizes.c"
cc -O1 -o "$dir/edge" "$(dirname "$0")/edge.c"
tag="m374-$$"
python3 - "$tag" <<'PY'
import socket, sys, time
s = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
s.connect("/var/run/syslog")
for pri, level in [(30, "info"), (29, "notice"), (27, "err")]:
    s.send(("<%d>%s %s[%d]: datagram at %s\n" % (pri, time.strftime("%b %d %H:%M:%S"), sys.argv[1], 1234, level)).encode())
PY
logger -t "$tag" -p user.notice "through syslog3"
"$dir/sizes" "s$tag"
"$dir/edge" "e$tag"
sleep 2
echo "== the datagrams, in the unified log and in /var/log/system.log"
/usr/bin/log show --last 1m --style syslog --predicate 'eventMessage CONTAINS "datagram at"' | grep -v 'log run noninteractively' || true
grep -a "$tag" /var/log/system.log || echo "(none in system.log)"
echo "== syslog(3)"
/usr/bin/log show --last 1m --style syslog --predicate 'eventMessage CONTAINS "through syslog3"' | grep -v 'log run noninteractively' || true
for cat in "s$tag" "e$tag"; do
  echo "== os_log, category $cat: each line's length as kept"
  /usr/bin/log show --last 1m --style ndjson --predicate "subsystem == \"shards.m374\" AND category == \"$cat\"" |
    python3 -c '
import json, sys
for line in sys.stdin:
    if line.startswith("{"):
        m = json.loads(line).get("eventMessage", "")
        if m:
            print(m.split(":", 1)[0], len(m), "cut" if m.endswith("<…>") else "whole")'
done
sw_vers -productVersion
rm -rf "$dir"
