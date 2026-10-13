#!/usr/bin/env bash
# M220, M221 (D148, D149): what Node.js sees of a run's memory and sizes its heap by: V8's
# heap_size_limit, os.totalmem() and process.constrainedMemory(), in MiB, for each set of
# `run` options given ("" for none), in shards and, with DOCKER set, in Docker.
#   heap.sh OPTIONS...        e.g.  heap.sh "" "-m 8g"
# SHARDS names the shards to run (SHARDS_KERNEL to boot another kernel); DOCKER, a docker
# command to run the same in, such as "docker exec shards-dind docker".
set -euo pipefail
shards=${SHARDS:-shards}
image=node:22-alpine
js='const v8 = require("v8"), os = require("os");
// libuv reads a cgroup2 "max" as UINT64_MAX: no limit, "none".
const mib = (x) => (x && x < 2 ** 63 ? Math.round(x / 2 ** 20) : "none");
console.log(mib(v8.getHeapStatistics().heap_size_limit), mib(os.totalmem()), mib(process.constrainedMemory()));'
echo "heap_size_limit totalmem constrained (MiB)"
for opts in "$@"; do
    # shellcheck disable=SC2086 # each set of options is split into its words
    echo "shards $opts: $("$shards" run --rm $opts "$image" node -e "$js")"
    if [ -n "${DOCKER:-}" ]; then
        # shellcheck disable=SC2086
        echo "docker $opts: $($DOCKER run --rm $opts "$image" node -e "$js")"
    fi
done
