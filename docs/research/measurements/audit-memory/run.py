#!/usr/bin/env python3
"""Fresh-process host memory and public image-API audit microbenchmarks."""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import random
import subprocess
import tempfile

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[3]


def run(*args):
    return subprocess.check_output(args, cwd=ROOT, text=True).strip()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--n", type=int, default=30)
    parser.add_argument("--out", type=Path, default=HERE / "results.json")
    args = parser.parse_args()
    if args.n < 1:
        parser.error("--n must be positive")
    subprocess.run([
        "cargo", "build", "--locked", "--offline", "--release",
        "--manifest-path", str(HERE / "Cargo.toml"),
        "--target-dir", str(ROOT / "target/audit-memory"),
    ], cwd=ROOT, check=True)
    binary = ROOT / "target/audit-memory/release/audit-memory"
    source_paths = sorted(
        p for directory in (ROOT / "crates/image", ROOT / "crates/vmm", HERE)
        for p in directory.rglob("*")
        if p.is_file() and p.suffix in (".rs", ".c", ".toml", ".py")
    )
    metadata = {
        "revision": run("git", "rev-parse", "HEAD"),
        "source_status": run("git", "status", "--short", "--", "crates"),
        "host": platform.node(), "os": platform.platform(),
        "rustc": run("rustc", "--version"),
        "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "source_sha256": {
            str(p.relative_to(ROOT)): hashlib.sha256(p.read_bytes()).hexdigest()
            for p in source_paths
        },
        "layout": json.loads(run(str(binary), "layout")),
        "n": args.n,
        "notes": [
            "Host-only microbenchmarks; no VM, stage-2 faults, or Firecracker comparison.",
            "File cache warmed by writing a nonzero backing file before samples.",
            "Timing samples disable allocation counters; count samples are separate.",
            "Heap counters exclude native/C allocations, allocator metadata, mmap RAM, kernel memory.",
            "EROFS uses a byte-counting sink, not filesystem I/O; byte count is checked.",
            "RSS and macOS physical footprint are process accounting, not PSS or fleet unique bytes.",
        ],
    }
    if platform.system() == "Darwin":
        metadata["hardware"] = run("sysctl", "-n", "hw.model", "hw.memsize", "hw.pagesize",
                                   "machdep.cpu.brand_string").splitlines()
    raw = []
    counts = []
    with tempfile.TemporaryDirectory(prefix="shards-audit-memory-") as tmp:
        backing = Path(tmp) / "backing"
        size = 64 << 20
        with backing.open("wb") as out:
            for _ in range(64):
                out.write(b"\x01" * (1 << 20))
            out.flush()
            os.fsync(out.fileno())
        metadata["backing_bytes"] = size
        memory_cases = [
            ("anonymous_read", ["memory", "read", str(size)]),
            ("anonymous_write", ["memory", "write", str(size)]),
            ("file_map_only", ["memory", "none", str(size), str(backing)]),
            ("file_read", ["memory", "read", str(size), str(backing)]),
            ("file_write", ["memory", "write", str(size), str(backing)]),
            ("file_read_then_write", ["memory", "readwrite", str(size), str(backing)]),
            ("file_sparse_write", ["memory", "sparse", str(size), str(backing)]),
            ("anonymous_save_zero", ["save", str(size), str(Path(tmp) / "snapshot"), "zero"]),
            ("anonymous_save_sparse", ["save", str(size), str(Path(tmp) / "snapshot"), "sparse"]),
        ]
        image_cases = [
            ("empty_files", ["image", "10000", "1", "0", "0"]),
            ("empty_files_replaced_4x", ["image", "10000", "4", "0", "0"]),
            ("inline_512", ["image", "10000", "1", "512", "0"]),
            ("plain_4095", ["image", "10000", "1", "4095", "0"]),
            ("plain_4096", ["image", "10000", "1", "4096", "0"]),
            ("xattr_1024", ["image", "10000", "1", "0", "1024"]),
            ("xattr_1024_replaced_4x", ["image", "10000", "4", "0", "1024"]),
        ]
        cases = memory_cases + image_cases

        def probe(label, command, counting):
            output = run(str(binary), *command, *( ["--count"] if counting else [] ))
            rows = [json.loads(line) for line in output.splitlines()]
            for row in rows:
                row["case"] = label
                row["counting"] = counting
            return rows

        for label, command in cases:
            counts.extend(probe(label, command, True))
            # Untimed process warmup, outside retained timing samples.
            probe(label, command, False)
        for iteration in range(args.n):
            # Reproducible rotating/randomized order reduces ordering/thermal bias.
            shuffled = list(cases)
            random.Random(iteration).shuffle(shuffled)
            for label, command in shuffled:
                rows = probe(label, command, False)
                for row in rows:
                    row["iteration"] = iteration
                raw.extend(rows)
        # Identical CoW stores must not change the immutable source.
        with backing.open("rb") as src:
            while chunk := src.read(1 << 20):
                if chunk != b"\x01" * len(chunk):
                    raise RuntimeError("private writes changed backing file")
        metadata["backing_unchanged"] = True
    summary = []
    for case, _ in cases:
        phases = sorted({r["phase"] for r in raw if r["case"] == case})
        for phase in phases:
            samples = [r for r in raw if (r["case"], r["phase"]) == (case, phase)]
            ns = sorted(r["ns"] for r in samples)
            def percentile(p):
                return ns[max(0, math.ceil(p * len(ns)) - 1)]
            row = {"case": case, "phase": phase, "n": len(ns),
                   "p50_ns": percentile(.5), "p90_ns": percentile(.9),
                   "p99_ns": percentile(.99), "max_ns": max(ns)}
            for key in ("minor_faults", "major_faults", "rss_delta", "footprint_delta"):
                if key in samples[0]:
                    values = sorted(r[key] for r in samples)
                    row[key + "_median"] = values[(len(values) - 1) // 2]
            summary.append(row)
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps({"metadata": metadata, "counts": counts,
                                    "summary": summary, "raw": raw}, indent=2) + "\n")
    print(json.dumps({"metadata": {k: v for k, v in metadata.items() if k != "source_sha256"},
                      "counts": counts, "summary": summary}, indent=2))


if __name__ == "__main__":
    main()
