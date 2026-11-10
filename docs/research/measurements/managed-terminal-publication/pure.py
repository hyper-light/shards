#!/usr/bin/env python3
"""Run exact Life/credit fragments without the daemon, endpoints or a VM.

This is a scoped state/wait fixture, not a build of the complete daemon. Every
production fragment and its source hash is retained; no control/transport mock
implements a substitute state machine. The additional Output call fixture uses
a declared call recorder, not a driver/transport model. The real remote crate's
separate pure tests cover Shared publication and its callback order.
"""

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import re
import subprocess


def digest(data):
    return hashlib.sha256(data).hexdigest()


def between(text, first, following):
    if text.count(first) != 1:
        raise ValueError(f"expected one source anchor: {first!r}")
    start = text.index(first)
    return text[start:text.index(following, start + len(first))]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--timeout-seconds", type=float, required=True)
    parser.add_argument("--filter", default="")
    args = parser.parse_args()
    if not math.isfinite(args.timeout_seconds) or not args.timeout_seconds > 0:
        parser.error("positive finite caller timeout required")
    args.output.mkdir(exist_ok=False, parents=True)
    paths = {
        "daemon": "crates/shards/src/daemon/managed.rs",
        "daemon_parent": "crates/shards/src/daemon.rs",
        "remote": "crates/remote/src/managed.rs",
        "abi": "crates/abi/src/run/managed.rs",
        "abi_parent": "crates/abi/src/run.rs",
    }
    sources = {name: (args.source / path).read_text() for name, path in paths.items()}
    daemon, remote, abi = (sources[key] for key in ["daemon", "remote", "abi"])
    fragments = {
        "channel": between(abi, "#[derive(Clone, Copy, Debug, Eq, PartialEq)]\n#[repr(u8)]\npub enum Channel", "\nimpl TryFrom<u8>"),
        "scope": between(remote, "#[derive(Debug, Clone, Copy, PartialEq, Eq)]\npub struct RunId", "\n/// The fixed subscription"),
        "state": between(remote, "#[derive(Debug, Clone, Copy, PartialEq, Eq)]\npub struct Progress", "\nfn progress(slot:"),
        "life": between(daemon, "#[derive(Clone, Copy, Debug, PartialEq, Eq)]\nenum Terminal", "\nstruct Output"),
        "credit": between(daemon, "#[derive(Clone, Copy)]\nenum After", "\nimpl Outputs"),
        "tests": between(daemon, "#[cfg(test)]\n#[allow(clippy::unwrap_used, clippy::panic)]\nmod terminal_tests", "\n#[cfg(test)]"),
        "lock": between(sources["daemon_parent"], "fn lock<T>(m: &Mutex<T>)", "\n/// A warm VM"),
        "output_methods": between(daemon, "    fn resolve(&self, event: Resolve)", "\n    fn complete(&self)"),
    }
    kinds = re.findall(r"^    pub const (?:STDOUT|STDERR): u8 = \d+;$", sources["abi_parent"], re.MULTILINE)
    if len(kinds) != 2:
        raise ValueError("expected both actual ABI channel kind constants")
    generated = """// Generated from the retained exact production fragments by pure.py.
#![allow(dead_code)]
use std::io;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Instant;
mod abi {
pub mod kind {
""" + "\n".join(kinds) + "\n}\npub mod managed {\n" + fragments["channel"] + "\n}\n}\nuse abi::managed::Channel;\n"
    generated += "\n".join(fragments[key] for key in ["scope", "state", "lock", "life", "credit", "tests"])
    shell = Path(__file__).with_name("output-calls.rs").read_text()
    if shell.count("// ACTUAL OUTPUT METHODS") != 1:
        raise ValueError("expected one Output-method insertion point")
    generated += "\n" + shell.replace("// ACTUAL OUTPUT METHODS", fragments["output_methods"])
    fixture = args.output / "terminal-pure.rs"
    fixture.write_text(generated)
    for name, content in fragments.items():
        (args.output / f"{name}.rs.fragment").write_text(content)
    identity = {
        "source": {paths[name]: digest(text.encode()) for name, text in sources.items()},
        "fragments": {name: digest(text.encode()) for name, text in fragments.items()},
        "fixture_sha256": digest(generated.encode()),
        "output_shell_sha256": digest(shell.encode()),
        "scope": "exact Life/credit/metadata and selected Output-method fragments; Output call recorder is not real Control/driver or complete daemon",
    }
    (args.output / "source.json").write_text(json.dumps(identity, indent=2) + "\n")
    env = {key: value for key, value in os.environ.items() if not key.startswith("DYLD_")}
    env["SHARDS_TERMINAL_LIGHT_DIR"] = str(args.output)
    commands = [
        ["rustc", "--edition=2024", "--test", str(fixture), "-C", "debuginfo=0", "-o", str(args.output / "terminal-pure")],
        [str(args.output / "terminal-pure"), args.filter, "--nocapture", "--test-threads=1"],
    ]
    receipts = []
    for index, command in enumerate(commands):
        try:
            result = subprocess.run(command, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=args.timeout_seconds)
            data, code = result.stdout, result.returncode
        except subprocess.TimeoutExpired as error:
            data, code = (error.stdout or b"") + b"\nCALLER TIMEOUT; scoped fixture killed\n", 124
        log = args.output / f"{index}.log"
        log.write_bytes(data)
        receipts.append({"argv": command, "exit_code": code, "log": str(log), "log_sha256": digest(data)})
        (args.output / "commands.json").write_text(json.dumps(receipts, indent=2) + "\n")
        print(json.dumps(receipts[-1]))
        print(data.decode(errors="replace"))
        if code:
            return code
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
