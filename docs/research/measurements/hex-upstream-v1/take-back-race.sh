#!/usr/bin/env bash
# Hex patch 0031 (R09/R10): after a taken-back guest is stopped and reaped, a second attach is
# refused; the two take-back tests in crates/api/src/client.rs accept the refusals of the reap
# landing at each step of Held::find but the stat file's open. Builds shards-api's lib test
# binary three ways and runs the whole lib RUNS times with each, counting each test's failures
# and the refusals the take-back tests saw:
#   red:         client.rs as committed (HEAD);
#   green:       the working tree's client.rs (OpenStat refused ENOENT or ESRCH accepted);
#   enoent-only: green accepting only OpenStat ENOENT, as Hex's 0031 has it.
# Restores the working tree's client.rs before it ends. From the repository root:
#   docs/research/measurements/hex-upstream-v1/take-back-race.sh LABEL [RUNS]
set -u
label=${1:?label}
runs=${2:-150}
if pgrep -x amphora-bench >/dev/null; then echo "amphora-bench is running: not starting" >&2; exit 75; fi
export TMPDIR=$HOME/g121-tmp
export PATH=$HOME/.local/share/shards-zig/zig-x86_64-linux-0.16.0:$PATH
out=$HOME/g121-tmp/hex-upstream-v1/$label
mkdir -p "$out"
source=crates/api/src/client.rs
cp "$source" "$out/green-client.rs"
git show HEAD:"$source" > "$out/red-client.rs"
sed 's/code: libc::ENOENT | libc::ESRCH,/code: libc::ENOENT,/' "$out/green-client.rs" > "$out/enoent-only-client.rs"
restore() { cp "$out/green-client.rs" "$source"; touch "$source"; }
trap restore EXIT
{ git rev-parse HEAD; uname -r; sha256sum "$out"/*-client.rs; diff "$out/red-client.rs" "$out/green-client.rs"; diff "$out/green-client.rs" "$out/enoent-only-client.rs"; } > "$out/identity.txt"
build() {
	cp "$out/$1-client.rs" "$source"
	touch "$source"
	systemd-run --user --scope -q -p MemoryMax=24G cargo test -j4 --target x86_64-unknown-linux-gnu -p shards-api --lib --no-run --message-format=json 2>"$out/$1-build.stderr" |
		python3 -c 'import json,sys
for l in sys.stdin:
    try: m=json.loads(l)
    except Exception: continue
    if m.get("reason")=="compiler-artifact" and m.get("executable") and m["target"]["kind"]==["lib"]: print(m["executable"])' | tail -1
}
for kind in red green enoent-only; do
	bin=$(build $kind)
	if [ -z "$bin" ]; then echo "$kind: build failed"; tail -5 "$out/$kind-build.stderr"; continue; fi
	cp "$bin" "$out/$kind.bin"
	: > "$out/$kind-failures.txt"
	: > "$out/$kind-refusals.txt"
	for i in $(seq 1 "$runs"); do
		"$out/$kind.bin" > "$out/$kind-run$i.log" 2>&1
		grep -E '^test .* FAILED$' "$out/$kind-run$i.log" | sed "s/^/run$i /" >> "$out/$kind-failures.txt"
		grep -A3 'attach_takes_back_a_guest_whose_[a-z_]* stdout' "$out/$kind-run$i.log" | grep -o 'failure: [A-Za-z]* {[^}]*}\|failure: [A-Za-z]*' | sed "s/^/run$i /" >> "$out/$kind-refusals.txt"
		grep -q 'test result: ok' "$out/$kind-run$i.log" && rm "$out/$kind-run$i.log"
	done
	echo "$kind: $(cut -d' ' -f1 "$out/$kind-failures.txt" | sort -u | wc -l) of $runs runs failed; failures by test:"
	awk '{print $3}' "$out/$kind-failures.txt" | sort | uniq -c
	echo "  take-back refusals that failed a test:"
	cut -d' ' -f2- "$out/$kind-refusals.txt" | sort | uniq -c
done
