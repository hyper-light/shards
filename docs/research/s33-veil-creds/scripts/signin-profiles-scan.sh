#!/bin/bash
# Streams a pinned archive (URL, its pinned sha256, a member path) through tar and greps the member for a pattern,
# writing nothing to disk but the matches and the archive's sha256. Usage: scan.sh URL SHA256 MEMBER PATTERN [CONTEXT]
set -euo pipefail
url=$1; want=$2; member=$3; pattern=$4; ctx=${5:-90}
sumfile=$(mktemp)
curl -sSfL "$url" | tee >(shasum -a 256 | cut -d' ' -f1 > "$sumfile") | tar -xzOf - "$member" \
  | rg -a -o --no-line-number "(?-u).{0,$ctx}(?:$pattern).{0,$ctx}" | LC_ALL=C tr -c '[:print:]\n' '.' || true
sleep 1
got=$(cat "$sumfile"); rm -f "$sumfile"
if [ "$got" = "$want" ]; then echo "# archive sha256 $got: the pin's"; else echo "# archive sha256 $got: NOT the pin's ($want)"; fi
