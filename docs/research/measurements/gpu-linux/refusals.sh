#!/bin/sh
# Every syscall the GPU's VM process's seccomp filter refuses as tests/gpu.rs runs (PM
# M382, D201): refusals.patch, applied to the tree, makes the filter's SIGSYS handler say
# each refused syscall and let the thread go on with ENOSYS, rather than end the process
# at the first; this counts them, by syscall, thread and first arguments. Measurement only:
# revert the patch after (`git apply -R`). Needs a shards built with SHARDS_GPU_LIBS on a
# host with a GPU, the test's environment set as tests/gpu.rs says.
#   refusals.sh LOG   (LOG: the output of `cargo test -p shards --test gpu -- --nocapture`)
set -eu
grep -oE 'seccomp refused syscall [0-9]+ in thread [^,]*, arguments 0x[0-9a-f]+ 0x[0-9a-f]+ 0x[0-9a-f]+' "$1" |
	awk '{print $4, $7, $9, $10, $11}' | sed -E 's/0x7f[0-9a-f]{10}/PTR/g' | sort | uniq -c | sort -k2n
