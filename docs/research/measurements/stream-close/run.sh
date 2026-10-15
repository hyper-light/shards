#!/bin/sh
# M238 (docs/research/platform-measurements.md): a log stream's close, under load, on Linux.
# Builds shards' unit tests for this machine's Linux (static musl), and runs
# daemon::ship::stream::tests::a_stream_closed_delivers_all_it_wrote in a microVM of two
# vCPUs, eight loops at once of 25 runs each (load.sh), ROUNDS times: each run closes 20
# TLS streams. probe.patch, applied for the run at 78811e3b, before the fix, has a close
# that fails say the socket's TCP_INFO and what the server got; load.sh then prints the
# guest kernel's TCP resets and aborts. Needs scripts/install-zig's zig, and SHARDS_KERNEL
# and SHARDS_INIT naming a guest; a home of its own in SHARDS_HOME.
#
#   run.sh SHARDS [ROUNDS]
set -eu
shards=$1
rounds=${2:-6}
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../../.." && pwd)
arch=$(uname -m | sed 's/arm64/aarch64/')
triple=$arch-unknown-linux-musl
cd "$root"
built=$(env SHARDS_HELPERS=skip SHARDS_INIT_BINARY="$SHARDS_INIT" \
	"CC_$(echo "$triple" | tr - _)=$root/scripts/zig-cc" "AR_$(echo "$triple" | tr - _)=$root/scripts/zig-ar" \
	ZIG_TARGET="$arch-linux-musl" "CARGO_TARGET_$(echo "$triple" | tr a-z- A-Z_)_LINKER=rust-lld" \
	cargo test -p shards --bin shards --target "$triple" --profile test-release --no-run --message-format=json \
	| sed -n 's/.*"executable":"\([^"]*\)".*/\1/p' | tail -1)
deps=$(mktemp -d)
trap 'rm -r -- "$deps"' EXIT
cp "$built" "$deps/shards-tests"
cp "$here/load.sh" "$deps/load.sh"
round=0
while [ "$round" -lt "$rounds" ]; do
	round=$((round + 1))
	echo "round $round"
	"$shards" run --rm --cpus 2 -u 1000:1000 -e TMPDIR=/scratch --tmpfs /scratch:mode=1777 \
		-v "$deps:/t:ro" alpine:3.22 /bin/sh /t/load.sh | grep -v ' done$'
done
