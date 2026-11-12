#!/bin/sh
# Runs one unit test of a test binary N times, P at once, each run alone in its own process
# (`--exact NAME --test-threads 1`), and counts its failures; each failing run's panic line
# kept. P runs at once load the host as CI's runners are loaded running the whole suite.
#
#   repeat.sh BINARY TEST N P LOG
set -u
bin=$1 test=$2 n=$3 p=$4 log=$5
: > "$log"
seq "$n" | xargs -P "$p" -I{} sh -c '
	out=$("$1" --exact "$2" --test-threads 1 2>&1); s=$?
	if [ $s -ne 0 ]; then
		printf "run %s FAILED: %s\n" "$3" "$(printf "%s\n" "$out" | grep -A1 "panicked at" | tr "\n" " ")"
	else
		printf "run %s ok\n" "$3"
	fi' sh "$bin" "$test" {} >> "$log"
ok=$(grep -c " ok$" "$log")
failed=$(grep -c " FAILED: " "$log")
echo "$test: n=$n parallel=$p ok=$ok failed=$failed"
grep " FAILED: " "$log" | sed 's/^run [0-9]* FAILED: //' | sort | uniq -c | sort -rn | head -10
