#!/usr/bin/env python3
"""D241's guard-removal mutants (R10): each guard the slice adds is taken out in turn, the
test that holds it is run, and the run must fail for that guard's reason; the source is put
back after each, whatever happens.

Run from the repository root, with the environment the light tests use:
    CARGO_TARGET_DIR=... SHARDS_HELPERS=skip python3 -I \
        docs/research/measurements/orchestrator-room-v1/mutants.py [NAME...]
It prints one line per mutant (`caught`, `SURVIVED` or `NOT APPLIED`) and exits 1 unless
every mutant it ran was caught.
"""

import os
import subprocess
import sys
from pathlib import Path

API = ["cargo", "test", "-p", "shards-api", "--lib"]
BIN = ["cargo", "test", "-p", "shards", "--bin", "shards"]

# name: (file, the guard as it stands, the guard taken out, the test command, the test that
# must fail, and what its failure must say)
MUTANTS = {
    "vcpu-order": (
        "crates/api/src/image/wire.rs",
        "if vcpus.last().is_some_and(|v| v.index >= index) {",
        "if false && vcpus.last().is_some_and(|v| v.index >= index) {",
        API,
        "image::wire::tests::a_runs_microvm_reads_back_as_its_host_said_it",
        "Order",
    ),
    "hostile-count": (
        "crates/api/src/image/wire.rs",
        "let n = self.count(4 + 8 + 1)?;",
        "let n = usize::try_from(self.u32()?).map_err(|_| WireError::TooLong)?;",
        API,
        "image::wire::tests::a_runs_microvm_reads_back_as_its_host_said_it",
        "memory allocation",
    ),
    "version-7": (
        "crates/api/src/image/wire.rs",
        "pub const VERSION: u8 = 7;",
        "pub const VERSION: u8 = 6;",
        API,
        "image::wire::tests::unknown_versions_and_tags_are_refused",
        "Version(6)",
    ),
    "answer-for-no-ask": (
        "crates/api/src/image/session.rs",
        "if let Some(awaited) = state.rooms.get_mut(&answer.id) {\n"
        "                *awaited = Some(answer.result);\n"
        "            }",
        "state.rooms.insert(answer.id, Some(answer.result));",
        API,
        "image::session::tests::a_rooms_answer_is_kept_for_its_awaited_ask_alone",
        "an answer no ask awaits is dropped",
    ),
    "ask-left-behind": (
        "crates/api/src/image/session.rs",
        "        lock(&self.state).rooms.remove(&id);\n        answer\n",
        "        answer\n",
        API,
        "image::session::tests::a_room_no_host_hears_is_refused_and_leaves_no_ask",
        "rooms.is_empty()",
    ),
    "held-without-budget": (
        "crates/shards/src/api_run.rs",
        '(None, Ok(n)) => Err(format!("its init held it within {n:?} with no budget")),',
        "(None, Ok(_)) => Ok(None),",
        BIN,
        "api_run::tests::a_rooms_answer_reads_as_it_was_asked",
        "None",
    ),
}


def run(name):
    path, guard, removed, command, test, says = MUTANTS[name]
    source = Path(path)
    original = source.read_text()
    if original.count(guard) != 1:
        return f"{name}: NOT APPLIED (the guard is in {path} {original.count(guard)} times)"
    try:
        source.write_text(original.replace(guard, removed))
        os.utime(source)
        done = subprocess.run(
            [*command, test, "--", "--exact", "--test-threads=1"],
            capture_output=True,
            text=True,
            check=False,
        )
    finally:
        source.write_text(original)
        # A restored file keeps the mutant's build unless cargo sees it change again.
        os.utime(source)
    out = done.stdout + done.stderr
    failed = done.returncode != 0 and (f"{test} ... FAILED" in out or "memory allocation" in out)
    if failed and says in out:
        return f"{name}: caught ({test} failed: {says!r} in its output)"
    if failed:
        return f"{name}: SURVIVED ({test} failed, but not saying {says!r})\n{out[-3000:]}"
    return f"{name}: SURVIVED (exit {done.returncode})\n{out[-3000:]}"


def main():
    names = sys.argv[1:] or list(MUTANTS)
    results = [run(name) for name in names]
    for line in results:
        print(line, flush=True)
    sys.exit(0 if all(": caught" in line for line in results) else 1)


if __name__ == "__main__":
    main()
