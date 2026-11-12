#!/bin/sh
# OPA v1.14.1's builtins on cases.json, one case a process, each given LIMIT seconds
# (default 120): docs/research/measurements/ci-rego-compare/README.md. Needs Go 1.26 and
# timeout(1).
#
#   run.sh [LIMIT] > opa.jsonl
set -eu
here=$(cd "$(dirname "$0")" && pwd)
limit=${1:-120}
case $(go env GOVERSION) in
go1.26 | go1.26.*) ;;
*)
	echo "OPA v1.14.1 is held to with Go 1.26 (scripts/rego/generate); this is $(go env GOVERSION)" >&2
	exit 1
	;;
esac
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
printf 'module shards.opa\n\ngo 1.26\n' >"$work/go.mod"
cp "$here/opa_test.go" "$work/"
cd "$work"
go get github.com/open-policy-agent/opa@v1.14.1 >/dev/null 2>&1
go mod tidy >/dev/null 2>&1
go test -c -o opa.test . >/dev/null
echo "{\"go\": \"$(go env GOVERSION)\", \"opa\": \"v1.14.1\", \"host\": \"$(uname -srm)\", \"limit_seconds\": $limit}"
SHARDS_COMPARES="$here/compares.json" timeout "$limit" ./opa.test -test.run '^TestCompares$' -test.timeout 0 | grep '^{' || true
n=$(python3 -I -c 'import json,sys; print(len(json.load(open(sys.argv[1]))))' "$here/cases.json")
i=0
while [ "$i" -lt "$n" ]; do
	start=$(date +%s)
	if ! SHARDS_CASES="$here/cases.json" SHARDS_CASE=$i timeout "$limit" ./opa.test -test.run '^TestCase$' -test.timeout 0 >out 2>&1; then
		if [ $(($(date +%s) - start)) -ge "$limit" ]; then
			echo "{\"case\": $i, \"outcome\": \"still working\", \"seconds\": $limit}"
			i=$((i + 1))
			continue
		fi
	fi
	grep '^{' out || { echo "{\"case\": $i, \"outcome\": \"died\", \"detail\": $(tail -c 300 out | python3 -I -c 'import json,sys; print(json.dumps(sys.stdin.read()))')}"; }
	i=$((i + 1))
done
