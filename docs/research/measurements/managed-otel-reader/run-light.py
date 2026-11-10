#!/usr/bin/env python3
"""Run only the private Reader light harness, with a single original deadline."""

import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time


def main():
    scope = Path(sys.argv[1]).resolve()
    source = scope / "source"
    raw = scope / "raw"
    raw.mkdir(exist_ok=False)
    env = dict(os.environ)
    env.update({
        "CARGO_HOME": str(scope / "cargo"),
        "CARGO_TARGET_DIR": str(scope / "target"),
        "CARGO_BUILD_JOBS": "2",
        "CARGO_INCREMENTAL": "0",
        "XDG_CACHE_HOME": str(scope / "xdg"),
        "SHARDS_CACHE": str(scope / "shards-cache"),
        "TMPDIR": str(scope / "tmp"),
    })
    Path(env["TMPDIR"]).mkdir(exist_ok=False)
    start = time.monotonic()
    # Operational caller bound, not a production limit or performance target.
    deadline = start + 240
    results = []
    commands = [
        ["rustc", "-Vv"],
        ["cargo", "-V"],
        ["uname", "-a"],
        ["getconf", "PAGESIZE"],
        ["cargo", "fmt", "--check"],
        ["cargo", "test", "--lib", "reader_tests::", "--", "--test-threads=1"],
        ["cargo", "clippy", "--lib", "--tests", "--", "-D", "warnings"],
    ]
    for index, command in enumerate(commands):
        output = raw / f"{index:02}.log"
        before = time.monotonic()
        with output.open("wb") as log:
            process = subprocess.Popen(command, cwd=source, env=env, stdout=log,
                                       stderr=subprocess.STDOUT, start_new_session=True)
            try:
                result = process.wait(timeout=max(0, deadline - before))
            except subprocess.TimeoutExpired:
                result = "deadline"
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                try:
                    process.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    result = "deadline; owned command cleanup unknown"
        results.append({"command": command, "result": result,
                        "elapsed_seconds": time.monotonic() - before,
                        "log": output.name,
                        "sha256": hashlib.sha256(output.read_bytes()).hexdigest()})
        print(json.dumps(results[-1]), flush=True)
        if result != 0:
            break
    temporary = sorted(str(path.relative_to(scope)) for path in (scope / "tmp").rglob("*"))
    binaries = []
    for path in sorted((scope / "target/debug/deps").glob("managed_otel_reader_light-*")):
        if path.is_file() and os.access(path, os.X_OK):
            binaries.append({"path": str(path.relative_to(scope)), "bytes": path.stat().st_size,
                             "sha256": hashlib.sha256(path.read_bytes()).hexdigest()})
    mapping = json.loads((source / "source-map.json").read_text())
    unchanged = {name: hashlib.sha256((source / name).read_bytes()).hexdigest() == facts["sha256"]
                 for name, facts in mapping.items()}
    summary = {"results": results, "elapsed_seconds": time.monotonic() - start,
               "temporary_entries_after_commands": temporary, "binaries": binaries,
               "source_map": mapping, "source_hashes_unchanged": unchanged}
    (raw / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps({"temporary_entries_after_commands": temporary, "binaries": binaries}))
    return 0 if (len(results) == len(commands) and all(r["result"] == 0 for r in results)
                 and all(unchanged.values())) else 1


if __name__ == "__main__":
    sys.exit(main())
