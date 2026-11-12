#!/bin/sh
# Builds shards' unit-test binary (`cargo test -p shards --bin shards --profile test-release`)
# on a Linux runner: hyper-light/shards (public) checked out at REV in ~/ci-fluentd-epipe/repo,
# with ~/ci-fluentd-epipe/NAME.stream.rs, where given, laid over the stream the drivers share
# (crates/shards/src/daemon/ship/stream.rs): the variant under test, which is all a variant
# changes of what is compiled. Bounded (16 GiB, 4 CPUs, -j4) as a transient user unit of its
# own, so that it outlives the kiln agent that started it. The binary is kept as
# ~/ci-fluentd-epipe/bin/NAME; NAME.build.done holds the build's exit status.
#
#   kiln run -q --on RUNNER --no-sync --stdin -- 'cat > ~/ci-fluentd-epipe/NAME.stream.rs' < FILE
#   kiln run -q --on RUNNER --no-sync -- 'sh ~/ci-fluentd-epipe/linux-build.sh NAME REV'
#   # then poll ~/ci-fluentd-epipe/NAME.build.done and NAME.build.log
set -eu
name=$1 rev=$2
root=$HOME/ci-fluentd-epipe
rm -f "$root/$name.build.done"
systemd-run --user --unit="cife-build-$name" --collect -q \
	-p MemoryMax=16G -p CPUQuota=400% \
	sh -c '
		set -u
		name=$1 rev=$2 root=$3
		export PATH=$HOME/.local/bin:$HOME/.cargo/bin:$PATH
		export CARGO_TARGET_DIR=$root/target SHARDS_HELPERS=skip
		cd "$root/repo"
		{
			echo "== $(date -u +%FT%TZ) $(uname -srm) $name at $rev"
			git fetch -q origin && git checkout -q --force --detach "$rev" &&
				git log --oneline -1 &&
				if [ -f "$root/$name.stream.rs" ]; then
					cp "$root/$name.stream.rs" crates/shards/src/daemon/ship/stream.rs &&
						echo "stream.rs: $(sha256sum < crates/shards/src/daemon/ship/stream.rs)"
				fi &&
				rustc --version &&
				cargo test -p shards --bin shards --profile test-release -j4 --no-run --message-format=short
		} > "$root/$name.build.log" 2>&1
		status=$?
		# No braces: systemd expands ${...} in the unit'"'"'s command line.
		bin=$(grep -o "/[^ ()]*/test-release/deps/shards-[0-9a-f]*" "$root/$name.build.log" | tail -1)
		mkdir -p "$root/bin"
		if [ "$status" = 0 ] && [ -n "$bin" ]; then
			cp "$bin" "$root/bin/$name" || status=1
		else
			status=1
		fi
		echo "$status" > "$root/$name.build.done"
	' sh "$name" "$rev" "$root"
echo "started cife-build-$name"
