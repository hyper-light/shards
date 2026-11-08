#!/usr/bin/env python3
"""Build offline and record metadata plus focused production-code measurements."""

import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess


HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[3]


def output(argv, env=None):
    return subprocess.check_output(argv, cwd=ROOT, text=True, env=env).strip()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fragmentation", action="store_true", help="Also bind temporary Unix sockets and probe oversized indirect chains")
    parser.add_argument("--output", type=Path, default=HERE / "results.json")
    args = parser.parse_args()
    source_paths = [
        "crates/vmm/src/devices/virtio/queue.rs",
        "crates/vmm/src/devices/virtio/vsock/mod.rs",
        "crates/vmm/src/devices/virtio/vsock/poll.rs",
        "crates/vmm/src/devices/virtio/vsock/conn.rs",
        "crates/vmm/src/devices/virtio/vsock/muxer.rs",
        "crates/vmm/src/devices/virtio/block.rs",
        "crates/vmm/src/memory.rs",
        "crates/vmm/src/platform/unix.rs",
    ]
    hashes = {p: hashlib.sha256((ROOT / p).read_bytes()).hexdigest() for p in source_paths}
    metadata = {
        "captured_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "revision": output(["git", "rev-parse", "HEAD"]),
        "tracked_changes": output(["git", "diff", "--name-only"]).splitlines(),
        "host": platform.node(),
        "os": platform.platform(),
        "architecture": platform.machine(),
        "rustc": output(["rustc", "--version"]),
        "source_sha256": hashes,
        "timing_notes": "Single process, System allocator instrumented only in one untimed census iteration. Timed iterations leave census disabled, but retain one AtomicBool branch per allocation. Includes Instant timing overhead, assertions and queue/poll bookkeeping. No VM, no Firecracker comparison, no implementation change.",
        "queue_warmup_iterations": 500,
        "poll_warmup_iterations": 100,
    }
    if platform.system() == "Darwin":
        metadata["hardware"] = output(["/usr/sbin/sysctl", "-n", "hw.model", "machdep.cpu.brand_string", "hw.memsize"])
    environment = {k: v for k, v in os.environ.items() if not k.startswith("DYLD_")}
    subprocess.run([
        "cargo", "build", "--offline", "--locked", "--release",
        "--manifest-path", str(HERE / "Cargo.toml"),
        "--target-dir", str(ROOT / "target/audit-device-allocations"),
    ], cwd=ROOT, env=environment, check=True)
    binary = ROOT / "target/audit-device-allocations/release/shards-audit-device-allocations"
    samples_path = args.output.with_name(args.output.stem + "-samples.jsonl")
    samples_path.unlink(missing_ok=True)
    environment["SHARDS_AUDIT_DEVICE_SAMPLES"] = str(samples_path)
    measurements = [json.loads(line) for line in output([str(binary)], env=environment).splitlines()]
    if args.fragmentation:
        measurements.extend(json.loads(line) for line in output([str(binary), "--vsock-fragmentation"], env=environment).splitlines())
    metadata["raw_samples_file"] = samples_path.name
    metadata["raw_samples_sha256"] = hashlib.sha256(samples_path.read_bytes()).hexdigest()
    for path, before in hashes.items():
        if hashlib.sha256((ROOT / path).read_bytes()).hexdigest() != before:
            raise RuntimeError(f"Source changed during measurement: {path}; rerun")
    document = {"metadata": metadata, "measurements": measurements}
    args.output.write_text(json.dumps(document, indent=2) + "\n")
    print(args.output)


if __name__ == "__main__":
    main()
