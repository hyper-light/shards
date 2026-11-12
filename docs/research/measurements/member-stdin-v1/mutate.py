#!/usr/bin/env python3
"""Guard-removal mutations for plan step 5's member stdin (R10): each removes one guard,
runs the test that holds it, and expects that test to fail; the file is restored by
rewriting it (a fresh mtime, so cargo rebuilds the original), and the test run again green.

    python3 -I mutate.py SRC_ROOT {mac|linux} [NAME_PREFIX]

Run from a checkout of the branch; build output where CARGO_TARGET_DIR says. Prints one line
per mutation: its name, whether its test failed with the guard gone (killed) and whether it
passed again restored.
"""
import os
import subprocess
import sys

# (name, file, guard as written, guard removed, cargo test args, hosts)
MUTATIONS = [
    (
        "abi: an input member must be active",
        "crates/abi/src/run/managed.rs",
        "if previous.is_some_and(|p| p >= value) || !active.contains(id) {",
        "if previous.is_some_and(|p| p >= value) {",
        ["-p", "shards-abi", "--lib", "input_members_are_active"],
        ("mac", "linux"),
    ),
    (
        "remote: a record starts where the last was taken",
        "crates/remote/src/managed/input.rs",
        "Slot::Free { at, buffer } if at == offset => {",
        "Slot::Free { at, buffer } => {",
        ["-p", "shards-remote", "--lib", "managed::input::tests::a_record_is_taken_whole"],
        ("mac", "linux"),
    ),
    (
        "remote: a record is at most its room",
        "crates/remote/src/managed/input.rs",
        "(true, Tag::Stdin) if len > OFFSET && len - OFFSET <= self.room as usize => OFFSET,",
        "(true, Tag::Stdin) if len > OFFSET => OFFSET,",
        ["-p", "shards-remote", "--lib", "managed::input::tests::a_record_is_taken_whole"],
        ("mac", "linux"),
    ),
    (
        "remote: a resubscription is never ahead of what was taken",
        "crates/remote/src/managed/input.rs",
        "(Some(next), _) if offset <= next => {}",
        "(Some(_), _) => {}",
        ["-p", "shards-remote", "--lib", "managed::input::tests::a_lost_subscription"],
        ("mac", "linux"),
    ),
    (
        "daemon: an input member runs a program",
        "crates/shards/src/daemon/managed.rs",
        "if !active.contains(&member) {",
        "if active.is_empty() {",
        ["-p", "shards", "--bin", "shards", "input_members_are_named"],
        ("mac", "linux"),
    ),
    (
        "vm process: an input's end is relayed once, for an open input",
        "crates/shards/src/workload/capture.rs",
        ".filter(|s| **s == InputState::Open)",
        ".filter(|_| true)",
        ["-p", "shards-vm-process", "--bin", "shards-vm", "an_inputs_end_is_relayed_once"],
        ("mac", "linux"),
    ),
    (
        "init: the connection is read only once all it gave is in the pipe",
        "crates/init/src/input.rs",
        "State::Relaying if !self.pending() => libc::POLLIN,",
        "State::Relaying => libc::POLLIN,",
        ["-p", "shards-init", "input::tests::an_unread_stdin_stops_reading"],
        ("linux",),
    ),
    (
        "init: a member no longer reading ends its input while idle",
        "crates/init/src/input.rs",
        "if revents & libc::POLLERR != 0 && !self.pending() {",
        "if false {",
        ["-p", "shards-init", "input::tests::a_member_that_stops_reading"],
        ("linux",),
    ),
    (
        "init: an end without its owner's is no owner's end",
        "crates/init/src/input.rs",
        "Ok(0) => Err(InputEnd::Aborted(libc::ECONNRESET)),",
        "Ok(0) => Err(InputEnd::Closed),",
        ["-p", "shards-init", "input::tests::a_connection_that_ends_without"],
        ("linux",),
    ),
]


def run(root, args):
    env = dict(os.environ)
    try:
        out = subprocess.run(
            ["cargo", "+1.98.1", "test", "-j", "16", *args],
            cwd=root,
            env=env,
            capture_output=True,
            text=True,
            # A test held for ever by a missing guard is killed by it too: bounded by the
            # longest build here, at most.
            timeout=1800,
        )
    except subprocess.TimeoutExpired:
        return 124, ["timed out"]
    tail = [l for l in (out.stdout + out.stderr).splitlines() if "test result" in l or "panicked" in l]
    return out.returncode, tail[-2:]


def main():
    root, host = sys.argv[1], sys.argv[2]
    prefix = sys.argv[3] if len(sys.argv) > 3 else ""
    for name, path, guard, removed, args, hosts in MUTATIONS:
        if host not in hosts or not name.startswith(prefix):
            continue
        file = os.path.join(root, path)
        original = open(file).read()
        if original.count(guard) != 1:
            print(f"SKIP {name}: guard found {original.count(guard)} times", flush=True)
            continue
        try:
            with open(file, "w") as f:
                f.write(original.replace(guard, removed, 1))
            code, tail = run(root, args)
            killed = code != 0
        finally:
            with open(file, "w") as f:
                f.write(original)
        restored, _ = run(root, args)
        print(
            f"{'KILLED' if killed else 'SURVIVED'} / {'green' if restored == 0 else 'RED'} restored: {name} :: {tail}",
            flush=True,
        )


main()
