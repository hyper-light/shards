#!/bin/sh
# PM M529: the Windows unit tests of how many threads a process may have
# (crates/vmm platform/windows_process.rs), green, then under each mutant in mutations/, each
# reverted after it. On Windows (Git Bash), from the repository's root; needs cargo 1.98.1.
set -u
F=crates/vmm/src/platform/windows_process.rs
here=docs/research/measurements/win-thread-limits
test_them() {
	cargo +1.98.1 test -p shards-vmm --profile test-release --lib platform::windows_process:: \
		-- --test-threads=1 --nocapture 2>&1 |
		grep -E "^test |thread limit|panicked at|^  left|^ right|assertion|test result"
}
echo "== green"
test_them
for p in "$here"/mutations/*.patch; do
	echo "== mutation $(basename "$p" .patch)"
	patch -s -p1 <"$p" || { echo "patch failed"; continue; }
	test_them
	git checkout -q -- "$F"
done
