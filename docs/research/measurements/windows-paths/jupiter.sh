#!/usr/bin/env bash
# Checks shards out on Windows as its consumers do, core.longpaths unset (Git for Windows'
# bash; run on jupiter): `git clone` of BRANCH into a directory whose path is CLONE characters
# long, and `cargo fetch` of a crate that takes shards-abi by git at REV, under a CARGO_HOME
# whose path is HOME characters long, for each HOME given. Only lengths matter to the limits,
# so each directory is a padded name of the length a consumer's has (README.md).
#
#   jupiter.sh WORK BRANCH REV CLONE HOME...
set -u
work=$1 branch=$2 rev=$3 clone=$4
shift 4
url=https://github.com/hyper-light/shards
mkdir -p "$work"
work=$(cd "$work" && pwd)
# A directory under $work whose Windows path is $1 characters long.
padded() {
	local base n
	base=$(cygpath -w "$work")
	n=$(($1 - ${#base} - 1))
	[ "$n" -ge 1 ] || { echo "$work is too long for $1" >&2; exit 2; }
	printf '%s/%s' "$work" "$(head -c "$n" /dev/zero | tr '\0' "${2:-p}")"
}
echo "== $(git --version), $(cargo --version); core.longpaths: $(git config --get core.longpaths || echo unset)"

dir=$(padded "$clone" c)
rm -rf "$dir"
for try in 1 2 3 4 5; do
	git -c core.longpaths=false clone --depth 1 --branch "$branch" "$url" "$dir" >"$work/clone.log" 2>&1
	status=$?
	grep -q -E 'early EOF|index-pack|unable to access' "$work/clone.log" || break
	echo "clone $try: the network failed; again"
	rm -rf "$dir"
	sleep 20
done
echo "== clone of $branch into $(cygpath -w "$dir") ($clone characters): exit $status," \
	"$(tr '\r' '\n' <"$work/clone.log" | grep -c 'Filename too long') files refused, HEAD $(git -C "$dir" rev-parse HEAD 2>&1)"
tr '\r' '\n' <"$work/clone.log" | grep -E '^(error|fatal|warning):' | head -3
[ "$status" -eq 0 ] && echo "   files $(git -C "$dir" ls-files | wc -l), status: $(git -C "$dir" status --porcelain | wc -l) changed," \
	"longest path $(git -C "$dir" ls-files | awk '{ if (length($0) > n) n = length($0) } END { print n }')"

mkdir -p "$work/consumer/src"
printf 'fn main() {}\n' >"$work/consumer/src/main.rs"
cat >"$work/consumer/Cargo.toml" <<TOML
[package]
name = "consumer"
version = "0.0.0"
edition = "2021"
publish = false

[dependencies]
shards-abi = { git = "$url", rev = "$rev" }

[workspace]
TOML
for len in "$@"; do
	home=$(padded "$len" h)
	rm -rf "$home" "$work/consumer/Cargo.lock"
	mkdir -p "$home"
	(cd "$work/consumer" && CARGO_HOME=$(cygpath -w "$home") cargo fetch) >"$work/fetch-$len.log" 2>&1
	status=$?
	checkout=$(ls -d "$home"/git/checkouts/shards-*/* 2>/dev/null | head -1)
	echo "== cargo fetch at $rev, CARGO_HOME $(cygpath -w "$home") ($len characters): exit $status," \
		"checkout $(cygpath -w "$checkout" 2>/dev/null) ($(($(cygpath -w "$checkout" | tr -d '\n' | wc -c) + 1)) with its separator)"
	grep -E 'path too long|^error' "$work/fetch-$len.log" | head -2
	if [ "$status" -eq 0 ]; then
		longest=$(cd "$checkout" && git ls-files | awk '{ if (length($0) > n) { n = length($0); p = $0 } } END { print p }')
		echo "   its longest file: $(($(cygpath -w "$checkout/$longest" | tr -d '\n' | wc -c))) characters, present: $([ -f "$checkout/$longest" ] && echo yes)"
	fi
	rm -rf "$home"
done
