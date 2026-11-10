#!/usr/bin/env python3
"""One owned official Collector POST; no application exporter or delivery guarantee."""
import argparse
import concurrent.futures
import hashlib
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import threading
import time
import urllib.request

def sha(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()

def listeners(pid):
    sockets = set()
    for fd in (Path("/proc") / str(pid) / "fd").iterdir():
        try:
            target = os.readlink(fd)
        except FileNotFoundError:
            continue
        if target.startswith("socket:[") and target.endswith("]"):
            sockets.add(target[8:-1])
    found = []
    for protocol in ["tcp", "tcp6"]:
        for line in (Path("/proc/net") / protocol).read_text().splitlines()[1:]:
            fields = line.split()
            if fields[3] == "0A" and fields[9] in sockets:
                address, port = fields[1].split(":")
                found.append({"protocol": protocol, "address": address, "port": int(port, 16), "inode": fields[9]})
    return found

def main():
    p = argparse.ArgumentParser(description=__doc__)
    for flag in ["collector", "input", "out", "collector_sha256"]:
        p.add_argument("--" + flag.replace("_", "-"), required=True)
    for flag in ["startup_seconds", "request_seconds", "cleanup_seconds"]:
        p.add_argument("--" + flag.replace("_", "-"), type=float, required=True)
    args = p.parse_args()
    assert min(args.startup_seconds, args.request_seconds, args.cleanup_seconds) > 0
    binary, source, out = (Path(getattr(args, key)).resolve() for key in ["collector", "input", "out"])
    assert sha(binary) == args.collector_sha256
    out.mkdir(exist_ok=False)
    payload = source.read_bytes()
    document = json.loads(payload)
    metrics = [m for r in document["resourceMetrics"] for s in r["scopeMetrics"] for m in s["metrics"]]
    names = sorted({m["name"] for m in metrics})
    points = sum(len(m.get("sum", m.get("gauge"))["dataPoints"]) for m in metrics)
    with socket.socket() as reservation:
        reservation.bind(("127.0.0.1", 0))
        port = reservation.getsockname()[1]
    config = {
        "receivers": {"otlp": {"protocols": {"http": {"endpoint": "127.0.0.1:" + str(port)}}}},
        "exporters": {"debug": {"verbosity": "detailed"}},
        "service": {
            "telemetry": {"metrics": {"level": "none"}, "logs": {"encoding": "json", "sampling": {"enabled": False}}},
            "pipelines": {"metrics": {"receivers": ["otlp"], "exporters": ["debug"]}},
        },
    }
    config_path = out / "config.json"
    config_path.write_text(json.dumps(config, indent=2) + "\n")
    (out / "request.json").write_bytes(payload)
    command = [str(binary), "--config", str(config_path)]
    (out / "command.json").write_text(json.dumps(command) + "\n")
    before = len(list(Path("/proc/self/fd").iterdir()))
    proc = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, start_new_session=True)
    startup = concurrent.futures.Future()
    def read_logs():
        try:
            with (out / "collector.log").open("wb") as log:
                for line in iter(proc.stdout.readline, b""):
                    log.write(line)
                    if b"Everything is ready. Begin running and processing data." in line and not startup.done():
                        startup.set_result("ready")
        finally:
            if not startup.done():
                startup.set_result("ended-before-ready")
    reader = threading.Thread(target=read_logs)
    reader.start()
    result = {
        "collector": str(binary), "collector_sha256": sha(binary),
        "input": str(source), "input_bytes": len(payload), "input_sha256": sha(source),
        "expected_resources": len(document["resourceMetrics"]), "expected_metrics": len(metrics),
        "expected_points": points, "names": names, "port": port,
        "bounds": vars(args), "started_unix_ns": time.time_ns(), "http_passed": False,
    }
    try:
        assert startup.result(timeout=args.startup_seconds) == "ready", "startup did not establish readiness"
        owned = listeners(proc.pid)
        result["listeners"] = owned
        assert len(owned) == 1 and owned[0]["port"] == port and owned[0]["address"] == "0100007F", owned
        request = urllib.request.Request(
            "http://127.0.0.1:" + str(port) + "/v1/metrics",
            data=payload, headers={"Content-Type": "application/json"}, method="POST",
        )
        with urllib.request.urlopen(request, timeout=args.request_seconds) as response:
            data = response.read(len(payload) + 1)
            assert len(data) <= len(payload), "response exceeded this case's admitted byte envelope"
            result["status"] = response.status
            result["headers"] = dict(response.headers)
        (out / "response.json").write_bytes(data)
        response = json.loads(data)
        partial = response.get("partialSuccess", {})
        assert result["status"] == 200 and int(partial.get("rejectedDataPoints", "0")) == 0
        assert not partial.get("errorMessage", ""), "partial/warning is not this case's full acceptance"
        result["http_passed"] = True
    except Exception as error:
        result["error"] = {"type": type(error).__name__, "message": str(error)}
    finally:
        proc.terminate()
        result["cleanup_signals"] = [int(signal.SIGTERM)]
        try:
            result["collector_exit"] = proc.wait(timeout=args.cleanup_seconds)
        except subprocess.TimeoutExpired:
            os.killpg(proc.pid, signal.SIGKILL)
            result["cleanup_signals"].append(int(signal.SIGKILL))
            try:
                result["collector_exit"] = proc.wait(timeout=args.cleanup_seconds)
            except subprocess.TimeoutExpired:
                result["collector_exit"] = None
        reader.join(timeout=args.cleanup_seconds)
        result["log_reader_joined"] = not reader.is_alive()
        proc.stdout.close()
        result["controller_fds"] = {"before": before, "after": len(list(Path("/proc/self/fd").iterdir()))}
        log = (out / "collector.log").read_bytes()
        result["collector_named_metric_witness"] = {name: name.encode() in log for name in names}
        result["passed"] = (
            result["http_passed"] and result.get("collector_exit") == 0 and result["log_reader_joined"]
            and all(result["collector_named_metric_witness"].values())
            and result["controller_fds"]["before"] == result["controller_fds"]["after"]
        )
        (out / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result, indent=2))
    return 0 if result["passed"] else 1

if __name__ == "__main__":
    raise SystemExit(main())
