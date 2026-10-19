#!/bin/bash
# How often a hybrid host refuses a template's restore (PM M258): N runs of each SHARDS
# build given, each in a home of its own, its processes where the scheduler puts them, each
# run restoring its home's template; the refusals counted from its daemon's log, and the
# runs' mean time, client to exit. Usage: rate.sh N SHARDS...
set -u
n=$1
shift
for shards in "$@"; do
    d=$(mktemp -d /tmp/cpuid-rate.XXXXXX)
    mkdir "$d/cfg"
    export SHARDS_HOME=$d/home DOCKER_CONFIG=$d/cfg
    "$shards" pull alpine:3.22 > /dev/null 2>&1
    # The template, and its working set.
    "$shards" run --rm --pull never alpine:3.22 true > /dev/null 2>&1
    "$shards" run --rm --pull never alpine:3.22 true > /dev/null 2>&1
    start=$(date +%s%N)
    fails=0
    for _ in $(seq "$n"); do
        "$shards" run --rm --pull never alpine:3.22 true > /dev/null 2>&1 || fails=$((fails + 1))
    done
    end=$(date +%s%N)
    refused=$(grep -c 'CPUID differs' "$SHARDS_HOME/daemon.log")
    echo "$shards: $n runs, $fails failed, $refused restores refused, $(((end - start) / n / 1000)) us a run"
    "$shards" daemon stop > /dev/null 2>&1
    case $d in /tmp/cpuid-rate.*) rm -rf "$d" ;; esac
done
