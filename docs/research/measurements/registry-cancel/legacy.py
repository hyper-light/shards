#!/usr/bin/env python3
"""Use original production code with the current exact measurement fixture."""
import pathlib, subprocess
path = pathlib.Path('crates/registry/src/http.rs')
legacy = subprocess.check_output(['git', 'show', 'f69ec070815fd18aece912ecaac468a13c6cfb82:crates/registry/src/http.rs'], text=True)
current = path.read_text()
marker = '#[cfg(test)]\n#[allow(clippy::unwrap_used, clippy::indexing_slicing)]\nmod tests {'
tests = current[current.index(marker):]
start = tests.index('    #[test]\n    fn worker_replies_keep_completion_cancel_deadline_and_disconnect_distinct()')
end = tests.index('    /// Controlled Linux libc fixture;', start)
# The three new state-machine tests need the new production types; the exact
# libc/timing/resource fixture and all earlier tests remain byte-for-byte current.
tests = tests[:start] + tests[end:]
path.write_text(legacy[:legacy.index(marker)] + tests)
