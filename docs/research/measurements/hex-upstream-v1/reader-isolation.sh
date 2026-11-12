#!/usr/bin/env bash
# Hex patch 0030 (R09/R10): the three taken::linux reader tests count /proc/self/fd, every
# descriptor of the test process, while the lib's other tests open and close their own on
# other threads. Builds shards-api's lib test binary three ways and runs each RUNS times with
# libtest's default threads, the whole lib at once:
#   red:      reader-tests.rs as committed (HEAD), the counts taken in the shared process;
#   green:    the working tree's reader-tests.rs, each counting test run alone in a child;
#   mutation: green with the isolation removed (each wrapper calls its _alone body in-process).
# Restores the working tree's fixture before it ends. Run from the repository root:
#   docs/research/measurements/hex-upstream-v1/reader-isolation.sh LABEL [RUNS]
set -u
label=${1:?label}
runs=${2:-30}
if pgrep -x amphora-bench >/dev/null; then echo "amphora-bench is running: not starting" >&2; exit 75; fi
export TMPDIR=$HOME/g121-tmp
export PATH=$HOME/.local/share/shards-zig/zig-x86_64-linux-0.16.0:$PATH
out=$HOME/g121-tmp/hex-upstream-v1/$label
mkdir -p "$out"
fixture=docs/research/measurements/managed-proc-main/reader-tests.rs
cp "$fixture" "$out/green-fixture.rs"
git show HEAD:"$fixture" > "$out/red-fixture.rs"
sed -E 's/^    alone\("([a-z_]+)"\);$/    \1();/' "$out/green-fixture.rs" > "$out/mutation-fixture.rs"
restore() { cp "$out/green-fixture.rs" "$fixture"; }
trap restore EXIT
build() {
	cp "$out/$1-fixture.rs" "$fixture"
	touch "$fixture" crates/api/src/taken.rs
	systemd-run --user --scope -q -p MemoryMax=24G cargo test -j4 --target x86_64-unknown-linux-gnu -p shards-api --lib --no-run --message-format=json 2>"$out/$1-build.stderr" |
		python3 -c 'import json,sys
for l in sys.stdin:
    try: m=json.loads(l)
    except Exception: continue
    if m.get("reason")=="compiler-artifact" and m.get("executable") and m["target"]["kind"]==["lib"]: print(m["executable"])' | tail -1
}
{ git rev-parse HEAD; sha256sum "$out"/*-fixture.rs; diff -u "$out/green-fixture.rs" "$out/mutation-fixture.rs"; } > "$out/identity.txt"
for kind in red green mutation; do
	bin=$(build $kind)
	if [ -z "$bin" ]; then echo "$kind: build failed"; tail -5 "$out/$kind-build.stderr"; continue; fi
	cp "$bin" "$out/$kind.bin"
	fails=0
	: > "$out/$kind-failures.txt"
	for i in $(seq 1 "$runs"); do
		if ! "$out/$kind.bin" > "$out/$kind-run$i.log" 2>&1; then
			fails=$((fails + 1))
			grep -E '^test .* FAILED$' "$out/$kind-run$i.log" | sed "s/^/run$i /" >> "$out/$kind-failures.txt"
		fi
	done
	echo "$kind: $fails of $runs runs failed; by test:"
	awk '{print $3}' "$out/$kind-failures.txt" | sort | uniq -c
	grep -h '^test result' "$out/$kind-run1.log"
done
