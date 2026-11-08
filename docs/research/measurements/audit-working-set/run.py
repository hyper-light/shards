#!/usr/bin/env python3
"""Build the real decoder probe; persist raw samples and nearest-rank percentiles."""

import datetime
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import subprocess
import tempfile

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[3]


def command(args):
    return subprocess.check_output(args, cwd=REPO, text=True).strip()


def percentile(values, fraction):
    ordered = sorted(values)
    return ordered[max(0, math.ceil(len(ordered) * fraction) - 1)]


def main():
    env = dict(os.environ, CARGO_TARGET_DIR=str(REPO / "target/audit-working-set"))
    subprocess.run(
        ["cargo", "build", "--manifest-path", str(HERE / "Cargo.toml"), "--release", "--offline"],
        cwd=REPO, env=env, check=True,
    )
    # A unique empty directory; the Rust probe creates and removes its only file.
    with tempfile.TemporaryDirectory(prefix="shards-audit-working-set-") as directory:
        lines = command([str(REPO / "target/audit-working-set/release/audit-working-set"), str(Path(directory) / "generation")])
        range_order = json.loads(command([str(REPO / "target/audit-working-set/release/range-order"), str(Path(directory) / "ranges.memory")]))
    raw = [json.loads(line) for line in lines.splitlines()]
    host_cpu = "unavailable"
    if platform.system() == "Darwin":
        try:
            host_cpu = command(["sysctl", "-n", "machdep.cpu.brand_string"])
        except subprocess.CalledProcessError:
            pass
    metadata = {
        "timestamp_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "revision": command(["git", "rev-parse", "HEAD"]),
        "status_before_results": command(["git", "status", "--short"]),
        "host": platform.node(), "host_cpu": host_cpu,
        "os": platform.platform(), "rustc": command(["rustc", "--version"]),
        "page_bytes": os.sysconf("SC_PAGE_SIZE"),
        "source_sha256": {
            str(path): hashlib.sha256((REPO / path).read_bytes()).hexdigest()
            for path in (
                Path("crates/vmm/src/snapshot/mod.rs"),
                Path("crates/vmm/src/snapshot/codec.rs"),
                Path("crates/vmm/src/platform/mod.rs"),
                Path("crates/vmm/src/platform/unix.rs"),
                Path("crates/vmm/src/hv/mod.rs"),
                Path("crates/vmm/src/initramfs.rs"),
                Path("crates/vmm/src/memory.rs"),
            )
        },
        "notes": "Rust System allocator requests only; cached local filesystem reads; no VM/hypervisor. Peak is requested live heap, not allocator RSS or physical footprint. Timings use the same binary with allocation counters disabled.",
    }
    allocations = {}
    timings = {}
    initramfs_allocations = {}
    initramfs_timings = {}
    for item in raw:
        if item["kind"] == "allocations":
            group = allocations.setdefault(str(item["pages"]), [])
            group.append({k: v for k, v in item.items() if k not in ("kind", "pages", "sample")})
        elif item["kind"] == "timing":
            timings.setdefault(str(item["pages"]), []).append(item["us"])
        elif item["kind"] == "initramfs_allocations":
            group = initramfs_allocations.setdefault(str(item["payload_bytes"]), [])
            group.append({k: v for k, v in item.items() if k not in ("kind", "payload_bytes", "sample")})
        elif item["kind"] == "initramfs_timing":
            initramfs_timings.setdefault(str(item["payload_bytes"]), []).append(item["us"])
    summary = {"metadata": metadata, "layout": raw[0], "allocation_counts": {}, "timing_us": {}}
    for name, groups in [("allocation_counts", allocations), ("initramfs_allocation_counts", initramfs_allocations)]:
        summary[name] = {}
        for size, samples in groups.items():
            summary[name][size] = {
                "n": len(samples),
                "all_samples_identical": all(s == samples[0] for s in samples),
                **samples[0],
            }
    for name, groups in [("timing_us", timings), ("initramfs_timing_us", initramfs_timings)]:
        summary[name] = {}
        for size, values in groups.items():
            summary[name][size] = {
                "n": len(values), "p50": percentile(values, 0.5),
                "p90": percentile(values, 0.9), "p99": percentile(values, 0.99),
                "max": max(values),
            }
    (HERE / "samples.json").write_text(json.dumps({"metadata": metadata, "samples": raw}, indent=2) + "\n")
    (HERE / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    (HERE / "range-order.json").write_text(json.dumps({"metadata": metadata, "range_order": range_order}, indent=2) + "\n")
    print(json.dumps(summary, indent=2))


if __name__ == "__main__":
    main()
