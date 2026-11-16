#!/bin/bash
# PM M611: a freshly written executable's first exec against its second, and which processes gain CPU while the
# first runs. BINARY defaults to a test-release `shards`; N copies, each written anew, each exec'd twice.
#   docs/research/measurements/first-exec/first-exec.sh [BINARY] [N]
set -u
B=${1:-${CARGO_TARGET_DIR:-target}/test-release/shards}
N=${2:-4}
TIMEFORMAT='%R'
cpu() { ps -A -o pid=,time=,comm= | awk '{n=split($2,a,":"); s=0; for(i=1;i<=n;i++) s=s*60+a[i]; print $1, s, $3}'; }
echo "binary $B, $(wc -c <"$B" | tr -d ' ') bytes, $(sw_vers -productVersion 2>/dev/null || uname -sr)"
for i in $(seq 1 "$N"); do
	d=$(mktemp -d "${TMPDIR:-/tmp}/first-exec.XXXXXX")
	cp "$B" "$d/x"
	cpu >"$d/before"
	first=$( { time "$d/x" --version >/dev/null 2>&1; } 2>&1 )
	cpu >"$d/after"
	second=$( { time "$d/x" --version >/dev/null 2>&1; } 2>&1 )
	echo "copy $i: first ${first}s, second ${second}s"
	awk 'NR==FNR{b[$1]=$2; next} ($1 in b) && $2-b[$1]>=0.05 {printf "  +%.2fs CPU %s\n", $2-b[$1], $3}' "$d/before" "$d/after" | sort -r | head -4
	rm -rf "$d"
done
