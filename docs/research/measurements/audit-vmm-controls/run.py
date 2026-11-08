#!/usr/bin/env python3
"""Fresh-process public VMM control probes; optional isolated HVF lifetime checks."""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import random
import subprocess
from datetime import datetime, timezone

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[3]


def run(*args):
    return subprocess.check_output(args, cwd=ROOT, text=True, timeout=30).strip()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--n", type=int, default=30)
    parser.add_argument("--hardware", action="store_true")
    parser.add_argument("--out", type=Path, default=HERE / "results.json")
    args = parser.parse_args()
    if args.n < 1:
        parser.error("--n must be positive")
    paths = [
        "crates/vmm/src/devices/control.rs", "crates/vmm/src/devices/serial.rs",
        "crates/vmm/src/hv/hvf/mod.rs", "crates/vmm/src/hv/hvf/sys.rs",
        "crates/vmm/src/hv/hvf/power.rs", "crates/vmm/src/memory.rs",
        "crates/vmm/src/vm/runtime.rs", "crates/vmm/src/log.rs",
        "crates/abi/src/lib.rs",
    ] + [str(p.relative_to(ROOT)) for p in sorted(HERE.rglob("*"))
         if p.is_file() and p.suffix in (".rs", ".toml", ".py")]

    def hashes():
        return {p: hashlib.sha256((ROOT / p).read_bytes()).hexdigest() for p in paths}

    before = hashes()
    subprocess.run([
        "cargo", "build", "--locked", "--offline", "--release", "--manifest-path",
        str(HERE / "Cargo.toml"), "--target-dir", str(ROOT / "target/audit-vmm-controls"),
    ], cwd=ROOT, check=True)
    binary = ROOT / "target/audit-vmm-controls/release/audit-vmm-controls"
    if args.hardware:
        if platform.system() != "Darwin" or platform.machine() != "arm64":
            parser.error("--hardware requires macOS arm64/HVF")
        subprocess.run([
            "codesign", "--entitlements", str(ROOT / "resources/hvf.entitlements"),
            "--force", "-s", "-", str(binary),
        ], cwd=ROOT, check=True)
    metadata = {
        "captured_utc": datetime.now(timezone.utc).isoformat(),
        "revision": run("git", "rev-parse", "HEAD"),
        "source_status": run("git", "status", "--short"),
        "os": platform.platform(), "architecture": platform.machine(),
        "host": platform.node(), "rustc": run("rustc", "--version"),
        "source_sha256": before,
        "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "timing_n": args.n,
        "notes": [
            "Control and Serial probes execute actual public APIs without a guest CPU.",
            "Marker samples run in fresh processes with info logging disabled; counters disabled for timings.",
            "Heap capacity excludes native/allocator/kernel/HVF memory and is not RSS.",
            "Serial correctness uses a real full pipe and a 200ms observation window, not a latency benchmark.",
            "Optional hardware probes create sequential empty VMs; kicker control runs one BRK instruction.",
            "No end-to-end optimization or Firecracker comparison; unrelated concurrent source edits are recorded.",
        ],
    }
    if platform.system() == "Darwin":
        metadata["hardware"] = run("sysctl", "-n", "hw.model", "hw.memsize", "hw.pagesize", "machdep.cpu.brand_string").splitlines()
    env = dict(os.environ, SHARDS_LOG="warn")

    def probe(*command):
        return json.loads(subprocess.check_output([str(binary), *map(str, command)], cwd=ROOT, env=env, text=True, timeout=30))

    sizes = [512, 8192, 65536, 262144]
    counts = [probe("markers", n, "--count") for n in sizes]
    for n in sizes:
        probe("markers", n)  # discarded warmup
    raw = []
    for iteration in range(args.n):
        order = sizes.copy()
        random.Random(iteration).shuffle(order)
        for n in order:
            row = probe("markers", n)
            row["iteration"] = iteration
            raw.append(row)
    summary = []
    for n in sizes:
        values = sorted(r["ns"] for r in raw if r["n_markers"] == n)
        q = lambda p: values[math.ceil(p * len(values)) - 1]
        summary.append({"n_markers": n, "n": len(values), "p50_ns": q(.5),
                        "p90_ns": q(.9), "p99_ns": q(.99), "max_ns": max(values)})
    correctness = [probe("serial") for _ in range(5)]
    if args.hardware:
        for _ in range(5):
            correctness.extend([probe("stale-gic"), probe("stale-kicker")])
    if hashes() != before:
        raise RuntimeError("measured source changed during the run; discard and repeat")
    args.out.write_text(json.dumps({"metadata": metadata, "counts": counts,
                                   "summary": summary, "raw": raw,
                                   "correctness": correctness}, indent=2) + "\n")
    print(json.dumps({"counts": counts, "summary": summary, "correctness": correctness}, indent=2))


if __name__ == "__main__":
    main()
