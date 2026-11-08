#!/usr/bin/env python3
"""Record host, revision, exact input hashes, raw samples, and summary in JSON."""
import hashlib
import json
import pathlib
import platform
import subprocess
import sys

here = pathlib.Path(__file__).resolve().parent
root = here.parents[3]
sources = ["crates/init/src/run.rs", "crates/shards/src/workload.rs", "crates/shards/src/spec.rs", "crates/shards/src/daemon/commands.rs", "crates/ipc/src/unix.rs", "crates/ipc/src/lib.rs", "crates/abi/src/run.rs"]
hashes = {path: hashlib.sha256((root / path).read_bytes()).hexdigest() for path in sources}
subprocess.run(["cargo", "build", "--release", "--locked", "--offline", "--quiet", "--manifest-path", str(here / "Cargo.toml")], check=True, cwd=root)
assert hashes == {path: hashlib.sha256((root / path).read_bytes()).hexdigest() for path in sources}, "input changed while building"
output = subprocess.check_output([str(here / "target/release/audit-stream-allocations"), sys.argv[1] if len(sys.argv) > 1 else "500"], text=True, timeout=90)
cases = {}
for line in output.splitlines():
    fields = line.split(",")
    if fields[0] == "case":
        cases[fields[1]] = dict(zip(fields[2::2], map(int, fields[3::2])))
    elif fields[0] == "samples":
        cases[fields[1]]["samples_ns"] = list(map(int, fields[2:]))
result = {
    "host": platform.node(), "platform": platform.platform(), "machine": platform.machine(),
    "revision": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True, cwd=root).strip(),
    "working_tree": subprocess.check_output(["git", "status", "--short"], text=True, cwd=root),
    "rustc": subprocess.check_output(["rustc", "-Vv"], text=True),
    "source_sha256": hashes, "cases": cases,
    "limitations": "Source-derived parser/log-record functions and actual ABI/IPC crates, native macOS System allocator; no guest allocator, VM, stage-2 fault, end-to-end, RSS/PSS, or Firecracker measurement. Parser cursor is a local prototype, not a product change. Allocator counters are disabled during separate timing samples but instrumentation's flag load remains. Preparation is excluded from parser timing. Requested bytes count realloc target size, not copied/retained bytes.",
}
print(json.dumps(result, indent=2))
