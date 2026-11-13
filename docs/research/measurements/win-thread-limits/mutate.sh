#!/bin/sh
# PM M529, M538: the Windows unit tests of how many threads a process may have
# (crates/vmm platform/windows_process.rs) and of its threads' raises (windows_thread.rs),
# green, then under each mutant in mutations/, each reverted after it. On Windows (Git Bash),
# from the repository's root; needs cargo 1.98.1. No line runs on past its end, so that a
# checkout with CRLF endings runs it alike.
set -u
here=docs/research/measurements/win-thread-limits
shown='^test |thread limit|a thread made|raise refused|SKIP|panicked at|^  left|^ right|assertion|test result'
test_them() {
	cargo +1.98.1 test -p shards-vmm --profile test-release --lib -- platform::windows_process:: platform::windows_thread:: --test-threads=1 --nocapture 2>&1 | tr -d '\r' | grep -E "$shown"
}
echo "== green"
test_them
for p in "$here"/mutations/*.patch; do
	echo "== mutation $(basename "$p" .patch)"
	tr -d '\r' <"$p" | patch -s -p1 || { echo "patch failed"; continue; }
	test_them
	git checkout -q -- crates/vmm/src/platform
done
