#!/bin/sh
# Hunts the VM grant stall (a VM process that sends no request for access for 60 s, its
# client's first line never coming): the fleet measurement's bursts, run again and again
# under whatever else the host runs, until the daemon says a VM stalled or the rounds run
# out. The daemon samples a stalled VM's stacks at 10 s (daemon.rs `grant`); the fleet
# prints what it said and the samples (`stalls said`, `vm-PID.sample`) before its home goes.
#
#   docs/research/measurements/vm-grant-stall/campaign.sh [TIMES] [LOG]
set -u
times=${1:-20}
log=${2:-vm-grant-stall.log}
i=0
while [ "$i" -lt "$times" ]; do
	i=$((i + 1))
	printf '== campaign %s of %s, %s\n' "$i" "$times" "$(date -u +%FT%TZ)" >>"$log"
	FLEET_SIZES=${FLEET_SIZES:-10} FLEET_BURST=${FLEET_BURST:-16} FLEET_ROUNDS=${FLEET_ROUNDS:-60} \
		cargo test -p shards --profile test-release --test fleet -- --ignored --nocapture >>"$log" 2>&1
	if grep -q "stalls said: [1-9]" "$log"; then
		printf '== a stall, campaign %s\n' "$i" >>"$log"
		exit 0
	fi
done
printf '== no stall in %s campaigns\n' "$times" >>"$log"
