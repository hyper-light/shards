#!/bin/bash
# Runs an experiment of tests/containers.rs (argv[1] its name, argv[2] the log's name) on
# adacachy's KVM, and once its output has not grown for 12 s, a VM process named in it, takes
# that process's threads once: each one's name, state and kernel wait channel, its user
# and kernel stacks of the threads that send to the guest, relay the daemon's messages and pause
# (eu-stack and /proc/TID/stack, as root: ptrace_scope is 1).
cd ~/g121-tmp/to-guest2/src || exit 1
export PATH="$(cat ../zigdir):$PATH" CARGO_TARGET_DIR=$HOME/g121-tmp/to-guest2/target
name=$1 out=../logs/$2.out log=../logs/$2.log
echo "# $name: $(git rev-parse --short=12 HEAD) $(git status --short | wc -l) changed; $(date -u +%FT%TZ) $(uname -srm); $(rustc --version); load $(cut -d' ' -f1-3 /proc/loadavg)" > $log
( nice -n 10 cargo test --profile test-release -p shards --test containers -- "$name" --exact --ignored --nocapture --test-threads 1 > $out 2>&1; echo "## test exit $? $(date -u +%T)" >> $log ) &
tpid=$!
size=0 still=0 taken=
while kill -0 $tpid 2>/dev/null; do
	sleep 2
	now=$(stat -c %s $out 2>/dev/null || echo 0)
	if [ "$now" = "$size" ]; then still=$((still + 2)); else still=0 size=$now; fi
	pid=$(grep -oP "VM \K[0-9]+$" $out 2>/dev/null | tail -1)
	if [ -z "$taken" ] && [ -n "$pid" ] && [ $still -ge 12 ]; then
		taken=1
		{
			echo "## stuck $still s at: $(tail -1 $out)"
			echo "## its VM process $pid's threads, $(date -u +%T): tid, name, state, wait channel"
			for t in /proc/$pid/task/*; do echo "$(basename $t) $(cat $t/comm) $(awk '{print $3}' $t/stat) $(cat $t/wchan)"; done
			for t in /proc/$pid/task/*; do
				c=$(cat $t/comm)
				case $c in
				exec | daemon-signals | client-signals | signal-send | signal-conn | pauses)
					echo "## $(basename $t) $c: its kernel stack, then its user stack (eu-stack -1)"
					sudo -n cat $t/stack
					sudo -n eu-stack -1 -p $(basename $t) 2>&1 | head -40
					;;
				esac
			done
		} >> $log 2>&1
	fi
done
wait $tpid
grep -E "^attempt |panicked|left:|right:|test result|did not|stuck|status" $out | head -300 >> $log
echo "# done $(date -u +%T)" >> $log
