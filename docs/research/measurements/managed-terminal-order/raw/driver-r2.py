#!/usr/bin/env python3
"""Private compile-only driver. Input is a base64 JSON map in the environment.

Run with admitted <=1 CPU / 1 GiB memory / 2 GiB disk in a NEW private directory.
No native fixture is executed. No existing source/target/cargo home is reused.
"""

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys


parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--base", type=Path, required=True)
parser.add_argument("--revision", required=True)
parser.add_argument("--step-timeout-seconds", type=float, required=True)
args = parser.parse_args()
assert args.step_timeout_seconds > 0
args.base.mkdir(mode=0o700)
base = args.base.resolve()
raw = base / "raw"
raw.mkdir()
environment = {key: value for key, value in os.environ.items() if not key.startswith("DYLD_")}
payload = json.loads(base64.b64decode(environment.pop("SHARDS_TERMINAL_LIGHT_INPUT_B64")))
environment.update({
    "CARGO_HOME": str(base / "cargo-home"),
    "CARGO_TARGET_DIR": str(base / "target"),
    "CARGO_BUILD_JOBS": "1",
    "CARGO_INCREMENTAL": "0",
    "CARGO_PROFILE_DEV_DEBUG": "0",
    "CARGO_PROFILE_TEST_DEBUG": "0",
})
receipts = []


def run(command, cwd=base):
    result = subprocess.run(command, cwd=cwd, env=environment, stdout=subprocess.PIPE,
                            stderr=subprocess.STDOUT, timeout=args.step_timeout_seconds)
    log = raw / f"{len(receipts)}.log"
    log.write_bytes(result.stdout)
    receipts.append({"argv": command, "cwd": str(cwd), "exit_code": result.returncode,
                     "log": log.name, "sha256": hashlib.sha256(result.stdout).hexdigest()})
    (raw / "commands.json").write_text(json.dumps(receipts, indent=2) + "\n")
    print(json.dumps(receipts[-1]), flush=True)
    if result.returncode:
        print(result.stdout.decode(errors="replace"), flush=True)
        raise SystemExit(result.returncode)
    return result.stdout


run(["uname", "-a"])
run(["rustc", "-Vv"])
run(["cargo", "-Vv"])
run(["git", "clone", "--filter=blob:none", "--sparse", "--no-checkout",
     "git@github.com:hyper-light/shards.git", str(base / "source")])
source = base / "source"
patterns = ["/Cargo.toml", "/Cargo.lock", "/clippy.toml", "/.cargo/", "/AGENTS.md", "/CLAUDE.md",
            "/docs/development-rules.md", "/docs/development-evidence.md", "/docs/design/managed-runtime-plan.md",
            "/crates/*/Cargo.toml", "/crates/shards/src/daemon.rs", "/crates/shards/src/daemon/managed.rs"]
patterns += [f"/crates/{name}/" for name in ("abi", "ipc", "remote", "testdir", "registry", "image", "erofs", "apple")]
patterns += [f"/vendor/{name}/" for name in ("aws-lc-rs", "aws-lc-sys", "rquickjs-sys")]
run(["git", "sparse-checkout", "set", "--no-cone", *patterns], source)
run(["git", "checkout", "--detach", args.revision], source)
actual = run(["git", "rev-parse", "HEAD"], source).decode().strip()
assert actual == args.revision
policy = hashlib.sha256((source / "docs/development-rules.md").read_bytes()).hexdigest()
assert policy == "d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242"
allowed = {"crates/shards/src/daemon/managed/terminal_order_tests.rs", "light.py"}
assert set(payload) == allowed
for relative, encoded in payload.items():
    target = source / relative if relative != "light.py" else base / relative
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_bytes(base64.b64decode(encoded))
    print(json.dumps({"input": relative, "sha256": hashlib.sha256(target.read_bytes()).hexdigest()}), flush=True)
run([sys.executable, str(base / "light.py"), "--source", str(source), "--output", str(base / "fixture")])
shutil.copyfile(source / "Cargo.lock", base / "fixture/Cargo.lock")
run(["cargo", "check", "--tests", "-j1"], base / "fixture")
run(["cargo", "clippy", "--tests", "--no-deps", "-j1", "--", "-D", "warnings"], base / "fixture")
run(["du", "-sk", str(base)])
print("COMPILE_ONLY_COMPLETE native_cases_executed=0", flush=True)
