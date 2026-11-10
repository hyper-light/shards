#!/usr/bin/env python3
"""Recheck the Apple-gated helper with ordinary existing API callers only."""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import time


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--bundle", type=Path, required=True)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--helper", type=Path, required=True)
    parser.add_argument("--concurrent", type=Path, required=True)
    args = parser.parse_args()
    deadline = time.monotonic() + 570
    root = args.root.resolve()
    root.mkdir(mode=0o700)
    env = {k: v for k, v in os.environ.items()
           if not k.startswith(("DYLD_", "CARGO_", "SHARDS_", "SCCACHE_"))
           and k not in ["RUSTFLAGS", "RUSTDOCFLAGS", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER",
                         "RUSTC", "RUSTDOC", "RUSTUP_TOOLCHAIN"]}
    argv = [sys.executable, str(args.concurrent), "--bundle", str(args.bundle),
            "--helper", str(args.helper), "--root", str(root / "execution"),
            "--case-seconds", "15", "--compile-seconds", "180"]
    with (root / "concurrent.stdout").open("xb") as out, (root / "concurrent.stderr").open("xb") as err:
        result = subprocess.run(argv, env=env, stdout=out, stderr=err,
                                timeout=max(0, deadline - time.monotonic()))
    if result.returncode:
        print((root / "concurrent.stdout").read_text())
        print((root / "concurrent.stderr").read_text())
        raise SystemExit(result.returncode)
    sys.dont_write_bytecode = True
    spec = importlib.util.spec_from_file_location("held_exit", args.helper)
    helper = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(helper)
    source = root / "execution/darwin-ordinary-portability"
    summary = json.loads((source / "summary.json").read_text())
    binary = Path(summary["binary"]["path"])
    env.update(summary["environment"])
    ordinary = []
    for name in helper.ORDINARY + ["attach_takes_back_a_guest_whose_handle_was_lost"]:
        if time.monotonic() >= deadline:
            raise SystemExit("original ordinary gate deadline exhausted")
        ordinary.append(helper.ordinary(binary, source, env, name, 15))
    argv = ["cargo", "+1.98.1", "clippy", "--locked", "-j1", "--profile", "test-release",
            "-p", "shards-api", "--lib", "--tests", "--", "-D", "warnings"]
    clippy = helper.command(argv, source, env, root / "clippy", min(180, max(0, deadline - time.monotonic())))
    frozen = root / "frozen-api-test"
    helper.write_new(frozen, binary.read_bytes())
    frozen.chmod(0o500)
    record = {"compile": summary["compile"], "concurrent": summary["cases"], "ordinary": ordinary,
              "clippy": clippy, "binary": summary["binary"], "frozen_binary": str(frozen),
              "source_identities": summary["file_identities"], "no_allocator_hook": summary["no_allocator_hook"]}
    helper.write_new(root / "summary.json", (json.dumps(record, indent=2) + "\n").encode())
    print(json.dumps({"root": str(root), "compile": summary["compile"]["exit"],
                      "concurrent_exits": [c["exit"] for c in summary["cases"]],
                      "ordinary_exits": [c["exit"] for c in ordinary], "clippy": clippy["exit"],
                      "binary": summary["binary"], "frozen_binary": str(frozen)}))
    if any(c["exit"] != 0 or c["timed_out"] for c in ordinary) or clippy["exit"] != 0:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
