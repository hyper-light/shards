#!/bin/bash
# Runs one E2E test of tests/containers.rs on adacachy's KVM (argv[1] its name, argv[2] the log's
# name), and, once it prints the paused microVM's VM process, waits 10 s for the signal and the
# unpause to be sent, then takes that process's threads: each one's name, state and kernel wait
# channel, its user stacks (eu-stack, as root: ptrace_scope is 1) and the kernel stacks of the
# threads that send to the guest and relay the daemon's messages.
cd ~/g121-tmp/to-guest2/src || exit 1
export PATH="$(cat ../zigdir):$PATH" CARGO_TARGET_DIR=$HOME/g121-tmp/to-guest2/target
name=$1 out=../logs/$2.out log=../logs/$2.log
echo "# $name: $(git rev-parse --short=12 HEAD) $(git status --short | wc -l) changed; $(date -u +%FT%TZ) $(uname -srm); $(rustc --version); load $(cut -d' ' -f1-3 /proc/loadavg)" > $log
( nice -n 10 cargo test --profile test-release -p shards --test containers -- "$name" --exact --nocapture --test-threads 1 > $out 2>&1; echo "## test exit $? $(date -u +%T)" >> $log ) &
tpid=$!
pid=
for _ in $(seq 1 900); do
	pid=$(grep -oP "the paused microVM's VM process: \K[0-9]+" $out 2>/dev/null | head -1)
	[ -n "$pid" ] && break
	kill -0 $tpid 2>/dev/null || break
	sleep 1
done
if [ -n "$pid" ]; then
	echo "# its VM process $pid, $(date -u +%T)" >> $log
	sleep 10
	{
		echo "## its threads, $(date -u +%T): tid, name, state, wait channel"
		for t in /proc/$pid/task/*; do echo "$(basename $t) $(cat $t/comm) $(awk '{print $3}' $t/stat) $(cat $t/wchan)"; done
		echo "## eu-stack -p $pid"
		sudo -n eu-stack -p $pid 2>&1 | head -600
		echo "## kernel stacks"
		for t in /proc/$pid/task/*; do
			c=$(cat $t/comm)
			case $c in exec | daemon-signals | signal-send | signal-conn) echo "--- $(basename $t) $c"; sudo -n cat $t/stack ;; esac
		done
	} >> $log 2>&1
fi
wait $tpid
grep -E "^test |panicked|left:|right:|test result|did not|timed out" $out | head -40 >> $log
echo "# done $(date -u +%T)" >> $log
