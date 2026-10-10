#!/bin/sh
# M160: what deeply nested policies cost shards' Rego (crates/rego/benches/nesting.rs)
# and OPA v1.14.1 (set up as scripts/rego/bench-opa sets it up, run as buildx runs a
# policy check), shape by shape of crates/rego/benches/nesting.json, one case a process
# so that each peak is the case's own. OPA's deep cases take it minutes to hours a run:
# they run once, stopped past a deadline or 32 GiB resident, and recorded as such.
#
#   docs/research/measurements/rego-nesting/run.sh [N]
#
# N (default 5) is how many times shards checks each case, after a warm-up.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../../.." && pwd)
shapes=$root/crates/rego/benches/nesting.json
n=${1:-5}
tag=v0.37.1
builtins_go=3864f6ebfc6eb97ddbc05bef8f380db30b54a81b4359cad23ba5bf06f56f26c2
builtins_rego=8990a878e2fa88b07ff2da57f2476bb79953d50f17823298d541f1d86761c8e2

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
fetch() {
	curl -fsSL -A shards-dev -o "$work/$1" "https://raw.githubusercontent.com/docker/buildx/$tag/policy/$1"
	if [ "$(shasum -a 256 "$work/$1" | cut -d' ' -f1)" != "$2" ]; then
		echo "buildx $tag policy/$1 is not the file pinned ($2)" >&2
		exit 1
	fi
}
fetch builtins.go "$builtins_go"
fetch builtins.rego "$builtins_rego"
sed -i.orig 's/^package policy$/package oracle/' "$work/builtins.go"
rm "$work/builtins.go.orig"
printf 'module shards.oracle\n\ngo 1.26\n' >"$work/go.mod"
cp "$root/scripts/rego/oracle_test.go" "$root/scripts/rego/bench_test.go" "$here/nesting_test.go" "$work/"
(cd "$work" && go get github.com/open-policy-agent/opa@v1.14.1 >/dev/null 2>&1 &&
	go mod tidy >/dev/null 2>&1 && go test -c -o "$work/opa.test" .)

echo "host: $(sysctl -n machdep.cpu.brand_string 2>/dev/null || uname -m); os: $(uname -sr);" \
	"revision: $(git -C "$root" rev-parse --short HEAD); load: $(uptime | sed 's/.*load averages*: //')"

# shards, N times a case.
cd "$root"
cargo bench -p shards-rego --bench nesting --no-run >/dev/null 2>&1
for c in every-unused:1000 every-unused:5000 every-unused:20000 every-unused:99990 \
	every-used:24 every-used:1000 every-used:4000 every-used:99990 every-printing:16 \
	every-printing:4000 template-failing:5 template-failing:20 template-failing:30 \
	else-chain:10000 else-chain:100000 rule-chain:2000 rule-chain:10000 rule-wrap:2000 \
	rule-wrap:10000 comprehension-nested:10 comprehension-nested:12 \
	every-comprehension-alternating:16 every-comprehension-alternating:20 \
	every-declaring-after:1000 every-declaring-after:2000 prints-wide:500 prints-wide:1000 \
	closures-wide:500 closures-wide:1000; do
	cargo bench -p shards-rego --bench nesting -- --shape "${c%%:*}" --depth "${c#*:}" --n "$n" 2>/dev/null |
		grep '^shards-bench ' | sed 's/^shards-bench //'
done

# OPA, once a case: SHAPE:DEPTH:DEADLINE_S.
cap=$((32 * 1024 * 1024))
for c in every-unused:1000:600 every-unused:5000:1800 every-used:16:600 every-used:20:600 \
	every-used:24:1800 every-printing:16:600 template-failing:5:600 template-failing:16:600 \
	template-failing:20:600 else-chain:10000:1800 else-chain:100000:3600 rule-chain:2000:1800 \
	rule-chain:10000:3600 rule-wrap:2000:600 rule-wrap:10000:1800 comprehension-nested:10:600 \
	comprehension-nested:12:600 every-comprehension-alternating:12:1800 \
	every-comprehension-alternating:16:3600 prints-wide:500:1800 prints-wide:1000:3600 \
	closures-wide:500:1800 closures-wide:1000:3600; do
	shape=${c%%:*}
	rest=${c#*:}
	depth=${rest%%:*}
	deadline=${rest#*:}
	NESTING_SHAPES="$shapes" NESTING_SHAPE="$shape" NESTING_DEPTH="$depth" NESTING_N=1 \
		"$work/opa.test" -test.run '^TestNesting$' -test.v -test.timeout "${deadline}s" >"$work/out" 2>&1 &
	pid=$!
	stopped=""
	while kill -0 "$pid" 2>/dev/null; do
		rss=$(ps -o rss= -p "$pid" | tr -d ' ')
		if [ -n "$rss" ] && [ "$rss" -gt "$cap" ]; then
			kill "$pid"
			stopped="stopped past 32 GiB resident"
		fi
		sleep 2
	done
	wait "$pid" || true
	out=$(grep -E '^[{]' "$work/out" || true)
	if [ -z "$out" ]; then
		why=${stopped:-"no answer within $deadline s"}
		out=$(printf '{"bench":"opa-nesting","case":"%s-%s","outcome":"%s"}' "$shape" "$depth" "$why")
	fi
	printf "%s\n" "$out"
done
