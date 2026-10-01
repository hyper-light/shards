#!/usr/bin/env python3
"""Actual-daemon shutdown probe: blocked passed stderr before VM handoff; no VM boots."""

import argparse
import array
import datetime
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import select
import socket
import struct
import subprocess
import tempfile
import time


HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[3]
BASELINE = "329d00a5d98135956664f7bcec5d289dab1fb8a2"


def string(value):
    encoded = value.encode()
    return struct.pack(">I", len(encoded)) + encoded


def optional(value):
    return b"\0" if value is None else b"\1" + string(value)


def strings(values):
    return struct.pack(">I", len(values)) + b"".join(map(string, values))


def rotate(value, n, bits=64):
    return ((value << n) | (value >> (bits - n))) & ((1 << bits) - 1)


def identity(daemon, vm):
    def stat_fields(path):
        st = path.stat()
        return [st.st_dev, st.st_ino, st.st_size, st.st_mtime_ns // 1_000_000_000, st.st_mtime_ns % 1_000_000_000]
    d, v = stat_fields(daemon), stat_fields(vm)
    merged = [d[i] ^ rotate(v[i], n, 32 if i == 4 else 64) for i, n in enumerate([17, 29, 37, 41, 13])]
    return struct.pack(">QQQQQ", *merged)


def failed_run(daemon, vm):
    # Both explicit guest paths select Boot::Given, avoiding kernel download. The
    # deliberately malformed image then fails Reference::parse before image I/O/boot.
    return b"".join([
        string("!"), strings([]), string(""), string(""), optional(None),
        b"\0", b"\0", strings(["/unused"]), b"\2",
        optional("/audit-unused-kernel"), optional("/audit-unused-init"),
        b"\0", optional(None), b"\0", b"\0", b"\0", identity(daemon, vm),
    ])


def frame(kind, payload=b""):
    return struct.pack(">BI", kind, len(payload)) + payload


def drained(fd):
    os.set_blocking(fd, False)
    chunks = []
    while True:
        try:
            chunk = os.read(fd, 65536)
        except BlockingIOError:
            break
        if not chunk:
            break
        chunks.append(chunk)
    return b"".join(chunks)


def one_sample(daemon, vm, observe_seconds):
    process = None
    descriptors = []
    connections = []
    with tempfile.TemporaryDirectory(prefix="shards-daemon-limits-", dir="/private/tmp" if platform.system() == "Darwin" else None) as directory:
        home = Path(directory)
        with (home / "probe-daemon.log").open("wb") as daemon_log:
            try:
                environment = dict(os.environ, SHARDS_HOME=str(home), SHARDS_DAEMON_IDLE="900", SHARDS_POOL="1")
                process = subprocess.Popen([str(daemon), "daemon"], stdout=subprocess.DEVNULL, stderr=daemon_log, env=environment)
                deadline = time.monotonic() + 5
                endpoint = home / "daemon.sock"
                while not endpoint.exists():
                    assert process.poll() is None, "daemon exited before listening"
                    assert time.monotonic() < deadline, "daemon did not listen"
                    time.sleep(0.01)
                reader, writer = os.pipe()
                descriptors.extend([reader, writer])
                os.set_blocking(writer, False)
                filled = 0
                while True:
                    try:
                        filled += os.write(writer, b"x" * 4096)
                    except BlockingIOError:
                        break
                os.set_blocking(writer, True)
                stdin = os.open(os.devnull, os.O_RDONLY)
                stdout = os.open(os.devnull, os.O_WRONLY)
                descriptors.extend([stdin, stdout])
                run = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                connections.append(run)
                run.connect(str(endpoint))
                message = frame(5, failed_run(daemon, vm))
                sent = run.sendmsg([message], [(socket.SOL_SOCKET, socket.SCM_RIGHTS, array.array("i", [stdin, stdout, writer]))])
                if sent < len(message):
                    run.sendall(message[sent:])
                # This observation precedes STOP: the error reply cannot complete.
                response_before_stop = bool(select.select([run], [], [], 0.2)[0])
                stopper = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                connections.append(stopper)
                stopper.connect(str(endpoint))
                stop_started = time.monotonic_ns()
                stopper.sendall(frame(7))
                stopped = bool(select.select([stopper], [], [], observe_seconds)[0])
                held_ns = time.monotonic_ns() - stop_started
                alive_while_pipe_full = process.poll() is None
                released = drained(reader)
                release_started = time.monotonic_ns()
                stopper.settimeout(5)
                stop_reply = stopper.recv(1)
                assert stop_reply == b"", "STOP did not end by connection EOF"
                exit_status = process.wait(timeout=5)
                release_to_exit_ns = time.monotonic_ns() - release_started
                stderr_after_release = drained(reader)
                daemon_log.flush()
                log_text = (home / "probe-daemon.log").read_text()
                return {
                    "prefilled_stderr_bytes": filled,
                    "run_answered_before_stop": response_before_stop,
                    "stop_completed_while_stderr_full": stopped,
                    "daemon_alive_while_stderr_full": alive_while_pipe_full,
                    "held_observation_ns": held_ns,
                    "release_to_daemon_exit_ns": release_to_exit_ns,
                    "exit_status": exit_status,
                    "prefill_bytes_drained": len(released),
                    "stderr_after_release": stderr_after_release.decode(errors="replace"),
                    "daemon_log": log_text,
                    "vm_started": "VM " in log_text or "starting a warm VM" in log_text,
                }
            finally:
                for conn in connections:
                    conn.close()
                for fd in descriptors:
                    try:
                        os.close(fd)
                    except OSError:
                        pass
                if process is not None and process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=1)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=2)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--daemon", type=Path, default=ROOT / "target/audit-daemon-limits/release/shardsd")
    parser.add_argument("--vm", type=Path, default=ROOT / "target/audit-daemon-limits/release/shards-vm")
    parser.add_argument("--n", type=int, default=3)
    parser.add_argument("--observe-seconds", type=float, default=1.5)
    parser.add_argument("--output", type=Path, default=HERE / "results.json")
    args = parser.parse_args()
    daemon, vm = args.daemon.resolve(), args.vm.resolve()
    paths = ["crates/shards/src/daemon.rs", "crates/shards/src/daemon/commands.rs", "crates/shards/src/run.rs", "crates/ipc/src/lib.rs", "crates/ipc/src/unix.rs", "crates/image/src/reference.rs"]
    hashes = {p: hashlib.sha256((ROOT / p).read_bytes()).hexdigest() for p in paths}
    baseline_matches = {p: hashlib.sha256(subprocess.check_output(["git", "show", BASELINE + ":" + p], cwd=ROOT)).hexdigest() == hashes[p] for p in paths}
    metadata = {
        "captured_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "baseline": BASELINE,
        "head_at_probe": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
        "git_status": subprocess.check_output(["git", "status", "--short"], cwd=ROOT, text=True).splitlines(),
        "host": platform.node(), "os": platform.platform(), "architecture": platform.machine(),
        "source_sha256": hashes, "source_matches_baseline": baseline_matches,
        "daemon_binary": str(daemon), "daemon_sha256": hashlib.sha256(daemon.read_bytes()).hexdigest(),
        "vm_binary": str(vm), "vm_sha256": hashlib.sha256(vm.read_bytes()).hexdigest(),
        "method": "Fresh daemon per sample, valid START/SCM_RIGHTS with explicit unused guest paths and malformed image. Passed stderr is prefilled until EAGAIN, then blocking restored. Observe STOP for configured period; drain pipe and wait for daemon EOF/exit. No guest or VM runs. Release-to-exit times describe the probe recovery, not a product speed benchmark.",
    }
    if platform.system() == "Darwin":
        metadata["hardware"] = subprocess.check_output(["/usr/sbin/sysctl", "-n", "hw.model", "machdep.cpu.brand_string", "hw.memsize"], text=True).strip()
    samples = [one_sample(daemon, vm, args.observe_seconds) for _ in range(args.n)]
    times = sorted(s["release_to_daemon_exit_ns"] for s in samples)
    summary = {"n": len(samples), "release_to_exit_ns": {f"p{q}": times[math.ceil(len(times) * q / 100) - 1] for q in (50, 90, 99)}}
    summary["release_to_exit_ns"]["max"] = times[-1]
    for path, expected in hashes.items():
        assert hashlib.sha256((ROOT / path).read_bytes()).hexdigest() == expected, f"source changed during probe: {path}"
    args.output.write_text(json.dumps({"metadata": metadata, "summary": summary, "samples": samples}, indent=2) + "\n")
    print(args.output)


if __name__ == "__main__":
    main()
